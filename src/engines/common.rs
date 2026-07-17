use bitcoin::Txid;
use bitvmx_bitcoin_rpc::types::BlockHeight;
use bitvmx_transaction_monitor::{monitor::Monitor, types::TypesToMonitor};
use std::cell::Cell;
use std::rc::Rc;
use tracing::{debug, error, info, warn};

use crate::{
    config::config::CoordinatorSettings,
    core::{
        dispatcher::{DispatchOutcome, Dispatcher},
        fee::FeeManager,
        funding::FundingManager,
        storage::CoordinatorStorage,
    },
    errors::BitcoinCoordinatorError,
    helper::now_secs,
    types::{CoordinatedTx, CoordinatorNews, FeeInfo, SpeedupKind, TransactionState, TxKind},
};

/// Shared service bundle held by both `SpeedupEngine` and `TransactionEngine`.
///
/// Both engines receive an `Rc<EngineContext>` so they share exactly the same
/// underlying storage, funding state, dispatcher, and fee engine.
pub struct EngineContext {
    pub monitor: Monitor,

    pub fee_manager: FeeManager,
    pub funding_manager: FundingManager,
    pub storage: Rc<CoordinatorStorage>,
    pub dispatcher: Dispatcher,

    pub coordinator_config: CoordinatorSettings,
    last_retry_at: Cell<Option<u64>>,
}

impl EngineContext {
    pub fn new(
        monitor: Monitor,
        fee_manager: FeeManager,
        funding_manager: FundingManager,
        dispatcher: Dispatcher,
        storage: Rc<CoordinatorStorage>,
        coordinator_config: CoordinatorSettings,
    ) -> Self {
        Self {
            storage,
            fee_manager,
            monitor,
            funding_manager,
            dispatcher,
            coordinator_config,
            last_retry_at: Cell::new(None),
        }
    }

    /// Reorg detected: a `Confirmed` tx reappeared in the mempool.
    /// Reset state to `InMempool` and refresh the broadcast height.
    pub fn mark_reorg(
        &self,
        tx: &CoordinatedTx,
        current_height: BlockHeight,
    ) -> Result<(), BitcoinCoordinatorError> {
        let mut updated = tx.clone();
        updated.state = TransactionState::InMempool;
        updated.broadcast_block_height = Some(current_height);
        self.storage.update_tx(&updated)?;
        Ok(())
    }

    /// Tx reached max confirmations. Settle as `Finalized`.
    pub fn mark_finalized(
        &self,
        txid: Txid,
        current_height: BlockHeight,
    ) -> Result<(), BitcoinCoordinatorError> {
        self.storage
            .settle_tx(txid, TransactionState::Finalized, current_height)?;
        Ok(())
    }

    /// Tx confirmed. Update state to `Confirmed`.
    pub fn mark_confirmed(&self, txid: Txid) -> Result<(), BitcoinCoordinatorError> {
        self.storage
            .update_tx_state(txid, TransactionState::Confirmed)?;
        Ok(())
    }

    /// Tx orphaned. Keep in `InMempool`.
    pub fn mark_orphan(&self, txid: Txid) -> Result<(), BitcoinCoordinatorError> {
        self.storage
            .update_tx_state(txid, TransactionState::InMempool)?;
        Ok(())
    }

    /// Filter out retry transactions if the retry interval has not elapsed.
    /// Shared by both engines so they use the same rate-limit window.
    pub fn apply_retry_rate_limit(&self, txs: Vec<CoordinatedTx>) -> Vec<CoordinatedTx> {
        let retry_ready = match self.last_retry_at.get() {
            None => true,
            Some(last) => {
                now_secs().saturating_sub(last) >= self.coordinator_config.retry_interval_seconds
            }
        };
        let mut has_retries = false;
        let filtered: Vec<CoordinatedTx> = txs
            .into_iter()
            .filter(|t| {
                if t.retry_count > 0 {
                    has_retries = true;
                    retry_ready
                } else {
                    true
                }
            })
            .collect();
        if has_retries && retry_ready {
            self.last_retry_at.set(Some(now_secs()));
        } else if has_retries {
            debug!("Skipping retry txs, retry interval not elapsed");
        }
        filtered
    }

