#![allow(dead_code)]
include!("../../src/test_utils/mod.rs");
use std::collections::HashMap;

pub use bitcoin_coordinator::types;
use bitvmx_transaction_monitor::types::{AckMonitorNews, MonitorNews};

use bitcoin::{Address, Amount, CompressedPublicKey, OutPoint};
use bitcoin_coordinator::{
    config::config::{BitcoinSettings, CoordinatorStorageSettings},
    coordinator::BitcoinCoordinator,
    core::storage::CoordinatorStorage,
    errors::BitcoinCoordinatorError,
    types::{AckNews, News},
};
use bitcoincore_rpc::{
    json::{AddressType::Bech32, CreateRawTransactionInput, SignRawTransactionInput},
    RpcApi as _,
};
use bitvmx_bitcoin_rpc::bitcoin_client::{BitcoinClient, BitcoinClientApi};
use key_manager::key_type::BitcoinKeyType;
use protocol_builder::types::output::SpeedupData;

/// Configuration for creating a test setup
pub struct TestSetupConfig {
    pub blocks_mined: u32,
    pub bitcoind_flags: Option<BitcoindFlags>,
}

impl Default for TestSetupConfig {
    fn default() -> Self {
        Self {
            blocks_mined: 102,
            bitcoind_flags: None,
        }
    }
}

/// Wrapper around an `Rc<KeyManager>` that owns its on-disk storage
/// directory under `temp-runs/` and removes it on drop
pub struct TestKeyManager {
    km: Option<Rc<KeyManager>>,
    path: String,
}

impl TestKeyManager {
    pub fn new() -> Self {
        let path = format!("temp-runs/km_{}", Uuid::new_v4());
        let config = StorageConfig {
            path: path.clone(),
            password: None,
        };
        let km = Rc::new(
            KeyManager::new(bitcoin::Network::Regtest, None, None, &config)
                .expect("TestKeyManager: failed to construct KeyManager"),
        );
        Self { km: Some(km), path }
    }

    pub fn rc(&self) -> Rc<KeyManager> {
        Rc::clone(
            self.km
                .as_ref()
                .expect("TestKeyManager: already cleaned up"),
        )
    }
}

impl Default for TestKeyManager {
    fn default() -> Self {
        Self::new()
    }
}

impl std::ops::Deref for TestKeyManager {
    type Target = KeyManager;
    fn deref(&self) -> &Self::Target {
        self.km
            .as_ref()
            .expect("TestKeyManager: already cleaned up")
    }
}

