use std::rc::Rc;

use bitcoin::{Transaction, Txid};
use bitvmx_bitcoin_rpc::types::BlockHeight;
use bitvmx_transaction_monitor::{monitor::Monitor, types::TypesToMonitor};
use key_manager::key_manager::KeyManager;
use protocol_builder::{
    builder::ProtocolBuilder,
    types::{output::SpeedupData, Utxo},
};
use tracing::{debug, info, warn};

use crate::{
    config::{
        config::SpeedupSettings,
        settings::{CPFP_TRANSACTION_CONTEXT, RBF_TRANSACTION_CONTEXT},
    },
    core::{
        dispatcher::{DispatchOutcome, Dispatcher},
        fee::FeeEngine,
        funding::FundingManager,
        storage::CoordinatorStorage,
    },
    errors::BitcoinCoordinatorError,
    types::{
        CoordinatedTx, CoordinatorNews, FeeInfo, SpeedupContext, SpeedupKind, TransactionState,
        TxKind,
    },
};

pub struct SpeedupEngine {
    pub settings: SpeedupSettings,
    key_manager: Rc<KeyManager>,
}

impl SpeedupEngine {
    pub fn new(settings: SpeedupSettings, key_manager: Rc<KeyManager>) -> Self {
        Self {
            settings,
            key_manager,
        }
    }

    // ── CPFP creation ─────────────────────────────────────────────────────────

    /// Build and store CPFP transactions for a batch of newly-dispatched parent
    /// transactions.  Parents are batched by weight up to the available ancestor
    /// slots; each CPFP spends the change output of the previous one.
    ///
    /// Early exits:
    /// - All ancestor slots are occupied by in-mempool CPFPs.
    /// - No funding UTXO is set.
    /// - The computed fee exceeds the available funding balance.
    pub fn create_cpfps_for_parents(
        &self,
        parents: &[CoordinatedTx],
        storage: &CoordinatorStorage,
        fee_engine: &FeeEngine,
        monitor: &Monitor,
        funding_manager: &FundingManager,
        dispatcher: &Dispatcher,
        max_weight: u64,
        current_height: BlockHeight,
        retry_attempts_sending_tx: u32,
    ) -> Result<(), BitcoinCoordinatorError> {
        // How many more CPFPs fit in the ancestor chain before hitting the node limit.
        let unconfirmed = storage.get_unconfirmed_speedups()?;
        let available_slots = self
            .settings
            .max_unconfirmed_speedups
            .saturating_sub(unconfirmed.len() as u32);
        if available_slots == 0 {
            return Ok(());
        }

        let (fee_rate, fee_news) = fee_engine.get_network_fee_rate(monitor)?;
        if let Some(news) = fee_news {
            storage.add_news(news)?;
        }

        let mut funding = match funding_manager.get_funding()? {
            Some(f) => f,
            None => {
                storage.add_news(CoordinatorNews::FundingNotAvailable)?;
                return Ok(());
            }
        };

        // Fee delta already covered by in-mempool ancestors.  The new child must
        // pay the shortfall to bring the whole package up to the target fee rate.
        let (chain_diff_fee, chain_vsize) = fee_engine.chain_fee_diff(fee_rate, &unconfirmed);
        let bump_fee = fee_engine.base_fee_multiplier();
        let batches = Self::batch_parents_by_weight(parents, max_weight, available_slots);

        for batch in batches {
            let parent_entries: Vec<(SpeedupData, usize)> = batch
                .iter()
                .filter_map(|p| {
                    if let TxKind::NeedsSpeedup(ref sd) = p.kind {
                        Some((sd.clone(), p.tx.vsize()))
                    } else {
                        None //TODO: is should never reach here because parents are supposed to be filtered beforehand
                    }
                })
                .collect();

            if parent_entries.is_empty() {
                continue;
            }

            let parent_txids: Vec<Txid> = batch.iter().map(|p| p.txid).collect();

            let (cpfp_tx, fee_paid) = self.build_cpfp(
                fee_engine,
                &parent_entries,
                &funding,
                bump_fee,
                false,
                fee_rate,
                chain_diff_fee,
                chain_vsize,
            )?;

            let context = Self::make_speedup_context(&funding, bump_fee, &parent_entries);
            let fee_info = fee_engine.compute_fee_for_tx(&cpfp_tx, fee_rate);
            let kind = SpeedupKind::CPFP {
                parents: parent_txids,
                context,
            };

            let Some(change_utxo) = self.commit_speedup(
                cpfp_tx,
                fee_paid,
                fee_info,
                kind,
                storage,
                monitor,
                funding_manager,
                dispatcher,
                current_height,
                retry_attempts_sending_tx,
            )?
            else {
                storage.add_news(CoordinatorNews::InsufficientFunds {
                    available: funding.amount,
                    required: fee_paid,
                })?;
                break;
            };

            // Advance the funding UTXO to the change output so the next batch
            // spends the correct output.
            funding = change_utxo;
        }

        Ok(())
    }