    /// Handle a single dispatch outcome. Failures route into `fail_and_cascade`, which settles
    /// the tx Failed, resets spent flags, and recursively cascades into ToDispatch descendants.
    pub fn handle_dispatch_result(
        &self,
        tx: &CoordinatedTx,
        txid: Txid,
        outcome: DispatchOutcome,
        current_height: BlockHeight,
        fee_info: FeeInfo,
    ) -> Result<(), BitcoinCoordinatorError> {
        tx.verify_tx_id(txid)?; // sanity check
        match outcome {
            DispatchOutcome::Success => {
                self.mark_accepted(tx, txid, current_height, fee_info)?;
            }
            DispatchOutcome::Fatal(msg) => {
                // Deterministic pre-send rejection (e.g. oversize). No node probe needed.
                warn!("Transaction({}) fatal dispatch error: {}", txid, msg);
                self.fail_and_cascade(tx, current_height, false)?;
            }
            DispatchOutcome::ParentFailed(parent) => {
                // A tracked parent this tx spends from has settled Failed, so the tx is doomed. A first CPFP
                // rebuilds a fresh CPFP for the parents whose output is still live. For an RBF this clears
                // the predecessor's `replaced_by`, un-masking the root so it fails the same way next tick.
                warn!(
                    "Transaction({}) parent {} has Failed; settling Failed and re-queuing survivors",
                    txid, parent
                );
                self.settle_failed_child(tx, current_height)?;
            }
            DispatchOutcome::DispatchError(raw) => {
                self.classify_dispatch_error(tx, txid, &raw, current_height, fee_info)?;
            }
        }
        Ok(())
    }

    /// Settle a tx the node accepted (dispatch `Success`) or already had in its mempool.
    /// For an RBF, mark its predecessor `replaced_by`.
    fn mark_accepted(
        &self,
        tx: &CoordinatedTx,
        txid: Txid,
        current_height: BlockHeight,
        fee_info: FeeInfo,
    ) -> Result<(), BitcoinCoordinatorError> {
        if let TxKind::Speedup(SpeedupKind::RBF { replaces, .. }) = &tx.kind {
            if let Some(mut replaced) = self.storage.get_tx_by_id(*replaces)? {
                replaced.speedup_kind_mut()?.context_mut().replaced_by = Some(txid);
                self.storage.update_tx(&replaced)?;
            }
        }

        let mut updated = tx.clone();
        updated.state = TransactionState::InMempool;
        updated.broadcast_block_height = Some(current_height);
        updated.fee_info = fee_info;
        // The tx is live again: disarm any reorg-flap fail guard. A future not_found re-arms it.
        updated.fail_guard_until = None;
        self.storage.update_tx(&updated)?;

        self.monitor.monitor(
            TypesToMonitor::Transactions(vec![txid], tx.context.clone(), tx.confirmation_trigger),
            true,
        )?;
        info!(
            "Transaction({}) dispatched at block height {}",
            txid, current_height
        );
        Ok(())
    }

    /// Settle a tx the node reports already confirmed on-chain. For an RBF landing directly on-chain,
    /// mark its predecessor `replaced_by`. State is capped at `Confirmed`. Preserve the original
    /// broadcast height.
    fn mark_already_confirmed(
        &self,
        tx: &CoordinatedTx,
        txid: Txid,
        current_height: BlockHeight,
    ) -> Result<(), BitcoinCoordinatorError> {
        if let TxKind::Speedup(SpeedupKind::RBF { replaces, .. }) = &tx.kind {
            if let Some(mut replaced) = self.storage.get_tx_by_id(*replaces)? {
                replaced.speedup_kind_mut()?.context_mut().replaced_by = Some(txid);
                self.storage.update_tx(&replaced)?;
            }
        }

        let mut updated = tx.clone();
        updated.broadcast_block_height.get_or_insert(current_height); // only if missing
        updated.state = TransactionState::Confirmed;
        // Recovered on-chain: disarm the fail guard.
        updated.fail_guard_until = None;
        self.storage.update_tx(&updated)?;

        self.monitor.monitor(
            TypesToMonitor::Transactions(vec![txid], tx.context.clone(), tx.confirmation_trigger),
            true,
        )?;
        info!("Transaction({}) already confirmed on-chain", txid);
        Ok(())
    }

