/// Testnet3 integration tests.
/// All tests are `#[ignore]`d — they never run in CI.
///
/// # Workflow
///
/// 0. [Optional] Check if testnet3 node is accessible via `testnet_rpc_config()`:
///     cargo test --test testnet test_rpc_connection -- --ignored
///
/// 1. Generate a fresh key and funding address:
///      cargo test --test testnet test_generate_wallet -- --ignored
///
/// 2. Fund the printed address via a testnet faucet.
///
/// 3. Fill in `tests/testnet_local.yaml` with the secret and UTXO details.
///
/// 4. Dispatch through the coordinator and verify it reaches the mempool:
///      cargo test --test testnet test_dispatch_transaction -- --ignored
mod common;
use common::*;

use bitcoin::hashes::Hash as _;
use bitcoin::{
    ecdsa::Signature as EcdsaSig,
    locktime::absolute::LockTime,
    secp256k1::{Message, Secp256k1, SecretKey},
    sighash::{EcdsaSighashType, SighashCache},
    transaction::{Sequence, Version},
    Address, CompressedPublicKey, Network, OutPoint, PublicKey, ScriptBuf, Transaction, TxIn,
    TxOut, Txid, Witness,
};
use bitcoin_indexer::config::IndexerSettings;
use bitcoincore_rpc::RpcApi as _;
use bitvmx_bitcoin_rpc::{bitcoin_client::BitcoinClient, rpc_config::RpcConfig};
use bitvmx_settings::settings::load_config_file;
use bitvmx_transaction_monitor::{
    config::MonitorSettingsConfig,
    types::{AckMonitorNews, MonitorNews},
};
use rust_bitvmx_bitcoin::{
    config::config::{BitcoinSettings, CoordinatorStorageSettings},
    coordinator::BitcoinCoordinator,
    core::storage::CoordinatorStorage,
    errors::BitcoinCoordinatorError,
    types::{AckNews, CoordinatorNews, News, TransactionState},
};
use serde::Deserialize;
use std::time::{Duration, Instant};
use tracing::info;

const FEE_RATE_SAT_PER_VBYTE: u64 = 2;

// =============================================================================
// Config
// =============================================================================

fn testnet_rpc_config() -> RpcConfig {
    let cfg = load_local_config();
    RpcConfig::new(
        Network::Testnet,
        cfg.url,
        "".to_string(),
        "".to_string(),
        "test_wallet".to_string(),
    )
}