    // ── Boost(CPFP) / RBF ───────────────────────────────────────────────────────────

    /// Build and store a boost CPFP or RBF transaction if the latest in-mempool
    /// speedup is stale and no other speedup is pending dispatch.
    ///
    /// A **boost** is a fresh CPFP with no new parents — it pays a higher fee to
    /// drag the whole package to a higher fee rate.  An **RBF** replaces the
    /// last in-mempool speedup entirely once the ancestor slot limit is reached.
    ///
    /// Early exits:
    /// - No in-mempool speedup exists yet (nothing to boost).
    /// - The latest in-mempool speedup was broadcast too recently (not stale).
    /// - A speedup is already pending dispatch — wait for the chain to clear.
    /// - No funding UTXO is set.
    /// - The computed fee exceeds the available funding balance.
    pub fn boost_if_stale(
        &self,
        storage: &CoordinatorStorage,
        fee_engine: &FeeEngine,
        monitor: &Monitor,
        funding_manager: &FundingManager,
        dispatcher: &Dispatcher,
        current_height: BlockHeight,
        retry_attempts_sending_tx: u32,
    ) -> Result<(), BitcoinCoordinatorError> {
        let all_speedups = storage.get_speedups_ordered()?;

        // Find the last active (non-replaced) in-mempool speedup.
        let last = match all_speedups.iter().rev().find(|tx| {
            tx.state == TransactionState::InMempool
                && !matches!(&tx.kind, TxKind::Speedup(k) if k.context().is_being_replaced())
        }) {
            Some(t) => t,
            None => return Ok(()), // No speedup to boost yet.
        };

        let broadcast_height = match last.broadcast_block_height {
            Some(h) => h,
            None => return Ok(()), // The speedup is not broadcast yet
        };

        // The speedup was broadcast too recently — do not bump yet.
        if current_height.saturating_sub(broadcast_height)
            < self.settings.min_blocks_before_resend_speedup
        {
            return Ok(());
        }

        // A speedup is already queued for dispatch — wait for it to land first.
        if all_speedups
            .iter()
            .any(|tx| tx.state == TransactionState::ToDispatch)
        {
            return Ok(());
        }

        let last_context = match &last.kind {
            TxKind::Speedup(k) => k.context(),
            _ => {
                tracing::warn!(txid = %last.txid, "expected Speedup kind in boost_if_stale; skipping"); // should never reach here //ASK: //TODO: return a BitcoinCoordinatorError (new error, like unexpected or somthing for all this cases)
                return Ok(());
            }
        };

        let (fee_rate, fee_news) = fee_engine.get_network_fee_rate(monitor)?;
        if let Some(news) = fee_news {
            storage.add_news(news)?;
        }

        let funding = match funding_manager.get_funding()? {
            Some(f) => f,
            None => {
                storage.add_news(CoordinatorNews::FundingNotAvailable)?;
                return Ok(());
            }
        };

        // Increase the bump fee multiplicatively to satisfy RBF relay rules.
        let next_bump = last_context.bump_fee_used * self.settings.bump_fee_percentage;
        let last_txid = last.txid;

        // Collect unconfirmed speedups once; derive both the RBF decision and
        // the chain-fee-diff from the same slice.
        let unconfirmed: Vec<_> = all_speedups
            .iter()
            .filter(|tx| tx.state == TransactionState::InMempool)
            .cloned()
            .collect();

        // Switch to RBF once the unconfirmed count reaches the ancestor limit.
        let use_rbf = (unconfirmed.len() as u32) >= self.settings.max_unconfirmed_speedups;
        let (chain_diff_fee, chain_vsize) = fee_engine.chain_fee_diff(fee_rate, &unconfirmed);

        // For RBF: re-include the original parents so the replacement covers their fees.
        // For boost: a standalone CPFP with no new parents.
        let parent_entries: Vec<(SpeedupData, usize)> = if use_rbf {
            last_context
                .parent_data
                .iter()
                .map(|(sd, _, vs)| (sd.clone(), *vs))
                .collect()
        } else {
            vec![]
        };

        let (new_tx, fee_paid) = self.build_cpfp(
            fee_engine,
            &parent_entries,
            &funding,
            next_bump,
            use_rbf,
            fee_rate,
            chain_diff_fee,
            chain_vsize,
        )?;

        let context = Self::make_speedup_context(&funding, next_bump, &parent_entries);
        let fee_info = fee_engine.compute_fee_for_tx(&new_tx, fee_rate);
        let kind = if use_rbf {
            SpeedupKind::RBF {
                replaces: last_txid,
                context,
            }
        } else {
            SpeedupKind::CPFP {
                parents: vec![],
                context,
            }
        };

        if self
            .commit_speedup(
                new_tx,
                fee_paid,
                fee_info,
                kind,
                storage,
                monitor,
                funding_manager,
                dispatcher,
                current_height,
                retry_attempts_sending_tx,
            )?
            .is_none()
        {
            storage.add_news(CoordinatorNews::InsufficientFunds {
                available: funding.amount,
                required: fee_paid,
            })?;
        }

        Ok(())
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Build a CPFP (or RBF) transaction via the fee convergence loop.
    ///
    /// Bitcoin fees depend on vsize, but signing changes vsize.  This loop:
    /// 1. Builds a dummy tx to measure the initial vsize.
    /// 2. Computes the target fee for that vsize.
    /// 3. Builds the real tx with that fee; if vsize grew, repeats until stable.
    ///
    /// Returns `(transaction, fee_paid_sats)`.
    fn build_cpfp(
        &self,
        fee_engine: &FeeEngine,
        parent_entries: &[(SpeedupData, usize)],
        funding: &Utxo,
        bump_fee: f64,
        is_rbf: bool,
        fee_rate: u64,
        chain_diff_fee: u64,
        chain_vsize: usize,
    ) -> Result<(Transaction, u64), BitcoinCoordinatorError> {
        let speedups_data: Vec<SpeedupData> =
            parent_entries.iter().map(|(d, _)| d.clone()).collect();

        let fee_entries: Vec<(u64, usize)> = parent_entries
            .iter()
            .map(|(data, vsize)| (amount_from_speedup_data(data), *vsize))
            .collect();

        let mut child_vsize = 0usize;
        loop {
            let dummy_vsize = ProtocolBuilder {}
                .speedup_transactions(
                    &speedups_data,
                    funding.clone(),
                    &funding.pub_key,
                    10_000,
                    &self.key_manager,
                )?
                .vsize();

            if child_vsize == 0 {
                child_vsize = dummy_vsize;
            }

            let fee = fee_engine.compute_speedup_fee(
                &fee_entries,
                child_vsize,
                bump_fee,
                fee_rate,
                is_rbf,
                chain_diff_fee,
                chain_vsize,
            );

            let final_tx = ProtocolBuilder {}.speedup_transactions(
                &speedups_data,
                funding.clone(),
                &funding.pub_key,
                fee,
                &self.key_manager,
            )?;

            let final_vsize = final_tx.vsize();
            if child_vsize >= final_vsize {
                return Ok((final_tx, fee));
            }
            child_vsize = final_vsize;
        }
    }

    /// Build a `SpeedupContext` from the fields common to every speedup record.
    fn make_speedup_context(
        funding: &Utxo,
        bump_fee: f64,
        parent_entries: &[(SpeedupData, usize)],
    ) -> SpeedupContext {
        SpeedupContext {
            funding_input: funding.clone(),
            replaced_by: None,
            bump_fee_used: bump_fee,
            parent_data: parent_entries
                .iter()
                .map(|(sd, vs)| (sd.clone(), amount_from_speedup_data(sd), *vs))
                .collect(),
        }
    }

    fn ctx_for_kind(kind: &SpeedupKind) -> &'static str {
        match kind {
            SpeedupKind::RBF { .. } => RBF_TRANSACTION_CONTEXT,
            SpeedupKind::CPFP { .. } => CPFP_TRANSACTION_CONTEXT,
        }
    }

