use crate::{
    core::storage::CoordinatorStorage,
    types::TransactionState::{self, *},
};
use std::{fs, path, rc::Rc};
use storage_backend::{storage::Storage, storage_config::StorageConfig};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

impl TransactionState {
    pub fn can_transition_to(&self, next: &TransactionState) -> bool {
        match (self, next) {
            // Normal flow
            (ToDispatch, InMempool) => true,
            (InMempool, Confirmed) => true,
            (Confirmed, Finalized) => true,

            // Other cases
            (InMempool, ToDispatch) => true, // Re-dispatch (not found)
            (_, Failed) => true,             // Failures
            (Confirmed, InMempool) => true,  // Reorg
            (a, b) if a == b => true,        // Idempotency
            // (InMempool, Replaced) => true, // Replacement (RBF)
            _ => false,
        }
    }
}

//Implement display for Transaction State
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

// TODO: this are test functions
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

    // Try to set the global default, but ignore if it's already set
    // This allows multiple tests to call this function without panicking
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
        // clean up the test’s storage file
        info!("Cleaning up storage file: {}", storage_path);
        if path::Path::new(&storage_path).exists() {
            fs::remove_dir_all(&storage_path)
                .unwrap_or_else(|e| error!("Warning: could not remove storage: {e}"))
        }
    }
}
