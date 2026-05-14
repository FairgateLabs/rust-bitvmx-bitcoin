mod common;
use common::*;

use bitcoin::Network;
use bitcoin_coordinator::{
    config::config::{
        BitcoinSettings, CoordinatorSettings, CoordinatorStorageSettings, FeeSettings,
        SpeedupSettings,
    },
    types::{CoordinatorNews, TransactionState},
};
use bitcoincore_rpc::RpcApi as _;
use bitvmx_bitcoin_rpc::bitcoin_client::BitcoinClientApi;
use bitvmx_transaction_monitor::config::MonitorSettingsConfig;
use std::rc::Rc;

// =============================================================================
// Helpers
// =============================================================================

fn cpfp_settings() -> BitcoinSettings {
    BitcoinSettings {
        monitor: MonitorSettingsConfig {
            max_monitoring_confirmations: Some(2), // 2 confirmations to finalize
            ..Default::default()
        },
        storage: CoordinatorStorageSettings {
            max_tracking_confirmations: 1, // 1 block tracking after finalization before eviction
        },
        speedup: SpeedupSettings {
            max_unconfirmed_speedups: 2, // A second boost creates RBF
            max_rbf_attempts: 10,
            min_blocks_before_resend_speedup: 1, // Enables boost after one block
            rbf_fee_multiplier: 1.5,
            bump_fee_percentage: 1.5,
        },
        coordinator: CoordinatorSettings {
            retry_interval_seconds: 1,
            retry_attempts_sending_tx: 3,
        },
        ..Default::default()
    }
}

/// Settings for tests that need more than 2 unconfirmed CPFP slots.
fn boost_settings(max_unconfirmed: u32) -> BitcoinSettings {
    BitcoinSettings {
        speedup: SpeedupSettings {
            max_unconfirmed_speedups: max_unconfirmed,
            max_rbf_attempts: 10,
            min_blocks_before_resend_speedup: 1,
            rbf_fee_multiplier: 1.5,
            bump_fee_percentage: 2.0,
        },
        ..cpfp_settings()
    }
}

/// Settings that inflate the CPFP fee so a 10 000-sat funding can't cover it.
fn multi_funding_settings() -> BitcoinSettings {
    BitcoinSettings {
        fee: FeeSettings {
            min_network_fee_rate: 80,
            max_feerate_sat_vb: 1000,
            base_fee_multiplier: 1.0,
        },
        ..cpfp_settings()
    }
}

// =============================================================================
// HAPPY PATH TESTS
// =============================================================================

