use std::rc::Rc;

use bitcoin::{Transaction, Txid};
use bitvmx_bitcoin_rpc::{rpc_config::RpcConfig, types::BlockHeight};
use bitvmx_transaction_monitor::{monitor::Monitor, types::TypesToMonitor, TransactionStatus};
use protocol_builder::types::Utxo;
use storage_backend::storage::Storage;
use tracing::{debug, info, warn};

use crate::{
    config::config::{BitcoinSettings, CoordinatorSettings},
    core::{
        dispatcher::{DispatchOutcome, Dispatcher},
        fee::FeeEngine,
        funding::FundingManager,
        speedup::SpeedupEngine,
        storage::CoordinatorStorage,
    },
    errors::BitcoinCoordinatorError,
    types::{AckNews, CoordinatedTx, CoordinatorNews, FeeInfo, News, TransactionState, TxKind},
};

pub struct BitcoinCoordinator {
    monitor: Monitor,

    // key_manager: Rc<KeyManager>,
    // _network: Network,
    storage: CoordinatorStorage,
    dispatcher: Dispatcher,
    fee_engine: FeeEngine,
    speedup_engine: SpeedupEngine,
    funding_manager: FundingManager,

    settings: CoordinatorSettings,
}

impl BitcoinCoordinator {
    pub fn new_with_paths(
        rpc_config: &RpcConfig,
        storage: Rc<Storage>,
        // key_manager: Rc<KeyManager>,
        settings: Option<BitcoinSettings>,
    ) -> Result<Self, BitcoinCoordinatorError> {
        let settings = settings.unwrap_or_default();
        settings.validate()?;

        // All modules share the same underlying Storage via Rc clones.
        let monitor = Monitor::new_with_paths(rpc_config, storage.clone(), Some(settings.monitor))?;
        let funding_manager = FundingManager::new(settings.funding, storage.clone());
        let coordinator_storage = CoordinatorStorage::new(storage);

        let dispatcher = Dispatcher::new(settings.dispatcher);
        let fee_engine = FeeEngine::new(settings.fee);
        let speedup_engine = SpeedupEngine::new(settings.speedup);
        let coordinator_settings = settings.coordinator;

        Ok(Self {
            monitor,
            storage: coordinator_storage,
            dispatcher,
            fee_engine,
            speedup_engine,
            funding_manager,
            settings: coordinator_settings,
        })
    }

    // =========================================================================
    // Public API
    // =========================================================================

    /// Returns `true` when the monitor is fully synced with the chain.
    pub fn is_ready(&self) -> Result<bool, BitcoinCoordinatorError> {
        Ok(self.monitor.is_ready()?)
    }

    /// Periodic processing: advances the monitor, then reviews and dispatches
    /// all active transactions.
    pub fn tick(&self) -> Result<(), BitcoinCoordinatorError> {
        self.monitor.tick()?;

        if !self.is_ready()? {
            debug!("Coordinator not ready, skipping tick");
            return Ok(());
        }

        self.process_active_transactions()?;

        Ok(())
    }

    /// Register a transaction to be dispatched (without speedup support).
    ///
    /// The transaction is persisted with state `ToDispatch` and registered with
    /// the monitor for confirmation tracking. The actual broadcast happens on
    /// the next `tick()` once `target_block_height` is reached.
    ///
    /// * `target_block_height` – earliest block at which to broadcast; `None`
    ///   means dispatch immediately on the next tick.
    /// * `confirmation_trigger` – generate monitor news once the tx reaches
    ///   exactly this many confirmations; `None` for every confirmation.
    /// * `stuck_in_mempool_blocks` – generate `TransactionStuckInMempool` news
    ///   if the tx has been in the mempool for this many blocks; `0` disables.
    pub fn dispatch_without_speedup(
        &self,
        tx: Transaction,
        context: String,
        target_block_height: Option<BlockHeight>,
        confirmation_trigger: Option<u32>,
        stuck_in_mempool_blocks: u32,
    ) -> Result<(), BitcoinCoordinatorError> {
        let txid = tx.compute_txid();
        let current_height = self.monitor.get_monitor_height()?;
        let target_height = target_block_height.unwrap_or(current_height);

        let (fee_rate, _) = self.fee_engine.get_network_fee_rate(&self.monitor)?;
        let fee_info = self.fee_engine.compute_fee_for_tx(&tx, fee_rate);

        // Register for confirmation tracking (mempool search disabled until after
        // the tx is actually broadcast, to avoid false positives).
        let trigger = confirmation_trigger.filter(|&t| t > 0);
        self.monitor.monitor(
            TypesToMonitor::Transactions(vec![txid], context.clone(), trigger),
            false,
        )?;

        let coordinated_tx = CoordinatedTx {
            txid,
            tx,
            kind: TxKind::Normal,
            state: TransactionState::ToDispatch,
            broadcast_block_height: 0,
            target_block_height: target_height,
            stuck_in_mempool_blocks,
            confirmation_trigger: confirmation_trigger.unwrap_or(0),
            retry_count: 0,
            fee_info,
            context,
        };

        self.storage.insert_tx(coordinated_tx)?;
        info!("Transaction({}) registered for dispatch", txid);

        Ok(())
    }

