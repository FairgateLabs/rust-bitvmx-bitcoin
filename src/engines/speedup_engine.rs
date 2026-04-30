use std::rc::Rc;

use bitcoin::{Transaction, Txid};
use bitvmx_bitcoin_rpc::types::BlockHeight;
use bitvmx_transaction_monitor::types::TypesToMonitor;
use key_manager::key_manager::KeyManager;
use protocol_builder::{
    builder::ProtocolBuilder,
    types::{output::SpeedupData, Utxo},
};

use crate::{
    config::{
        config::SpeedupSettings,
        settings::{CPFP_TRANSACTION_CONTEXT, RBF_TRANSACTION_CONTEXT},
    },
    engines::common::EngineContext,
    errors::BitcoinCoordinatorError,
    helper::verify_single_dispatch_result,
    types::{
        CoordinatedTx, CoordinatorNews, FeeInfo, SpeedupContext, SpeedupKind, TransactionState,
        TxKind,
    },
};

pub struct SpeedupEngine {
    ctx: Rc<EngineContext>,
    key_manager: Rc<KeyManager>,
    settings: SpeedupSettings,
}

impl SpeedupEngine {
    pub fn new(
        ctx: Rc<EngineContext>,
        key_manager: Rc<KeyManager>,
        settings: SpeedupSettings,
    ) -> Self {
        Self {
            ctx,
            key_manager,
            settings,
        }
    }

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
    ) -> Result<(), BitcoinCoordinatorError> {
        if parents.is_empty() {
            return Ok(());
        }
        let current_height = self.ctx.monitor.get_monitor_height()?;

        // Don't create new CPFPs while evicted speedups are pending re-dispatch —
        // their pre-built txs already claim the UTXOs in the funding chain.
        let all_speedups = self.ctx.storage.get_speedups_ordered()?;
        if all_speedups
            .iter()
            .any(|tx| tx.state == TransactionState::ToDispatch)
        {
            return Ok(());
        }

        let unconfirmed = self.ctx.storage.get_unconfirmed_speedups()?;
        let available_slots = self
            .settings
            .max_unconfirmed_speedups
            .saturating_sub(unconfirmed.len() as u32);
        if available_slots == 0 {
            return Ok(());
        }

        let (fee_rate, fee_news) = self
            .ctx
            .fee_manager
            .get_network_fee_rate(&self.ctx.monitor)?;
        if let Some(news) = fee_news {
            self.ctx.storage.add_news(news)?;
        }

        let mut funding = match self.ctx.funding_manager.get_funding(&all_speedups)? {
            Some(f) => f,
            None => {
                self.ctx
                    .storage
                    .add_news(CoordinatorNews::FundingNotAvailable)?;
                return Ok(());
            }
        };

        let (chain_diff_fee, chain_vsize) =
            self.ctx.fee_manager.chain_fee_diff(fee_rate, &unconfirmed);
        let bump_fee = self.ctx.fee_manager.base_fee_multiplier();
        let batches = self
            .ctx
            .dispatcher
            .batch_by_weight(parents, available_slots);

        for batch in batches {
            let parent_entries: Vec<(SpeedupData, usize)> = batch
                .iter()
                .filter_map(|p| {
                    if let TxKind::NeedsSpeedup(ref sd) = p.kind {
                        Some((sd.clone(), p.tx.vsize()))
                    } else {
                        None
                    }
                })
                .collect();

            if parent_entries.is_empty() {
                continue;
            }

            let parent_txids: Vec<Txid> = batch.iter().map(|p| p.txid).collect();

            let (cpfp_tx, fee_paid) = self.build_cpfp(
                &parent_entries,
                &funding,
                bump_fee,
                false,
                fee_rate,
                chain_diff_fee,
                chain_vsize,
            )?;

            let context = Self::make_speedup_context(&funding, bump_fee, &parent_entries);
            let fee_info = self.ctx.fee_manager.compute_fee_for_tx(&cpfp_tx, fee_rate);
            let kind = SpeedupKind::CPFP {
                parents: parent_txids,
                context,
            };

            let Some(change_utxo) =
                self.commit_speedup(cpfp_tx, fee_paid, fee_info, kind, current_height)?
            else {
                self.ctx
                    .storage
                    .add_news(CoordinatorNews::InsufficientFunds {
                        available: funding.amount,
                        required: fee_paid,
                    })?;
                break;
            };

            funding = change_utxo;
        }

        Ok(())
    }

    pub fn process_active_transactions(&self) -> Result<(), BitcoinCoordinatorError> {
        let current_height = self.ctx.monitor.get_monitor_height()?;
        self.review_in_flight(current_height)?;
        self.boost_if_stale(current_height)?;
        Ok(())
    }

    // =========================================================================
    // Private helpers
    // =========================================================================

    /// Build and store a boost CPFP or RBF transaction if the latest in-mempool
    /// speedup is stale and no other speedup is pending dispatch.
    fn boost_if_stale(&self, current_height: BlockHeight) -> Result<(), BitcoinCoordinatorError> {
        let all_speedups = self.ctx.storage.get_speedups_ordered()?;

        let last = match all_speedups.iter().rev().find(|tx| {
            tx.state == TransactionState::InMempool
                && !matches!(&tx.kind, TxKind::Speedup(k) if k.context().is_being_replaced())
        }) {
            Some(t) => t,
            None => return Ok(()),
        };

        let broadcast_height = match last.broadcast_block_height {
            Some(h) => h,
            None => return Ok(()),
        };

        if current_height.saturating_sub(broadcast_height)
            < self.settings.min_blocks_before_resend_speedup
        {
            return Ok(());
        }

        if all_speedups
            .iter()
            .any(|tx| tx.state == TransactionState::ToDispatch)
        {
            return Ok(());
        }

        let last_context = match &last.kind {
            TxKind::Speedup(k) => k.context(),
            _ => {
                tracing::warn!(txid = %last.txid, "expected Speedup kind in boost_if_stale; skipping");
                return Ok(());
            }
        };

        let (fee_rate, fee_news) = self
            .ctx
            .fee_manager
            .get_network_fee_rate(&self.ctx.monitor)?;
        if let Some(news) = fee_news {
            self.ctx.storage.add_news(news)?;
        }

        let funding = match self.ctx.funding_manager.get_funding(&all_speedups)? {
            Some(f) => f,
            None => {
                self.ctx
                    .storage
                    .add_news(CoordinatorNews::FundingNotAvailable)?;
                return Ok(());
            }
        };

        let next_bump = last_context.bump_fee_used * self.settings.bump_fee_percentage;
        let last_txid = last.txid;

        let unconfirmed: Vec<_> = all_speedups
            .iter()
            .filter(|tx| tx.state == TransactionState::InMempool)
            .cloned()
            .collect();

        let use_rbf = (unconfirmed.len() as u32) >= self.settings.max_unconfirmed_speedups;
        let (chain_diff_fee, chain_vsize) =
            self.ctx.fee_manager.chain_fee_diff(fee_rate, &unconfirmed);

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
            &parent_entries,
            &funding,
            next_bump,
            use_rbf,
            fee_rate,
            chain_diff_fee,
            chain_vsize,
        )?;

        let context = Self::make_speedup_context(&funding, next_bump, &parent_entries);
        let fee_info = self.ctx.fee_manager.compute_fee_for_tx(&new_tx, fee_rate);
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
            .commit_speedup(new_tx, fee_paid, fee_info, kind, current_height)?
            .is_none()
        {
            self.ctx
                .storage
                .add_news(CoordinatorNews::InsufficientFunds {
                    available: funding.amount,
                    required: fee_paid,
                })?;
        }

        Ok(())
    }

    /// Review in-flight speedups and dispatch any that are pending.
    ///
    /// Phase 1 — review active (`InMempool`/`Confirmed`) speedups:
    /// common helpers handle reorg, finalized, confirmed, orphan;
    /// speedup-specific arms handle funding restore and RBF cancel.
    ///
    /// Phase 2 — dispatch all `ToDispatch` speedups in creation order;
    /// stop on first failure to preserve the funding-UTXO chain ordering.
    fn review_in_flight(&self, current_height: BlockHeight) -> Result<(), BitcoinCoordinatorError> {
        let all_speedups = self.ctx.storage.get_speedups_ordered()?;
        if all_speedups.is_empty() {
            return Ok(());
        }

        let max_confs = self.ctx.monitor.settings.max_monitoring_confirmations;

        for tx in &all_speedups {
            if !matches!(
                tx.state,
                TransactionState::InMempool | TransactionState::Confirmed
            ) {
                // Nothing to review until the tx is broadcast, finalized, or evicted.
                continue;
            }

            let status = self.ctx.monitor.get_tx_status(&tx.txid, true)?;

            if status.is_in_mempool() {
                if tx.state == TransactionState::Confirmed {
                    self.ctx.handle_reorg(tx, current_height)?;
                }
                // Already live in the mempool — stale detection is handled by boost_if_stale.
                continue;
            }

            let context = tx.speedup_kind()?.context();

            if status.is_not_found() {
                if context.is_being_replaced() {
                    continue;
                }
                self.ctx
                    .storage
                    .update_tx_state(tx.txid, TransactionState::ToDispatch)?;
                continue;
            }

            if status.is_finalized(max_confs) {
                self.ctx.handle_finalized(tx.txid, current_height)?;
                self.remove_replaced_rbf(tx, current_height)?;

                // Advance base funding to the last finalized on-chain change output.
                let k = tx.speedup_kind()?;
                let (out, vout) = tx.last_output()?;
                self.ctx.funding_manager.update_funding(Utxo::new(
                    tx.txid,
                    vout,
                    out.value.to_sat(),
                    &k.context().funding_input.pub_key,
                ))?;
                continue;
            }

            if status.is_confirmed() {
                self.ctx.handle_confirmed(tx.txid)?;
                continue;
            }

            if status.is_orphan() {
                self.ctx.handle_orphan(tx.txid)?;
            }
        }

        // Phase 2: dispatch all pending `ToDispatch` speedups in order.
        let pending: Vec<CoordinatedTx> = self
            .ctx
            .storage
            .get_speedups_ordered()?
            .into_iter()
            .filter(|tx| tx.state == TransactionState::ToDispatch)
            .collect();
        let pending = self.ctx.apply_retry_rate_limit(pending);
        for tx in &pending {
            if !self.dispatch_speedup(tx, current_height)? {
                break; // stop on first failure to preserve funding-UTXO chain ordering
            }
        }

        Ok(())
    }

    /// Build a CPFP (or RBF) transaction via the fee convergence loop.
    fn build_cpfp(
        &self,
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

            let fee = self.ctx.fee_manager.compute_speedup_fee(
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

    /// Store the completed CPFP/RBF, dispatch it immediately, and advance
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
        current_height: BlockHeight,
    ) -> Result<Option<Utxo>, BitcoinCoordinatorError> {
        let funding = &kind.context().funding_input;
        if fee_paid >= funding.amount {
            return Ok(None);
        }

        let ctx_str = Self::ctx_for_kind(&kind);
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
            context: ctx_str.to_string(),
        };

        self.ctx.storage.insert_speedup(record.clone())?;
        self.ctx.monitor.monitor(
            TypesToMonitor::Transactions(vec![txid], ctx_str.to_string(), None),
            false,
        )?;
        self.dispatch_speedup(&record, current_height)?;
        Ok(Some(change_utxo))
    }

    /// Broadcast a single speedup transaction and update its state.
    ///
    /// Returns `true` if the tx landed in the mempool (Success or AlreadyKnown),
    /// `false` on failure — callers stop chained dispatch on `false`.
    fn dispatch_speedup(
        &self,
        tx: &CoordinatedTx,
        current_height: BlockHeight,
    ) -> Result<bool, BitcoinCoordinatorError> {
        let results = self.ctx.dispatcher.dispatch(vec![tx.tx.clone()]);
        let (txid, outcome) = verify_single_dispatch_result(tx.txid, results)?;
        self.ctx
            .handle_dispatch_result(tx, txid, outcome, current_height, tx.fee_info.clone())
    }

    /// Remove any replaced by RBF transactions from monitoring and mark them as failed
    fn remove_replaced_rbf(
        &self,
        tx: &CoordinatedTx,
        current_height: BlockHeight,
    ) -> Result<(), BitcoinCoordinatorError> {
        if let TxKind::Speedup(SpeedupKind::RBF { replaces, .. }) = &tx.kind {
            let mut current_id = *replaces;
            loop {
                let next_id =
                    self.ctx
                        .storage
                        .get_tx_by_id(current_id)?
                        .and_then(|prev| match &prev.kind {
                            TxKind::Speedup(SpeedupKind::RBF {
                                replaces: older, ..
                            }) => Some(*older),
                            _ => None,
                        });
                self.ctx.monitor.cancel(TypesToMonitor::Transactions(
                    vec![current_id],
                    tx.context.clone(),
                    None,
                ))?;
                self.ctx
                    .storage
                    .settle_tx(current_id, TransactionState::Failed, current_height)?;
                match next_id {
                    Some(older) => current_id = older,
                    None => break,
                }
            }
        }
        Ok(())
    }
}

fn amount_from_speedup_data(data: &SpeedupData) -> u64 {
    data.utxo
        .as_ref()
        .map(|u| u.amount)
        .or_else(|| data.partial_utxo.map(|(_, _, a)| a))
        .unwrap_or(0)
}