    /// Store the completed CPFP/RBF, register it with the monitor, and advance
    /// the funding UTXO to the change output.
    ///
    /// Returns `Some(change_utxo)` on success, or `None` if the fee exceeds
    /// the available balance (caller should emit `InsufficientFunds` and stop).
    fn commit_speedup(
        &self,
        tx: Transaction,
        fee_paid: u64,
        fee_info: FeeInfo,
        kind: SpeedupKind,
        storage: &CoordinatorStorage,
        monitor: &Monitor,
        funding_manager: &FundingManager,
        dispatcher: &Dispatcher,
        current_height: BlockHeight,
        retry_attempts_sending_tx: u32,
    ) -> Result<Option<Utxo>, BitcoinCoordinatorError> {
        let funding = &kind.context().funding_input;
        if fee_paid >= funding.amount {
            return Ok(None);
        }

        let ctx = Self::ctx_for_kind(&kind);
        let txid = tx.compute_txid();
        let vout_change = (tx.output.len() - 1) as u32;
        let change_utxo = Utxo::new(
            txid,
            vout_change,
            funding.amount.saturating_sub(fee_paid),
            &funding.pub_key,
        );
        let record = CoordinatedTx {
            txid,
            tx,
            kind: TxKind::Speedup(kind),
            state: TransactionState::ToDispatch,
            broadcast_block_height: None,
            target_block_height: current_height,
            stuck_in_mempool_blocks: None,
            confirmation_trigger: None,
            settled_block_height: None,
            retry_count: 0,
            fee_info,
            context: ctx.to_string(),
        };

        storage.insert_speedup(record.clone())?;
        monitor.monitor(
            TypesToMonitor::Transactions(vec![txid], ctx.to_string(), None),
            false,
        )?;
        self.dispatch_speedup(
            &record,
            dispatcher,
            storage,
            monitor,
            current_height,
            retry_attempts_sending_tx,
        )?;
        funding_manager.update_funding(change_utxo.clone())?;

        Ok(Some(change_utxo))
    }

