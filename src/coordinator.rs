use std::rc::Rc;

use bitcoin::{Transaction, Txid};
use bitvmx_bitcoin_rpc::{
    bitcoin_client::BitcoinClient, rpc_config::RpcConfig, types::BlockHeight,
};
use bitvmx_transaction_monitor::{monitor::Monitor, types::TypesToMonitor, TransactionStatus};
use key_manager::key_manager::KeyManager;
use protocol_builder::types::{output::SpeedupData, Utxo};
use storage_backend::storage::Storage;
use tracing::{debug, info};

use crate::{
    config::config::BitcoinSettings,
    core::{
        dispatcher::Dispatcher, fee::FeeManager, funding::FundingManager,
        storage::CoordinatorStorage,
    },
    engines::{
        common::EngineContext, speedup_engine::SpeedupEngine, transaction_engine::TransactionEngine,
    },
    errors::BitcoinCoordinatorError,
    types::{AckNews, CoordinatedTx, News, TransactionState, TxKind},
};

pub struct BitcoinCoordinator {
    speedup_engine: SpeedupEngine,
    tx_engine: TransactionEngine,
}

impl BitcoinCoordinator {
    pub fn new_with_paths(
        rpc_config: &RpcConfig,
        storage: Rc<Storage>,
        key_manager: Rc<KeyManager>,
        settings: Option<BitcoinSettings>,
    ) -> Result<Self, BitcoinCoordinatorError> {
        let settings = settings.unwrap_or_default();
        settings.validate()?;

        let bitcoin_client = Rc::new(BitcoinClient::new_from_config(rpc_config)?);
        let monitor = Monitor::new_with_paths(rpc_config, storage.clone(), Some(settings.monitor))?;

        let funding_manager = FundingManager::new(settings.funding, storage.clone());
        let coordinator_storage = CoordinatorStorage::new(storage, settings.storage);
        let dispatcher = Dispatcher::new(settings.dispatcher, bitcoin_client);
        let fee_manager = FeeManager::new(settings.fee);
        let coordinator_config = settings.coordinator;

        let ctx = Rc::new(EngineContext::new(
            monitor,
            fee_manager,
            funding_manager,
            dispatcher,
            coordinator_storage,
            coordinator_config,
        ));

        let speedup_engine = SpeedupEngine::new(Rc::clone(&ctx), key_manager, settings.speedup);
        let tx_engine = TransactionEngine::new(ctx);

        Ok(Self {
            speedup_engine,
            tx_engine,
        })
    }

    // =========================================================================
    // Public API
    // =========================================================================

    /// Returns `true` when the monitor is fully synced with the chain.
    pub fn is_ready(&self) -> Result<bool, BitcoinCoordinatorError> {
        Ok(self.tx_engine.ctx.monitor.is_ready()?)
    }

    /// Periodic processing: advances the monitor, then reviews and dispatches
    /// all active transactions.
    pub fn tick(&self) -> Result<(), BitcoinCoordinatorError> {
        self.tx_engine.ctx.monitor.tick()?;

        if !self.is_ready()? {
            debug!("Coordinator not ready, skipping tick");
            return Ok(());
        }

        self.speedup_engine.process_active_transactions()?;
        let dispatched_parents = self.tx_engine.process_active_transactions()?;
        self.speedup_engine
            .create_cpfps_for_parents(&dispatched_parents)?;

        Ok(())
    }

    /// Register a transaction to be dispatched (without speedup support).
    pub fn dispatch_without_speedup(
        &self,
        tx: Transaction,
        context: String,
        target_block_height: Option<u32>,
        confirmation_trigger: Option<u32>,
        stuck_in_mempool_blocks: Option<u32>,
    ) -> Result<(), BitcoinCoordinatorError> {
        let txid = tx.compute_txid();
        self.register_tx(
            tx,
            crate::types::TxKind::Normal,
            context,
            target_block_height,
            confirmation_trigger,
            stuck_in_mempool_blocks,
        )?;
        info!("Transaction({}) registered for dispatch", txid);
        Ok(())
    }

