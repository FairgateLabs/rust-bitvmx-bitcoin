use crate::{
    core::storage::CoordinatorStorage,
    types::{
        CoordinatedTx,
        TransactionState::{self, *},
    },
};
use bitvmx_bitcoin_rpc::types::BlockHeight;
use std::{fs, path, rc::Rc};
use storage_backend::{storage::Storage, storage_config::StorageConfig};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

impl TransactionState {
    /// Returns `true` when transitioning from `self` to `next` is a valid
    /// lifecycle step.
    pub fn can_transition_to(&self, next: &TransactionState) -> bool {
        match (self, next) {
            // Normal forward flow
            (ToDispatch, InMempool) => true,
            (InMempool, Confirmed) => true,
            (Confirmed, Finalized) => true,

            // Crash recovery: tx already on-chain when we restart
            (ToDispatch, Confirmed) => true, // dispatched but crash before InMempool record or someone else broadcast the tx
            (ToDispatch, Finalized) => true, // dispatched but crash before Confirmed record or someone else broadcast the tx
            (InMempool, Finalized) => true,  // confirmed so fast we never saw Confirmed

            // Requeue after not-found in mempool
            (InMempool, ToDispatch) => true,

            // Reorg: confirmed block rolled back
            (Confirmed, InMempool) => true,

            // Any state can fail
            (_, Failed) => true,

            // Idempotency
            (a, b) if a == b => true,

            _ => false,
        }
    }
}

impl std::fmt::Display for TransactionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToDispatch => write!(f, "ToDispatch"),
            InMempool => write!(f, "InMempool"),
            Confirmed => write!(f, "Confirmed"),
            Finalized => write!(f, "Finalized"),
            Failed => write!(f, "Failed"),
        }
    }
}

impl CoordinatedTx {
    /// Returns `true` when the transaction is due to be dispatched at `current_height`.
    pub fn is_ready_to_dispatch(&self, current_height: BlockHeight) -> bool {
        current_height >= self.target_block_height
    }

    /// Returns `true` when the transaction has been waiting in the mempool for
    /// longer than its `stuck_in_mempool_blocks` threshold.
    ///
    /// Returns `false` if the threshold is disabled (`stuck_in_mempool_blocks
    /// == 0`) or if the transaction has not been broadcast yet
    /// (`broadcast_block_height == 0`).
    pub fn is_stuck_in_mempool(&self, current_height: BlockHeight) -> bool {
        self.stuck_in_mempool_blocks > 0
            && self.broadcast_block_height > 0
            && current_height.saturating_sub(self.broadcast_block_height)
                >= self.stuck_in_mempool_blocks
    }
}

// =============================================================================
// Test utilities
// =============================================================================

pub fn init_trace() {
    let default_modules = [
        "info",
        "libp2p=off",
        "bitvmx_transaction_monitor=debug",
        "bitcoin_indexer=debug",
        "bitcoin_coordinator=debug",
        "bitcoin_rpc=debug",
        "bitcoin_client=debug",
        "p2p_protocol=off",
        "p2p_handler=off",
        "tarpc=off",
        "key_manager=off",
        "memory=off",
    ];

    let filter = EnvFilter::builder()
        .parse(default_modules.join(","))
        .expect("Invalid filter");

    let _ = tracing_subscriber::fmt()
        .with_target(true)
        .with_env_filter(filter)
        .try_init();
}

pub struct StorageTestConfig {
    path: String,
    storage: Rc<Storage>,
}

impl StorageTestConfig {
    pub fn new() -> Self {
        let path = Self::get_storage_path();
        let config = StorageConfig {
            path: path.clone(),
            password: None,
        };

        let storage = Rc::new(Storage::new(&config).unwrap());
        info!("Initialized test storage at: {}", path);

        Self { path, storage }
    }

    pub fn get_coordinator_storage(&self) -> CoordinatorStorage {
        CoordinatorStorage::new(Rc::clone(&self.storage))
    }

    pub fn get_raw_storage(&self) -> Rc<Storage> {
        Rc::clone(&self.storage)
    }

    pub fn remove(self) {
        let path = self.path.clone();
        drop(self);
        std::thread::sleep(std::time::Duration::from_millis(50));
        Self::remove_storage_path(&path);
    }

    fn get_storage_path() -> String {
        let storage_path = format!("temp-runs/storage_{}.db", std::process::id());
        if path::Path::new(&storage_path).exists() {
            Self::remove_storage_path(&storage_path);
        }
        storage_path
    }

    fn remove_storage_path(storage_path: &str) {
        info!("Cleaning up storage file: {}", storage_path);
        if path::Path::new(&storage_path).exists() {
            fs::remove_dir_all(&storage_path)
                .unwrap_or_else(|e| error!("Warning: could not remove storage: {e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_ready_to_dispatch() {
        use crate::types::{CoordinatedTx, FeeInfo, TxKind};
        use bitcoin::hashes::{sha256d, Hash};
        use bitcoin::{absolute::LockTime, transaction::Version, Transaction, Txid};

        let make_tx = |target: BlockHeight| CoordinatedTx {
            txid: Txid::from_raw_hash(sha256d::Hash::hash(&[0u8; 32])),
            tx: Transaction {
                version: Version::TWO,
                lock_time: LockTime::ZERO,
                input: vec![],
                output: vec![],
            },
            kind: TxKind::Normal,
            state: ToDispatch,
            broadcast_block_height: 0,
            target_block_height: target,
            stuck_in_mempool_blocks: 0,
            confirmation_trigger: 0,
            retry_count: 0,
            fee_info: FeeInfo {
                fee: 0,
                fee_rate: 1,
                weight: 0,
            },
            context: String::new(),
        };

        assert!(make_tx(100).is_ready_to_dispatch(100));
        assert!(make_tx(100).is_ready_to_dispatch(101));
        assert!(!make_tx(100).is_ready_to_dispatch(99));
    }

    #[test]
    fn test_is_stuck_in_mempool() {
        use crate::types::{CoordinatedTx, FeeInfo, TxKind};
        use bitcoin::hashes::{sha256d, Hash};
        use bitcoin::{absolute::LockTime, transaction::Version, Transaction, Txid};

        let make_tx = |broadcast: BlockHeight, threshold: u32| CoordinatedTx {
            txid: Txid::from_raw_hash(sha256d::Hash::hash(&[1u8; 32])),
            tx: Transaction {
                version: Version::TWO,
                lock_time: LockTime::ZERO,
                input: vec![],
                output: vec![],
            },
            kind: TxKind::Normal,
            state: InMempool,
            broadcast_block_height: broadcast,
            target_block_height: 0,
            stuck_in_mempool_blocks: threshold,
            confirmation_trigger: 0,
            retry_count: 0,
            fee_info: FeeInfo {
                fee: 0,
                fee_rate: 1,
                weight: 0,
            },
            context: String::new(),
        };

        // threshold disabled
        assert!(!make_tx(100, 0).is_stuck_in_mempool(200));
        // not yet broadcast
        assert!(!make_tx(0, 10).is_stuck_in_mempool(200));
        // below threshold
        assert!(!make_tx(100, 10).is_stuck_in_mempool(109));
        // exactly at threshold
        assert!(make_tx(100, 10).is_stuck_in_mempool(110));
        // above threshold
        assert!(make_tx(100, 10).is_stuck_in_mempool(200));
    }
}