/// Returns `BitcoinSettings` with a checkpoint 10 blocks behind the current tip,
/// so the coordinator only needs to sync a handful of blocks.
fn testnet_settings_near_tip() -> BitcoinSettings {
    let client = BitcoinClient::new_from_config(&testnet_rpc_config())
        .expect("failed to connect to testnet RPC");
    let tip = client
        .client
        .get_block_count()
        .expect("failed to get block count") as u32;
    let checkpoint = tip.saturating_sub(10);
    BitcoinSettings {
        monitor: MonitorSettingsConfig {
            indexer_settings: Some(IndexerSettings {
                checkpoint_height: Some(checkpoint),
            }),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn testnet_coord_storage(storage: &StorageTestConfig) -> CoordinatorStorage {
    CoordinatorStorage::new(
        storage.get_raw_storage(),
        CoordinatorStorageSettings::default(),
    )
}

/// Settings for the full lifecycle test: 2 confirmations to finalize, evict after 1
/// tracking block. Both values are as small as the system allows so the test
/// completes in ~3 testnet blocks (~30 min).
fn testnet_lifecycle_settings() -> BitcoinSettings {
    let client = BitcoinClient::new_from_config(&testnet_rpc_config())
        .expect("failed to connect to testnet RPC");
    let tip = client
        .client
        .get_block_count()
        .expect("failed to get block count") as u32;
    let checkpoint = tip.saturating_sub(10);
    BitcoinSettings {
        monitor: MonitorSettingsConfig {
            max_monitoring_confirmations: Some(2),
            indexer_settings: Some(IndexerSettings {
                checkpoint_height: Some(checkpoint),
            }),
        },
        storage: CoordinatorStorageSettings {
            max_tracking_confirmations: 1,
        },
        ..Default::default()
    }
}

/// Tick once, then sleep `interval`. Returns `true` as soon as `txid` reaches
/// `expected` state in storage; returns `false` if `timeout` is exceeded.
fn poll_until_state_with_sleep(
    coordinator: &BitcoinCoordinator,
    storage: &CoordinatorStorage,
    txid: Txid,
    expected: TransactionState,
    timeout: Duration,
    interval: Duration,
) -> Result<bool, BitcoinCoordinatorError> {
    let start = Instant::now();
    loop {
        coordinator.tick()?;
        if let Some(tx) = storage.get_tx_by_id(txid)? {
            if tx.state == expected {
                return Ok(true);
            }
        }
        if start.elapsed() >= timeout {
            return Ok(false);
        }
        std::thread::sleep(interval);
    }
}

/// Get pending news, ack every item, and return the snapshot.
fn drain_news(coordinator: &BitcoinCoordinator) -> News {
    let news = coordinator.get_news().unwrap();
    for n in &news.monitor_news {
        let ack = match n {
            MonitorNews::Transaction(t, _, ctx) => AckMonitorNews::Transaction(*t, ctx.clone()),
            MonitorNews::NewBlock(_, _) => AckMonitorNews::NewBlock,
            MonitorNews::SpendingUTXOTransaction(t, v, _, ctx) => {
                AckMonitorNews::SpendingUTXOTransaction(*t, *v, ctx.clone())
            }
            MonitorNews::RskPeginTransaction(t, _) => AckMonitorNews::RskPeginTransaction(*t),
        };
        coordinator.ack_news(AckNews::Monitor(ack)).unwrap();
    }
    for n in &news.coordinator_news {
        coordinator
            .ack_news(AckNews::Coordinator(n.clone()))
            .unwrap();
    }
    news
}

/// Tick + sleep until  `TransactionEvicted` coordinator news item is seen for it.
fn poll_until_evicted(
    coordinator: &BitcoinCoordinator,
    storage: &CoordinatorStorage,
    txid: Txid,
    timeout: Duration,
    interval: Duration,
) -> Result<bool, BitcoinCoordinatorError> {
    let start = Instant::now();
    loop {
        coordinator.tick()?;
        let news = coordinator.get_news()?;
        let evicted = news.coordinator_news.iter().any(
            |n| matches!(n, CoordinatorNews::TransactionEvicted { txid: id, .. } if *id == txid),
        );
        if evicted {
            // ack everything accumulated
            for n in &news.monitor_news {
                let ack = match n {
                    MonitorNews::Transaction(t, _, ctx) => {
                        AckMonitorNews::Transaction(*t, ctx.clone())
                    }
                    MonitorNews::NewBlock(_, _) => AckMonitorNews::NewBlock,
                    MonitorNews::SpendingUTXOTransaction(t, v, _, ctx) => {
                        AckMonitorNews::SpendingUTXOTransaction(*t, *v, ctx.clone())
                    }
                    MonitorNews::RskPeginTransaction(t, _) => {
                        AckMonitorNews::RskPeginTransaction(*t)
                    }
                };
                coordinator.ack_news(AckNews::Monitor(ack)).unwrap();
            }
            for n in &news.coordinator_news {
                coordinator
                    .ack_news(AckNews::Coordinator(n.clone()))
                    .unwrap();
            }
            return Ok(true);
        }
        if start.elapsed() >= timeout {
            return Ok(false);
        }
        std::thread::sleep(interval);
    }
}

/// Fields from `tests/testnet_local.yaml`.
#[derive(Deserialize)]
struct TestnetLocalConfig {
    url: String,
    secret: String,
    txid: String,
    vout: u32,
    amount_sats: u64,
}

fn load_local_config() -> TestnetLocalConfig {
    load_config_file(Some("tests/testnet_local.yaml".to_string()))
        .expect("Fill in tests/testnet_local.yaml before running this test")
}

/// Rewrites the UTXO fields in `tests/testnet_local.yaml` after a spend so
/// the remaining output is ready for the next run.
fn update_local_config_after_spend(new_txid: Txid, new_amount_sats: u64) {
    let path = "tests/testnet_local.yaml";
    let content = std::fs::read_to_string(path).expect("failed to read testnet_local.yaml");
    let updated = content
        .lines()
        .map(|line| {
            if line.starts_with("txid:") {
                format!("txid: \"{new_txid}\"")
            } else if line.starts_with("vout:") {
                "vout: 0".to_string()
            } else if line.starts_with("amount_sats:") {
                format!("amount_sats: {new_amount_sats}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(path, updated).expect("failed to update testnet_local.yaml");
}

// =============================================================================
// Transaction building
// =============================================================================

/// Builds and signs a P2WPKH self-transfer. Accepts WIF or 64-char hex secret.
/// Returns the signed transaction and the fee paid.
fn build_signed_p2wpkh_tx(
    secret: &str,
    funding_txid: Txid,
    funding_vout: u32,
    funding_amount_sats: u64,
) -> (Transaction, u64) {
    let secp = Secp256k1::new();

    let secret_key = if secret.len() == 64 {
        let bytes = hex::decode(secret).expect("invalid hex secret");
        SecretKey::from_slice(&bytes).expect("invalid secret key bytes")
    } else {
        bitcoin::PrivateKey::from_wif(secret)
            .expect("invalid WIF secret")
            .inner
    };

    let public_key = PublicKey::new(secret_key.public_key(&secp));
    let compressed = CompressedPublicKey::try_from(public_key).expect("key is not compressed");
    let address = Address::p2wpkh(&compressed, Network::Testnet);
    let spk = address.script_pubkey();
    let input_amount = bitcoin::Amount::from_sat(funding_amount_sats);

    // Build with a zero-value placeholder output so the witness is in place before we measure
    // weight, so the fee can be accurately derived.
    let mut tx = Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: funding_txid,
                vout: funding_vout,
            },
            script_sig: ScriptBuf::new(),
            sequence: Sequence::MAX,
            witness: Witness::new(),
        }],
        output: vec![TxOut {
            value: bitcoin::Amount::ZERO,
            script_pubkey: spk.clone(),
        }],
    };

    // Sign once to populate the witness (P2WPKH witness size is deterministic,
    // so this gives us the true transaction weight).
    let sign = |tx: &Transaction| {
        let sighash = SighashCache::new(tx)
            .p2wpkh_signature_hash(0, &spk, input_amount, EcdsaSighashType::All)
            .expect("failed to compute sighash");
        let sig = secp.sign_ecdsa(&Message::from_digest(sighash.to_byte_array()), &secret_key);
        Witness::p2wpkh(&EcdsaSig::sighash_all(sig), &public_key.inner)
    };
    tx.input[0].witness = sign(&tx);

    // Derive fee from the actual weight of the signed transaction.
    let fee_sats = tx.weight().to_vbytes_ceil() * FEE_RATE_SAT_PER_VBYTE;
    tx.output[0].value = bitcoin::Amount::from_sat(
        funding_amount_sats
            .checked_sub(fee_sats)
            .expect("amount too small to cover fee"),
    );

    // Re-sign now that the output amount is correct (outputs are part of the sighash).
    tx.input[0].witness = sign(&tx);

    info!(
        "Built transaction {} with weight {} wu, fee {} sats",
        tx.compute_txid(),
        tx.weight(),
        fee_sats
    );

    (tx, fee_sats)
}

// =============================================================================
// Tests
// =============================================================================

#[test]
#[ignore]
fn test_rpc_connection() {
    init_trace();
    let client = BitcoinClient::new_from_config(&testnet_rpc_config())
        .expect("failed to connect to testnet RPC");
    let block_count = client
        .client
        .get_block_count()
        .expect("failed to get block count");
    info!("Successfully connected to testnet RPC. Current block count: {block_count}");
}

/// Generates a fresh P2WPKH key and writes `tests/testnet_local.yaml` with the
/// secret pre-filled. Fund the printed address via a faucet, then edit the
/// txid/vout/amount_sats fields and run `test_dispatch_transaction`.
#[test]
#[ignore]
fn test_generate_wallet() {
    init_trace();

    let secp = Secp256k1::new();
    let (secret_key, _) = secp.generate_keypair(&mut bitcoin::secp256k1::rand::rngs::OsRng);
    let private_key = bitcoin::PrivateKey::new(secret_key, Network::Testnet);
    let wif = private_key.to_wif();

    let public_key = PublicKey::new(secret_key.public_key(&secp));
    let compressed = CompressedPublicKey::try_from(public_key).expect("compressed");
    let address = Address::p2wpkh(&compressed, Network::Testnet);

    let yaml = format!(
        "# Generated by test_generate_wallet — fill in the UTXO fields after funding.\n\
         url: \"https://bitcoin-testnet.g.alchemy.com/v2/YOUR_API_KEY\"\n\
         secret: \"{wif}\"\n\
         \n\
         # Fund this address via a testnet faucet, then fill in the fields below.\n\
         # address: {address}\n\
         txid: \"0000000000000000000000000000000000000000000000000000000000000000\"\n\
         vout: 0\n\
         amount_sats: 0\n"
    );
    std::fs::write("tests/testnet_local.yaml", &yaml).expect("failed to write testnet_local.yaml");

    info!("tests/testnet_local.yaml written — fund the address in the file and fill in the UTXO fields.");
}

/// Verifies that the coordinator can connect to testnet3 and sync to the tip.
#[test]
#[ignore]
fn test_coordinator_connects_and_syncs() {
    init_trace();

    let storage = StorageTestConfig::new();
    let coordinator = BitcoinCoordinator::new_with_paths(
        &testnet_rpc_config(),
        storage.get_raw_storage(),
        dummy_key_manager(),
        Some(testnet_settings_near_tip()),
    )
    .expect("Failed to create coordinator");

    assert!(!coordinator.is_ready().unwrap());

    let max_ticks = 200;
    let mut became_ready = false;
    for tick in 1..=max_ticks {
        coordinator.tick().unwrap();
        if coordinator.is_ready().unwrap() {
            became_ready = true;
            info!("Ready after {tick} tick(s)");
            break;
        }
    }

    drop(coordinator);
    storage.remove().unwrap();

    assert!(became_ready, "not ready after {max_ticks} ticks");
}

/// Dispatches a real P2WPKH transaction through the coordinator and verifies
/// it reaches the mempool.
///
/// Reads credentials and UTXO from `tests/testnet_local.yaml`.
/// Run `test_generate_wallet` first to create a funded address.
#[test]
#[ignore]
fn test_dispatch_transaction() {
    init_trace();

    let cfg = load_local_config();
    let funding_txid: Txid = cfg
        .txid
        .parse()
        .expect("invalid txid in testnet_local.yaml");

    let (tx, fee_sats) =
        build_signed_p2wpkh_tx(&cfg.secret, funding_txid, cfg.vout, cfg.amount_sats);
    let txid = tx.compute_txid();

    let storage = StorageTestConfig::new();
    let coordinator = BitcoinCoordinator::new_with_paths(
        &testnet_rpc_config(),
        storage.get_raw_storage(),
        dummy_key_manager(),
        Some(testnet_settings_near_tip()),
    )
    .expect("Failed to create coordinator");

    tick_until_ready(&coordinator).unwrap();

    coordinator
        .dispatch_without_speedup(tx, "testnet_dispatch_test".to_string(), None, None, None)
        .expect("dispatch failed");

    let remaining = cfg.amount_sats - fee_sats;
    update_local_config_after_spend(txid, remaining);
    info!("Dispatched txid: {txid}  |  remaining: {remaining} sats");

    let coord_storage = testnet_coord_storage(&storage);
    let reached_mempool = tick_until_state(
        &coordinator,
        &coord_storage,
        txid,
        TransactionState::InMempool,
        100,
    )
    .unwrap();

    drop(coordinator);
    drop(coord_storage);
    storage.remove().unwrap();

    assert!(reached_mempool, "tx {txid} did not reach InMempool");
}

/// End-to-end lifecycle test: dispatches a real P2WPKH transaction on testnet3
/// and drives it through every coordinator state from `ToDispatch` to eviction.
///
/// Expected test duration: ~3 testnet blocks (~30 minutes).
///
/// Settings used:
/// - `max_monitoring_confirmations = 2`  (finalize after 2 confirmations)
/// - `max_tracking_confirmations  = 1`  (evict 1 block after finalization)
///
/// Phases verified:
///   1. Coordinator syncs to near-tip
///   2. Transaction dispatched → `ToDispatch`
///   3. Transaction accepted by mempool → `InMempool` + monitor news
///   4. First confirmation → `Confirmed` + monitor news
///   5. Second confirmation → `Finalized` + monitor news
///   6. One more block → `TransactionEvicted` coordinator news; tx removed from storage
#[test]
#[ignore]
fn test_coordinator_full_lifecycle() {
    init_trace();

    let cfg = load_local_config();
    let funding_txid: Txid = cfg
        .txid
        .parse()
        .expect("invalid txid in testnet_local.yaml");

    let (tx, fee_sats) =
        build_signed_p2wpkh_tx(&cfg.secret, funding_txid, cfg.vout, cfg.amount_sats);
    let txid = tx.compute_txid();

    let storage_cfg = StorageTestConfig::new();
    let coordinator = BitcoinCoordinator::new_with_paths(
        &testnet_rpc_config(),
        storage_cfg.get_raw_storage(),
        dummy_key_manager(),
        Some(testnet_lifecycle_settings()),
    )
    .expect("failed to create coordinator");

    // ── Phase 1: sync ──────────────────────────────────────────────────────────
    info!("[lifecycle] phase 1: syncing to near-tip...");
    tick_until_ready(&coordinator).unwrap();
    info!("[lifecycle] phase 1: ready");

    // ── Phase 2: dispatch ──────────────────────────────────────────────────────
    info!("[lifecycle] phase 2: dispatching {txid}");
    coordinator
        .dispatch_without_speedup(tx, "lifecycle_test".to_string(), None, Some(1), None)
        .expect("dispatch failed");

    let remaining = cfg.amount_sats - fee_sats;
    update_local_config_after_spend(txid, remaining);
    info!("[lifecycle] dispatched {txid} | remaining: {remaining} sats");

    let coord_storage = testnet_coord_storage(&storage_cfg);

    // ── Phase 3: InMempool ─────────────────────────────────────────────────────
    info!("[lifecycle] phase 3: waiting for InMempool (up to 15 min)...");
    let reached = poll_until_state_with_sleep(
        &coordinator,
        &coord_storage,
        txid,
        TransactionState::InMempool,
        Duration::from_secs(15 * 60),
        Duration::from_secs(15),
    )
    .unwrap();
    assert!(reached, "tx {txid} never reached InMempool within 15 min");

    info!("[lifecycle] phase 3: InMempool confirmed");

    // ── Phase 4: Confirmed ─────────────────────────────────────────────────────
    info!("[lifecycle] phase 4: waiting for Confirmed (needs 1 testnet block, up to 30 min)...");
    let reached = poll_until_state_with_sleep(
        &coordinator,
        &coord_storage,
        txid,
        TransactionState::Confirmed,
        Duration::from_secs(30 * 60),
        Duration::from_secs(30),
    )
    .unwrap();
    assert!(reached, "tx {txid} never reached Confirmed within 30 min");

    let news = drain_news(&coordinator);
    let has_confirmed_news = news.monitor_news.iter().any(|n| {
        matches!(n, MonitorNews::Transaction(id, status, _) if *id == txid && status.is_confirmed())
    });
    assert!(
        has_confirmed_news,
        "no Confirmed monitor news seen for {txid}"
    );
    info!("[lifecycle] phase 4: Confirmed");

    // ── Phase 5: Finalized ─────────────────────────────────────────────────────
    info!("[lifecycle] phase 5: waiting for Finalized (needs 2nd block, up to 30 min)...");
    let reached = poll_until_state_with_sleep(
        &coordinator,
        &coord_storage,
        txid,
        TransactionState::Finalized,
        Duration::from_secs(30 * 60),
        Duration::from_secs(30),
    )
    .unwrap();
    assert!(reached, "tx {txid} never reached Finalized within 30 min");
    info!("[lifecycle] phase 5: Finalized");

    // ── Phase 6: Evicted ───────────────────────────────────────────────────────
    info!("[lifecycle] phase 6: waiting for eviction (needs 3rd block, up to 30 min)...");
    let evicted = poll_until_evicted(
        &coordinator,
        &coord_storage,
        txid,
        Duration::from_secs(30 * 60),
        Duration::from_secs(30),
    )
    .unwrap();
    assert!(evicted, "tx {txid} was not evicted within 20 min");

    assert!(
        coord_storage.get_tx_by_id(txid).unwrap().is_none(),
        "tx {txid} still present in storage after eviction"
    );
    info!("[lifecycle] phase 6: evicted — full lifecycle complete ✓");

    drop(coordinator);
    drop(coord_storage);
    storage_cfg.remove().unwrap();
}
