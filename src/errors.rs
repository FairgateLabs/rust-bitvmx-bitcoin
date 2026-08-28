use bitvmx_bitcoin_rpc::errors::BitcoinClientError;
use bitvmx_transaction_monitor::errors::MonitorError;
use key_manager::errors::KeyManagerError;
use protocol_builder::errors::ProtocolBuilderError;
use storage_backend::error::StorageError;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum BitcoinCoordinatorError {
    #[error("Bad configuration: {0}")]
    InvalidConfiguration(String),

    #[error("Monitor Error: {0}")]
    MonitorError(#[from] MonitorError),

    #[error("Storage Backend Error: {0}")]
    StorageBackendError(#[from] StorageError),

    #[error("Bitcoin Client Error: {0}")]
    BitcoinClientError(#[from] BitcoinClientError),

    // Boxed because ProtocolBuilderError large.
    #[error("Protocol Builder Error: {0}")]
    ProtocolBuilderError(Box<ProtocolBuilderError>),

    #[error("Key Manager Error: {0}")]
    KeyManagerError(#[from] KeyManagerError),

    #[error("Internal error (programmer bug): {0}")]
    Internal(String),

    #[error("Invariant violation: {0}")]
    InvariantViolation(String),
}

// Manual From so `?` on a ProtocolBuilderError can convert.
impl From<ProtocolBuilderError> for BitcoinCoordinatorError {
    fn from(e: ProtocolBuilderError) -> Self {
        BitcoinCoordinatorError::ProtocolBuilderError(Box::new(e))
    }
}
