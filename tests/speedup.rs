mod common;
use common::*;

use bitcoin::Network;
use bitcoin_coordinator::{
    config::config::{
        BitcoinSettings, CoordinatorSettings, CoordinatorStorageSettings, FeeSettings,
        SpeedupSettings,
    },
    coordinator::BitcoinCoordinator,
    core::storage::CoordinatorStorage,
    types::{CoordinatorNews, TransactionState},
};
use bitcoincore_rpc::RpcApi as _;
use bitvmx_bitcoin_rpc::bitcoin_client::BitcoinClientApi;
use bitvmx_transaction_monitor::config::MonitorSettingsConfig;

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
            min_safe_fee_rate: 80,
            max_feerate_sat_vb: 1000,
            base_fee_multiplier: 1.0,
        },
        ..cpfp_settings()
    }
}

/// Settings with an aggressive bump and a moderate `max_feerate_sat_vb`, so
/// the cap is reached within a few boosts. The network rate is set high
/// enough (and the parent output small enough in the test below) for the
/// initial CPFP to have a non-zero effective rate, so the bump-doubling
/// model holds from the start.
fn cap_settings() -> BitcoinSettings {
    BitcoinSettings {
        fee: FeeSettings {
            min_safe_fee_rate: 10,
            max_feerate_sat_vb: 100,
            base_fee_multiplier: 1.0,
        },
        speedup: SpeedupSettings {
            max_unconfirmed_speedups: 5,
            max_rbf_attempts: 10,
            min_blocks_before_resend_speedup: 1,
            rbf_fee_multiplier: 1.5,
            bump_fee_percentage: 2.0,
        },
        ..cpfp_settings()
    }
}