impl Drop for TestKeyManager {
    fn drop(&mut self) {
        self.km.take();
        std::thread::sleep(std::time::Duration::from_millis(100));
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Test setup components that are commonly used across tests
pub struct TestSetup {
    pub storage: StorageTestConfig,
    pub bitcoind: TestBitcoind,
    pub bitcoin_client: Rc<BitcoinClient>,
    pub regtest_wallet: Address,
    pub key_manager: TestKeyManager,
}

impl TestSetup {
    /// Creates a complete test setup with all common components
    pub fn new(config: TestSetupConfig) -> Result<Self, anyhow::Error> {
        let bitcoind = TestBitcoind::new(None, config.bitcoind_flags)?;

        let storage = StorageTestConfig::new();
        let bitcoin_client = Rc::new(BitcoinClient::new_from_config(&bitcoind.rpc_config)?);
        let (_public_key, _funding_wallet, regtest_wallet) = Self::setup_wallet_and_mine_blocks(
            &bitcoin_client,
            bitcoind.rpc_config.network,
            config.blocks_mined,
        )?;
        let key_manager = TestKeyManager::new();

        Ok(TestSetup {
            bitcoind,
            storage,
            bitcoin_client,
            regtest_wallet,
            key_manager,
        })
    }

    /// Sets up wallet and mines initial blocks
    fn setup_wallet_and_mine_blocks(
        bitcoin_client: &Rc<BitcoinClient>,
        network: Network,
        blocks_mined: u32,
    ) -> Result<(PublicKey, Address, Address), anyhow::Error> {
        let public_key = dummy_pubkey();
        let compressed = CompressedPublicKey::try_from(public_key)
            .map_err(|e| anyhow::anyhow!("Failed to compress public key: {:?}", e))?;
        let funding_wallet = Address::p2wpkh(&compressed, network);
        let regtest_wallet = bitcoin_client
            .init_wallet("test_wallet")
            .map_err(|e| anyhow::anyhow!("Failed to init wallet: {:?}", e))?;

        info!(
            "Mine {} blocks to address {:?}",
            blocks_mined, regtest_wallet
        );

        bitcoin_client
            .mine_blocks_to_address(blocks_mined as u64, &regtest_wallet)
            .map_err(|e| anyhow::anyhow!("Failed to mine blocks: {:?}", e))?;

        Ok((public_key, funding_wallet, regtest_wallet))
    }

    pub fn end_all(self) -> Result<(), anyhow::Error> {
        self.bitcoind.stop()?;
        self.storage.remove()?;
        Ok(())
    }
}

// =============================================================================
// Coordinator construction helpers
// =============================================================================

/// Creates a `BitcoinCoordinator` from a `TestSetup` using default settings.
pub fn create_coordinator(setup: &TestSetup) -> BitcoinCoordinator {
    BitcoinCoordinator::new_with_paths(
        &setup.bitcoind.rpc_config,
        setup.storage.get_raw_storage(),
        setup.key_manager.rc(),
        None,
    )
    .expect("Failed to create BitcoinCoordinator")
}

/// Creates a `BitcoinCoordinator` with a caller-supplied `KeyManager` and settings.
/// Use this for speedup tests where the funding/parent UTXOs must be owned by the
/// same key_manager that signs CPFP inputs.
pub fn create_coordinator_with_km(
    setup: &TestSetup,
    key_manager: Rc<KeyManager>,
    settings: BitcoinSettings,
) -> BitcoinCoordinator {
    BitcoinCoordinator::new_with_paths(
        &setup.bitcoind.rpc_config,
        setup.storage.get_raw_storage(),
        key_manager,
        Some(settings),
    )
    .expect("Failed to create coordinator with key_manager")
}

/// Creates a `BitcoinCoordinator` from a `TestSetup` with custom `BitcoinSettings`.
pub fn create_coordinator_with_settings(
    setup: &TestSetup,
    settings: BitcoinSettings,
) -> BitcoinCoordinator {
    BitcoinCoordinator::new_with_paths(
        &setup.bitcoind.rpc_config,
        setup.storage.get_raw_storage(),
        setup.key_manager.rc(),
        Some(settings),
    )
    .expect("Failed to create BitcoinCoordinator with settings")
}

/// Returns a `CoordinatorStorage` view over the test setup's shared storage.
pub fn get_coord_storage(setup: &TestSetup) -> CoordinatorStorage {
    CoordinatorStorage::new(
        setup.storage.get_raw_storage(),
        CoordinatorStorageSettings::default(),
    )
}

// =============================================================================
// Transaction helpers
// =============================================================================

/// Creates a funded, signed Bitcoin transaction that is **not yet broadcast**.
///
/// Internally this:
/// 1. Sends 'fund_amount' sats from the test wallet to itself (via `fund_address`),
///    mining one confirming block in the process.
/// 2. Builds a raw transaction that spends that output ('fund_amount' - 'fee_amount'
///    sats to a new wallet address, leaving 'fee_amount' sats for fees).
/// 3. Signs the transaction with the test wallet.
/// 4. Returns the signed `Transaction` object without broadcasting it.
///
/// The returned transaction is immediately valid for broadcast and can be
/// handed to the coordinator for dispatch.
/// Load the test wallet, fund a fresh address with `amount` (mines 1 block so it's confirmed),
/// and lock the resulting UTXO so later `fund_address` calls don't spend it. Returns its outpoint.
fn fund_and_lock_utxo(
    bitcoin_client: &BitcoinClient,
    amount: u64,
) -> anyhow::Result<(bitcoin::Txid, u32)> {
    let wallet_address = bitcoin_client
        .init_wallet("test_wallet")
        .map_err(|e| anyhow::anyhow!("init_wallet failed: {:?}", e))?;
    let (funding_tx, funding_vout) = bitcoin_client
        .fund_address(&wallet_address, Amount::from_sat(amount))
        .map_err(|e| anyhow::anyhow!("fund_address failed: {:?}", e))?;
    let funding_txid = funding_tx.compute_txid();
    bitcoin_client
        .client
        .lock_unspent(&[OutPoint {
            txid: funding_txid,
            vout: funding_vout,
        }])
        .map_err(|e| anyhow::anyhow!("lock_unspent failed: {:?}", e))?;
    Ok((funding_txid, funding_vout))
}

/// A fresh wallet-owned Bech32 address.
fn fresh_wallet_address(bitcoin_client: &BitcoinClient) -> anyhow::Result<Address> {
    Ok(bitcoin_client
        .client
        .get_new_address(None, Some(Bech32))
        .map_err(|e| anyhow::anyhow!("get_new_address failed: {:?}", e))?
        .assume_checked())
}

/// Build, sign, and decode a transaction spending `inputs` to `outputs`, returning it unbroadcast.
/// `prevouts` supplies prevout info for inputs the wallet can't auto-locate (e.g. spending a parent
/// that isn't on-chain yet); pass `None` when spending confirmed UTXOs.
fn sign_spend(
    bitcoin_client: &BitcoinClient,
    inputs: &[CreateRawTransactionInput],
    outputs: &HashMap<String, Amount>,
    prevouts: Option<&[SignRawTransactionInput]>,
) -> anyhow::Result<Transaction> {
    let raw = bitcoin_client
        .client
        .create_raw_transaction(inputs, outputs, None, None)
        .map_err(|e| anyhow::anyhow!("create_raw_transaction failed: {:?}", e))?;
    let signed = bitcoin_client
        .client
        .sign_raw_transaction_with_wallet(&raw, prevouts, None)
        .map_err(|e| anyhow::anyhow!("sign_raw_transaction_with_wallet failed: {:?}", e))?;
    anyhow::ensure!(signed.complete, "signing incomplete: {:?}", signed.errors);
    bitcoin::consensus::Decodable::consensus_decode(&mut &signed.hex[..])
        .map_err(|e| anyhow::anyhow!("consensus_decode failed: {:?}", e))
}

fn create_signed_tx_to_dispatch_internal(
    bitcoin_client: &BitcoinClient,
    fund_amount: u64,
    fee_amount: u64,
) -> anyhow::Result<Transaction> {
    let (funding_txid, funding_vout) = fund_and_lock_utxo(bitcoin_client, fund_amount)?;
    let recipient = fresh_wallet_address(bitcoin_client)?;
    let mut outputs = HashMap::new();
    outputs.insert(
        format!("{}", recipient),
        Amount::from_sat(fund_amount - fee_amount),
    );
    let inputs = vec![CreateRawTransactionInput {
        txid: funding_txid,
        vout: funding_vout,
        sequence: None,
    }];
    sign_spend(bitcoin_client, &inputs, &outputs, None)
}

/// Creates a funded, signed Bitcoin transaction with 100 000 sats fee, ready for dispatch.
pub fn create_signed_tx_to_dispatch(bitcoin_client: &BitcoinClient) -> anyhow::Result<Transaction> {
    create_signed_tx_to_dispatch_internal(bitcoin_client, 1_000_000, 100_000)
}

/// Creates a funded, signed Bitcoin transaction with zero fee.
pub fn create_zero_fee_tx(bitcoin_client: &BitcoinClient) -> anyhow::Result<Transaction> {
    create_signed_tx_to_dispatch_internal(bitcoin_client, 1_000_000, 0)
}

/// Build two conflicting signed, unbroadcast transactions (`t`, `t_prime`) that both
/// spend the same confirmed wallet UTXO to different destinations. They therefore have
/// different txids but cannot both be mined. Exactly the shape of a reorg double-spend
/// (with `t` = our tx, `t_prime` = a competing counterparty/timeout spend).
pub fn create_conflicting_txs(
    bitcoin_client: &BitcoinClient,
) -> anyhow::Result<(Transaction, Transaction)> {
    // One confirmed UTXO both txs will fight over.
    let (funding_txid, funding_vout) = fund_and_lock_utxo(bitcoin_client, 1_000_000)?;
    let inputs = vec![CreateRawTransactionInput {
        txid: funding_txid,
        vout: funding_vout,
        sequence: None,
    }];

    // Build + sign one spend of the shared UTXO to a fresh wallet address, paying `fee`.
    let build = |fee: u64| -> anyhow::Result<Transaction> {
        let recipient = fresh_wallet_address(bitcoin_client)?;
        let mut outputs = HashMap::new();
        outputs.insert(format!("{}", recipient), Amount::from_sat(1_000_000 - fee));
        sign_spend(bitcoin_client, &inputs, &outputs, None)
    };

    // Different fees → different output amounts → different txids, same input.
    let t = build(100_000)?;
    let t_prime = build(50_000)?;
    anyhow::ensure!(
        t.compute_txid() != t_prime.compute_txid(),
        "conflicting txs must have distinct txids"
    );
    Ok((t, t_prime))
}

/// Invalidate a block by hash, rolling the chain back below it (used to simulate reorgs).
pub fn invalidate_block(
    bitcoin_client: &BitcoinClient,
    hash: &bitcoin::BlockHash,
) -> anyhow::Result<()> {
    bitcoin_client
        .client
        .invalidate_block(hash)
        .map_err(|e| anyhow::anyhow!("invalidate_block failed: {:?}", e))
}

/// Mine a block containing exactly the given transactions (plus coinbase), ignoring the mempool.
/// Returns the new block hash.
pub fn generate_block_with(
    bitcoin_client: &BitcoinClient,
    address: &Address,
    txs: &[&Transaction],
) -> anyhow::Result<bitcoin::BlockHash> {
    let raw_hexes: Vec<serde_json::Value> = txs
        .iter()
        .map(|tx| serde_json::Value::String(bitcoin::consensus::encode::serialize_hex(*tx)))
        .collect();
    let result: serde_json::Value = bitcoin_client
        .client
        .call(
            "generateblock",
            &[
                serde_json::Value::String(format!("{}", address)),
                serde_json::Value::Array(raw_hexes),
            ],
        )
        .map_err(|e| anyhow::anyhow!("generateblock failed: {:?}", e))?;
    let hash_str = result
        .get("hash")
        .and_then(|h| h.as_str())
        .ok_or_else(|| anyhow::anyhow!("generateblock returned no hash: {:?}", result))?;
    hash_str
        .parse()
        .map_err(|e| anyhow::anyhow!("parse block hash failed: {:?}", e))
}

/// Build a (parent, child) pair of signed, unbroadcast transactions where the
/// child spends the parent's first output.  Both pay 100_000 sats in fees.
pub fn create_parent_and_child_signed_txs(
    bitcoin_client: &BitcoinClient,
) -> (Transaction, Transaction) {
    // Fund 2_000_000 sats so the parent + child chain can each pay a 100_000 fee.
    let (funding_txid, funding_vout) = fund_and_lock_utxo(bitcoin_client, 2_000_000).unwrap();

    // Parent: spends the funded UTXO and sends 1_900_000 sats to a wallet-owned address so the
    // wallet can sign the child.
    let parent_dest = fresh_wallet_address(bitcoin_client).unwrap();
    let mut parent_outputs = HashMap::new();
    parent_outputs.insert(format!("{}", parent_dest), Amount::from_sat(1_900_000));
    let parent = sign_spend(
        bitcoin_client,
        &[CreateRawTransactionInput {
            txid: funding_txid,
            vout: funding_vout,
            sequence: None,
        }],
        &parent_outputs,
        None,
    )
    .unwrap();
    let parent_txid = parent.compute_txid();

    // Child: spends parent:0 and sends 1_800_000 sats to a fresh address. The parent isn't on-chain
    // yet, so supply its prevout explicitly for signing.
    let child_dest = fresh_wallet_address(bitcoin_client).unwrap();
    let mut child_outputs = HashMap::new();
    child_outputs.insert(format!("{}", child_dest), Amount::from_sat(1_800_000));
    let parent_out0 = &parent.output[0];
    let child = sign_spend(
        bitcoin_client,
        &[CreateRawTransactionInput {
            txid: parent_txid,
            vout: 0,
            sequence: None,
        }],
        &child_outputs,
        Some(&[bitcoincore_rpc::json::SignRawTransactionInput {
            txid: parent_txid,
            vout: 0,
            script_pub_key: parent_out0.script_pubkey.clone(),
            redeem_script: None,
            amount: Some(parent_out0.value),
        }]),
    )
    .unwrap();

    (parent, child)
}

/// Mines `n` blocks to `address` using `bitcoin_client`.
pub fn mine_blocks(
    bitcoin_client: &BitcoinClient,
    n: u64,
    address: &Address,
) -> anyhow::Result<()> {
    bitcoin_client
        .mine_blocks_to_address(n, address)
        .map_err(|e| anyhow::anyhow!("mine_blocks_to_address failed: {:?}", e))
}

// =============================================================================
// Speedup / funding helpers
// =============================================================================

/// Fund a P2WPKH address owned by `key_manager` and return the resulting UTXO.
pub fn create_funded_speedup_utxo(
    bitcoin_client: &BitcoinClient,
    key_manager: &KeyManager,
    network: Network,
    amount_sats: u64,
) -> anyhow::Result<Utxo> {
    let pub_key = key_manager
        .next_keypair(BitcoinKeyType::P2wpkh)
        .map_err(|e| anyhow::anyhow!("next_keypair: {:?}", e))?;
    let compressed = CompressedPublicKey::try_from(pub_key)
        .map_err(|e| anyhow::anyhow!("compress pubkey: {:?}", e))?;
    let address = Address::p2wpkh(&compressed, network);

    let (funding_tx, vout) = bitcoin_client
        .fund_address(&address, Amount::from_sat(amount_sats))
        .map_err(|e| anyhow::anyhow!("fund_address: {:?}", e))?;

    let actual_amount = funding_tx.output[vout as usize].value.to_sat();
    Ok(Utxo::new(
        funding_tx.compute_txid(),
        vout,
        actual_amount,
        &pub_key,
    ))
}

/// Create an unbroadcast parent transaction plus a `SpeedupData`.
pub fn create_coordinator_parent_tx(
    bitcoin_client: &BitcoinClient,
    key_manager: &KeyManager,
    network: Network,
    output_sats: u64,
) -> anyhow::Result<(Transaction, SpeedupData)> {
    // Derive a coordinator-owned key for the parent output.
    let out_pub_key = key_manager
        .next_keypair(BitcoinKeyType::P2wpkh)
        .map_err(|e| anyhow::anyhow!("next_keypair: {:?}", e))?;
    let compressed = CompressedPublicKey::try_from(out_pub_key)
        .map_err(|e| anyhow::anyhow!("compress pubkey: {:?}", e))?;
    let coordinator_addr = Address::p2wpkh(&compressed, network);

    // Fund + lock a confirmed wallet UTXO to spend (fee = 100 000 sats).
    let (funding_txid, funding_vout) = fund_and_lock_utxo(bitcoin_client, output_sats + 100_000)?;

    // Build, sign, decode: wallet UTXO → coordinator address.
    let inputs = vec![CreateRawTransactionInput {
        txid: funding_txid,
        vout: funding_vout,
        sequence: None,
    }];
    let mut outputs = HashMap::new();
    outputs.insert(
        format!("{}", coordinator_addr),
        Amount::from_sat(output_sats),
    );
    let tx = sign_spend(bitcoin_client, &inputs, &outputs, None)?;

    let speedup_data = SpeedupData::new(Utxo::new(tx.compute_txid(), 0, output_sats, &out_pub_key));
    Ok((tx, speedup_data))
}

// =============================================================================
// Monitor / coordinator sync helpers
// =============================================================================

/// Tick the coordinator until `is_ready()` returns `true`.
pub fn tick_until_ready(coordinator: &BitcoinCoordinator) -> Result<(), BitcoinCoordinatorError> {
    loop {
        coordinator.tick()?;
        if coordinator.is_ready()? {
            break;
        }
    }
    Ok(())
}

/// Poll the coordinator storage until `txid` reaches `expected_state`, or
/// until `max_ticks` ticks have been performed.  Returns `true` if the state
/// was reached.
pub fn tick_until_state(
    coordinator: &BitcoinCoordinator,
    storage: &CoordinatorStorage,
    txid: bitcoin::Txid,
    expected_state: TransactionState,
    max_ticks: u32,
) -> Result<bool, BitcoinCoordinatorError> {
    for i in 0..max_ticks {
        coordinator.tick()?;
        if let Some(tx) = storage.get_tx_by_id(txid)? {
            if tx.state == expected_state {
                info!(
                    "After {} ticks, reached expected state {:?} for txid {}",
                    i + 1,
                    expected_state,
                    txid
                );
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Poll the coordinator until all `txids` reach `expected_state`, or until
/// `max_ticks` ticks have been performed.  Returns `true` if every txid reached
/// the state within the tick budget.
pub fn tick_until_all_states(
    coordinator: &BitcoinCoordinator,
    storage: &CoordinatorStorage,
    txids: &[bitcoin::Txid],
    expected_state: TransactionState,
    max_ticks: u32,
    sleep_ms: Option<u64>, // Optional wall-clock pause before each tick, in milliseconds.
) -> Result<bool, BitcoinCoordinatorError> {
    for i in 0..max_ticks {
        if let Some(ms) = sleep_ms {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
        coordinator.tick()?;
        let all_reached = txids.iter().all(|txid| {
            storage
                .get_tx_by_id(*txid)
                .ok()
                .flatten()
                .map_or(false, |tx| tx.state == expected_state)
        });
        if all_reached {
            info!(
                "After {} ticks, all {} txids reached {:?}",
                i + 1,
                txids.len(),
                expected_state
            );
            return Ok(true);
        }
    }
    Ok(false)
}

/// Evict all transactions from the mempool by advancing the node's mock clock
/// past the default mempool-expiry window (336 h) and triggering the sweep.
pub fn expire_mempool(bitcoin_client: &BitcoinClient, address: &Address) -> anyhow::Result<()> {
    let best_hash = bitcoin_client
        .client
        .get_best_block_hash()
        .map_err(|e| anyhow::anyhow!("get_best_block_hash failed: {:?}", e))?;
    let header = bitcoin_client
        .client
        .get_block_header_info(&best_hash)
        .map_err(|e| anyhow::anyhow!("get_block_header_info failed: {:?}", e))?;

    // Jump 15 days ahead — comfortably past the default 336 h (14 d) expiry.
    let eviction_time = header.time as i64 + 15 * 24 * 3600;
    bitcoin_client
        .client
        .call::<serde_json::Value>(
            "setmocktime",
            &[serde_json::Value::Number(eviction_time.into())],
        )
        .map_err(|e| anyhow::anyhow!("setmocktime failed: {:?}", e))?;

    // Trigger the eviction sweep by pushing a fresh wallet tx through the
    // mempool. The wallet was funded with mined blocks in `setup_wallet_and_mine_blocks`.
    let wallet_addr = bitcoin_client
        .init_wallet("test_wallet")
        .map_err(|e| anyhow::anyhow!("init_wallet failed: {:?}", e))?;
    bitcoin_client
        .client
        .call::<serde_json::Value>(
            "sendtoaddress",
            &[
                serde_json::Value::String(format!("{}", wallet_addr)),
                serde_json::Value::String("0.00001".to_string()),
            ],
        )
        .map_err(|e| anyhow::anyhow!("sendtoaddress failed: {:?}", e))?;

    // Advance height once so the coordinator sees a new block tick.
    mine_empty_blocks(bitcoin_client, 1, address)
}

/// Mine `n` blocks that contain no mempool transactions.
pub fn mine_empty_blocks(
    bitcoin_client: &BitcoinClient,
    n: u64,
    address: &Address,
) -> anyhow::Result<()> {
    let addr_str = address.to_string();
    for _ in 0..n {
        bitcoin_client // TODO: abstract this behind a `mine_empty_block` method on `BitcoinClient`
            .client
            .call::<serde_json::Value>(
                "generateblock",
                &[
                    serde_json::Value::String(addr_str.clone()),
                    serde_json::Value::Array(vec![]),
                ],
            )
            .map_err(|e| anyhow::anyhow!("generateblock failed: {:?}", e))?;
    }
    Ok(())
}

pub fn ctx(label: &str) -> String {
    format!("test_ctx:{}", label)
}

/// Ack every item in `news` so it is not returned again.
pub fn ack_all_news(coordinator: &BitcoinCoordinator, news: &News) {
    for n in &news.monitor_news {
        let ack = match n {
            MonitorNews::Transaction(t, _, ctx) => AckMonitorNews::Transaction(*t, ctx.clone()),
            MonitorNews::NewBlock(_, _) => AckMonitorNews::NewBlock,
            MonitorNews::SpendingUTXOTransaction(t, v, _, ctx) => {
                AckMonitorNews::SpendingUTXOTransaction(*t, *v, ctx.clone())
            }
            MonitorNews::OutputPatternTransaction(t, _, ctx) => {
                AckMonitorNews::OutputPatternTransaction(*t, ctx.clone())
            }
        };
        coordinator.ack_news(AckNews::Monitor(ack)).unwrap();
    }
    for n in &news.coordinator_news {
        coordinator
            .ack_news(AckNews::Coordinator(n.clone()))
            .unwrap();
    }
}
