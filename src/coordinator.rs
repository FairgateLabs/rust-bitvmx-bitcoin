use std::rc::Rc;

use crate::{
    config::config::{BitcoinSettings, CoordinatorSettings},
    core::{
        dispatcher::Dispatcher, fee::FeeEngine, funding::FundingManager, speedup::SpeedupEngine,
        storage::CoordinatorStorage,
    },
    errors::BitcoinCoordinatorError,
};
use bitvmx_bitcoin_rpc::rpc_config::RpcConfig;
use bitvmx_transaction_monitor::monitor::Monitor;
use storage_backend::storage::Storage;

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
        let settings = settings.clone().unwrap_or_default();
        settings.validate()?;
        let monitor = Monitor::new_with_paths(rpc_config, storage.clone(), Some(settings.monitor))?;
        // let rpc = BitcoinClient::new_from_config(rpc_config)?;
        let storage = CoordinatorStorage::new(storage);
        let dispatcher = Dispatcher::new(settings.dispatcher);
        let fee_engine = FeeEngine::new(settings.fee);
        let speedup_engine = SpeedupEngine::new(settings.speedup);
        let funding_manager = FundingManager::new(settings.funding);
        let settings = settings.coordinator;

        Ok(Self {
            monitor,
            storage,
            dispatcher,
            fee_engine,
            speedup_engine,
            funding_manager,
            settings,
        })
    }
}

// pub trait BitcoinCoordinatorApi {
//     /// Checks if the coordinator is ready to process transactions
//     /// Returns true if the coordinator is ready, false otherwise
//     fn is_ready(&self) -> Result<bool, BitcoinCoordinatorError>;

//     /// Processes active transactions and updates their status
//     /// This method should be called periodically to keep the coordinator state up-to-date
//     fn tick(&self) -> Result<(), BitcoinCoordinatorError>;

//     /// Registers a type of data to be monitored by the coordinator
//     /// The data will be tracked for confirmations and status changes, and updates will be reported through the news.
//     ///
//     /// # Arguments
//     /// * `data` - The data to monitor
//     fn monitor(&self, data: TypesToMonitor) -> Result<(), BitcoinCoordinatorError>;

//     /// Dispatches a transaction to the Bitcoin network
//     ///
//     /// # Arguments
//     /// * `tx` - The Bitcoin transaction to dispatch
//     /// * `speedup` - Speed up information for the transaction (None means it should not be speed up)
//     /// * `context` - Additional context information for the transaction to be returned in news
//     /// * `block_height` - Block height to dispatch the transaction (None means now)
//     /// * `number_confirmation_trigger` - Just trigger news when the transaction has exactly this number of confirmations (None means all confirmations)
//     fn dispatch(
//         &self,
//         tx: Transaction,
//         speedup: Option<SpeedupData>,
//         context: String,
//         block_height: Option<BlockHeight>,
//         number_confirmation_trigger: Option<u32>,
//     ) -> Result<(), BitcoinCoordinatorError>;

//     /// Dispatches a transaction to the Bitcoin network without speedup support
//     ///
//     /// # Arguments
//     /// * `tx` - The Bitcoin transaction to dispatch
//     /// * `context` - Additional context information for the transaction to be returned in news
//     /// * `block_height` - Block height to dispatch the transaction (None means now)
//     /// * `number_confirmation_trigger` - Just trigger news when the transaction has exactly this number of confirmations (None means all confirmations)
//     /// * `stuck_in_mempool_blocks` - Number of blocks to wait before considering the transaction stuck in mempool
//     fn dispatch_without_speedup(
//         &self,
//         tx: Transaction,
//         context: String,
//         block_height: Option<BlockHeight>,
//         number_confirmation_trigger: Option<u32>,
//         stuck_in_mempool_blocks: u32,
//     ) -> Result<(), BitcoinCoordinatorError>;

//     /// Cancels the monitor and the dispatch of a type of data
//     /// This method removes the monitor and the dispatch from the coordinator's store.
//     /// Which means that the data will no longer be monitored.
//     ///
//     /// # Arguments
//     /// * `data` - The data to cancel
//     fn cancel(&self, data: TypesToMonitor) -> Result<(), BitcoinCoordinatorError>;

//     /// Registers funding information for potential transaction speed-ups
//     /// This allows the coordinator to create child pays for parents transactions when needed
//     ///
//     /// # Arguments
//     /// * `utxo` - Utxo to use for speed-ups
//     fn add_funding(&self, utxo: Utxo) -> Result<(), BitcoinCoordinatorError>;

//     fn get_transaction(&self, txid: Txid) -> Result<TransactionStatus, BitcoinCoordinatorError>;

//     /// Retrieves news about monitored transactions
//     /// Returns information about transaction confirmations.
//     fn get_news(&self) -> Result<News, BitcoinCoordinatorError>;

//     /// Acknowledges that news has been processed
//     /// This prevents the same news from being returned in subsequent calls to get_news()
//     ///
//     /// # Arguments
//     /// * `news` - The news items to acknowledge
//     fn ack_news(&self, news: AckNews) -> Result<(), BitcoinCoordinatorError>;
// }
