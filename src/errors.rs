use bitvmx_bitcoin_rpc::errors::BitcoinClientError;
use bitvmx_transaction_monitor::{errors::MonitorError, IndexerError};
use protocol_builder::errors::ProtocolBuilderError;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum BitcoinCoordinatorError {
    #[error("Bad configuration: {0}")]
    InvalidConfiguration(String),

    #[error("Bitcoin Client Error: {0}")]
    BitcoinClientError(#[from] BitcoinClientError),

    #[error("Monitor Error: {0}")]
    MonitorError(#[from] MonitorError),
    // #[error("Indexer Error: {0}")]
    // IndexerError(#[from] IndexerError),
}
