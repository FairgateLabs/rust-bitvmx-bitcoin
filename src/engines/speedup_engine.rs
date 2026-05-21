use std::rc::Rc;

use bitcoin::{Transaction, Txid};
use bitvmx_bitcoin_rpc::types::BlockHeight;
use bitvmx_transaction_monitor::types::TypesToMonitor;
use key_manager::key_manager::KeyManager;
use protocol_builder::{
    builder::ProtocolBuilder,
    types::{
        output::{SpeedupData, MAX_DUST_LIMIT},
        Utxo,
    },
};
use tracing::{debug, info, warn};

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

/// SpeedupEngine implements the four speedup-related phases of `tick()`:
/// 1. `dispatch_pending_speedups` — broadcast TO-DISPATCH speedups built in a prior tick.
/// 2. `review_speedups`           — update state from chain (no dispatch).
/// 3. `boost_if_stale`            — build a boost CPFP or RBF, save as TO-DISPATCH (no dispatch).
/// 4. `create_cpfp_batch`         — build one CPFP for the next PendingSpeedupParents batch, save as TO-DISPATCH (no dispatch).
///
/// Invariants:
/// - At most one TO-DISPATCH speedup at a time (more only in reorg/restart edge cases).
/// - Build/save happens in one tick; dispatch happens the next tick.
/// - Boost takes priority over new CPFP.
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

    /// Step 4 of `tick`: broadcast TO-DISPATCH speedups (built in a prior tick, or
    /// re-queued this tick by `review_speedups` on a not_found observation).
    ///
    /// Funding guard: If no funding source is currently available, skip the step entirely.
    /// Any pre-built speedup whose `fee_info.fee_rate` is below the current `min_safe_fee_rate`
    /// setting is settled `Failed` and its CPFP parents are re-queued into PendingSpeedupParents.
    /// On Fatal or retries-exhausted Retryable, the speedup is settled `Failed` and the user is
    /// notified via news. The CPFP's parents are NOT re-queued into PendingSpeedupParents (except
    /// "speedup disappeared from mempool" case)
    pub fn dispatch_pending_speedups(&self) -> Result<(), BitcoinCoordinatorError> {
        let current_height = self.ctx.monitor.get_monitor_height()?;
        let all_speedups = self.ctx.storage.get_speedups_ordered()?;

        if !all_speedups
            .iter()
            .any(|tx| tx.state == TransactionState::ToDispatch)
        {
            return Ok(());
        }

        if self
            .ctx
            .funding_manager
            .get_funding(&all_speedups)?
            .is_none()
        {
            self.ctx
                .storage
                .add_news(CoordinatorNews::FundingNotAvailable)?;
            return Ok(());
        }

        let pending: Vec<CoordinatedTx> = all_speedups
            .into_iter()
            .filter(|tx| tx.state == TransactionState::ToDispatch)
            .collect();
        let dispatchable = self.ctx.apply_retry_rate_limit(pending);
        for tx in &dispatchable {
            let _ = self.try_dispatch_speedup(tx, current_height)?;
        }
        Ok(())
    }

    /// Step 2 of `tick`: update each speedup's state from chain. Never dispatches.
    /// Reorg of a Confirmed speedup moves it back to InMempool
    pub fn review_speedups(&self) -> Result<(), BitcoinCoordinatorError> {
        let current_height = self.ctx.monitor.get_monitor_height()?;
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
                continue;
            }

            let status = self.ctx.monitor.get_tx_status(&tx.txid, true)?;

            if status.is_in_mempool() {
                if tx.state == TransactionState::Confirmed {
                    self.ctx.handle_reorg(tx, current_height)?;
                }
                continue;
            }

            // The tx is no longer in mempool and not on chain. If an RBF is replacing it.
            if status.is_not_found() {
                // If an RBF is replacing it, `remove_replaced_rbf` will clean it up when the RBF finalizes.
                if matches!(&tx.kind, TxKind::Speedup(k) if k.context().is_being_replaced()) {
                    continue;
                }
                // Otherwise, re-queue the same tx for dispatch this tick in step 4 sends the exact same tx. Possible outcomes:
                //   - AlreadyKnown / AlreadyConfirmed → false positive; revert to InMempool / Confirmed.
                //   - Success                         → re-broadcast accepted; back to InMempool.
                debug!(
                    txid = %tx.txid,
                    state = ?tx.state,
                    "speedup not in mempool / chain; re-queueing the same tx for dispatch",
                );
                self.ctx
                    .storage
                    .update_tx_state(tx.txid, TransactionState::ToDispatch)?;
                continue;
            }

            if status.is_finalized(max_confs) {
                self.ctx.handle_finalized(tx.txid, current_height)?;
                self.remove_replaced_rbf(tx, current_height)?;
                // Advance base funding to the last finalized on-chain change output.
                self.ctx.funding_manager.update_funding_from_tx(tx)?;
                continue;
            }

            if status.is_confirmed() {
                self.ctx.handle_confirmed(tx.txid)?;
                continue;
            }

            if status.is_orphan() {
                self.ctx.handle_orphan(tx.txid)?;
                continue;
            }
        }

        Ok(())
    }

    /// Step 5 of `tick`: if the latest live speedup is stale, build a boost (new
    /// CPFP when slots are available, otherwise RBF) and save it as TO-DISPATCH.
    /// Never dispatches. Short-circuits if any speedup is already TO-DISPATCH.
    pub fn boost_if_stale(&self) -> Result<(), BitcoinCoordinatorError> {
        let current_height = self.ctx.monitor.get_monitor_height()?;
        let all_speedups = self.ctx.storage.get_speedups_ordered()?;

        let (last_txid, next_bump, use_rbf, parent_entries) = {
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

            // Short-circuit if any speedup is already TO-DISPATCH to avoid building multiple boosts in the same tick
            if all_speedups
                .iter()
                .any(|tx| tx.state == TransactionState::ToDispatch)
            {
                return Ok(());
            }

            let last_context = match &last.kind {
                TxKind::Speedup(k) => k.context(),
                _ => {
                    warn!(txid = %last.txid, "expected Speedup kind in boost_if_stale; skipping");
                    return Ok(());
                }
            };

            let inmempool_count = all_speedups
                .iter()
                .filter(|tx| tx.state == TransactionState::InMempool)
                .count() as u32;
            let use_rbf = inmempool_count >= self.settings.max_unconfirmed_speedups;
            let parent_entries: Vec<(SpeedupData, usize)> = if use_rbf {
                last_context
                    .parent_data
                    .iter()
                    .map(|(sd, _, vs)| (sd.clone(), *vs))
                    .collect()
            } else {
                vec![]
            };

            (
                last.txid,
                last_context.bump_fee_used * self.settings.bump_fee_percentage,
                use_rbf,
                parent_entries,
            )
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

        let unconfirmed: Vec<CoordinatedTx> = all_speedups
            .into_iter()
            .filter(|tx| tx.state == TransactionState::InMempool)
            .collect();

        let (chain_diff_fee, chain_vsize) =
            self.ctx.fee_manager.chain_fee_diff(fee_rate, &unconfirmed);

        let Some((new_tx, _fee_paid)) = self.build_speedup(
            &parent_entries,
            &funding,
            next_bump,
            use_rbf,
            fee_rate,
            chain_diff_fee,
            chain_vsize,
        )?
        else {
            emit_funding_news_for_speedup(&self.ctx, &funding)?;
            return Ok(());
        };

        let context = Self::make_speedup_context(&funding, next_bump, &parent_entries);
        let fee_info = self.ctx.fee_manager.compute_fee_for_tx(&new_tx, fee_rate);
        let kind = if use_rbf {
            SpeedupKind::RBF {
                replaces: last_txid,
                context,
            }
        } else {
            SpeedupKind::CPFP {
                parents: vec![last_txid],
                context,
            }
        };
        self.save_speedup(new_tx, fee_info, kind, current_height)?;

        Ok(())
    }

    /// Step 6 of `tick`: build a single CPFP covering the next batch of PendingSpeedupParents parents and
    /// save it as TO-DISPATCH. Never dispatches. Short-circuits when a TO-DISPATCH speedup already exists
    /// or when funding is not available.
    pub fn create_cpfp_batch(&self) -> Result<(), BitcoinCoordinatorError> {
        let parents = self.ctx.storage.get_pending_speedup_parents()?;
        if parents.is_empty() {
            return Ok(());
        }
        let current_height = self.ctx.monitor.get_monitor_height()?;
        let all_speedups = self.ctx.storage.get_speedups_ordered()?;

        if all_speedups
            .iter()
            .any(|tx| tx.state == TransactionState::ToDispatch)
        {
            return Ok(());
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

        let unconfirmed: Vec<CoordinatedTx> = all_speedups
            .into_iter()
            .filter(|tx| tx.state == TransactionState::InMempool)
            .collect();
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

        let (chain_diff_fee, chain_vsize) =
            self.ctx.fee_manager.chain_fee_diff(fee_rate, &unconfirmed);
        let bump_fee = self.ctx.fee_manager.base_fee_multiplier();

        // Take only the first batch: one CPFP per tick.
        let mut batches = self.ctx.dispatcher.batch_by_weight(&parents, 1);
        let batch = match batches.pop() {
            Some(b) => b,
            None => return Ok(()),
        };

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
            return Ok(());
        }

        let parent_txids: Vec<Txid> = batch.iter().map(|p| p.txid).collect();

        let Some((cpfp_tx, _fee_paid)) = self.build_speedup(
            &parent_entries,
            &funding,
            bump_fee,
            false,
            fee_rate,
            chain_diff_fee,
            chain_vsize,
        )?
        else {
            emit_funding_news_for_speedup(&self.ctx, &funding)?;
            return Ok(());
        };

        // Build succeeded: pull these parents out of PendingSpeedupParents and persist the CPFP.
        for parent_txid in &parent_txids {
            self.ctx
                .storage
                .remove_pending_speedup_parent(*parent_txid)?;
        }

        let context = Self::make_speedup_context(&funding, bump_fee, &parent_entries);
        let fee_info = self.ctx.fee_manager.compute_fee_for_tx(&cpfp_tx, fee_rate);
        let kind = SpeedupKind::CPFP {
            parents: parent_txids,
            context,
        };
        self.save_speedup(cpfp_tx, fee_info, kind, current_height)?;

        Ok(())
    }

    // =========================================================================
    // Private helpers
    // =========================================================================

    /// Build a CPFP or RBF transaction via the fee convergence loop.
    /// Returns `Ok(Some((tx, fee)))` on success.
    fn build_speedup(
        &self,
        parent_entries: &[(SpeedupData, usize)],
        funding: &Utxo,
        bump_fee: f64,
        is_rbf: bool,
        fee_rate: u64,
        chain_diff_fee: u64,
        chain_vsize: usize,
    ) -> Result<Option<(Transaction, u64)>, BitcoinCoordinatorError> {
        let speedups_data: Vec<SpeedupData> =
            parent_entries.iter().map(|(d, _)| d.clone()).collect();

        let fee_entries: Vec<(u64, usize)> = parent_entries
            .iter()
            .map(|(data, vsize)| (amount_from_speedup_data(data), *vsize))
            .collect();

        let mut child_vsize = 0usize;
        loop {
            // Use a nominal fee of 1 sat so the probe build always produces a
            // valid change output regardless of funding size.
            let dummy_vsize = ProtocolBuilder {}
                .speedup_transactions(
                    &speedups_data,
                    funding.clone(),
                    &funding.pub_key,
                    1,
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

            // If the fee would leave a below dust change output,
            // signal insufficient funding.
            if funding.amount.saturating_sub(fee) < MAX_DUST_LIMIT {
                return Ok(None);
            }

            let final_tx = ProtocolBuilder {}.speedup_transactions(
                &speedups_data,
                funding.clone(),
                &funding.pub_key,
                fee,
                &self.key_manager,
            )?;

            let final_vsize = final_tx.vsize();
            if child_vsize >= final_vsize {
                return Ok(Some((final_tx, fee)));
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

    /// Persist a freshly-built CPFP/RBF as TO-DISPATCH and register it with the monitor.
    fn save_speedup(
        &self,
        tx: Transaction,
        fee_info: FeeInfo,
        kind: SpeedupKind,
        current_height: BlockHeight,
    ) -> Result<(), BitcoinCoordinatorError> {
        let ctx_str = Self::ctx_for_kind(&kind);
        let txid = tx.compute_txid();
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
        match record.speedup_kind()? {
            SpeedupKind::RBF { replaces, .. } => {
                info!("Built RBF | Txid({}) | Replaces({})", record.txid, replaces)
            }
            SpeedupKind::CPFP { parents, .. } => info!(
                "Built CPFP | Txid({}) | Parents({:?})",
                record.txid, parents
            ),
        }
        self.ctx.monitor.monitor(
            TypesToMonitor::Transactions(vec![txid], ctx_str.to_string(), None),
            false,
        )?;
        Ok(())
    }

    /// Broadcast a single speedup transaction and update its state per the dispatch outcome.
    fn try_dispatch_speedup(
        &self,
        tx: &CoordinatedTx,
        current_height: BlockHeight,
    ) -> Result<(), BitcoinCoordinatorError> {
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

    // // Verify the pre-built speedup's fee rate against the current `min_safe_fee_rate` setting.
    // // Below-floor speedups are settled Failed; covered parents are NOT re-queued —
    // // the operator is responsible for the chosen floor.
    // fn verify_min_fee_rate(
    //     &self,
    //     txs: Vec<CoordinatedTx>,
    //     current_height: BlockHeight,
    // ) -> Result<Vec<CoordinatedTx>, BitcoinCoordinatorError> {
    //     let min_safe_fee_rate = self.ctx.fee_manager.settings.min_safe_fee_rate;
    //     let mut dispatchable = Vec::new();
    //     for tx in txs {
    //         if tx.fee_info.fee_rate < min_safe_fee_rate {
    //             warn!(
    //                 txid = %tx.txid,
    //                 fee_rate = tx.fee_info.fee_rate,
    //                 min_safe_fee_rate,
    //                 "pre-built speedup fee_rate below min_safe_fee_rate; settling Failed",
    //             );
    //             self.ctx
    //                 .storage
    //                 .settle_tx(tx.txid, TransactionState::Failed, current_height)?;
    //             continue;
    //         }
    //         dispatchable.push(tx);
    //     }
    //     Ok(dispatchable)
    // }
}

// Funding too small. Emit FundingConsumed; also emit
// InsufficientFunds when the queue is now empty.
fn emit_funding_news_for_speedup(
    ctx: &EngineContext,
    funding: &Utxo,
) -> Result<(), BitcoinCoordinatorError> {
    ctx.storage.add_news(CoordinatorNews::FundingConsumed {
        txid: funding.txid,
        vout: funding.vout,
        amount: funding.amount,
    })?;
    if ctx.funding_manager.advance_funding()?.is_none() {
        ctx.storage.add_news(CoordinatorNews::InsufficientFunds {
            available: funding.amount,
            required: funding.amount,
        })?;
    }
    Ok(())
}

fn amount_from_speedup_data(data: &SpeedupData) -> u64 {
    data.utxo
        .as_ref()
        .map(|u| u.amount)
        .or_else(|| data.partial_utxo.map(|(_, _, a)| a))
        .unwrap_or(0)
}
