use super::types::{CoordinatedTx, FeeInfo, SpeedupContext, SpeedupKind, TransactionState, TxKind};
use bitcoin::{
    absolute::LockTime,
    hashes::{sha256d, Hash},
    secp256k1::{Secp256k1, SecretKey},
    transaction::Version,
    Network, PublicKey, Transaction, Txid,
};
use bitcoind::{
    bitcoind::{Bitcoind, BitcoindFlags},
    config::BitcoindConfig,
};
use bitvmx_bitcoin_rpc::rpc_config::RpcConfig;
use bitvmx_transaction_monitor::monitor::Monitor;
use key_manager::key_manager::KeyManager;
use protocol_builder::types::Utxo;
use rand::Rng;
use std::{default, fs, path, rc::Rc, sync::Mutex};
use storage_backend::{storage::Storage, storage_config::StorageConfig};
use tracing::info;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

pub fn init_trace() {
    let default_modules = [
        "info",
        "bitvmx_transaction_monitor=debug",
        "bitcoin_indexer=debug",
        "bitcoin_rpc=debug",
        "bitcoin_client=debug",
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

pub fn generate_random_string() -> String {
    let mut rng = rand::rng();
    (0..12).map(|_| rng.random_range('a'..='z')).collect()
}

/// An empty transaction, useful for testing coordinator API calls that only
/// need a `Transaction` argument and where the actual network broadcast is not
/// the focus of the test.
pub fn dummy_tx() -> Transaction {
    Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![],
        output: vec![],
    }
}

/// A deterministic public key derived from a known secret.  Used when a
/// `PublicKey` is required but actual signing is not needed.
pub fn dummy_pubkey() -> PublicKey {
    let secp = Secp256k1::new();
    let secret_key = SecretKey::from_slice(&[0x42u8; 32]).unwrap();
    PublicKey::new(bitcoin::secp256k1::PublicKey::from_secret_key(
        &secp,
        &secret_key,
    ))
}

/// A `KeyManager` backed by temporary storage. Suitable for unit / integration
/// tests that do not perform real signing and do not need mnemonic persistence.
pub fn dummy_key_manager() -> Rc<KeyManager> {
    let path = format!("temp-runs/km_{}", Uuid::new_v4());
    let config = StorageConfig {
        path,
        password: None,
    };
    Rc::new(KeyManager::new(Network::Regtest, None, None, &config).unwrap())
}

/// A UTXO with the given `amount` that satisfies the coordinator's default
/// minimum funding threshold (10 000 sats).
pub fn utxo(amount: u64) -> Utxo {
    let txid = Txid::from_raw_hash(sha256d::Hash::hash(amount.to_le_bytes().as_ref()));
    Utxo::new(txid, 0, amount, &dummy_pubkey())
}

/// Minimal `CoordinatedTx` with `TxKind::Normal` and `ToDispatch` state.
/// `seed` determines a unique deterministic txid.
pub fn normal_coordinated_tx(seed: u8) -> CoordinatedTx {
    let txid = Txid::from_raw_hash(sha256d::Hash::hash(&[seed; 32]));
    CoordinatedTx {
        txid,
        tx: dummy_tx(),
        kind: TxKind::Normal,
        state: TransactionState::ToDispatch,
        broadcast_block_height: None,
        target_block_height: 0,
        stuck_in_mempool_blocks: None,
        confirmation_trigger: None,
        settled_block_height: None,
        retry_count: 0,
        fee_info: FeeInfo {
            fee: 0,
            fee_rate: 1,
            weight: 0,
        },
        context: String::new(),
    }
}

/// `CoordinatedTx` with `TxKind::Speedup(CPFP)` and `InMempool` state.
/// `fee_rate` is stored in `fee_info` so callers can drive fee-diff tests.
pub fn cpfp_coordinated_tx(seed: u8, fee_rate: u64) -> CoordinatedTx {
    let txid = Txid::from_raw_hash(sha256d::Hash::hash(&[seed; 32]));
    let mut tx = normal_coordinated_tx(seed);
    tx.kind = TxKind::Speedup(SpeedupKind::CPFP {
        parents: vec![],
        context: SpeedupContext {
            funding_inputs: vec![Utxo::new(txid, 0, 100_000, &dummy_pubkey())],
            replaced_by: None,
            bump_fee_used: 1.0,
            parent_data: vec![],
            spent: false,
        },
    });
    tx.state = TransactionState::InMempool;
    tx.fee_info.fee_rate = fee_rate;
    tx
}

// =============================================================================
// Bitocind & RPC construction
// =============================================================================
static RPC_LOCK: Mutex<()> = Mutex::new(());

pub struct TestBitcoind {
    pub rpc_config: RpcConfig,
    bitcoind: Bitcoind,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl TestBitcoind {
    pub fn new(
        rpc_config: Option<RpcConfig>,
        flags: Option<BitcoindFlags>,
    ) -> Result<Self, anyhow::Error> {
        let _guard = RPC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let rpc_config = rpc_config.unwrap_or_else(Self::default_rpc_config);
        let bitcoind = Self::create_and_start_bitcoind(&rpc_config, flags)?;
        Ok(Self {
            rpc_config: Self::default_rpc_config(),
            bitcoind,
            _guard,
        })
    }
    pub fn create_monitor(&self, storage: Rc<Storage>) -> Monitor {
        let monitor = Monitor::new_with_paths(&self.rpc_config, storage, None).unwrap();
        monitor
    }
    fn default_rpc_config() -> RpcConfig {
        RpcConfig::new(
            Network::Regtest,
            "http://127.0.0.1:18443".to_string(),
            "foo".to_string(),
            "rpcpassword".to_string(),
            format!("test_wallet_{}", Uuid::new_v4()),
        )
    }
    /// Creates and starts bitcoind with optional flags
    fn create_and_start_bitcoind(
        config_bitcoin_client: &RpcConfig,
        flags: Option<BitcoindFlags>,
    ) -> Result<Bitcoind, anyhow::Error> {
        let bitcoind_config = BitcoindConfig::default();
        let bitcoind = Bitcoind::new(bitcoind_config, config_bitcoin_client.clone(), flags);

        info!("Starting bitcoind");
        bitcoind.start().map_err(|e| {
            anyhow::anyhow!(
                "Failed to start bitcoind: {:?}. Make sure Docker is running.",
                e
            )
        })?;

        Ok(bitcoind)
    }

    pub fn stop(self) -> Result<(), anyhow::Error> {
        info!("Stopping bitcoind");
        self.bitcoind.stop()?;
        Ok(())
    }
}

impl default::Default for TestBitcoind {
    fn default() -> Self {
        Self::new(None, None).unwrap()
    }
}

// =============================================================================
// Storage construction
// =============================================================================
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

    pub fn get_raw_storage(&self) -> Rc<Storage> {
        Rc::clone(&self.storage)
    }

    pub fn remove(self) -> Result<(), anyhow::Error> {
        let path = self.path.clone();
        drop(self);
        std::thread::sleep(std::time::Duration::from_millis(100));
        Self::remove_storage_path(&path)
    }

    fn get_storage_path() -> String {
        let storage_path = format!("temp-runs/storage_{}.db", Uuid::new_v4());
        if path::Path::new(&storage_path).exists() {
            Self::remove_storage_path(&storage_path).unwrap();
        }
        storage_path
    }

    fn remove_storage_path(storage_path: &str) -> Result<(), anyhow::Error> {
        info!("Cleaning up storage file: {}", storage_path);
        let result = if path::Path::new(&storage_path).exists() {
            fs::remove_dir_all(&storage_path).map_err(|e| {
                anyhow::anyhow!("Failed to remove storage path {}: {}", storage_path, e)
            })
        } else {
            Err(anyhow::anyhow!(
                "Storage path {} does not exist",
                storage_path
            ))
        };
        result
    }
}