    /// Classify a raw broadcast failure by querying node state over RPC. Order:
    ///   1. Node already has the tx:
    ///        Some(0)    → in mempool        → accept (InMempool).
    ///        Some(n>=1) → confirmed         → Confirmed (review finalizes by depth).
    ///   2. None (absent): inspect inputs. A funding input gone → recreate; an external/parent input gone → fail.
    ///   3. Inputs intact, cause unknown (fee/policy/transient): retry until the budget is spent.
    fn classify_dispatch_error(
        &self,
        tx: &CoordinatedTx,
        txid: Txid,
        raw: &str,
        current_height: BlockHeight,
        fee_info: FeeInfo,
    ) -> Result<(), BitcoinCoordinatorError> {
        // Step 1. Does the node already have this tx, in the mempool or on chain?
        match self.monitor.get_tx_confirmations(&txid)? {
            Some(0) => {
                info!(
                    "Transaction({}) already in node mempool; treating as dispatched: {}",
                    txid, raw
                );
                return self.mark_accepted(tx, txid, current_height, fee_info);
            }
            Some(_) => {
                return self.mark_already_confirmed(tx, txid, current_height);
            }
            None => {}
        }

        // Step 2. The tx is absent from the node, so it is NOT mined, which makes a missing or spent input definitive:
        // a funding input gone means recreate, an external or parent input gone means fail (fail_and_cascade false).
        if let Some(funding_missing) = self.missing_input_kind(tx)? {
            // Reorg-flap guard. An "input consumed" verdict is reversible while a reorg is still unsettled.
            // Read more in `CoordinatedTx::fail_guard_until` docs.
            if let Some(deadline) = tx.fail_guard_until {
                if current_height < deadline {
                    debug!(
                        "Transaction({}) input consumed but within reorg-flap guard window \
                         (until block {}, now {}); deferring Failed and re-queuing: {}",
                        txid, deadline, current_height, raw
                    );
                    // mark_as_retry keeps it ToDispatch and paces the re-dispatch via the shared retry-interval window.
                    self.storage.mark_as_retry(txid)?;
                    return Ok(());
                }
            }
            if funding_missing {
                warn!(
                    "Transaction({}) funding input missing/spent; settling Failed to recreate: {}",
                    txid, raw
                );
                return self.fail_and_cascade(tx, current_height, true);
            }
            // External (non-funding) input gone. A first (batched) CPFP spends protocol-parent outputs
            // directly, so it rebuilds a fresh CPFP for the parents whose output is still live (one dead
            // parent must not sink the batch); anything else just fails and cascades.
            error!(
                "Transaction({}) external input missing/spent; settling Failed: {}",
                txid, raw
            );
            return self.settle_failed_child(tx, current_height);
        }

        // Step 3. Inputs intact, cause unknown. Retry until the budget is spent, then fail.
        if tx.retry_count + 1 >= self.coordinator_config.retry_attempts_sending_tx {
            warn!(
                "Transaction({}) failed after {} attempts: {}",
                txid,
                tx.retry_count + 1,
                raw
            );
            self.fail_and_cascade(tx, current_height, false)?;
        } else {
            debug!(
                "Transaction({}) dispatch failed (attempt {}/{}), will retry: {}",
                txid,
                tx.retry_count + 1,
                self.coordinator_config.retry_attempts_sending_tx,
                raw
            );
            self.storage.mark_as_retry(txid)?;
        }
        Ok(())
    }