    /// Broadcast a single speedup transaction and update its state.
    ///
    /// Returns `true` if the tx landed in the mempool (Success or AlreadyKnown),
    /// `false` if it failed (retry or fatal) — callers use this to stop chained dispatch.
    fn dispatch_speedup(
        &self,
        tx: &CoordinatedTx,
        dispatcher: &Dispatcher,
        storage: &CoordinatorStorage,
        monitor: &Monitor,
        current_height: BlockHeight,
        retry_attempts_sending_tx: u32,
    ) -> Result<bool, BitcoinCoordinatorError> {
        let results = dispatcher.dispatch(vec![tx.tx.clone()]);
        for (txid, outcome) in results {
            match outcome {
                DispatchOutcome::Success | DispatchOutcome::AlreadyKnown => {
                    if matches!(outcome, DispatchOutcome::AlreadyKnown) {
                        warn!("Speedup({}) already known — treating as in-mempool", txid);
                    }
                    let mut updated = tx.clone();
                    updated.state = TransactionState::InMempool;
                    updated.broadcast_block_height = Some(current_height);
                    storage.update_tx(&updated)?;
                    monitor.monitor(
                        TypesToMonitor::Transactions(
                            vec![txid],
                            tx.context.clone(),
                            tx.confirmation_trigger,
                        ),
                        true,
                    )?;
                    if let TxKind::Speedup(SpeedupKind::RBF { replaces, .. }) = &tx.kind {
                        if let Some(mut replaced) = storage.get_tx_by_id(*replaces)? {
                            if let TxKind::Speedup(ref mut k) = replaced.kind {
                                k.context_mut().replaced_by = Some(txid);
                            }
                            storage.update_tx(&replaced)?;
                        }
                    }
                    info!(
                        "Speedup({}) dispatched at block height {}",
                        txid, current_height
                    );
                    return Ok(true);
                }
                DispatchOutcome::Retryable(msg) => {
                    if tx.retry_count + 1 >= retry_attempts_sending_tx {
                        warn!(
                            "Speedup({}) failed after {} attempts: {}",
                            txid,
                            tx.retry_count + 1,
                            msg
                        );
                        storage.settle_tx(txid, TransactionState::Failed, current_height)?;
                        storage.add_news(CoordinatorNews::SpeedupDispatchError {
                            txid,
                            context: tx.context.clone(),
                        })?;
                    } else {
                        debug!(
                            "Speedup({}) dispatch failed (attempt {}/{}) — will retry: {}",
                            txid,
                            tx.retry_count + 1,
                            retry_attempts_sending_tx,
                            msg
                        );
                        storage.mark_as_retry(txid)?;
                    }
                    return Ok(false);
                }
                DispatchOutcome::Fatal(msg) => {
                    warn!("Speedup({}) fatal dispatch error: {}", txid, msg);
                    storage.settle_tx(txid, TransactionState::Failed, current_height)?;
                    storage.add_news(CoordinatorNews::SpeedupDispatchError {
                        txid,
                        context: tx.context.clone(),
                    })?;
                    return Ok(false);
                }
            }
        }
        Ok(false)
    }