    /// Cancel monitoring and remove a transaction from the coordinator.
    pub fn cancel(&self, data: TypesToMonitor) -> Result<(), BitcoinCoordinatorError> {
        self.monitor.cancel(data.clone())?;

        if let TypesToMonitor::Transactions(txids, _, _) = data {
            for txid in txids {
                self.storage.remove_tx(txid)?;
            }
        }

        Ok(())
    }

    /// Register a funding UTXO for potential future speedups.
    ///
    /// Validation is performed immediately; if the UTXO is invalid (e.g. below
    /// dust threshold) a news item is stored and the call still returns `Ok`.
    /// Valid UTXOs are persisted to storage.
    pub fn add_funding(&self, utxo: Utxo) -> Result<(), BitcoinCoordinatorError> {
        info!(
            "Funding added | Txid({}) | Vout({}) | Amount({})",
            utxo.txid, utxo.vout, utxo.amount
        );
        if let Some(news) = self.funding_manager.set_funding(utxo)? {
            self.storage.add_news(news)?;
        }
        Ok(())
    }

    /// Query the current blockchain status of a transaction via the monitor.
    pub fn get_transaction(
        &self,
        txid: Txid,
    ) -> Result<TransactionStatus, BitcoinCoordinatorError> {
        Ok(self.monitor.get_tx_status(&txid, true)?)
    }

    // /// Registers a type of data to be monitored by the coordinator
    // /// The data will be tracked for confirmations and status changes, and updates will be reported through the news.
    // ///
    // /// # Arguments
    // /// * `data` - The data to monitor
    // pub fn monitor(&self, data: TypesToMonitor) -> Result<(), BitcoinCoordinatorError> {
    //     self.monitor.monitor(data, true)?; //ASK: why true or false?
    //     Ok(())
    // }

    pub fn get_news(&self) -> Result<News, BitcoinCoordinatorError> {
        let monitor_news = self.monitor.get_news()?;
        let coordinator_news = self.storage.get_news()?;
        Ok(News {
            monitor_news,
            coordinator_news,
        })
    }

    /// Acknowledge a news item so it is not returned again.
    pub fn ack_news(&self, news: AckNews) -> Result<(), BitcoinCoordinatorError> {
        match news {
            AckNews::Monitor(news) => self.monitor.ack_news(news)?,
            AckNews::Coordinator(news) => self.storage.ack_news(news)?,
        }
        Ok(())
    }

    // =========================================================================
    // Private helpers
    // =========================================================================

    /// Main processing loop: review active txs, then dispatch pending ones.
    fn process_active_transactions(&self) -> Result<(), BitcoinCoordinatorError> {
        let active_txs = self.storage.get_active_txs()?;

        if active_txs.is_empty() {
            return Ok(());
        }

        debug!("Processing {} active transactions", active_txs.len());

        let current_height = self.monitor.get_monitor_height()?;

        let mut to_dispatch: Vec<CoordinatedTx> = Vec::new();
        let mut to_review: Vec<CoordinatedTx> = Vec::new();

        for tx in active_txs {
            match tx.state {
                TransactionState::ToDispatch => {
                    if tx.is_ready_to_dispatch(current_height) {
                        to_dispatch.push(tx);
                    }
                }
                TransactionState::InMempool | TransactionState::Confirmed => {
                    to_review.push(tx);
                }
                _ => {} //TODO: notify error via news, it should never reach here. And remove it?
            }
        }

        // Review in-flight transactions first; this may re-queue some for dispatch.
        self.review_transactions(to_review, &mut to_dispatch, current_height)?;

        if !to_dispatch.is_empty() {
            self.dispatch_pending(to_dispatch, current_height)?;
        }

        Ok(())
    }