/// Full CPFP lifecycle: parent registers with speedup → both parent and CPFP
/// progress through InMempool → Confirmed → Finalized → Evicted in 3 regtest
/// blocks (max_monitoring_confirmations=2, max_tracking_confirmations=1).
#[test]
fn test_cpfp_lifecycle() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = dummy_key_manager();
    let coordinator = create_coordinator_with_km(&setup, Rc::clone(&key_manager), cpfp_settings());
    let coord_storage = get_coord_storage(&setup);

    let funding_utxo = create_funded_speedup_utxo(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        500_000,
    )
    .unwrap();
    let (parent_tx, speedup_data) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        200_000,
    )
    .unwrap();
    let parent_txid = parent_tx.compute_txid();

    tick_until_ready(&coordinator).unwrap();
    coordinator.add_funding(funding_utxo).unwrap();
    coordinator
        .dispatch_with_speedup(parent_tx, speedup_data, ctx("lifecycle"), None, None)
        .unwrap();

    // Dispatch tick: parent → InMempool, CPFP created and dispatched
    coordinator.tick().unwrap();
    assert_eq!(
        coord_storage
            .get_tx_by_id(parent_txid)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::InMempool,
        "parent must be InMempool after dispatch tick"
    );
    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(speedups.len(), 1, "exactly one CPFP must be created");
    let cpfp_txid = speedups[0].txid;
    assert_eq!(
        speedups[0].state,
        TransactionState::InMempool,
        "CPFP must be InMempool right after creation"
    );
    assert!(
        coordinator.get_news().unwrap().is_empty(),
        "no news expected after successful dispatch with funding"
    );

    // Block 1: both Confirmed
    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    let reached = tick_until_all_states(
        &coordinator,
        &coord_storage,
        &[parent_txid, cpfp_txid],
        TransactionState::Confirmed,
        10,
    )
    .unwrap();
    assert!(
        reached,
        "parent and CPFP must both reach Confirmed after 1 block"
    );
    ack_all_news(&coordinator, &coordinator.get_news().unwrap());

    // Block 2: both Finalized
    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    let reached = tick_until_all_states(
        &coordinator,
        &coord_storage,
        &[parent_txid, cpfp_txid],
        TransactionState::Finalized,
        10,
    )
    .unwrap();
    assert!(
        reached,
        "parent and CPFP must both reach Finalized after 2 blocks"
    );
    ack_all_news(&coordinator, &coordinator.get_news().unwrap());

    // Block 3: both Evicted
    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    let mut parent_evicted = false;
    let mut cpfp_evicted = false;
    for _ in 0..10 {
        coordinator.tick().unwrap();
        let news = coordinator.get_news().unwrap();
        for n in &news.coordinator_news {
            if matches!(n, CoordinatorNews::TransactionEvicted { txid: id, .. } if *id == parent_txid)
            {
                parent_evicted = true;
            }
            if matches!(n, CoordinatorNews::TransactionEvicted { txid: id, .. } if *id == cpfp_txid)
            {
                cpfp_evicted = true;
            }
        }
        ack_all_news(&coordinator, &news);
        if parent_evicted && cpfp_evicted {
            break;
        }
    }
    assert!(
        parent_evicted,
        "TransactionEvicted news must fire for parent"
    );
    assert!(cpfp_evicted, "TransactionEvicted news must fire for CPFP");
    assert!(
        coord_storage.get_tx_by_id(parent_txid).unwrap().is_none(),
        "parent must be removed from storage after eviction"
    );
    assert!(
        coord_storage.get_tx_by_id(cpfp_txid).unwrap().is_none(),
        "CPFP must be removed from storage after eviction"
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// Two parents covered by one CPFP, when dispatched in the same tick.
#[test]
fn test_cpfp_two_parents() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = dummy_key_manager();
    let coordinator = create_coordinator_with_km(&setup, Rc::clone(&key_manager), cpfp_settings());
    let coord_storage = get_coord_storage(&setup);

    // Generous funding to cover both CPFPs.
    let funding_utxo = create_funded_speedup_utxo(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        1_000_000,
    )
    .unwrap();
    let (parent_tx1, speedup_data1) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        200_000,
    )
    .unwrap();
    let (parent_tx2, speedup_data2) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        200_000,
    )
    .unwrap();
    let parent_txid1 = parent_tx1.compute_txid();
    let parent_txid2 = parent_tx2.compute_txid();

    tick_until_ready(&coordinator).unwrap();
    coordinator.add_funding(funding_utxo).unwrap();
    coordinator
        .dispatch_with_speedup(parent_tx1, speedup_data1, ctx("batch1"), None, None)
        .unwrap();
    coordinator
        .dispatch_with_speedup(parent_tx2, speedup_data2, ctx("batch2"), None, None)
        .unwrap();

    // Single tick dispatches both parents and creates CPFPs for them.
    coordinator.tick().unwrap();

    assert_eq!(
        coord_storage
            .get_tx_by_id(parent_txid1)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::InMempool,
        "parent1 must be InMempool"
    );
    assert_eq!(
        coord_storage
            .get_tx_by_id(parent_txid2)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::InMempool,
        "parent2 must be InMempool"
    );

    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(
        speedups.len(),
        1,
        "exactly one CPFP must be created for two parents dispatched in the same tick"
    );
    assert!(
        speedups[0].state == TransactionState::InMempool,
        "CPFP must be InMempool right after creation"
    );

    let news = coordinator.get_news().unwrap();
    assert!(
        news.coordinator_news.is_empty(),
        "no coordinator news expected; got {:?}",
        news.coordinator_news
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

// =============================================================================
// ERROR / EDGE CASE TESTS
// =============================================================================

/// Registering a parent with speedup but without calling `add_funding` causes
/// the coordinator to emit `FundingNotAvailable` news on the dispatch tick.
/// The parent reaches InMempool; no CPFP is created.
#[test]
fn test_cpfp_no_funding() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = dummy_key_manager();
    let coordinator = create_coordinator_with_km(&setup, Rc::clone(&key_manager), cpfp_settings());
    tick_until_ready(&coordinator).unwrap();

    let (parent_tx, speedup_data) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        200_000,
    )
    .unwrap();
    let parent_txid = parent_tx.compute_txid();

    // Register without funding.
    coordinator
        .dispatch_with_speedup(parent_tx, speedup_data, ctx("no_funding"), None, None)
        .unwrap();

    // Tick: parent dispatched → InMempool; CPFP creation finds no funding.
    coordinator.tick().unwrap();

    let coord_storage = get_coord_storage(&setup);
    assert_eq!(
        coord_storage
            .get_tx_by_id(parent_txid)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::InMempool,
        "parent must be InMempool even without funding"
    );

    let news = coordinator.get_news().unwrap();
    assert!(
        news.coordinator_news
            .iter()
            .any(|n| matches!(n, CoordinatorNews::FundingNotAvailable)),
        "Expected FundingNotAvailable news; got {:?}",
        news.coordinator_news
    );
    assert!(
        coord_storage.get_speedups_ordered().unwrap().is_empty(),
        "no CPFP must be created when funding is absent"
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// After one confirming block is invalidated, the coordinator detects the reorg
/// and resets both the parent and its CPFP from Confirmed back to InMempool.
#[test]
fn test_cpfp_reorg() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = dummy_key_manager();
    let coordinator = create_coordinator_with_km(&setup, Rc::clone(&key_manager), cpfp_settings());
    let coord_storage = get_coord_storage(&setup);

    let funding_utxo = create_funded_speedup_utxo(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        500_000,
    )
    .unwrap();
    let (parent_tx, speedup_data) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        200_000,
    )
    .unwrap();
    let parent_txid = parent_tx.compute_txid();

    tick_until_ready(&coordinator).unwrap();
    coordinator.add_funding(funding_utxo).unwrap();
    coordinator
        .dispatch_with_speedup(parent_tx, speedup_data, ctx("reorg"), None, None)
        .unwrap();

    // Dispatch tick.
    coordinator.tick().unwrap();
    let cpfp_txid = coord_storage.get_speedups_ordered().unwrap()[0].txid;

    // Record current height; the next mine will produce height + 1.
    let height_before = setup.bitcoin_client.get_best_block().unwrap() as u64;

    // Mine 1 block → both Confirmed.
    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    let confirm_block_hash = setup
        .bitcoin_client
        .client
        .get_block_hash(height_before + 1)
        .unwrap();

    let reached = tick_until_all_states(
        &coordinator,
        &coord_storage,
        &[parent_txid, cpfp_txid],
        TransactionState::Confirmed,
        10,
    )
    .unwrap();
    assert!(
        reached,
        "parent and CPFP must both be Confirmed before the reorg"
    );
    ack_all_news(&coordinator, &coordinator.get_news().unwrap());

    // Reorg: invalidate the confirming block; txs return to the mempool.
    setup
        .bitcoin_client
        .client
        .invalidate_block(&confirm_block_hash)
        .unwrap();

    // After ticking, the coordinator must detect the reorg and reset both to InMempool.
    let reached = tick_until_all_states(
        &coordinator,
        &coord_storage,
        &[parent_txid, cpfp_txid],
        TransactionState::InMempool,
        10,
    )
    .unwrap();
    assert!(
        reached,
        "parent and CPFP must both be reset to InMempool after reorg"
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// Mining empty blocks (no mempool transactions included) advances the block
/// height without confirming the parent or CPFP.  Once height advances by
/// ≥ min_blocks_before_resend_speedup, the coordinator creates and dispatches a
/// second CPFP (boost) with a higher fee multiplier in the same tick.
#[test]
fn test_cpfp_boost() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = dummy_key_manager();
    let coordinator = create_coordinator_with_km(&setup, Rc::clone(&key_manager), cpfp_settings());
    let coord_storage = get_coord_storage(&setup);

    let funding_utxo = create_funded_speedup_utxo(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        1_000_000,
    )
    .unwrap();
    let (parent_tx, speedup_data) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        200_000,
    )
    .unwrap();

    tick_until_ready(&coordinator).unwrap();
    coordinator.add_funding(funding_utxo).unwrap();
    coordinator
        .dispatch_with_speedup(parent_tx, speedup_data, ctx("boost"), None, None)
        .unwrap();

    // Dispatch tick: parent → InMempool, CPFP1 created and dispatched.
    coordinator.tick().unwrap();
    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(speedups.len(), 1, "one CPFP after dispatch");
    let cpfp1_txid = speedups[0].txid;
    let cpfp1_bump = speedups[0].speedup_kind().unwrap().context().bump_fee_used;

    // Mine 1 empty block: height advances without confirming parent or CPFP.
    mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();

    //boost_if_stale fires → CPFP2 built and dispatched in the same tick.
    coordinator.tick().unwrap();

    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(speedups.len(), 2, "boost must add a second speedup");
    let cpfp2 = &speedups[1];
    let cpfp2_bump = cpfp2.speedup_kind().unwrap().context().bump_fee_used;
    assert_eq!(
        cpfp2.state,
        TransactionState::InMempool,
        "boosted CPFP must be dispatched in the same tick"
    );
    assert!(
        cpfp2_bump > cpfp1_bump,
        "boosted CPFP must have a higher fee multiplier: {} > {}",
        cpfp2_bump,
        cpfp1_bump
    );
    assert_ne!(cpfp2.txid, cpfp1_txid, "boost must produce a distinct txid");

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// Each successive boost must have a higher fee multiplier than the previous boost.
#[test]
fn test_cpfp_fee_escalates_across_boosts() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = dummy_key_manager();
    // max_unconfirmed=3 so both boosts stay as CPFP (not RBF).
    let coordinator =
        create_coordinator_with_km(&setup, Rc::clone(&key_manager), boost_settings(3));
    let coord_storage = get_coord_storage(&setup);

    let funding_utxo = create_funded_speedup_utxo(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        2_000_000,
    )
    .unwrap();
    let (parent_tx, speedup_data) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        200_000,
    )
    .unwrap();

    tick_until_ready(&coordinator).unwrap();
    coordinator.add_funding(funding_utxo).unwrap();
    coordinator
        .dispatch_with_speedup(parent_tx, speedup_data, ctx("escalate"), None, None)
        .unwrap();

    coordinator.tick().unwrap();
    assert_eq!(coord_storage.get_speedups_ordered().unwrap().len(), 1);

    // Boost 1.
    mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    coordinator.tick().unwrap();
    assert_eq!(
        coord_storage.get_speedups_ordered().unwrap().len(),
        2,
        "boost 1 must add CPFP2"
    );

    // Boost 2.
    mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    coordinator.tick().unwrap();
    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(speedups.len(), 3, "boost 2 must add CPFP3");

    let b0 = speedups[0].speedup_kind().unwrap().context().bump_fee_used;
    let b1 = speedups[1].speedup_kind().unwrap().context().bump_fee_used;
    let b2 = speedups[2].speedup_kind().unwrap().context().bump_fee_used;
    assert!(
        b0 < b1 && b1 < b2,
        "fee multiplier must escalate with each boost: {} < {} < {}",
        b0,
        b1,
        b2
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// Once the in-mempool speedup count reaches max_unconfirmed_speedups, the next
/// boost switches from CPFP to RBF.
/// cpfp_settings: max_unconfirmed_speedups=2.
#[test]
fn test_cpfp_rbf_after_max_unconfirmed_reached() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = dummy_key_manager();
    let coordinator = create_coordinator_with_km(&setup, Rc::clone(&key_manager), cpfp_settings());
    let coord_storage = get_coord_storage(&setup);

    let funding_utxo = create_funded_speedup_utxo(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        2_000_000,
    )
    .unwrap();
    let (parent_tx, speedup_data) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        200_000,
    )
    .unwrap();

    tick_until_ready(&coordinator).unwrap();
    coordinator.add_funding(funding_utxo).unwrap();
    coordinator
        .dispatch_with_speedup(
            parent_tx.clone(),
            speedup_data,
            ctx("rbf_limit"),
            None,
            None,
        )
        .unwrap();

    // Dispatch tick: CPFP1 InMempool (1 unconfirmed).
    coordinator.tick().unwrap();
    assert_eq!(coord_storage.get_speedups_ordered().unwrap().len(), 1);

    // Boost 1: 1 < 2 → CPFP.
    mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    coordinator.tick().unwrap();
    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(speedups.len(), 2, "boost 1 must create CPFP2");
    assert!(
        !speedups[1].speedup_kind().unwrap().is_rbf(),
        "boost 1 must be a CPFP (1 unconfirmed < limit of 2)"
    );

    // Boost 2: 2 >= 2 → RBF.
    mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    coordinator.tick().unwrap();
    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(speedups.len(), 3, "boost 2 must add a third speedup (RBF)");
    assert!(
        speedups[2].speedup_kind().unwrap().is_rbf(),
        "boost 2 must be RBF once the unconfirmed limit is reached"
    );

    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    coordinator.tick().unwrap();
    let reached = tick_until_all_states(
        &coordinator,
        &coord_storage,
        &[
            parent_tx.compute_txid(),
            speedups[0].txid,
            speedups[1].txid,
            speedups[2].txid,
        ],
        TransactionState::Confirmed,
        10,
    )
    .unwrap();
    assert!(
        reached,
        "parent, CPFP1, CPFP2, and RBF must all confirm after the boosts"
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// After both a parent and its CPFP enter the mempool, all transactions are
/// evicted from mempool. Both must be back in InMempool within a few ticks.
#[test]
fn test_cpfp_orphan_requeue() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = dummy_key_manager();
    let mut settings = cpfp_settings();
    settings.speedup.min_blocks_before_resend_speedup = 3; // Disable auto-boost for this test to avoid interference
    let coordinator = create_coordinator_with_km(&setup, Rc::clone(&key_manager), settings);
    let coord_storage = get_coord_storage(&setup);

    let funding_utxo = create_funded_speedup_utxo(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        500_000,
    )
    .unwrap();
    let (parent_tx, speedup_data) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        200_000,
    )
    .unwrap();
    let parent_txid = parent_tx.compute_txid();

    tick_until_ready(&coordinator).unwrap();
    coordinator.add_funding(funding_utxo).unwrap();
    coordinator
        .dispatch_with_speedup(parent_tx, speedup_data, ctx("orphan"), None, None)
        .unwrap();

    // Dispatch tick: both parent and CPFP land in the mempool.
    coordinator.tick().unwrap();
    let cpfp_txid = coord_storage.get_speedups_ordered().unwrap()[0].txid;
    assert_eq!(
        coord_storage
            .get_tx_by_id(parent_txid)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::InMempool,
        "parent must be InMempool before the orphan test"
    );

    // Evict all mempool transactions.
    expire_mempool(&setup.bitcoin_client, &setup.regtest_wallet).unwrap();

    // Both transactions must return to InMempool within a few ticks.
    let reached = tick_until_all_states(
        &coordinator,
        &coord_storage,
        &[parent_txid, cpfp_txid],
        TransactionState::InMempool,
        5,
    )
    .unwrap();
    assert!(
        reached,
        "parent and CPFP must both return to InMempool after orphan re-queue"
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// When a CPFP is finalized the coordinator advances the base funding UTXO to
/// the confirmed change output of that CPFP.  A second parent dispatched later
/// must produce a new CPFP whose funding input is the first CPFP's change
/// output, forming a clean on-chain funding chain.
#[test]
fn test_cpfp_funding_restored_after_finalization() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = dummy_key_manager();
    let coordinator = create_coordinator_with_km(&setup, Rc::clone(&key_manager), cpfp_settings());
    let coord_storage = get_coord_storage(&setup);

    let funding_utxo = create_funded_speedup_utxo(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        1_000_000,
    )
    .unwrap();
    let (parent_tx1, speedup_data1) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        200_000,
    )
    .unwrap();
    let parent1_txid = parent_tx1.compute_txid();

    tick_until_ready(&coordinator).unwrap();
    coordinator.add_funding(funding_utxo).unwrap();
    coordinator
        .dispatch_with_speedup(parent_tx1, speedup_data1, ctx("restore_p1"), None, None)
        .unwrap();

    // Dispatch tick: parent1 → InMempool, CPFP1 created and dispatched.
    coordinator.tick().unwrap();
    let cpfp1_txid = coord_storage.get_speedups_ordered().unwrap()[0].txid;

    // Block 1 → both Confirmed.
    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    let reached = tick_until_all_states(
        &coordinator,
        &coord_storage,
        &[parent1_txid, cpfp1_txid],
        TransactionState::Confirmed,
        10,
    )
    .unwrap();
    assert!(reached, "parent1 and CPFP1 must reach Confirmed");
    ack_all_news(&coordinator, &coordinator.get_news().unwrap());

    // Block 2 → both Finalized (max_monitoring_confirmations=2).
    // At finalization, update_funding stores CPFP1's change output as the new base funding.
    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    let reached = tick_until_all_states(
        &coordinator,
        &coord_storage,
        &[parent1_txid, cpfp1_txid],
        TransactionState::Finalized,
        10,
    )
    .unwrap();
    assert!(reached, "parent1 and CPFP1 must reach Finalized");
    ack_all_news(&coordinator, &coordinator.get_news().unwrap());

    // Dispatch a second parent- No new block mined so CPFP1 is still in storage.
    let (parent_tx2, speedup_data2) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        200_000,
    )
    .unwrap();
    let parent2_txid = parent_tx2.compute_txid();
    coordinator
        .dispatch_with_speedup(parent_tx2, speedup_data2, ctx("restore_p2"), None, None)
        .unwrap();

    // Parent2 dispatched, CPFP2 built using CPFP1's confirmed change output as funding.
    coordinator.tick().unwrap();

    let speedups = coord_storage.get_speedups_ordered().unwrap();
    let cpfp2 = speedups
        .iter()
        .find(|s| s.txid != cpfp1_txid)
        .expect("a new CPFP must exist for parent2");
    assert_eq!(
        cpfp2.state,
        TransactionState::InMempool,
        "CPFP2 must be InMempool"
    );
    assert_eq!(
        coord_storage
            .get_tx_by_id(parent2_txid)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::InMempool,
        "parent2 must be InMempool"
    );

    // CPFP2's funding input must spend CPFP1's change output.
    assert!(
        cpfp2
            .tx
            .input
            .iter()
            .any(|inp| inp.previous_output.txid == cpfp1_txid),
        "CPFP2 must spend CPFP1's confirmed change output (cpfp1={}). Funding chain not restored",
        cpfp1_txid
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// Two fundings are registered; the first is below the CPFP fee, the second
/// is plenty. The coordinator must advance past the first and build the CPFP
/// against the second without emitting `InsufficientFunds`.
#[test]
fn test_cpfp_advances_to_next_funding() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = dummy_key_manager();
    let coordinator =
        create_coordinator_with_km(&setup, Rc::clone(&key_manager), multi_funding_settings());
    let coord_storage = get_coord_storage(&setup);

    // Funding A: exactly at min_funding_amount_sats (10 000). Will pass the
    // add-time validator but the inflated CPFP fee (~100 sat/vB * ~200 vB =
    // ~20 000) will exceed its amount.
    let funding_a = create_funded_speedup_utxo(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        10_000,
    )
    .unwrap();
    // Funding B: comfortably above any plausible CPFP fee at this multiplier.
    let funding_b = create_funded_speedup_utxo(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        1_000_000,
    )
    .unwrap();

    // Small parent output so the CPFP can't offset its fee with the value it
    // claims back from the parent.
    let (parent_tx, speedup_data) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        1_000,
    )
    .unwrap();
    let parent_txid = parent_tx.compute_txid();

    tick_until_ready(&coordinator).unwrap();
    coordinator.add_funding(funding_a.clone()).unwrap();
    coordinator.add_funding(funding_b.clone()).unwrap();
    coordinator
        .dispatch_with_speedup(parent_tx, speedup_data, ctx("multi_funding"), None, None)
        .unwrap();

    // Tick 1: parent dispatches (InMempool), CPFP build with A fails (fee
    // exceeds A.amount), funding advances to B.
    coordinator.tick().unwrap();
    assert_eq!(
        coord_storage
            .get_tx_by_id(parent_txid)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::InMempool,
        "parent must stay InMempool after CPFP build fails (pending set handles retry)"
    );
    assert!(
        coord_storage.get_speedups_ordered().unwrap().is_empty(),
        "no CPFP must be created when funding A is insufficient"
    );
    let news_after_tick1 = coordinator.get_news().unwrap();
    assert!(
        news_after_tick1
            .coordinator_news
            .iter()
            .any(|n| matches!(n, CoordinatorNews::FundingConsumed { txid, .. } if *txid == funding_a.txid)),
        "FundingConsumed must fire for funding A; got {:?}",
        news_after_tick1.coordinator_news
    );
    assert!(
        !news_after_tick1
            .coordinator_news
            .iter()
            .any(|n| matches!(n, CoordinatorNews::InsufficientFunds { .. })),
        "no InsufficientFunds while the queue still has B; got {:?}",
        news_after_tick1.coordinator_news
    );
    ack_all_news(&coordinator, &news_after_tick1);

    // Tick 2: pending set still has parent (InMempool), CPFP built with B.
    coordinator.tick().unwrap();

    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(
        speedups.len(),
        1,
        "exactly one CPFP must exist after the second tick"
    );
    let cpfp = &speedups[0];
    assert_eq!(
        cpfp.state,
        TransactionState::InMempool,
        "CPFP must be InMempool"
    );

    // The CPFP must spend funding B, not funding A.
    assert!(
        cpfp.tx
            .input
            .iter()
            .any(|i| i.previous_output.txid == funding_b.txid),
        "CPFP must spend funding B",
    );
    assert!(
        cpfp.tx
            .input
            .iter()
            .all(|i| i.previous_output.txid != funding_a.txid),
        "CPFP must not spend funding A (advanced past)"
    );

    // Confirming block: parent and CPFP both reach Confirmed.
    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    let reached = tick_until_all_states(
        &coordinator,
        &coord_storage,
        &[parent_txid, cpfp.txid],
        TransactionState::Confirmed,
        10,
    )
    .unwrap();
    assert!(
        reached,
        "parent and CPFP must both reach Confirmed after one block"
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// When the funding queue is entirely exhausted (all entries too small),
/// `InsufficientFunds` is emitted and the parent stays in `InMempool` with
/// no CPFP. Once the user registers new funding, the next tick picks up the
/// parent from the pending set and creates the CPFP
#[test]
fn test_cpfp_recovers_after_queue_was_exhausted() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = dummy_key_manager();
    let coordinator =
        create_coordinator_with_km(&setup, Rc::clone(&key_manager), multi_funding_settings());
    let coord_storage = get_coord_storage(&setup);

    // Create both UTXOs up front (each mines a block) so no block is mined between ticks
    let funding_a = create_funded_speedup_utxo(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        10_000,
    )
    .unwrap();
    let funding_b = create_funded_speedup_utxo(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        1_000_000,
    )
    .unwrap();

    let (parent_tx, speedup_data) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        1_000,
    )
    .unwrap();
    let parent_txid = parent_tx.compute_txid();

    tick_until_ready(&coordinator).unwrap();
    // Register only A with the coordinator. B is confirmed on-chain but
    // the coordinator doesn't know about it yet (simulates user adding funding
    // after the queue was exhausted).
    let funding_a_txid = funding_a.txid;
    coordinator.add_funding(funding_a).unwrap();
    coordinator
        .dispatch_with_speedup(
            parent_tx,
            speedup_data,
            ctx("exhausted_funding"),
            None,
            None,
        )
        .unwrap();

    // Tick 1: CPFP fails, queue advances → empty, InsufficientFunds fired.
    coordinator.tick().unwrap();
    assert_eq!(
        coord_storage
            .get_tx_by_id(parent_txid)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::InMempool,
        "parent must stay InMempool after queue exhaustion"
    );
    assert!(
        coord_storage.get_speedups_ordered().unwrap().is_empty(),
        "no CPFP must exist after queue exhaustion"
    );
    let news = coordinator.get_news().unwrap();
    assert!(
        news.coordinator_news
            .iter()
            .any(|n| matches!(n, CoordinatorNews::FundingConsumed { txid, .. } if *txid == funding_a_txid)),
        "FundingConsumed must fire for funding A; got {:?}",
        news.coordinator_news
    );
    assert!(
        news.coordinator_news
            .iter()
            .any(|n| matches!(n, CoordinatorNews::InsufficientFunds { .. })),
        "InsufficientFunds must be emitted when the queue is empty; got {:?}",
        news.coordinator_news
    );
    ack_all_news(&coordinator, &news);

    // User now registers B. The coordinator learns about it without mining.
    coordinator.add_funding(funding_b.clone()).unwrap();

    // Tick 2: pending set has parent → CPFP built with B → dispatched.
    coordinator.tick().unwrap();

    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(speedups.len(), 1, "exactly one CPFP after recovery");
    let cpfp = &speedups[0];
    assert_eq!(cpfp.state, TransactionState::InMempool);
    assert!(
        cpfp.tx
            .input
            .iter()
            .any(|i| i.previous_output.txid == funding_b.txid),
        "CPFP must spend funding B"
    );
    let news = coordinator.get_news().unwrap();
    assert!(
        !news
            .coordinator_news
            .iter()
            .any(|n| matches!(n, CoordinatorNews::InsufficientFunds { .. })),
        "no further InsufficientFunds after recovery; got {:?}",
        news.coordinator_news
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}