    /// Settle `tx` Failed (applying spent-flag reset and re-add semantics), then recursively settle every ToDispatch
    /// speedup whose primary funding input references `tx.txid`.
    /// `funding_missing` for the root reflects the actual failure cause: only when true does the root re-add its
    ///  NeedsSpeedup parents. For cascade victims we always pass `true`: their funding input is the dead parent's
    /// change output, which is by definition missing. Boost victims naturally skip the re-add.
    pub fn fail_and_cascade(
        &self,
        tx: &CoordinatedTx,
        current_height: BlockHeight,
        funding_missing: bool,
    ) -> Result<(), BitcoinCoordinatorError> {
        self.settle_failed_dispatch(tx, current_height, funding_missing)?;

        let speedups = self.storage.get_speedups_ordered()?;
        for d in speedups
            .iter()
            .filter(|s| s.state == TransactionState::ToDispatch)
        {
            // Cascade target if EITHER (a) the speedup's primary funding input was produced
            // by `tx` (boost-of-CPFP chain), or (b) `tx.txid` appears in the speedup's
            // `parents` list (CPFP covering a NeedsSpeedup parent that just failed).
            let depends_on_tx = match d.speedup_kind() {
                Ok(k) => {
                    k.context()
                        .funding_inputs
                        .first()
                        .map_or(false, |fi| fi.txid == tx.txid)
                        || k.parents().contains(&tx.txid)
                }
                Err(_) => false,
            };
            if depends_on_tx {
                warn!(
                    txid = %d.txid,
                    "cascade-fail: primary funding parent {} is Failed; settling Failed",
                    tx.txid,
                );
                // Descendants are always treated as funding-missing: their primary funding is gone,
                //so any non-boost CPFP victim must rebuild (funding-missing true).
                self.fail_and_cascade(d, current_height, true)?;
            }
        }
        Ok(())
    }

    /// Settle a transaction that can never be valid because a tracked parent it spends has settled Failed.
    fn settle_failed_child(
        &self,
        tx: &CoordinatedTx,
        current_height: BlockHeight,
    ) -> Result<(), BitcoinCoordinatorError> {
        // A first CPFP is the only speedup that spends protocol-parent outputs directly and so can rebuild survivors.
        let is_first_cpfp = tx.speedup_kind().map(|k| !k.is_boost()).unwrap_or(false);
        if is_first_cpfp {
            self.rebuild_survivors(tx, current_height)
        } else {
            self.fail_and_cascade(tx, current_height, false)
        }
    }

    /// A batched CPFP failed because a covered parent settled Failed. Settle the CPFP Failed (cascading to
    /// its descendants and releasing its funding), then re-queue every covered parent except the ones that
    /// have themselves settled Failed, so a parent that is only transiently absent keeps its acceleration.
    fn rebuild_survivors(
        &self,
        tx: &CoordinatedTx,
        current_height: BlockHeight,
    ) -> Result<(), BitcoinCoordinatorError> {
        // Classify each covered parent by its own settled state, not by an instantaneous UTXO probe. A parent
        // counts as dead only once it has settled Failed, which already waited out its own reorg-flap guard window.
        let mut survivors: Vec<Txid> = Vec::new();
        let mut dead: Vec<Txid> = Vec::new();
        for op in tx.non_funding_inputs() {
            let failed = self
                .storage
                .get_tx_by_id(op.txid)?
                .map_or(false, |p| p.state == TransactionState::Failed);
            if failed {
                dead.push(op.txid);
            } else {
                survivors.push(op.txid);
            }
        }

        // Settle the CPFP Failed: emits SpeedupDispatchError, cascades ToDispatch descendants, releases its funding.
        self.fail_and_cascade(tx, current_height, false)?;

        // Re-queue only still-live parents that are still NeedsSpeedup.
        let mut to_requeue: Vec<Txid> = Vec::new();
        for p in &survivors {
            if to_requeue.contains(p) {
                continue;
            }
            if let Some(parent) = self.storage.get_tx_by_id(*p)? {
                if matches!(parent.kind, TxKind::NeedsSpeedup(_)) {
                    to_requeue.push(*p);
                }
            }
        }
        self.storage.prepend_pending_speedup_parents(&to_requeue)?;

        if to_requeue.is_empty() {
            warn!(
                "CPFP({}) failed: all parent output(s) dead {:?}; nothing to rebuild",
                tx.txid, dead
            );
        } else {
            info!(
                "CPFP({}) failed: dead parent(s) {:?} dropped, re-queuing {} live parent(s) {:?} for a fresh CPFP",
                tx.txid,
                dead,
                to_requeue.len(),
                to_requeue
            );
        }
        Ok(())
    }