    /// Check the chain/mempool status of each `InMempool` or `Confirmed`
    /// transaction and transition its state accordingly.
    fn review_transactions(
        &self,
        txs: Vec<CoordinatedTx>,
        to_dispatch: &mut Vec<CoordinatedTx>,
        current_height: BlockHeight,
    ) -> Result<(), BitcoinCoordinatorError> {
        let max_confs = self.monitor.settings.max_monitoring_confirmations;

        for tx in txs {
            // Only search the mempool after the tx has been broadcast.
            let search_in_mempool = tx.broadcast_block_height > 0;
            let status = self.monitor.get_tx_status(&tx.txid, search_in_mempool)?;

            if status.is_in_mempool() {
                if tx.is_stuck_in_mempool(current_height) {
                    warn!(
                        "Transaction({}) stuck in mempool for {} blocks (threshold: {})",
                        tx.txid,
                        current_height.saturating_sub(tx.broadcast_block_height),
                        tx.stuck_in_mempool_blocks,
                    );
                    self.storage
                        .add_news(CoordinatorNews::TransactionStuckInMempool {
                            txid: tx.txid,
                            context: tx.context.clone(),
                        })?;
                }
                continue;
            }

            if status.is_not_found() {
                debug!(
                    "Transaction({}) not found — re-queuing for dispatch",
                    tx.txid
                );
                self.storage
                    .update_tx_state(tx.txid, TransactionState::ToDispatch)?;
                to_dispatch.push(tx);
                continue;
            }

            if status.is_finalized(max_confs) {
                debug!(
                    "Transaction({}) finalized ({} confirmations)",
                    tx.txid, status.confirmations
                );
                self.storage
                    .update_tx_state(tx.txid, TransactionState::Finalized)?;
                continue;
            }

            if status.is_confirmed() {
                debug!(
                    "Transaction({}) confirmed ({} confirmations)",
                    tx.txid, status.confirmations
                );
                self.storage
                    .update_tx_state(tx.txid, TransactionState::Confirmed)?;
                continue;
            }

            if status.is_orphan() {
                debug!("Transaction({}) orphaned — keeping InMempool", tx.txid);
                self.storage
                    .update_tx_state(tx.txid, TransactionState::InMempool)?;
            }
        }

        Ok(())
    }

    /// Broadcast a batch of `ToDispatch` transactions and update their states.
    fn dispatch_pending(
        &self,
        txs: Vec<CoordinatedTx>,
        current_height: BlockHeight,
    ) -> Result<(), BitcoinCoordinatorError> {
        let (fee_rate, fee_news) = self.fee_engine.get_network_fee_rate(&self.monitor)?;
        if let Some(news) = fee_news {
            self.storage.add_news(news)?;
        }

        let raw_txs: Vec<Transaction> = txs.iter().map(|t| t.tx.clone()).collect();
        let results = self.dispatcher.dispatch(&self.monitor, raw_txs);

        for (txid, outcome) in results {
            let tx = match txs.iter().find(|t| t.txid == txid) {
                Some(t) => t,
                None => continue,
            };

            match outcome {
                DispatchOutcome::Success => {
                    self.on_dispatch_success(tx, txid, current_height, fee_rate)?;
                }

                DispatchOutcome::AlreadyKnown => {
                    warn!(
                        "Transaction({}) already known by node — treating as in-mempool",
                        txid
                    );
                    self.on_dispatch_success(tx, txid, current_height, fee_rate)?;
                }

                DispatchOutcome::Retryable(msg) => {
                    if tx.retry_count + 1 >= self.settings.retry_attempts_sending_tx {
                        warn!(
                            "Transaction({}) failed after {} attempts: {}",
                            txid,
                            tx.retry_count + 1,
                            msg
                        );
                        self.storage
                            .update_tx_state(txid, TransactionState::Failed)?;
                        self.storage.add_news(CoordinatorNews::DispatchError {
                            txid,
                            context: tx.context.clone(),
                        })?;
                    } else {
                        debug!(
                            "Transaction({}) dispatch failed (attempt {}/{}) — will retry: {}",
                            txid,
                            tx.retry_count + 1,
                            self.settings.retry_attempts_sending_tx,
                            msg
                        );
                        self.storage.mark_as_retry(txid)?;
                    }
                }

                DispatchOutcome::Fatal(msg) => {
                    warn!(
                        "Transaction({}) failed with fatal dispatch error: {}",
                        txid, msg
                    );
                    self.storage
                        .update_tx_state(txid, TransactionState::Failed)?;
                    self.storage.add_news(CoordinatorNews::DispatchError {
                        txid,
                        context: tx.context.clone(),
                    })?;
                }
            }
        }

        Ok(())
    }

    /// Shared success path for both a clean broadcast and an `AlreadyKnown` response.
    fn on_dispatch_success(
        &self,
        tx: &CoordinatedTx,
        txid: Txid,
        current_height: BlockHeight,
        fee_rate: u64,
    ) -> Result<(), BitcoinCoordinatorError> {
        let fee_info = self.fee_engine.compute_fee_for_tx(&tx.tx, fee_rate);

        let mut updated = tx.clone();
        updated.state = TransactionState::InMempool;
        updated.broadcast_block_height = current_height;
        updated.fee_info = fee_info;
        self.storage.update_tx(&updated)?;

        // Re-register with mempool search enabled so the monitor tracks the tx
        // while it waits for inclusion in a block.
        let trigger = (tx.confirmation_trigger > 0).then_some(tx.confirmation_trigger);
        self.monitor.monitor(
            TypesToMonitor::Transactions(vec![txid], tx.context.clone(), trigger),
            true,
        )?;

        info!(
            "Transaction({}) dispatched at block height {}",
            txid, current_height
        );
        Ok(())
    }
}