    /// Register a transaction for dispatch and enable CPFP speedup support.
    pub fn dispatch_with_speedup(
        &self,
        tx: Transaction,
        speedup_data: SpeedupData,
        context: String,
        target_block_height: Option<u32>,
        confirmation_trigger: Option<u32>,
        stuck_in_mempool_blocks: Option<u32>,
    ) -> Result<(), BitcoinCoordinatorError> {
        let txid = tx.compute_txid();
        self.register_tx(
            tx,
            TxKind::NeedsSpeedup(speedup_data),
            context,
            target_block_height,
            confirmation_trigger,
            stuck_in_mempool_blocks,
        )?;
        info!("Transaction({}) registered for dispatch with speedup", txid);
        Ok(())
    }

    /// Cancel monitoring and remove a transaction from the coordinator.
    pub fn cancel(&self, data: TypesToMonitor) -> Result<(), BitcoinCoordinatorError> {
        self.tx_engine.ctx.monitor.cancel(data.clone())?;
        if let TypesToMonitor::Transactions(txids, _, _) = data.clone() {
            for txid in txids {
                self.tx_engine.ctx.storage.remove_tx(txid)?;
            }
        }
        info!("Cancelled monitoring for {:?}", data);
        Ok(())
    }

    /// Register a funding UTXO for potential future speedups.
    pub fn add_funding(&self, utxo: Utxo) -> Result<(), BitcoinCoordinatorError> {
        info!(
            "Funding added | Txid({}) | Vout({}) | Amount({})",
            utxo.txid, utxo.vout, utxo.amount
        );
        if let Some(news) = self.tx_engine.ctx.funding_manager.set_funding(utxo)? {
            self.tx_engine.ctx.storage.add_news(news)?;
        }
        Ok(())
    }

    /// Query the current blockchain status of a transaction via the monitor.
    pub fn get_transaction(
        &self,
        txid: Txid,
    ) -> Result<TransactionStatus, BitcoinCoordinatorError> {
        Ok(self.tx_engine.ctx.monitor.get_tx_status(&txid, true)?)
    }

    pub fn get_news(&self) -> Result<News, BitcoinCoordinatorError> {
        let monitor_news = self.tx_engine.ctx.monitor.get_news()?;
        let coordinator_news = self.tx_engine.ctx.storage.get_news()?;
        Ok(News {
            monitor_news,
            coordinator_news,
        })
    }

    /// Acknowledge a news item so it is not returned again.
    pub fn ack_news(&self, news: AckNews) -> Result<(), BitcoinCoordinatorError> {
        match news {
            AckNews::Monitor(n) => self.tx_engine.ctx.monitor.ack_news(n)?,
            AckNews::Coordinator(n) => self.tx_engine.ctx.storage.ack_news(n)?,
        }
        Ok(())
    }

    /// Register data to be monitored.
    pub fn monitor(&self, data: TypesToMonitor) -> Result<(), BitcoinCoordinatorError> {
        self.tx_engine.ctx.monitor.monitor(data, true)?;
        Ok(())
    }

    // =========================================================================
    // Private helpers
    // =========================================================================

    /// Persist a transaction and register it with the monitor.
    fn register_tx(
        &self,
        tx: Transaction,
        kind: TxKind,
        context: String,
        target_block_height: Option<BlockHeight>,
        confirmation_trigger: Option<u32>,
        stuck_in_mempool_blocks: Option<u32>,
    ) -> Result<(), BitcoinCoordinatorError> {
        let txid = tx.compute_txid();
        let current_height = self.tx_engine.ctx.monitor.get_monitor_height()?;
        let target_height = target_block_height.unwrap_or(current_height);
        let fee_manager = &self.tx_engine.ctx.fee_manager;

        let (fee_rate, _) = fee_manager.get_network_fee_rate(&self.tx_engine.ctx.monitor)?;
        let fee_info = fee_manager.compute_fee_for_tx(&tx, fee_rate);

        // Register for confirmation tracking (mempool search disabled until after
        // the tx is actually broadcast).
        self.tx_engine.ctx.monitor.monitor(
            TypesToMonitor::Transactions(vec![txid], context.clone(), confirmation_trigger),
            false,
        )?;

        self.tx_engine.ctx.storage.insert_tx(CoordinatedTx {
            txid,
            tx,
            kind,
            state: TransactionState::ToDispatch,
            broadcast_block_height: None,
            target_block_height: target_height,
            stuck_in_mempool_blocks,
            confirmation_trigger,
            settled_block_height: None,
            retry_count: 0,
            fee_info,
            context,
        })?;

        Ok(())
    }
}
