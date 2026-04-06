use bitcoin::Txid;
use bitvmx_bitcoin_rpc::errors::BitcoinClientError;
use bitvmx_transaction_monitor::{errors::MonitorError, IndexerError};
use protocol_builder::errors::ProtocolBuilderError;
use storage_backend::error::StorageError;
use thiserror::Error;

use crate::types::TransactionState;

#[derive(Error, Debug)]
pub enum BitcoinCoordinatorError {
    #[error("Bad configuration: {0}")]
    InvalidConfiguration(String),

    #[error("Bitcoin Client Error: {0}")]
    BitcoinClientError(#[from] BitcoinClientError),

    #[error("Monitor Error: {0}")]
    MonitorError(#[from] MonitorError),

    #[error("Storage Backend Error: {0}")]
    StorageBackendError(#[from] StorageError),
    // #[error("Invalid state transition from {from} to {to}")] //TODO: this should be a notification
    // InvalidStateTransition {
    //     from: TransactionState,
    //     to: TransactionState,
    // },

    // #[error("Transaction not found: {0}")] //TODO: this should be a notification
    // NotFound(Txid),
    // #[error("Indexer Error: {0}")]
    // IndexerError(#[from] IndexerError),
}
