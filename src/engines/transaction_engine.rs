use std::rc::Rc;

use crate::{
    engines::common::EngineContext,
    errors::BitcoinCoordinatorError,
    helper::find_tx_in_batch,
    types::{CoordinatedTx, CoordinatorNews, TransactionState, TxKind},
};
use bitcoin::Transaction;
use bitvmx_bitcoin_rpc::types::BlockHeight;
use tracing::{debug, error, info, warn};

pub struct TransactionEngine {
    pub ctx: Rc<EngineContext>,
}

impl TransactionEngine {
    pub fn new(ctx: Rc<EngineContext>) -> Self {
        Self { ctx }
    }

    /// Step 1 of `tick`: walk in-flight non-speedup transactions and update
    /// their state from the chain. Never dispatches.
    pub fn review_active(&self) -> Result<(), BitcoinCoordinatorError> {
        let current_height = self.ctx.monitor.get_monitor_height()?;
        self.ctx.storage.evict_stale_txs(current_height)?;

        let active_txs = self.ctx.storage.get_active_txs()?;
        if active_txs.is_empty() {
            return Ok(());
        }

        let to_review: Vec<CoordinatedTx> = active_txs
            .into_iter()
            .filter(|tx| {
                !matches!(tx.kind, TxKind::Speedup(_))
                    && matches!(
                        tx.state,
                        TransactionState::InMempool | TransactionState::Confirmed
                    )
            })
            .collect();

        if to_review.is_empty() {
            return Ok(());
        }

        self.review_transactions(to_review, current_height)?;
        Ok(())
    }

    /// Step 3 of `tick`: broadcast every non-speedup transaction currently in
    /// `ToDispatch` whose `target_block_height` has been reached.
    pub fn dispatch_pending(&self) -> Result<(), BitcoinCoordinatorError> {
        let current_height = self.ctx.monitor.get_monitor_height()?;
        let active_txs = self.ctx.storage.get_active_txs()?;
        if active_txs.is_empty() {
            return Ok(());
        }

        let mut to_dispatch: Vec<CoordinatedTx> = Vec::new();
        for tx in active_txs {
            if matches!(tx.kind, TxKind::Speedup(_)) {
                continue;
            }
            if tx.state == TransactionState::ToDispatch && tx.is_ready_to_dispatch(current_height) {
                to_dispatch.push(tx);
            }
        }

        if to_dispatch.is_empty() {
            return Ok(());
        }

        debug!("Dispatching {} pending transactions", to_dispatch.len());
        self.dispatch_batch(to_dispatch, current_height)?;
        Ok(())
    }

    // =========================================================================
    // Private helpers
    // =========================================================================

    fn review_transactions(
        &self,
        txs: Vec<CoordinatedTx>,
        current_height: BlockHeight,
    ) -> Result<(), BitcoinCoordinatorError> {
        let max_confs = self.ctx.monitor.settings.max_monitoring_confirmations;

        for tx in txs {
            // Only search the mempool after the tx has been broadcast.
            let search_in_mempool = tx.broadcast_block_height.is_some();
            let status = self
                .ctx
                .monitor
                .get_tx_status(&tx.txid, search_in_mempool)?;

            if status.is_in_mempool() {
                if tx.state == TransactionState::Confirmed {
                    info!("Transaction({}) reorged back to mempool", tx.txid);
                    self.ctx.handle_reorg(&tx, current_height)?;
                } else if tx.is_stuck_in_mempool(current_height) {
                    warn!(
                        "Transaction({}) stuck in mempool for {} blocks (threshold: {})",
                        tx.txid,
                        tx.broadcast_block_height
                            .map(|h| current_height.saturating_sub(h))
                            .unwrap_or(0),
                        tx.stuck_in_mempool_blocks.unwrap_or(0),
                    );
                    self.ctx
                        .storage
                        .add_news(CoordinatorNews::TransactionStuckInMempool {
                            txid: tx.txid,
                            context: tx.context.clone(),
                        })?;
                }
                continue;
            }

            if status.is_not_found() {
                debug!(
                    "Transaction({}) not found, re-queuing for dispatch this tick",
                    tx.txid
                );
                self.ctx
                    .storage
                    .update_tx_state(tx.txid, TransactionState::ToDispatch)?;
                // No need to re-add a NeedsSpeedup parent to PendingSpeedupParents here: the
                // covering CPFP is independently re-queued in `review_speedups`'s not_found arm
                continue;
            }

            if status.is_finalized(max_confs) {
                info!(
                    "Transaction({}) finalized ({} confirmations)",
                    tx.txid, status.confirmations
                );
                self.ctx.handle_finalized(tx.txid, current_height)?;
                continue;
            }

            if status.is_confirmed() {
                debug!(
                    "Transaction({}) confirmed ({} confirmations)",
                    tx.txid, status.confirmations
                );
                self.ctx.handle_confirmed(tx.txid)?;
                continue;
            }

            if status.is_orphan() {
                debug!("Transaction({}) orphaned, keeping InMempool", tx.txid);
                self.ctx.handle_orphan(tx.txid)?;
                continue;
            }

            error!(
                "Inconsistent state: transaction {} in unexpected chain status",
                tx.txid
            );
        }

        Ok(())
    }

    fn dispatch_batch(
        &self,
        txs: Vec<CoordinatedTx>,
        current_height: BlockHeight,
    ) -> Result<(), BitcoinCoordinatorError> {
        // Update fee rates before dispatching to ensure we are using the latest network conditions.
        let (fee_rate, fee_news) = self
            .ctx
            .fee_manager
            .get_network_fee_rate(&self.ctx.monitor)?;
        if let Some(news) = fee_news {
            self.ctx.storage.add_news(news)?;
        }

        let txs = self.ctx.apply_retry_rate_limit(txs);
        let raw_txs: Vec<Transaction> = txs.iter().map(|t| t.tx.clone()).collect();
        let results = self.ctx.dispatcher.dispatch(raw_txs);

        for (txid, outcome) in results {
            let tx = find_tx_in_batch(&txs, txid)?;
            let fee_info = self.ctx.fee_manager.compute_fee_for_tx(&tx.tx, fee_rate);
            self.ctx
                .handle_dispatch_result(tx, txid, outcome, current_height, fee_info)?;
        }

        Ok(())
    }
}
