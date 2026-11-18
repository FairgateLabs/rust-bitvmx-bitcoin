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

impl BitcoinCoordinatorError {
    /// True when the error came from the Bitcoin node client, here or inside the monitor or indexer.
    pub fn is_bitcoin_client_error(&self) -> bool {
        match self {
            BitcoinCoordinatorError::BitcoinClientError(_) => true,
            BitcoinCoordinatorError::MonitorError(e) => e.is_bitcoin_client_error(),
            _ => false,
        }
    }

    /// True when the error came from the storage backend, here or inside the monitor or indexer. Storage errors
    /// wrapped by the key manager or the protocol builder are not counted.
    pub fn is_storage_error(&self) -> bool {
        match self {
            BitcoinCoordinatorError::StorageBackendError(_) => true,
            BitcoinCoordinatorError::MonitorError(e) => e.is_storage_error(),
            _ => false,
        }
    }
}

// Manual From so `?` on a ProtocolBuilderError can convert.
impl From<ProtocolBuilderError> for BitcoinCoordinatorError {
    fn from(e: ProtocolBuilderError) -> Self {
        BitcoinCoordinatorError::ProtocolBuilderError(Box::new(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin_indexer::errors::IndexerError;

    #[test]
    fn test_error_classification() {
        let client = BitcoinCoordinatorError::from(BitcoinClientError::InvalidHeight);
        assert!(client.is_bitcoin_client_error());
        assert!(!client.is_storage_error());

        let storage = BitcoinCoordinatorError::from(StorageError::WriteError);
        assert!(storage.is_storage_error());
        assert!(!storage.is_bitcoin_client_error());

        // Errors wrapped by the monitor and the indexer keep their classification.
        let nested_client = BitcoinCoordinatorError::from(MonitorError::from(
            IndexerError::from(BitcoinClientError::InvalidHeight),
        ));
        assert!(nested_client.is_bitcoin_client_error());
        let nested_storage = BitcoinCoordinatorError::from(MonitorError::from(
            IndexerError::from(StorageError::WriteError),
        ));
        assert!(nested_storage.is_storage_error());

        let other = BitcoinCoordinatorError::Internal("bug".to_string());
        assert!(!other.is_bitcoin_client_error());
        assert!(!other.is_storage_error());
    }
}