    /// Review in-flight speedups and dispatch any that are pending.
    ///
    /// Phase 1 — review active (InMempool/Confirmed) speedups:
    /// - `is_in_mempool()` + was `Confirmed` → reorg: reset to InMempool
    /// - `is_not_found()` + being replaced → skip (wait for replacement)
    /// - `is_not_found()` → reset to ToDispatch; restore funding (oldest only)
    /// - `is_finalized()` → settle; cancel monitoring for superseded RBF target
    /// - `is_confirmed()` → update state
    /// - `is_orphan()` → keep InMempool
    ///
    /// Phase 2 — dispatch all ToDispatch speedups in creation order; stop on
    /// first failure to preserve the funding-UTXO chain ordering.
    pub fn review_in_flight(
        &self,
        storage: &CoordinatorStorage,
        monitor: &Monitor,
        funding_manager: &FundingManager,
        dispatcher: &Dispatcher,
        current_height: BlockHeight,
        retry_attempts_sending_tx: u32,
    ) -> Result<(), BitcoinCoordinatorError> {
        let all_speedups = storage.get_speedups_ordered()?;
        if all_speedups.is_empty() {
            return Ok(());
        }

        let max_confs = monitor.settings.max_monitoring_confirmations;
        let mut funding_restored = false;

        for tx in &all_speedups {
            if !matches!(
                tx.state,
                TransactionState::InMempool | TransactionState::Confirmed
            ) {
                continue;
            }

            let status = monitor.get_tx_status(&tx.txid, true)?;

            if status.is_in_mempool() {
                if tx.state == TransactionState::Confirmed {
                    let mut updated = tx.clone();
                    updated.state = TransactionState::InMempool;
                    updated.broadcast_block_height = Some(current_height);
                    storage.update_tx(&updated)?;
                }
                continue;
            }

            let context = match &tx.kind {
                TxKind::Speedup(k) => k.context(),
                _ => {
                    tracing::warn!(txid = %tx.txid, "non-speedup tx in speedup list; skipping");
                    continue;
                }
            };

            if status.is_not_found() {
                if context.is_being_replaced() {
                    continue;
                }
                storage.update_tx_state(tx.txid, TransactionState::ToDispatch)?;
                if !funding_restored {
                    funding_manager.update_funding(context.funding_input.clone())?;
                    funding_restored = true;
                }
                continue;
            }

            if status.is_finalized(max_confs) {
                storage.settle_tx(tx.txid, TransactionState::Finalized, current_height)?;
                if let TxKind::Speedup(SpeedupKind::RBF { replaces, .. }) = &tx.kind {
                    monitor.cancel(TypesToMonitor::Transactions(
                        vec![*replaces],
                        tx.context.clone(),
                        None,
                    ))?;
                }
                continue;
            }

            if status.is_confirmed() {
                storage.update_tx_state(tx.txid, TransactionState::Confirmed)?;
                continue;
            }

            if status.is_orphan() {
                storage.update_tx_state(tx.txid, TransactionState::InMempool)?;
            }
        }

        for tx in storage
            .get_speedups_ordered()?
            .iter()
            .filter(|tx| tx.state == TransactionState::ToDispatch)
        {
            if !self.dispatch_speedup(
                tx,
                dispatcher,
                storage,
                monitor,
                current_height,
                retry_attempts_sending_tx,
            )? {
                break;
            }
        }

        Ok(())
    }

    /// Batch parents by weight, returning at most `max_batches` batches.
    fn batch_parents_by_weight<'a>(
        parents: &'a [CoordinatedTx],
        max_weight: u64,
        max_batches: u32,
    ) -> Vec<Vec<&'a CoordinatedTx>> {
        let mut batches: Vec<Vec<&CoordinatedTx>> = Vec::new();
        let mut current_batch: Vec<&CoordinatedTx> = Vec::new();
        let mut current_weight = 0u64;

        for parent in parents {
            if batches.len() as u32 >= max_batches {
                break;
            }
            let weight = parent.tx.weight().to_wu();
            if !current_batch.is_empty() && current_weight + weight > max_weight {
                batches.push(current_batch);
                current_batch = Vec::new();
                current_weight = 0;
            }
            current_batch.push(parent);
            current_weight += weight;
        }

        if !current_batch.is_empty() && (batches.len() as u32) < max_batches {
            batches.push(current_batch);
        }

        batches
    }
}

fn amount_from_speedup_data(data: &SpeedupData) -> u64 {
    data.utxo
        .as_ref()
        .map(|u| u.amount)
        .or_else(|| data.partial_utxo.map(|(_, _, a)| a))
        .unwrap_or(0)
}
