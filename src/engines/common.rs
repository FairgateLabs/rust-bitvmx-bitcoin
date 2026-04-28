use std::{cell::Cell, rc::Rc};

use bitcoin::Txid;
use bitvmx_bitcoin_rpc::types::BlockHeight;
use bitvmx_transaction_monitor::{monitor::Monitor, types::TypesToMonitor};
use tracing::{debug, info, warn};

use crate::{
    core::{
        dispatcher::{DispatchOutcome, Dispatcher},
        fee::FeeEngine,
        funding::FundingManager,
        storage::CoordinatorStorage,
    },
    errors::BitcoinCoordinatorError,
    helper::now_secs,
    types::{CoordinatedTx, CoordinatorNews, FeeInfo, SpeedupKind, TransactionState, TxKind},
};

/// Retry and dispatch settings shared by both engines.
#[derive(Debug, Clone)]
pub struct DispatchConfig {
    pub retry_attempts_sending_tx: u32,
    pub retry_interval_seconds: u64,
}

/// Shared service bundle held by both `SpeedupEngine` and `TransactionEngine`.
///
/// Both engines receive an `Rc<EngineContext>` so they share exactly the same
/// underlying storage, funding state, dispatcher, and fee engine — no copies.
pub struct EngineContext {
    pub storage: CoordinatorStorage,
    pub fee_engine: FeeEngine,
    pub monitor: Rc<Monitor>,
    pub funding_manager: FundingManager,
    pub dispatcher: Dispatcher,
    pub dispatch_config: DispatchConfig,
    last_retry_at: Cell<Option<u64>>,
}

impl EngineContext {
    pub fn new(
        storage: CoordinatorStorage,
        fee_engine: FeeEngine,
        monitor: Rc<Monitor>,
        funding_manager: FundingManager,
        dispatcher: Dispatcher,
        dispatch_config: DispatchConfig,
    ) -> Self {
        Self {
            storage,
            fee_engine,
            monitor,
            funding_manager,
            dispatcher,
            dispatch_config,
            last_retry_at: Cell::new(None),
        }
    }

    // ── Shared state-transition helpers ──────────────────────────────────────

    /// Transition a successfully-dispatched tx to `InMempool` and enable
    /// monitor mempool search.
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
    pub fn handle_reorg(
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

    /// Tx reached max confirmations — settle as `Finalized`.
    pub fn handle_finalized(
        &self,
        txid: Txid,
        current_height: BlockHeight,
    ) -> Result<(), BitcoinCoordinatorError> {
        self.storage
            .settle_tx(txid, TransactionState::Finalized, current_height)?;
        Ok(())
    }

    /// Tx confirmed — update state to `Confirmed`.
    pub fn handle_confirmed(&self, txid: Txid) -> Result<(), BitcoinCoordinatorError> {
        self.storage
            .update_tx_state(txid, TransactionState::Confirmed)?;
        Ok(())
    }

    /// Tx orphaned — keep in `InMempool`.
    pub fn handle_orphan(&self, txid: Txid) -> Result<(), BitcoinCoordinatorError> {
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
                now_secs().saturating_sub(last) >= self.dispatch_config.retry_interval_seconds
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
            debug!("Skipping retry txs — retry interval not elapsed");
        }
        filtered
    }

    /// Handle a single dispatch outcome.
    ///
    /// Returns `true` if the tx was accepted into the mempool.
    pub fn handle_dispatch_result(
        &self,
        tx: &CoordinatedTx,
        txid: Txid,
        outcome: DispatchOutcome,
        current_height: BlockHeight,
        fee_info: FeeInfo,
    ) -> Result<bool, BitcoinCoordinatorError> {
        match outcome {
            DispatchOutcome::Success | DispatchOutcome::AlreadyKnown => {
                if matches!(outcome, DispatchOutcome::AlreadyKnown) {
                    warn!("tx({}) already known — treating as in-mempool", txid);
                }
                self.on_dispatch_success(tx, txid, current_height, fee_info)?;
                Ok(true)
            }
            DispatchOutcome::Retryable(msg) => {
                if tx.retry_count + 1 >= self.dispatch_config.retry_attempts_sending_tx {
                    warn!(
                        "tx({}) failed after {} attempts: {}",
                        txid,
                        tx.retry_count + 1,
                        msg
                    );
                    self.storage
                        .settle_tx(txid, TransactionState::Failed, current_height)?;
                    self.storage.add_news(Self::dispatch_error_news(tx, txid))?;
                } else {
                    debug!(
                        "tx({}) dispatch failed (attempt {}/{}) — will retry: {}",
                        txid,
                        tx.retry_count + 1,
                        self.dispatch_config.retry_attempts_sending_tx,
                        msg
                    );
                    self.storage.mark_as_retry(txid)?;
                }
                Ok(false)
            }
            DispatchOutcome::Fatal(msg) => {
                warn!("tx({}) fatal dispatch error: {}", txid, msg);
                self.storage
                    .settle_tx(txid, TransactionState::Failed, current_height)?;
                self.storage.add_news(Self::dispatch_error_news(tx, txid))?;
                Ok(false)
            }
        }
    }

    fn on_dispatch_success(
        &self,
        tx: &CoordinatedTx,
        txid: Txid,
        current_height: BlockHeight,
        fee_info: FeeInfo,
    ) -> Result<(), BitcoinCoordinatorError> {
        if let TxKind::Speedup(SpeedupKind::RBF { replaces, .. }) = &tx.kind {
            if let Some(mut replaced) = self.storage.get_tx_by_id(*replaces)? {
                if let TxKind::Speedup(ref mut k) = replaced.kind {
                    k.context_mut().replaced_by = Some(txid);
                }
                self.storage.update_tx(&replaced)?;
            }
        }
        self.mark_dispatched(tx, current_height, fee_info)?;
        info!("tx({}) dispatched at block height {}", txid, current_height);
        Ok(())
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
