use bitcoin::Txid;
use bitvmx_bitcoin_rpc::types::BlockHeight;
use bitvmx_transaction_monitor::{monitor::Monitor, types::TypesToMonitor};
use std::cell::Cell;
use std::rc::Rc;
use tracing::{debug, info, warn};

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

    /// Transition a successfully-dispatched tx to `InMempool` and enable monitor mempool search.
    pub fn mark_dispatched(
        &self,
        tx: &CoordinatedTx,
        current_height: BlockHeight,
        fee_info: FeeInfo,
    ) -> Result<(), BitcoinCoordinatorError> {
        let mut updated = tx.clone();
        updated.state = TransactionState::InMempool;
        updated.broadcast_block_height = Some(current_height);
        updated.fee_info = fee_info;
        self.storage.update_tx(&updated)?;
        self.monitor.monitor(
            TypesToMonitor::Transactions(
                vec![tx.txid],
                tx.context.clone(),
                tx.confirmation_trigger,
            ),
            true,
        )?;
        Ok(())
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
            DispatchOutcome::Success | DispatchOutcome::AlreadyKnown => {
                if let TxKind::Speedup(SpeedupKind::RBF { replaces, .. }) = &tx.kind {
                    if let Some(mut replaced) = self.storage.get_tx_by_id(*replaces)? {
                        replaced.speedup_kind_mut()?.context_mut().replaced_by = Some(txid);
                        self.storage.update_tx(&replaced)?;
                    }
                }
                self.mark_dispatched(tx, current_height, fee_info)?;
                info!(
                    "Transaction({}) dispatched at block height {}",
                    txid, current_height
                );
            }
            DispatchOutcome::AlreadyConfirmed => {
                // If an RBF lands directly on-chain, mark the predecessor as replaced.
                if let TxKind::Speedup(SpeedupKind::RBF { replaces, .. }) = &tx.kind {
                    if let Some(mut replaced) = self.storage.get_tx_by_id(*replaces)? {
                        replaced.speedup_kind_mut()?.context_mut().replaced_by = Some(txid);
                        self.storage.update_tx(&replaced)?;
                    }
                }
                debug!("Transaction({}) already confirmed on-chain", txid);
                self.mark_confirmed(txid)?;
            }
            DispatchOutcome::Retryable(msg) => {
                if tx.retry_count + 1 >= self.coordinator_config.retry_attempts_sending_tx {
                    warn!(
                        "Transaction({}) failed after {} attempts: {}",
                        txid,
                        tx.retry_count + 1,
                        msg
                    );
                    self.fail_and_cascade(tx, current_height, false)?;
                } else {
                    debug!(
                        "Transaction({}) dispatch failed (attempt {}/{}), will retry: {}",
                        txid,
                        tx.retry_count + 1,
                        self.coordinator_config.retry_attempts_sending_tx,
                        msg
                    );
                    self.storage.mark_as_retry(txid)?;
                }
            }
            DispatchOutcome::Fatal(msg) => {
                warn!("Transaction({}) fatal dispatch error: {}", txid, msg);
                self.fail_and_cascade(tx, current_height, false)?;
            }
            DispatchOutcome::MissingInput(msg) => {
                warn!(
                    "Transaction({}) missing-input dispatch error: {}",
                    txid, msg
                );
                let funding_missing = self.is_funding_input_missing(tx)?;
                self.fail_and_cascade(tx, current_height, funding_missing)?;
            }
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
            let depends_on_tx = match d.speedup_kind() {
                Ok(k) => k
                    .context()
                    .funding_inputs
                    .first()
                    .map_or(false, |fi| fi.txid == tx.txid),
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

        // If this is an RBF, clear the `replaced_by` flag on its predecessor so it can be re-boosted by a future boost.
        if let TxKind::Speedup(SpeedupKind::RBF { replaces, .. }) = &tx.kind {
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

    // Check if any funding input is missing from the mempool.
    fn is_funding_input_missing(
        &self,
        tx: &CoordinatedTx,
    ) -> Result<bool, BitcoinCoordinatorError> {
        let k = match tx.speedup_kind() {
            Ok(k) => k,
            Err(_) => return Ok(false),
        };
        for fi in &k.context().funding_inputs {
            // Check in real-time on the mempool.
            let unspent = self.monitor.is_utxo_unspent_rpc(&fi.txid, fi.vout)?;
            if !unspent {
                return Ok(true);
            }
        }
        Ok(false)
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