    /// Settle Failed, emit news, reset spent flags on parents (CPFP only. RBF skips), reset `replaced_by` on
    /// an RBF predecessor, and on funding-missing re-add NeedsSpeedup parents to PendingSpeedupParents.
    fn settle_failed_dispatch(
        &self,
        tx: &CoordinatedTx,
        current_height: BlockHeight,
        funding_missing: bool,
    ) -> Result<(), BitcoinCoordinatorError> {
        self.storage
            .settle_tx(tx.txid, TransactionState::Failed, current_height)?;
        self.storage
            .add_news(Self::dispatch_error_news(tx, tx.txid))?;

        // Only CPFPs reset spent flags and re-add parents. RBFs skip both since they don't reserve their parents' UTXOs.
        if let TxKind::Speedup(SpeedupKind::CPFP { .. }) = &tx.kind {
            self.funding_manager.mark_parents_unspent(tx)?;
        }

        // If this is an RBF: release only the funding inputs this RBF newly claimed (inherited inputs stay marked
        // because the predecessor still reserves them), and clear the `replaced_by` flag on its predecessor.
        if let TxKind::Speedup(SpeedupKind::RBF {
            replaces,
            new_funding_inputs,
            ..
        }) = &tx.kind
        {
            self.funding_manager.release_marks(new_funding_inputs)?;
            if let Some(mut replaced) = self.storage.get_tx_by_id(*replaces)? {
                if let Ok(ctx) = replaced.speedup_kind_mut() {
                    ctx.context_mut().replaced_by = None;
                    self.storage.update_tx(&replaced)?;
                }
            }
        }

        // On funding-missing failures, re-add NeedsSpeedup parents to PendingSpeedupParents so they can be retried in the next tick.
        if funding_missing {
            self.storage.requeue_protocol_parents(tx)?;
        }
        Ok(())
    }

    /// Probe the node for a missing or spent input of `tx`. Returns:
    ///   * `Some(true)`: a coordinator funding input is gone, recoverable by recreating funding.
    ///   * `Some(false)`: a non-funding or external (parent) input is gone, fatal for this tx.
    ///   * `None`: every input is still unspent, so the failure is transient (fee or policy), retry.
    fn missing_input_kind(
        &self,
        tx: &CoordinatedTx,
    ) -> Result<Option<bool>, BitcoinCoordinatorError> {
        // Funding inputs first, since a gone funding UTXO is recoverable.
        for op in tx.funding_outpoints() {
            if !self.monitor.is_utxo_unspent_rpc(&op.txid, op.vout, true)? {
                return Ok(Some(true));
            }
        }

        // Remaining external or parent inputs, where a gone one is fatal for this tx.
        for op in tx.non_funding_inputs() {
            if !self.monitor.is_utxo_unspent_rpc(&op.txid, op.vout, true)? {
                return Ok(Some(false));
            }
        }

        Ok(None)
    }

    fn dispatch_error_news(tx: &CoordinatedTx, txid: Txid) -> CoordinatorNews {
        if matches!(tx.kind, TxKind::Speedup(_)) {
            CoordinatorNews::SpeedupDispatchError {
                txid,
                context: tx.context.clone(),
            }
        } else {
            CoordinatorNews::DispatchError {
                txid,
                context: tx.context.clone(),
            }
        }
    }
}