/// Drive the coordinator through the build-then-dispatch sequence and return
/// the CPFP txid once it is InMempool. Asserts that:
/// - After the first tick, the CPFP exists in storage as `ToDispatch`.
/// - After at most `extra_ticks` more ticks, it reaches `InMempool`.
fn build_and_dispatch_cpfp(
    coordinator: &BitcoinCoordinator,
    coord_storage: &CoordinatorStorage,
    extra_ticks: u32,
) -> bitcoin::Txid {
    coordinator.tick().unwrap();
    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(
        speedups.len(),
        1,
        "exactly one CPFP must be built in the first tick after dispatch"
    );
    let cpfp_txid = speedups[0].txid;
    assert_eq!(
        speedups[0].state,
        TransactionState::ToDispatch,
        "CPFP must be saved as ToDispatch on the build tick (broadcast happens on the next tick)"
    );
    let reached = tick_until_state(
        coordinator,
        coord_storage,
        cpfp_txid,
        TransactionState::InMempool,
        extra_ticks,
    )
    .unwrap();
    assert!(
        reached,
        "CPFP must reach InMempool within {} ticks after the build tick",
        extra_ticks
    );
    cpfp_txid
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
    let key_manager = TestKeyManager::new();
    let coordinator = create_coordinator_with_km(&setup, key_manager.rc(), cpfp_settings());
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

    // Tick 1: parent → InMempool, CPFP saved as ToDispatch.
    // Tick 2: CPFP → InMempool.
    let cpfp_txid = build_and_dispatch_cpfp(&coordinator, &coord_storage, 3);
    assert_eq!(
        coord_storage
            .get_tx_by_id(parent_txid)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::InMempool,
        "parent must be InMempool once the CPFP is dispatched"
    );

    // Block 1: both Confirmed.
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

    // Block 2: both Finalized.
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

    // Block 3: both Evicted.
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
    let key_manager = TestKeyManager::new();
    let coordinator = create_coordinator_with_km(&setup, key_manager.rc(), cpfp_settings());
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

    // Tick 1: both parents dispatched to InMempool; one CPFP built covering both, saved as ToDispatch.
    let cpfp_txid = build_and_dispatch_cpfp(&coordinator, &coord_storage, 3);

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

    // The CPFP must reference both parents.
    let cpfp = coord_storage.get_tx_by_id(cpfp_txid).unwrap().unwrap();
    let parents = cpfp.speedup_kind().unwrap().parents();
    assert_eq!(
        parents.len(),
        2,
        "the single CPFP must cover both parents; got {:?}",
        parents
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
    let key_manager = TestKeyManager::new();
    let coordinator = create_coordinator_with_km(&setup, key_manager.rc(), cpfp_settings());
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
    let key_manager = TestKeyManager::new();
    let coordinator = create_coordinator_with_km(&setup, key_manager.rc(), cpfp_settings());
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

    // Drive parent + CPFP to InMempool.
    let cpfp_txid = build_and_dispatch_cpfp(&coordinator, &coord_storage, 3);

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

/// Mining an empty block (no mempool transactions included) advances the chain without confirming
/// the parent or CPFP. Once height advances by ≥ min_blocks_before_resend_speedup, the coordinator
/// builds a boost CPFP and dispatches it on the next tick.
#[test]
fn test_cpfp_boost() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = TestKeyManager::new();
    let coordinator = create_coordinator_with_km(&setup, key_manager.rc(), cpfp_settings());
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

    // CPFP1 saved + dispatched.
    let cpfp1_txid = build_and_dispatch_cpfp(&coordinator, &coord_storage, 3);
    let cpfp1_bump = coord_storage
        .get_tx_by_id(cpfp1_txid)
        .unwrap()
        .unwrap()
        .speedup_kind()
        .unwrap()
        .context()
        .bump_fee_used;

    // Mine 1 empty block so CPFP1 becomes stale.
    mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();

    // Tick triggers boost_if_stale → builds CPFP2 → save as TO-DISPATCH.
    coordinator.tick().unwrap();
    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(speedups.len(), 2, "boost must add a second speedup");
    let cpfp2 = &speedups[1];
    let cpfp2_txid = cpfp2.txid;
    let cpfp2_bump = cpfp2.speedup_kind().unwrap().context().bump_fee_used;
    assert_eq!(
        cpfp2.state,
        TransactionState::ToDispatch,
        "boost CPFP must be saved as ToDispatch on the build tick"
    );
    assert!(
        cpfp2_bump > cpfp1_bump,
        "boosted CPFP must have a higher fee multiplier: {} > {}",
        cpfp2_bump,
        cpfp1_bump
    );
    assert_ne!(cpfp2_txid, cpfp1_txid, "boost must produce a distinct txid");

    // Tick again to dispatch the boost CPFP.
    let reached = tick_until_state(
        &coordinator,
        &coord_storage,
        cpfp2_txid,
        TransactionState::InMempool,
        3,
    )
    .unwrap();
    assert!(reached, "boost CPFP must reach InMempool on the next tick");

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

#[test]
/// Drive a deep boost chain (initial CPFP + N boost CPFP-of-CPFP layers, all
/// covering the same original parent) and verify:
/// * `bump_fee_used` strictly escalates across every speedup
/// * Effective `fee_info.fee_rate` (= `fee_paid / vsize`) strictly escalates
/// * The initial CPFP's `parents` field references the original NeedsSpeedup parent.
/// * Every boost CPFP's `parents = [previous_cpfp.txid]`
/// * Each boost CPFP's tx spends the previous CPFP's last output
/// * Initial CPFP has 2 inputs (parent anchor + funding); boost CPFPs have only the
/// funding-chain input.
/// * A single confirming block carries the entire package (parent + all N
///   boosts) into `Confirmed` together.
fn test_cpfp_fee_escalates_across_boosts() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = TestKeyManager::new();
    // max_unconfirmed_speedups large enough that every boost stays CPFP (not RBF).
    let coordinator = create_coordinator_with_km(&setup, key_manager.rc(), boost_settings(20));
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
    let parent_txid = parent_tx.compute_txid();

    tick_until_ready(&coordinator).unwrap();
    coordinator.add_funding(funding_utxo).unwrap();
    coordinator
        .dispatch_with_speedup(parent_tx, speedup_data, ctx("escalate"), None, None)
        .unwrap();

    // Initial CPFP (covers the NeedsSpeedup parent directly).
    let _cpfp1_txid = build_and_dispatch_cpfp(&coordinator, &coord_storage, 3);

    // Drive N boost iterations. Bump escalation is 1.0 → 2.0 → 4.0 → … so with
    // default max_feerate_sat_vb we can stack quite a few without hitting the cap.
    let boost_iters = 5u32;
    for i in 0..boost_iters {
        mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
        coordinator.tick().unwrap();
        let speedups = coord_storage.get_speedups_ordered().unwrap();
        let expected_len = 2 + i as usize;
        assert_eq!(
            speedups.len(),
            expected_len,
            "boost {} must produce CPFP{} (chain length {})",
            i + 1,
            expected_len,
            expected_len
        );
        let latest_txid = speedups[expected_len - 1].txid;
        let reached = tick_until_state(
            &coordinator,
            &coord_storage,
            latest_txid,
            TransactionState::InMempool,
            3,
        )
        .unwrap();
        assert!(
            reached,
            "boost {} (CPFP{}) must reach InMempool",
            i + 1,
            expected_len
        );
    }

    // Verify the entire boost chain's structure
    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(speedups.len(), boost_iters as usize + 1,);

    // Initial CPFP's `parents` references the original NeedsSpeedup parent.
    let init_parents = speedups[0].speedup_kind().unwrap().parents();
    assert_eq!(
        init_parents,
        vec![parent_txid],
        "initial CPFP's parents must reference the NeedsSpeedup parent (got {:?})",
        init_parents
    );

    // Initial CPFP has 2 inputs (parent anchor + funding utxo).
    assert_eq!(
        speedups[0].tx.input.len(),
        2,
        "initial CPFP must have 2 inputs (anchor + funding)"
    );

    // Walk the boost chain.
    let mut prev_bump = speedups[0].speedup_kind().unwrap().context().bump_fee_used;
    let mut prev_rate: Option<u64> = None;
    for i in 1..speedups.len() {
        let s = &speedups[i];
        let prev = &speedups[i - 1];

        // Boost CPFP-of-CPFP: parents = [previous_cpfp.txid], NOT the original parent.
        let parents = s.speedup_kind().unwrap().parents();
        assert_eq!(
            parents,
            vec![prev.txid],
            "boost {}'s parents must reference the immediate predecessor (got {:?})",
            i,
            parents
        );

        // Boost CPFP has exactly 1 input (the funding-chain link).
        assert_eq!(
            s.tx.input.len(),
            1,
            "boost CPFP {} must have a single funding-chain input",
            i
        );

        // That input must point to the previous CPFP's last output (its change).
        let inp = &s.tx.input[0];
        let prev_last_vout = (prev.tx.output.len() - 1) as u32;
        assert_eq!(
            inp.previous_output.txid, prev.txid,
            "boost {} must spend predecessor {}, got {}",
            i, prev.txid, inp.previous_output.txid
        );
        assert_eq!(
            inp.previous_output.vout, prev_last_vout,
            "boost {} must spend predecessor's last output (change at vout {})",
            i, prev_last_vout
        );

        // bump_fee_used strictly escalates.
        let bump = s.speedup_kind().unwrap().context().bump_fee_used;
        assert!(
            bump > prev_bump,
            "bump_fee must strictly escalate at boost {}: {} -> {}",
            i,
            prev_bump,
            bump
        );
        prev_bump = bump;

        // Effective fee_rate strictly escalates across the boost portion of
        // the chain (skip initial→boost1: their fee shapes differ, see doc).
        if let Some(pr) = prev_rate {
            assert!(
                s.fee_info.fee_rate > pr,
                "effective fee_rate must strictly escalate at boost {}: {} -> {}",
                i,
                pr,
                s.fee_info.fee_rate
            );
        }
        prev_rate = Some(s.fee_info.fee_rate);
    }

    // Confirming block: parent + every speedup must reach Confirmed in lockstep.
    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    let mut all_txids = vec![parent_txid];
    for s in &speedups {
        all_txids.push(s.txid);
    }
    let reached = tick_until_all_states(
        &coordinator,
        &coord_storage,
        &all_txids,
        TransactionState::Confirmed,
        10,
    )
    .unwrap();
    assert!(
        reached,
        "parent + all {} speedups in the chain must confirm together as one package",
        speedups.len()
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// Boost escalation stops once `max_feerate_sat_vb` is reached. The boost that
/// would exceed the cap is saved at the cap, a `MaxFeeRateReached` news item
/// is emitted, and subsequent stale-tip ticks must not produce any new
/// speedup record.
#[test]
fn test_boost_cap_reached() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = TestKeyManager::new();
    let settings = cap_settings();
    let coordinator = create_coordinator_with_km(&setup, key_manager.rc(), settings.clone());
    let coord_storage = get_coord_storage(&setup);

    let funding_utxo = create_funded_speedup_utxo(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        2_000_000,
    )
    .unwrap();
    // Small parent output so `parent_amount_outputs` doesn't dominate the fee.
    let (parent_tx, speedup_data) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        1_000,
    )
    .unwrap();

    tick_until_ready(&coordinator).unwrap();
    coordinator.add_funding(funding_utxo).unwrap();
    coordinator
        .dispatch_with_speedup(parent_tx, speedup_data, ctx("cap_stops_boost"), None, None)
        .unwrap();

    // Build & dispatch the initial CPFP, then read its measured effective rate.
    let cpfp1_id = build_and_dispatch_cpfp(&coordinator, &coord_storage, 3);
    let initial_rate = coord_storage
        .get_tx_by_id(cpfp1_id)
        .unwrap()
        .unwrap()
        .fee_info
        .fee_rate;
    let max_rate = settings.fee.max_feerate_sat_vb;
    let bump_pct = settings.speedup.bump_fee_percentage;
    let network_rate = settings.fee.min_safe_fee_rate; // bitcoind regtest estimate falls back to this floor
    assert!(
        initial_rate > 0 && initial_rate < max_rate,
        "initial CPFP must start strictly between 0 and the cap to exercise the boost flow; got rate={} cap={}",
        initial_rate,
        max_rate
    );

    // A boost CPFP-of-CPFP has the `network_rate * bump_pct^N` (N = boost number).
    // The smallest N with `network_rate * bump_pct^N >= max_rate` is exactly
    // the boost on which the cap is hit. Any extra iterations past that point
    // are no-ops because `boost_if_stale` skips a tip already at cap.
    let boosts_until_cap =
        ((max_rate as f64 / network_rate as f64).log(bump_pct).ceil() as u32).max(1);

    for _ in 0..boosts_until_cap {
        mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
        coordinator.tick().unwrap();
        let latest_txid = coord_storage
            .get_speedups_ordered()
            .unwrap()
            .last()
            .unwrap()
            .txid;
        let _ = tick_until_state(
            &coordinator,
            &coord_storage,
            latest_txid,
            TransactionState::InMempool,
            3,
        )
        .unwrap();
    }

    // The latest speedup must now be sitting at the cap.
    let speedups_at_cap = coord_storage.get_speedups_ordered().unwrap();
    let capped_tx = speedups_at_cap.last().unwrap();
    assert_eq!(
        capped_tx.fee_info.fee_rate, max_rate,
        "the capped speedup's effective rate must equal max_feerate_sat_vb; got {}",
        capped_tx.fee_info.fee_rate,
    );

    // The `MaxFeeRateReached` news must reference the capped txid.
    let news = coord_storage.get_news().unwrap();
    let cap_news = news
        .iter()
        .find(|n| matches!(n, CoordinatorNews::MaxFeeRateReached { .. }))
        .expect("MaxFeeRateReached must be present");
    if let CoordinatorNews::MaxFeeRateReached {
        txid,
        effective_fee_rate,
        ..
    } = cap_news
    {
        assert_eq!(*txid, capped_tx.txid);
        assert_eq!(*effective_fee_rate, settings.fee.max_feerate_sat_vb);
    }

    // After the cap, additional stale-tip ticks must not produce any new speedup.
    let count_at_cap = speedups_at_cap.len();
    for _ in 0..4 {
        mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
        coordinator.tick().unwrap();
    }
    let speedups_after = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(
        speedups_after.len(),
        count_at_cap,
        "boost_if_stale must not save any new speedup once the cap is reached"
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// When `boost_if_stale` selects RBF (in-mempool count reached `max_unconfirmed_speedups`) and
/// the BIP-125 bandwidth floor would exceed the configured  `max_feerate_sat_vb` cap, the RBF
/// is still built, clamped at the cap, but bitcoind will reject the broadcast for being below
/// the floor. To break the doomed-retry busy-loop, `boost_if_stale` marks the predecessor at-cap
/// so subsequent stale intervals skip via the existing tip-at-cap check. A `MaxFeeRateReached`
/// news item is emitted referencing the new RBF.
#[test]
fn test_rbf_floor_above_cap_marks_predecessor_terminal() {
    init_trace();

    // Network rate floors at 6; cap is 10. RBF bandwidth floor would be
    // 6 × 2 = 12 sat/vB, above the cap. max_unconfirmed_speedups = 1 forces
    // the first boost to be an RBF.
    let settings = BitcoinSettings {
        fee: FeeSettings {
            min_safe_fee_rate: 6,
            max_feerate_sat_vb: 10,
            base_fee_multiplier: 1.0,
        },
        speedup: SpeedupSettings {
            max_unconfirmed_speedups: 1,
            max_rbf_attempts: 10,
            min_blocks_before_resend_speedup: 1,
            rbf_fee_multiplier: 1.5,
            bump_fee_percentage: 1.5,
        },
        ..cpfp_settings()
    };

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = TestKeyManager::new();
    let coordinator = create_coordinator_with_km(&setup, key_manager.rc(), settings);
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
        1_000,
    )
    .unwrap();

    tick_until_ready(&coordinator).unwrap();
    coordinator.add_funding(funding_utxo).unwrap();
    coordinator
        .dispatch_with_speedup(
            parent_tx,
            speedup_data,
            ctx("rbf_floor_above_cap"),
            None,
            None,
        )
        .unwrap();

    // Build + dispatch the initial CPFP. unconfirmed.len() == 1 == max_unconfirmed so the next stale-interval boost will choose RBF.
    let cpfp1_id = build_and_dispatch_cpfp(&coordinator, &coord_storage, 3);

    // Drive boost_if_stale: mine a block (stale check passes) and tick.
    mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    coordinator.tick().unwrap();

    // A capped RBF was built and saved (count grows to 2).
    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(
        speedups.len(),
        2,
        "RBF must be built and saved (clamped at the cap); got {} speedups",
        speedups.len()
    );
    let rbf_id = speedups[1].txid;
    assert_eq!(
        speedups[1].fee_info.fee_rate, 10,
        "RBF effective rate must be clamped at max_feerate_sat_vb"
    );

    // The predecessor CPFP is marked at-cap (`fee_info.fee_rate = max`) so `boost_if_stale` will skip it
    // on subsequent stale intervals via the existing tip-at-cap check.
    let predecessor = coord_storage.get_tx_by_id(cpfp1_id).unwrap().unwrap();
    assert_eq!(
        predecessor.fee_info.fee_rate, 10,
        "predecessor must be marked at-cap so the chain stops attempting further RBFs"
    );

    // News log must contain MaxFeeRateReached referencing the new RBF.
    let news = coord_storage.get_news().unwrap();
    let cap_news = news
        .iter()
        .find(|n| matches!(n, CoordinatorNews::MaxFeeRateReached { txid, .. } if *txid == rbf_id))
        .expect("MaxFeeRateReached news must be emitted against the new RBF");
    if let CoordinatorNews::MaxFeeRateReached {
        effective_fee_rate, ..
    } = cap_news
    {
        assert_eq!(
            *effective_fee_rate, 10,
            "news effective_fee_rate must equal the cap"
        );
    }

    // Subsequent stale intervals must not introduce any new txids.
    let known_txids: std::collections::HashSet<_> = speedups.iter().map(|tx| tx.txid).collect();
    for _ in 0..3 {
        mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
        coordinator.tick().unwrap();
    }
    let speedups_after = coord_storage.get_speedups_ordered().unwrap();
    for tx in &speedups_after {
        assert!(
            known_txids.contains(&tx.txid),
            "no new speedup must be built once the predecessor is marked at-cap; found unexpected txid {}",
            tx.txid
        );
    }

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// Once the in-mempool speedup count reaches max_unconfirmed_speedups, the next boost switches from CPFP to RBF.
/// After the RBF is dispatched, the predecessor must have its `replaced_by` set so the funding walk-back and
/// boost_if_stale skip it. cpfp_settings: max_unconfirmed_speedups = 2.
#[test]
fn test_cpfp_rbf_after_max_unconfirmed_reached() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = TestKeyManager::new();
    let coordinator = create_coordinator_with_km(&setup, key_manager.rc(), cpfp_settings());
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

    // CPFP1 built + dispatched (1 unconfirmed).
    let cpfp1_txid = build_and_dispatch_cpfp(&coordinator, &coord_storage, 3);

    // Boost 1: 1 unconfirmed < 2 → CPFP.
    mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    coordinator.tick().unwrap();
    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(speedups.len(), 2, "boost 1 must create CPFP2");
    assert!(
        !speedups[1].speedup_kind().unwrap().is_rbf(),
        "boost 1 must be a CPFP (1 unconfirmed < limit of 2)"
    );
    let cpfp2_txid = speedups[1].txid;
    let reached = tick_until_state(
        &coordinator,
        &coord_storage,
        cpfp2_txid,
        TransactionState::InMempool,
        3,
    )
    .unwrap();
    assert!(reached, "CPFP2 must reach InMempool");

    // Boost 2: 2 unconfirmed >= 2 → RBF.
    mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    coordinator.tick().unwrap();
    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(speedups.len(), 3, "boost 2 must add a third speedup (RBF)");
    let rbf = &speedups[2];
    assert!(
        rbf.speedup_kind().unwrap().is_rbf(),
        "boost 2 must be RBF once the unconfirmed limit is reached"
    );
    let rbf_txid = rbf.txid;
    let reached = tick_until_state(
        &coordinator,
        &coord_storage,
        rbf_txid,
        TransactionState::InMempool,
        3,
    )
    .unwrap();
    assert!(reached, "RBF must reach InMempool after the next tick");

    // After RBF dispatch, the predecessor must be marked `replaced_by`.
    let predecessor = coord_storage.get_tx_by_id(cpfp2_txid).unwrap().unwrap();
    let predecessor_context = predecessor.speedup_kind().unwrap().context();
    assert_eq!(
        predecessor_context.replaced_by,
        Some(rbf_txid),
        "RBF dispatch must set `replaced_by = Some(rbf_txid)` on the predecessor (CPFP2)"
    );

    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    let reached = tick_until_all_states(
        &coordinator,
        &coord_storage,
        &[parent_tx.compute_txid(), cpfp1_txid, cpfp2_txid, rbf_txid],
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

/// After parent + CPFP are both in the mempool, evict all mempool transactions.
/// Recovery sequence (single tick):
/// 1. review_active marks the parent not_found → ToDispatch.
/// 2. review_speedups marks the CPFP not_found → ToDispatch (re-dispatch the exact same tx).
/// 3. dispatch_pending re-broadcasts the parent → InMempool.
/// 4. dispatch_pending_speedups re-broadcasts the CPFP (same txid). Bitcoind accepts it
///    because the parent is now back in the mempool (step 3 just put it there).
#[test]
fn test_cpfp_orphan_requeue() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = TestKeyManager::new();
    let coordinator = create_coordinator_with_km(&setup, key_manager.rc(), cpfp_settings());
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
        .dispatch_with_speedup(parent_tx, speedup_data, ctx("eviction"), None, None)
        .unwrap();

    let cpfp_txid = build_and_dispatch_cpfp(&coordinator, &coord_storage, 3);

    // Evict all mempool transactions.
    expire_mempool(&setup.bitcoin_client, &setup.regtest_wallet).unwrap();

    // Both parent and CPFP must end up InMempool again — one tick suffices but
    // give the recovery a small budget.
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
        "parent and CPFP must both be back in InMempool after eviction"
    );

    // Exactly one speedup record exists. The original txid was re-broadcast, not rebuilt as a new record.
    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(
        speedups.len(),
        1,
        "no new CPFP record must be created; the same txid is re-dispatched"
    );
    assert_eq!(speedups[0].txid, cpfp_txid);
    assert_eq!(speedups[0].state, TransactionState::InMempool);

    // The CPFP still covers the original parent.
    let parents = speedups[0].speedup_kind().unwrap().parents();
    assert!(
        parents.contains(&parent_txid),
        "the recovered CPFP must still cover the original parent; got parents = {:?}",
        parents
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// `EngineContext::last_retry_at` is shared between the transaction engine and the speedup engine.
///  When a non-speedup retry consumes the rate-limit slot, a speedup retry queued in the same tick
///  must be blocked until the configured interval elapses.
#[test]
fn test_retry_rate_limit_shared_across_engines() {
    init_trace();
    let retry_interval_seconds = 8u64;

    let settings = BitcoinSettings {
        coordinator: CoordinatorSettings {
            retry_interval_seconds,
            retry_attempts_sending_tx: 10,
        },
        ..cpfp_settings()
    };

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = TestKeyManager::new();
    let coordinator = create_coordinator_with_km(&setup, key_manager.rc(), settings);
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
        .dispatch_with_speedup(parent_tx, speedup_data, ctx("shared_rate"), None, None)
        .unwrap();

    // Build & dispatch the CPFP; parent reaches InMempool alongside it.
    let cpfp_txid = build_and_dispatch_cpfp(&coordinator, &coord_storage, 3);
    assert_eq!(
        coord_storage
            .get_tx_by_id(parent_txid)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::InMempool,
        "parent must be InMempool before the rate-limit test starts"
    );

    // Flip both back to ToDispatch with retry_count = 1. Bitcoind still has
    // them in mempool, so the eventual redispatch will return AlreadyKnown.
    coord_storage.mark_as_retry(parent_txid).unwrap();
    coord_storage.mark_as_retry(cpfp_txid).unwrap();

    // Single tick: phase 3 redispatches the parent (consumes the rate-limit
    // slot), phase 4 sees the just-set `last_retry_at` and rate-limits the
    // CPFP retry.
    coordinator.tick().unwrap();
    assert_eq!(
        coord_storage
            .get_tx_by_id(parent_txid)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::InMempool,
        "non-speedup retry must dispatch (no prior retry had armed the window)"
    );
    assert_eq!(
        coord_storage
            .get_tx_by_id(cpfp_txid)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::ToDispatch,
        "CPFP retry must be blocked by the shared last_retry_at slot the parent just consumed"
    );

    // Tick again immediately; the interval has NOT elapsed so the CPFP is
    // still rate-limited.
    coordinator.tick().unwrap();
    assert_eq!(
        coord_storage
            .get_tx_by_id(cpfp_txid)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::ToDispatch,
        "CPFP retry must remain blocked while the interval has not elapsed"
    );

    // After the interval, the CPFP retry passes.
    std::thread::sleep(std::time::Duration::from_secs(retry_interval_seconds + 1));
    coordinator.tick().unwrap();
    assert_eq!(
        coord_storage
            .get_tx_by_id(cpfp_txid)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::InMempool,
        "CPFP retry must redispatch once the shared retry interval has elapsed"
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// When a CPFP is finalized the coordinator advances the base funding UTXO to
/// the confirmed change output of that CPFP. A second parent dispatched later
/// must produce a new CPFP whose funding input is the first CPFP's change
/// output, forming a clean on-chain funding chain.
#[test]
fn test_cpfp_funding_restored_after_finalization() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = TestKeyManager::new();
    let coordinator = create_coordinator_with_km(&setup, key_manager.rc(), cpfp_settings());
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

    let cpfp1_txid = build_and_dispatch_cpfp(&coordinator, &coord_storage, 3);

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

    // Block 2 → both Finalized. At finalization, update_funding stores CPFP1's
    // change output as the new base funding.
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

    // Dispatch a second parent.
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

    // Build + dispatch CPFP2 across two ticks.
    coordinator.tick().unwrap();
    let speedups = coord_storage.get_speedups_ordered().unwrap();
    let cpfp2 = speedups
        .iter()
        .find(|s| s.txid != cpfp1_txid)
        .expect("a new CPFP must exist for parent2");
    let cpfp2_txid = cpfp2.txid;
    assert_eq!(
        cpfp2.state,
        TransactionState::ToDispatch,
        "CPFP2 must be saved as ToDispatch on the build tick"
    );
    let reached = tick_until_state(
        &coordinator,
        &coord_storage,
        cpfp2_txid,
        TransactionState::InMempool,
        3,
    )
    .unwrap();
    assert!(reached, "CPFP2 must reach InMempool on the next tick");
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
    let cpfp2 = coord_storage.get_tx_by_id(cpfp2_txid).unwrap().unwrap();
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
    let key_manager = TestKeyManager::new();
    let coordinator =
        create_coordinator_with_km(&setup, key_manager.rc(), multi_funding_settings());
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
    coordinator.add_funding(funding_a.clone()).unwrap();
    coordinator.add_funding(funding_b.clone()).unwrap();
    coordinator
        .dispatch_with_speedup(parent_tx, speedup_data, ctx("multi_funding"), None, None)
        .unwrap();

    // Tick 1: parent dispatches; CPFP build with A fails (insufficient),
    // funding queue advances to B. No CPFP saved this tick.
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

    // Tick 2: CPFP built with funding B and saved as ToDispatch.
    coordinator.tick().unwrap();
    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(
        speedups.len(),
        1,
        "exactly one CPFP must be built after the second tick"
    );
    let cpfp_txid = speedups[0].txid;
    assert_eq!(
        speedups[0].state,
        TransactionState::ToDispatch,
        "CPFP must be saved as ToDispatch on the build tick"
    );

    // The CPFP must spend funding B, not funding A.
    let cpfp_tx = &speedups[0].tx;
    assert!(
        cpfp_tx
            .input
            .iter()
            .any(|i| i.previous_output.txid == funding_b.txid),
        "CPFP must spend funding B",
    );
    assert!(
        cpfp_tx
            .input
            .iter()
            .all(|i| i.previous_output.txid != funding_a.txid),
        "CPFP must not spend funding A (advanced past)"
    );

    // Tick 3: CPFP dispatched.
    let reached = tick_until_state(
        &coordinator,
        &coord_storage,
        cpfp_txid,
        TransactionState::InMempool,
        3,
    )
    .unwrap();
    assert!(reached, "CPFP must reach InMempool on the next tick");

    // Confirming block: parent and CPFP both reach Confirmed.
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
        "parent and CPFP must both reach Confirmed after one block"
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// When the funding queue is entirely exhausted (all entries too small),
/// `InsufficientFunds` is emitted and the parent stays in `InMempool` with
/// no CPFP. Once the user registers new funding, the next tick picks up the
/// parent from the pending set and creates the CPFP.
#[test]
fn test_cpfp_recovers_after_queue_was_exhausted() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = TestKeyManager::new();
    let coordinator =
        create_coordinator_with_km(&setup, key_manager.rc(), multi_funding_settings());
    let coord_storage = get_coord_storage(&setup);

    // Create both UTXOs up front (each mines a block) so no block is mined between ticks.
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

    // Tick 1: CPFP build fails, queue advances → empty, InsufficientFunds fired.
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

    // User now registers B.
    coordinator.add_funding(funding_b.clone()).unwrap();

    // Tick 2: pending set has parent → CPFP built with B → saved as ToDispatch.
    coordinator.tick().unwrap();
    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(speedups.len(), 1, "exactly one CPFP after recovery");
    let cpfp_txid = speedups[0].txid;
    assert_eq!(
        speedups[0].state,
        TransactionState::ToDispatch,
        "CPFP must be saved as ToDispatch on the build tick"
    );
    assert!(
        speedups[0]
            .tx
            .input
            .iter()
            .any(|i| i.previous_output.txid == funding_b.txid),
        "CPFP must spend funding B"
    );

    // Tick 3: CPFP dispatched.
    let reached = tick_until_state(
        &coordinator,
        &coord_storage,
        cpfp_txid,
        TransactionState::InMempool,
        3,
    )
    .unwrap();
    assert!(reached, "CPFP must reach InMempool on the next tick");

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

/// A `NeedsSpeedup` parent registered without funding gets dispatched on the next tick and confirms naturally
/// before any CPFP is built. PendingSpeedupParents must retain the parent  and `create_cpfp_batch` must build
/// the CPFP once funding becomes available. `evict_stale_txs` must not remove the parent record while the CPFP
/// has not been built so would lose the `SpeedupData` needed to construct the CPFP.
#[test]
fn test_cpfp_built_for_parent_confirmed_before_funding() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = TestKeyManager::new();
    let coordinator = create_coordinator_with_km(&setup, key_manager.rc(), cpfp_settings());
    let coord_storage = get_coord_storage(&setup);

    let (parent_tx, speedup_data) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        200_000,
    )
    .unwrap();
    let parent_txid = parent_tx.compute_txid();

    tick_until_ready(&coordinator).unwrap();
    // No add_funding here. The coordinator will dispatch the parent but cannot yet build a CPFP.
    coordinator
        .dispatch_with_speedup(parent_tx, speedup_data, ctx("late_funding"), None, None)
        .unwrap();

    // Tick: parent → InMempool; create_cpfp_batch tries and fails (no funding).
    coordinator.tick().unwrap();
    assert_eq!(
        coord_storage
            .get_tx_by_id(parent_txid)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::InMempool,
        "parent must reach InMempool even without funding"
    );
    assert!(
        coord_storage.get_speedups_ordered().unwrap().is_empty(),
        "no CPFP can be built before funding is registered"
    );
    assert!(
        coord_storage
            .get_news()
            .unwrap()
            .iter()
            .any(|n| matches!(n, CoordinatorNews::FundingNotAvailable)),
        "FundingNotAvailable news must be emitted while funding is absent"
    );

    // Mine 1 block: parent confirms on its own.
    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    let reached = tick_until_state(
        &coordinator,
        &coord_storage,
        parent_txid,
        TransactionState::Confirmed,
        5,
    )
    .unwrap();
    assert!(
        reached,
        "parent must reach Confirmed before any CPFP exists"
    );

    // Parent is Confirmed but the protocol still needs the CPFP to be built. The parent must
    // remain in the pending speedup parents set and the record must not be evicted.
    let pending = coord_storage.get_pending_speedup_parents().unwrap();
    assert!(
        pending.iter().any(|p| p.txid == parent_txid),
        "Confirmed NeedsSpeedup parent must stay in pending speedup parents until its CPFP is built"
    );

    // Provide funding. create_cpfp_batch must build a CPFP for the Confirmed parent on the next tick.
    let funding_utxo = create_funded_speedup_utxo(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        500_000,
    )
    .unwrap();
    coordinator.add_funding(funding_utxo).unwrap();

    // Tick 1 after funding: CPFP built and saved as ToDispatch.
    coordinator.tick().unwrap();
    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(
        speedups.len(),
        1,
        "exactly one CPFP must be built after funding is registered, even though the parent is already Confirmed"
    );
    let cpfp_txid = speedups[0].txid;
    assert_eq!(
        speedups[0].state,
        TransactionState::ToDispatch,
        "CPFP must be saved as ToDispatch on the build tick"
    );
    let parents = speedups[0].speedup_kind().unwrap().parents();
    assert!(
        parents.contains(&parent_txid),
        "the CPFP must reference the Confirmed parent"
    );

    // Parent has been removed from the pending speedup parents now that the CPFP is built.
    let pending_after = coord_storage.get_pending_speedup_parents().unwrap();
    assert!(
        !pending_after.iter().any(|p| p.txid == parent_txid),
        "parent must be removed from pending speedup parents once its CPFP is saved"
    );

    // Tick 2: dispatch the CPFP.
    let reached = tick_until_state(
        &coordinator,
        &coord_storage,
        cpfp_txid,
        TransactionState::InMempool,
        3,
    )
    .unwrap();
    assert!(reached, "CPFP must reach InMempool on the next tick");

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}
