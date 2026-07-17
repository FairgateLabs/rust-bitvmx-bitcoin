mod common;
use common::*;

use std::rc::Rc;

use bitcoin::{Network, OutPoint};
use bitcoin_coordinator::{
    config::config::{
        BitcoinSettings, CoordinatorSettings, CoordinatorStorageSettings, FeeSettings,
        FundingSettings, SpeedupSettings,
    },
    coordinator::BitcoinCoordinator,
    core::{
        funding::{FundingManager, FundingStorage},
        storage::CoordinatorStorage,
    },
    types::{CoordinatorNews, TransactionState},
};
use bitcoincore_rpc::RpcApi as _;
use bitvmx_bitcoin_rpc::bitcoin_client::BitcoinClientApi;
use bitvmx_transaction_monitor::config::MonitorSettingsConfig;
use bitvmx_transaction_monitor::types::TypesToMonitor;

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
            max_unconfirmed_speedups: 2,         // A second boost creates RBF
            min_blocks_before_resend_speedup: 1, // Enables boost after one block
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
            min_blocks_before_resend_speedup: 1,
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
            min_blocks_before_resend_speedup: 1,
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

/// `cpfp_settings` with a chosen unconfirmed-slot count, which drives whether a boost is a CPFP child
/// (slots free) or an RBF replacement (slots full).
fn rebuild_settings(max_unconfirmed: u32) -> BitcoinSettings {
    let mut s = cpfp_settings();
    s.speedup.max_unconfirmed_speedups = max_unconfirmed;
    s
}

/// Drive the coordinator until the failed root CPFP settles and the surviving parent gets a fresh CPFP
/// that confirms. Returns the survivor CPFP txid. Phase 1 mines empty blocks to advance height (walking the
/// reorg-flap guard windows on the evicted chain) while keeping the surviving parent unconfirmed. Phase 2
/// mines real blocks to confirm the survivor CPFP and its parent.
fn drive_dead_parent_recovery(
    coordinator: &BitcoinCoordinator,
    coord_storage: &CoordinatorStorage,
    setup: &TestSetup,
    settings: &BitcoinSettings,
    root_cpfp_txid: bitcoin::Txid,
    dead_parent: bitcoin::Txid,
    survivor_parent: bitcoin::Txid,
) -> bitcoin::Txid {
    // Timings derived from config. Each iteration advances one block and waits out one retry interval, so a
    // re-dispatch deferred on the previous tick fires now. Recovery walks a few sequential reorg-flap guard
    // windows in turn (the dead parent, the boost or RBF over the root, then the root itself), each
    // max_monitoring_confirmations blocks long, plus retry pacing; the budget covers them generously.
    let max_confs = settings.monitor.max_monitoring_confirmations.unwrap_or(6) as usize;
    let retry_interval_ms = settings.coordinator.retry_interval_seconds * 1000;
    let sleep = std::time::Duration::from_millis(retry_interval_ms + 300);
    let phase1_budget = (max_confs + 2) * 8;
    let phase2_budget = max_confs + 10;

    let find_survivor = |storage: &CoordinatorStorage| -> Option<bitcoin::Txid> {
        for s in storage.get_speedups_ordered().unwrap() {
            if s.txid == root_cpfp_txid {
                continue;
            }
            if let Ok(k) = s.speedup_kind() {
                let parents = k.parents();
                if parents.contains(&survivor_parent) && !parents.contains(&dead_parent) {
                    return Some(s.txid);
                }
            }
        }
        None
    };

    let ok_state = |st: Option<TransactionState>| {
        matches!(
            st,
            Some(TransactionState::Confirmed) | Some(TransactionState::Finalized)
        )
    };

    // Phase 1: drive until the root CPFP settles Failed and a fresh survivor CPFP is built. The sequence
    // self-drives: the dead parent settles first, then the boost or RBF over the root (clearing the mask),
    // then the root CPFP, whose failure runs rebuild_survivors. Empty blocks advance guard windows while
    // keeping the surviving parent unconfirmed; ticking after each single mined block keeps the monitor synced.
    let mut saw_speedup_err = false;
    let mut survivor_cpfp = None;
    for _ in 0..phase1_budget {
        std::thread::sleep(sleep);
        coordinator.tick().unwrap();
        let news = coordinator.get_news().unwrap();
        for n in &news.coordinator_news {
            if matches!(n, CoordinatorNews::SpeedupDispatchError { txid, .. } if *txid == root_cpfp_txid)
            {
                saw_speedup_err = true;
            }
        }
        ack_all_news(coordinator, &news);

        if survivor_cpfp.is_none() {
            survivor_cpfp = find_survivor(coord_storage);
        }
        let root_failed = coord_storage
            .get_tx_by_id(root_cpfp_txid)
            .unwrap()
            .map_or(false, |t| t.state == TransactionState::Failed);
        if survivor_cpfp.is_some() && root_failed {
            break;
        }
        mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    }

    assert!(
        saw_speedup_err,
        "a SpeedupDispatchError must fire for the failed root CPFP"
    );
    assert_eq!(
        coord_storage
            .get_tx_by_id(root_cpfp_txid)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::Failed,
        "root CPFP must settle Failed on the dead parent",
    );
    let survivor_cpfp =
        survivor_cpfp.expect("a fresh CPFP over the surviving parent must be built");

    // Phase 2: confirm the survivor CPFP and its parent with real blocks.
    let mut confirmed = false;
    for _ in 0..phase2_budget {
        mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
        coordinator.tick().unwrap();
        ack_all_news(coordinator, &coordinator.get_news().unwrap());
        let sc = coord_storage
            .get_tx_by_id(survivor_cpfp)
            .unwrap()
            .map(|t| t.state);
        let sp = coord_storage
            .get_tx_by_id(survivor_parent)
            .unwrap()
            .map(|t| t.state);
        if ok_state(sc) && ok_state(sp) {
            confirmed = true;
            break;
        }
    }
    assert!(
        confirmed,
        "the surviving parent and its fresh CPFP must confirm"
    );
    survivor_cpfp
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
    let funding_txid = funding_utxo.txid;
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
        .dispatch(parent_tx, Some(speedup_data), ctx("lifecycle"), None, None)
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
        None,
    )
    .unwrap();
    assert!(
        reached,
        "parent and CPFP must both reach Confirmed after 1 block"
    );
    ack_all_news(&coordinator, &coordinator.get_news().unwrap());

    // Block 2: both Finalized. When the CPFP finalizes, `replace_funding_on_finalize` consumes the
    // user funding record and materializes the CPFP change into the funding queue. Funding-queue
    // mutations are silent: `TransactionEvicted` is only for CoordinatedTx records leaving storage.
    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    let mut reached_finalized = false;
    for _ in 0..10 {
        coordinator.tick().unwrap();
        let news = coordinator.get_news().unwrap();
        ack_all_news(&coordinator, &news);
        let parent_state = coord_storage
            .get_tx_by_id(parent_txid)
            .unwrap()
            .map(|t| t.state);
        let cpfp_state = coord_storage
            .get_tx_by_id(cpfp_txid)
            .unwrap()
            .map(|t| t.state);
        if parent_state == Some(TransactionState::Finalized)
            && cpfp_state == Some(TransactionState::Finalized)
        {
            reached_finalized = true;
            break;
        }
    }
    assert!(
        reached_finalized,
        "parent and CPFP must both reach Finalized after 2 blocks"
    );
    // The user funding UTXO was consumed: silently gone from the funding queue, replaced by the CPFP change.
    let records = coord_storage.read_funding_records().unwrap();
    assert!(
        !records.iter().any(|r| r.utxo.txid == funding_txid),
        "user funding UTXO must be consumed and removed from the funding queue"
    );

    // Block 3: parent is eligible for eviction. The finalized CPFP's tx record now evicts normally too.
    // Its spendable change survives as an independent funding record.
    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    let mut parent_evicted = false;
    for _ in 0..10 {
        coordinator.tick().unwrap();
        let news = coordinator.get_news().unwrap();
        for n in &news.coordinator_news {
            if matches!(n, CoordinatorNews::TransactionEvicted { txid: id, .. } if *id == parent_txid)
            {
                parent_evicted = true;
            }
        }
        ack_all_news(&coordinator, &news);
        if parent_evicted {
            break;
        }
    }
    assert!(
        parent_evicted,
        "TransactionEvicted news must fire for parent"
    );
    assert!(
        coord_storage.get_tx_by_id(parent_txid).unwrap().is_none(),
        "parent must be removed from storage after eviction"
    );
    // The CPFP's change output survives as a funding-queue record even though the CPFP tx record itself is now eligible for eviction.
    let funding_records = coord_storage.read_funding_records().unwrap();
    assert!(
        funding_records
            .iter()
            .any(|r| r.utxo.txid == cpfp_txid && r.from_speedup),
        "finalized CPFP's change must remain in the funding queue"
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// Regression for the same-txid funding collision: two funding UTXOs produced by one transaction
/// (shared txid, different vouts) must both be stored and both be independently consumable.
#[test]
fn test_two_funding_utxos_same_txid_both_consumed() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = TestKeyManager::new();
    let coordinator = create_coordinator_with_km(&setup, key_manager.rc(), cpfp_settings());
    let coord_storage = get_coord_storage(&setup);

    // One real on-chain tx, two coordinator-owned outputs: same txid, different vouts.
    let (u0, u1) = create_two_funding_utxos_same_txid(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        500_000,
    )
    .unwrap();
    assert_eq!(u0.txid, u1.txid, "the two funding UTXOs must share a txid");
    assert_ne!(
        u0.vout, u1.vout,
        "the two funding UTXOs must differ by vout"
    );

    tick_until_ready(&coordinator).unwrap();
    coordinator.add_funding(u0.clone()).unwrap();
    coordinator.add_funding(u1.clone()).unwrap();

    // Both must be stored (old bug: the second same-txid UTXO was dropped).
    let records = coord_storage.read_funding_records().unwrap();
    assert_eq!(
        records.len(),
        2,
        "both same-txid funding UTXOs must be stored; got {:?}",
        records
    );
    assert!(records.iter().any(|r| r.utxo == u0));
    assert!(records.iter().any(|r| r.utxo == u1));

    // Both must be independently handed out by the funding queue. A FundingManager over the same shared
    // storage claims them one after another: get_funding returns u0 then u1.
    let fstore = Rc::new(get_coord_storage(&setup));
    let mgr = FundingManager::new(
        FundingSettings {
            min_funding_amount_sats: 10_000,
        },
        Rc::clone(&fstore) as Rc<dyn FundingStorage>,
    );
    let no_speedups = fstore.get_speedups_ordered().unwrap();
    let (first, _) = mgr.get_funding(&no_speedups).unwrap().expect("first claim");
    let (second, _) = mgr
        .get_funding(&no_speedups)
        .unwrap()
        .expect("second claim: the sibling same-txid UTXO must be claimable");
    assert!(
        mgr.get_funding(&no_speedups).unwrap().is_none(),
        "exactly two funding UTXOs were available"
    );
    let claimed: std::collections::HashSet<OutPoint> = [
        OutPoint::new(first.txid, first.vout),
        OutPoint::new(second.txid, second.vout),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        claimed.len(),
        2,
        "the two claims must be distinct outpoints"
    );
    assert!(claimed.contains(&OutPoint::new(u0.txid, u0.vout)));
    assert!(claimed.contains(&OutPoint::new(u1.txid, u1.vout)));

    // Release the manual reservations so the coordinator can spend them for real.
    mgr.release_marks(&[first, second]).unwrap();
    for r in coord_storage.read_funding_records().unwrap() {
        assert!(!r.spent, "funding must be unspent again after release");
    }

    // End-to-end: a real parent + CPFP consumes one of the same-txid UTXOs on-chain.
    let (parent_tx, speedup_data) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        200_000,
    )
    .unwrap();
    let parent_txid = parent_tx.compute_txid();
    coordinator
        .dispatch(parent_tx, Some(speedup_data), ctx("same_txid"), None, None)
        .unwrap();

    let cpfp_txid = build_and_dispatch_cpfp(&coordinator, &coord_storage, 3);
    let cpfp = coord_storage.get_tx_by_id(cpfp_txid).unwrap().unwrap();
    let consumed = &cpfp.speedup_kind().unwrap().context().funding_inputs;
    assert_eq!(consumed.len(), 1, "CPFP must fund from a single UTXO");
    let consumed_op = OutPoint::new(consumed[0].txid, consumed[0].vout);
    assert!(
        consumed_op == OutPoint::new(u0.txid, u0.vout)
            || consumed_op == OutPoint::new(u1.txid, u1.vout),
        "CPFP must consume one of the two same-txid funding UTXOs"
    );

    // The other same-txid UTXO must still be present and unspent: not dropped, still usable.
    let sibling = if consumed_op == OutPoint::new(u0.txid, u0.vout) {
        &u1
    } else {
        &u0
    };
    let sib = coord_storage
        .get_funding_record(&OutPoint::new(sibling.txid, sibling.vout))
        .unwrap()
        .expect("sibling same-txid funding must still exist");
    assert!(
        !sib.spent,
        "sibling same-txid funding must remain unspent and available"
    );

    // Drive parent + CPFP to Confirmed to prove the on-chain spend is valid.
    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    let reached = tick_until_all_states(
        &coordinator,
        &coord_storage,
        &[parent_txid, cpfp_txid],
        TransactionState::Confirmed,
        10,
        None,
    )
    .unwrap();
    assert!(
        reached,
        "parent and CPFP must confirm, proving the same-txid funding was spent on-chain"
    );

    // Drop every storage handle before tearing down so the DB file is not still open (Windows lock).
    drop(mgr);
    drop(fstore);
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

/// Two related fee-cap paths::
/// Phase A, `MaxFeeRateReached` from a fresh CPFP: the bump multiplier pushes the first CPFP's
/// computed fee above the package ceiling and `compute_speedup_fee` clamps it.
/// Phase B, `InsufficientFunds` from a Speedup-derived primary that cannot combine: after CPFP1 finalizes-into-mempool,
/// its tiny change becomes the only Speedup-derived chain tip. A new parent's CPFP build picks that tip as the primary input.
/// The capped fee still exceeds available, combine is attempted but `get_combine_funding`  returns `None`.
#[test]
fn test_cpfp_capped_then_insufficient_funding_emit_news() {
    init_trace();

    // base_fee_multiplier 2.0 with the rate floored near the cap makes the first CPFP's final_fee
    // exceed the package ceiling (`compute_speedup_fee` cap = max*(parent_vsize+child_vsize) - parent_credit),
    // so the build clamps and emits MaxFeeRateReached.
    let settings = BitcoinSettings {
        fee: FeeSettings {
            min_safe_fee_rate: 80,
            max_feerate_sat_vb: 100,
            base_fee_multiplier: 2.0,
        },
        ..cpfp_settings()
    };

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = TestKeyManager::new();
    let coordinator = create_coordinator_with_km(&setup, key_manager.rc(), settings);
    let coord_storage = get_coord_storage(&setup);

    // ─── Phase A: fresh CPFP build is capped → MaxFeeRateReached ───────────
    // Funding sized above the package cap so the build succeeds but `capped == true`.
    let funding_a = create_funded_speedup_utxo(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        40_000,
    )
    .unwrap();
    let (parent1_tx, speedup_data1) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        1_000,
    )
    .unwrap();
    let parent1_txid = parent1_tx.compute_txid();

    // Sync the indexer past the fund_address-mined blocks so the first tick after dispatch can actually build the CPFP.
    tick_until_ready(&coordinator).unwrap();
    coordinator.add_funding(funding_a.clone()).unwrap();
    coordinator
        .dispatch_with_speedup(parent1_tx, speedup_data1, ctx("capped_path"), None, None)
        .unwrap();

    // Drive ticks until CPFP1 is built and reaches InMempool.
    let cpfp1_txid = build_and_dispatch_cpfp(&coordinator, &coord_storage, 5);
    let reached = tick_until_state(
        &coordinator,
        &coord_storage,
        parent1_txid,
        TransactionState::InMempool,
        5,
    )
    .unwrap();
    assert!(reached, "parent1 must reach InMempool");

    let cpfp1 = coord_storage.get_tx_by_id(cpfp1_txid).unwrap().unwrap();
    let max_rate = 100u64;
    // The cap bounds the package rate to max_feerate, so a capped CPFP sits exactly at it. (Its child
    // standalone rate is higher because the child also absorbs the parents' shortfall.)
    assert_eq!(
        cpfp1.fee_info.package_fee_rate, max_rate,
        "capped CPFP's package rate must equal max_feerate_sat_vb; got {}",
        cpfp1.fee_info.package_fee_rate,
    );
    assert!(
        cpfp1.fee_info.fee_rate >= max_rate,
        "capped CPFP's standalone child rate must be >= max; got {}",
        cpfp1.fee_info.fee_rate,
    );

    let news_a = coordinator.get_news().unwrap();
    let cap_news = news_a
        .coordinator_news
        .iter()
        .find(|n| {
            matches!(
                n,
                CoordinatorNews::MaxFeeRateReached { txid, .. } if *txid == cpfp1_txid,
            )
        })
        .unwrap_or_else(|| {
            panic!(
                "MaxFeeRateReached must be emitted against CPFP1 ({}); got {:?}",
                cpfp1_txid, news_a.coordinator_news,
            )
        });
    if let CoordinatorNews::MaxFeeRateReached {
        effective_fee_rate, ..
    } = cap_news
    {
        assert_eq!(
            *effective_fee_rate, max_rate,
            "MaxFeeRateReached must carry the cap-clamped package rate (== max); got {}",
            effective_fee_rate,
        );
    }

    // Ack everything so Phase B's news assertion is not ambiguous.
    ack_all_news(&coordinator, &news_a);

    // ─── Phase B: Speedup-derived primary, combine returns None → InsufficientFunds ─
    // CPFP1's leftover change is now the Speedup-derived chain tip. Its amount cannot cover another capped CPFP fee.
    let (parent2_tx, speedup_data2) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        1_000,
    )
    .unwrap();
    let parent2_txid = parent2_tx.compute_txid();
    coordinator
        .dispatch_with_speedup(
            parent2_tx,
            speedup_data2,
            ctx("insufficient_path"),
            None,
            None,
        )
        .unwrap();

    let reached = tick_until_state(
        &coordinator,
        &coord_storage,
        parent2_txid,
        TransactionState::InMempool,
        10,
    )
    .unwrap();
    assert!(reached, "parent2 must reach InMempool");
    assert_eq!(
        coord_storage.get_speedups_ordered().unwrap().len(),
        1,
        "no second CPFP must be created when the chain tip + combine cannot cover the fee",
    );

    let news_b = coordinator.get_news().unwrap();
    let cpfp1_leftover = cpfp1.speedup_kind().unwrap().context().funding_inputs[0]
        .amount
        .saturating_sub(cpfp1.fee_info.fee);
    assert!(
        news_b.coordinator_news.iter().any(|n| matches!(
            n,
            CoordinatorNews::InsufficientFunds { available, required }
                if *available <= cpfp1_leftover && *required > *available,
        )),
        "expected InsufficientFunds with available<=cpfp1_leftover ({}) and required>available; got {:?}",
        cpfp1_leftover,
        news_b.coordinator_news,
    );

    // release_marks must have reset the Speedup-kind chain tip's spent flag.
    let cpfp1_after = coord_storage.get_tx_by_id(cpfp1_txid).unwrap().unwrap();
    assert!(
        !cpfp1_after.speedup_kind().unwrap().context().spent,
        "release_marks must reset the Speedup-derived primary's spent flag",
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
        None,
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
        None,
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

    // Tick triggers boost_if_stale → builds CPFP2 → save as `ToDispatch`.
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

    // Drive N boost iterations. Bump escalation (×2.0 each step) combined with the `FeeManager::boost_fee_rate`
    // floor (predecessor + 1) compounds the effective fee rate quickly.
    let boost_iters = 3u32;
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

        // Boost CPFP-of-CPFP: parents = [previous_cpfp.txid], not the original parent.
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
        None,
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

    // Build & dispatch the initial CPFP, then read its measured package rate (what the cap bounds).
    let cpfp1_id = build_and_dispatch_cpfp(&coordinator, &coord_storage, 3);
    let initial_rate = coord_storage
        .get_tx_by_id(cpfp1_id)
        .unwrap()
        .unwrap()
        .fee_info
        .package_fee_rate;
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

    // The latest speedup must now be sitting at the cap (package rate == max).
    let speedups_at_cap = coord_storage.get_speedups_ordered().unwrap();
    let capped_tx = speedups_at_cap.last().unwrap();
    assert_eq!(
        capped_tx.fee_info.package_fee_rate, max_rate,
        "the capped speedup's package rate must equal max_feerate_sat_vb; got {}",
        capped_tx.fee_info.package_fee_rate,
    );

    // The `MaxFeeRateReached` news must reference the capped txid.
    let news = coordinator.get_news().unwrap();
    let cap_news = news
        .coordinator_news
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
            min_blocks_before_resend_speedup: 1,
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
        speedups[1].fee_info.package_fee_rate, 10,
        "RBF package rate must be clamped at max_feerate_sat_vb"
    );

    // The predecessor CPFP is marked at-cap (`fee_info.package_fee_rate = max`) so `boost_if_stale`
    // will skip it on subsequent stale intervals via the existing tip-at-cap check.
    let predecessor = coord_storage.get_tx_by_id(cpfp1_id).unwrap().unwrap();
    assert_eq!(
        predecessor.fee_info.package_fee_rate, 10,
        "predecessor must be marked at-cap so the chain stops attempting further RBFs"
    );

    // News log must contain MaxFeeRateReached referencing the new RBF.
    let news = coordinator.get_news().unwrap();
    let cap_news = news
        .coordinator_news
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

    // Mine one block. RBF's inputs share CPFP1_change with CPFP2 replacement so CPFP2 is skipped
    // via the `is_being_replaced` short-circuit.
    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    let reached = tick_until_all_states(
        &coordinator,
        &coord_storage,
        &[parent_tx.compute_txid(), cpfp1_txid, rbf_txid],
        TransactionState::Confirmed,
        10,
        None,
    )
    .unwrap();
    assert!(
        reached,
        "parent, CPFP1, and RBF must all confirm after the first block"
    );
    // CPFP2 was replaced in the mempool; it remains InMempool (replaced_by set)
    // until remove_replaced_rbf walks the chain at RBF finalization.
    assert_eq!(
        coord_storage
            .get_tx_by_id(cpfp2_txid)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::InMempool,
        "CPFP2 must stay InMempool while its RBF replacement is in flight"
    );
    ack_all_news(&coordinator, &coordinator.get_news().unwrap());

    // Mine the second confirming block (max_monitoring_confirmations=2) so the RBF
    // reaches is_finalized. review_speedups then calls remove_replaced_rbf(rbf, height),
    // which walks the `replaces` chain and settles each predecessor as Failed.
    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    let reached = tick_until_state(
        &coordinator,
        &coord_storage,
        rbf_txid,
        TransactionState::Finalized,
        10,
    )
    .unwrap();
    assert!(
        reached,
        "RBF must reach Finalized after the second confirming block (exercises remove_replaced_rbf)"
    );
    // remove_replaced_rbf settles CPFP2 as Failed when the RBF finalizes.
    let reached = tick_until_state(
        &coordinator,
        &coord_storage,
        cpfp2_txid,
        TransactionState::Failed,
        5,
    )
    .unwrap();
    assert!(
        reached,
        "CPFP2 must be settled Failed by remove_replaced_rbf when its replacing RBF finalizes"
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// When the RBF speedup is manually broadcast and confirmed before the coordinator
/// dispatches it, `handle_dispatch_result` fires the `AlreadyConfirmed` path, which
/// must mark the predecessor CPFP's `replaced_by` field. Simulates a crash and restart.
#[test]
fn test_rbf_already_confirmed_marks_predecessor() {
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
    let parent_txid = parent_tx.compute_txid();

    tick_until_ready(&coordinator).unwrap();
    coordinator.add_funding(funding_utxo).unwrap();
    coordinator
        .dispatch_with_speedup(parent_tx, speedup_data, ctx("rbf"), None, None)
        .unwrap();

    // CPFP1 built + dispatched (1 unconfirmed).
    let cpfp1_txid = build_and_dispatch_cpfp(&coordinator, &coord_storage, 3);

    // Boost 1: 1 unconfirmed < 2 → CPFP2.
    mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    coordinator.tick().unwrap();
    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(speedups.len(), 2, "boost 1 must create CPFP2");
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

    // Boost 2: 2 unconfirmed >= 2 → RBF built as ToDispatch. The coordinator
    // has not dispatched it yet; the build tick saves it as ToDispatch only.
    mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    coordinator.tick().unwrap();
    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(speedups.len(), 3, "boost 2 must add RBF");
    let rbf_txid = speedups[2].txid;
    let rbf_tx = speedups[2].tx.clone();
    assert!(speedups[2].speedup_kind().unwrap().is_rbf());
    assert_eq!(speedups[2].state, TransactionState::ToDispatch);

    // Manually broadcast the RBF before the coordinator's next tick dispatches it (simulating a crash and restart)
    // RBF shares CPFP1_change with CPFP2 replacement. The next block contains parent + CPFP1 + RBF.
    setup.bitcoin_client.send_transaction(&rbf_tx).unwrap();
    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();

    // Run several ticks. Expected sequence:
    // - review_speedups: parent / CPFP1 → Confirmed. CPFP2 → not_found,
    //   replaced_by None at this point → re-queued ToDispatch.
    // - dispatch_pending_speedups: CPFP2 ToDispatch dispatched first
    //   → MissingInput (its inputs spent by RBF on-chain) → settle Failed.
    //   RBF ToDispatch dispatched next → AlreadyConfirmed → sets
    //   CPFP2.replaced_by = Some(rbf_txid), marks RBF Confirmed.
    let reached = tick_until_all_states(
        &coordinator,
        &coord_storage,
        &[parent_txid, cpfp1_txid, rbf_txid],
        TransactionState::Confirmed,
        5,
        None,
    )
    .unwrap();
    assert!(reached, "parent, CPFP1, and RBF must reach Confirmed");

    // CPFP2 went `not_found` because the RBF replaced it on-chain. review_speedups recognizes
    // this as a local replacement (RBF) and does not re-queue or guard it
    mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    let reached = tick_until_state(
        &coordinator,
        &coord_storage,
        cpfp2_txid,
        TransactionState::Failed,
        5,
    )
    .unwrap();
    assert!(
        reached,
        "CPFP2 must settle Failed once the replacing RBF finalizes"
    );

    let cpfp2 = coord_storage.get_tx_by_id(cpfp2_txid).unwrap().unwrap();
    assert_eq!(
        cpfp2.state,
        TransactionState::Failed,
        "CPFP2 must be settled Failed (remove_replaced_rbf on RBF finalization)"
    );
    assert_eq!(
        cpfp2.speedup_kind().unwrap().context().replaced_by,
        Some(rbf_txid),
        "AlreadyConfirmed path on the RBF must set CPFP2.replaced_by = Some(rbf_txid)"
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

    // Both parent and CPFP must end up InMempool again. One tick suffices but give the recovery a small budget.
    let reached = tick_until_all_states(
        &coordinator,
        &coord_storage,
        &[parent_txid, cpfp_txid],
        TransactionState::InMempool,
        10,
        None,
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

/// Drives a NeedsSpeedup parent to `Failed` after its covering CPFP is in flight, then asserts the
/// cascade-via-parents gate settles the CPFP too, releases its funding and re-adds protocol parents to PSP.
/// Sequence:
///   1. Register parent + funding, drive parent → InMempool + CPFP → InMempool.
///   2. `expire_mempool` evicts both from bitcoind's mempool.
///   3. Double-spend the parent's wallet UTXO via a competing raw tx, mined into a block.
///   4. Tick → review_active flips parent ToDispatch → step 3 re-dispatches → bitcoind
///      returns MissingInput (input already spent on-chain) → `fail_and_cascade(parent)`.
///   5. Cascade walks ToDispatch speedups; the new gate `parents().contains(&tx.txid)`
///      catches the CPFP (also re-queued by review_speedups) and settles it Failed.
///   6. Verify both records are Failed and the funding UTXO is unspent again so it can be reused.
#[test]
fn test_parent_failure_cascades_cpfp_via_parents_gate() {
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
    let funding_txid = funding_utxo.txid;
    let (parent_tx, speedup_data) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        200_000,
    )
    .unwrap();
    let parent_txid = parent_tx.compute_txid();
    let parent_input = parent_tx.input[0].previous_output;

    tick_until_ready(&coordinator).unwrap();
    coordinator.add_funding(funding_utxo).unwrap();
    coordinator
        .dispatch_with_speedup(parent_tx, speedup_data, ctx("parent_fail"), None, None)
        .unwrap();

    // Drive parent + CPFP to InMempool.
    let cpfp_txid = build_and_dispatch_cpfp(&coordinator, &coord_storage, 3);
    assert_eq!(
        coord_storage
            .get_tx_by_id(parent_txid)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::InMempool,
    );

    // Build a boost chain on top of the CPFP so the cascade walks through every descendant.
    mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    coordinator.tick().unwrap();
    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(speedups.len(), 2, "first boost must add CPFP-of-CPFP");
    let boost_cpfp_txid = speedups[1].txid;
    assert!(
        !speedups[1].speedup_kind().unwrap().is_rbf(),
        "first boost must be a CPFP (1 unconfirmed < 2)"
    );
    let reached = tick_until_state(
        &coordinator,
        &coord_storage,
        boost_cpfp_txid,
        TransactionState::InMempool,
        3,
    )
    .unwrap();
    assert!(reached, "boost CPFP must reach InMempool");

    mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    coordinator.tick().unwrap();
    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(speedups.len(), 3, "second boost must add an RBF");
    let rbf_txid = speedups[2].txid;
    assert!(
        speedups[2].speedup_kind().unwrap().is_rbf(),
        "second boost must be RBF (2 unconfirmed >= max)"
    );
    let reached = tick_until_state(
        &coordinator,
        &coord_storage,
        rbf_txid,
        TransactionState::InMempool,
        3,
    )
    .unwrap();
    assert!(reached, "RBF must reach InMempool before the eviction");

    // After RBF dispatched, its predecessor's `replaced_by` is set.
    let predecessor = coord_storage
        .get_tx_by_id(boost_cpfp_txid)
        .unwrap()
        .unwrap();
    assert_eq!(
        predecessor.speedup_kind().unwrap().context().replaced_by,
        Some(rbf_txid),
        "RBF dispatch must set replaced_by on its predecessor"
    );

    // Evict bitcoind's mempool (parent + every speedup gone).
    expire_mempool(&setup.bitcoin_client, &setup.regtest_wallet).unwrap();

    // Build a raw conflict tx spending the same UTXO to a fresh wallet address.
    let conflict_addr = setup.bitcoin_client.init_wallet("test_wallet").unwrap();
    let conflict_inputs = vec![bitcoincore_rpc::json::CreateRawTransactionInput {
        txid: parent_input.txid,
        vout: parent_input.vout,
        sequence: None,
    }];
    let mut conflict_outputs = std::collections::HashMap::new();
    conflict_outputs.insert(
        format!("{}", conflict_addr),
        bitcoin::Amount::from_sat(180_000),
    );
    let raw_tx = setup
        .bitcoin_client
        .client
        .create_raw_transaction(&conflict_inputs, &conflict_outputs, None, None)
        .expect("create conflict raw tx");
    let signed = setup
        .bitcoin_client
        .client
        .sign_raw_transaction_with_wallet(&raw_tx, None, None)
        .expect("sign conflict tx");
    assert!(signed.complete);
    let conflict_tx: bitcoin::Transaction =
        bitcoin::consensus::Decodable::consensus_decode(&mut &signed.hex[..])
            .expect("decode conflict tx");

    setup
        .bitcoin_client
        .send_transaction(&conflict_tx)
        .expect("broadcast conflict tx");
    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();

    // Tick 1: indexer syncs. Parent not yet seen missing, guard not armed.
    coordinator.tick().unwrap();
    // Tick 2: indexer reaches the conflict height, review sees the parent `not_found`, re-queues it
    // ToDispatch and arms the fail guard at `current_height + max_monitoring_confirmations`.
    coordinator.tick().unwrap();

    // Mine strictly past the guard window (max_monitoring_confirmations = 2, so 3) so the
    // genuinely-spent input is allowed to settle Failed (permanent conflict, not a transient reorg).
    mine_empty_blocks(&setup.bitcoin_client, 3, &setup.regtest_wallet).unwrap();

    // Tick until parent + descendants settle Failed. The guard defers via `mark_as_retry`, so the
    // re-dispatch that re-runs classify (and now fails the parent → cascade) is rate-limited to
    // `retry_interval_seconds`; sleep just over it before each tick to clear the limit.
    let reached = tick_until_all_states(
        &coordinator,
        &coord_storage,
        &[parent_txid, cpfp_txid, rbf_txid],
        TransactionState::Failed,
        cpfp_settings().coordinator.retry_attempts_sending_tx + 2,
        Some(cpfp_settings().coordinator.retry_interval_seconds * 1000 + 1),
    )
    .unwrap();
    assert!(
        reached,
        "parent, CPFP, boost CPFP, and RBF must all settle Failed after the parent double-spend is mined"
    );

    // CPFP's funding UTXO must have been released by `mark_parents_unspent`.
    let funding_records = coord_storage.read_funding_records().unwrap();
    let funding_record = funding_records
        .iter()
        .find(|r| r.utxo.txid == funding_txid)
        .expect("funding record must still be present");
    assert!(
        !funding_record.spent,
        "funding mark must be released after the CPFP was cascade-failed"
    );

    // RBF settle path: `replaced_by` on its predecessor must have been cleared by `settle_failed_dispatch`.
    let boost_cpfp = coord_storage
        .get_tx_by_id(boost_cpfp_txid)
        .unwrap()
        .unwrap();
    assert_eq!(
        boost_cpfp.speedup_kind().unwrap().context().replaced_by,
        None,
        "RBF cascade-fail must clear `replaced_by` on its predecessor (settle_failed_dispatch RBF branch)"
    );

    // `SpeedupDispatchError` news must have fired for the cascaded speedups.
    let news = coordinator.get_news().unwrap();
    for cascaded in [cpfp_txid, rbf_txid] {
        assert!(
            news.coordinator_news.iter().any(|n| matches!(
                n,
                CoordinatorNews::SpeedupDispatchError { txid, .. } if *txid == cascaded
            )),
            "SpeedupDispatchError news must fire for cascaded speedup {}",
            cascaded
        );
    }

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

    // Tick again immediately; the interval has not elapsed so the CPFP is still rate-limited.
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
        None,
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
        None,
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

/// Speedup-primary combine + chain continuation. ///
/// 1. A modest `funding_initial` and a plentiful `funding_extra` are queued. P1 dispatches; CPFP1 builds from
///   `funding_initial` alone. The inflated fee from `multi_funding_settings` makes CPFP1's change tight.
/// 2. CPFP1 confirms and finalizes. `replace_funding_on_finalize` collapses `funding_initial` into a single entry.
/// 3. P2 dispatches. The chain tip CPFP1_change is now Speedup-derived primary but its amount is below the next
///     CPFP fee + dust, so the unified build path pulls in `funding_extra` as the combine partner.
#[test]
fn test_chain_tip_combine_after_finalize() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = TestKeyManager::new();
    let coordinator =
        create_coordinator_with_km(&setup, key_manager.rc(), multi_funding_settings());
    let coord_storage = get_coord_storage(&setup);

    let funding_initial = create_funded_speedup_utxo(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        25_000,
    )
    .unwrap();
    let funding_extra = create_funded_speedup_utxo(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        1_000_000,
    )
    .unwrap();

    let (parent1, sd1) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        1_000,
    )
    .unwrap();
    let parent1_txid = parent1.compute_txid();

    tick_until_ready(&coordinator).unwrap();
    coordinator.add_funding(funding_initial.clone()).unwrap();
    coordinator.add_funding(funding_extra.clone()).unwrap();
    coordinator
        .dispatch_with_speedup(parent1, sd1, ctx("chain_combine_p1"), None, None)
        .unwrap();

    // Phase 1: CPFP1 built from funding_initial alone.
    let cpfp1_txid = build_and_dispatch_cpfp(&coordinator, &coord_storage, 3);
    {
        let cpfp1 = coord_storage.get_tx_by_id(cpfp1_txid).unwrap().unwrap();
        let inputs = &cpfp1.speedup_kind().unwrap().context().funding_inputs;
        assert_eq!(
            inputs.len(),
            1,
            "CPFP1 must use a single Funding-kind input (Fi primary, no combine)"
        );
        assert_eq!(inputs[0].txid, funding_initial.txid);
    }

    // Phase 2: confirm + finalize CPFP1.
    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    let reached = tick_until_all_states(
        &coordinator,
        &coord_storage,
        &[parent1_txid, cpfp1_txid],
        TransactionState::Confirmed,
        10,
        None,
    )
    .unwrap();
    assert!(reached, "parent1 and CPFP1 must reach Confirmed");
    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    let reached = tick_until_all_states(
        &coordinator,
        &coord_storage,
        &[parent1_txid, cpfp1_txid],
        TransactionState::Finalized,
        10,
        None,
    )
    .unwrap();
    assert!(reached, "parent1 and CPFP1 must reach Finalized");
    ack_all_news(&coordinator, &coordinator.get_news().unwrap());

    // After finalize the funding queue holds: CPFP1's materialized change (from_speedup) + funding_extra (user, unspent).
    let funding_records = coord_storage.read_funding_records().unwrap();
    assert_eq!(
        funding_records.len(),
        2,
        "funding queue must hold CPFP1's change + funding_extra"
    );
    let cpfp1_record = funding_records
        .iter()
        .find(|r| r.utxo.txid == cpfp1_txid)
        .expect("CPFP1's change must be parked in the funding queue");
    assert!(
        cpfp1_record.from_speedup,
        "materialized change must be flagged from_speedup"
    );
    assert!(funding_records
        .iter()
        .any(|r| r.utxo.txid == funding_extra.txid));

    // Phase 3: dispatch P2. CPFP2 combines chain tip + extra Fi.
    let (parent2, sd2) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        1_000,
    )
    .unwrap();
    let parent2_txid = parent2.compute_txid();
    coordinator
        .dispatch_with_speedup(parent2, sd2, ctx("chain_combine_p2"), None, None)
        .unwrap();

    coordinator.tick().unwrap();
    let speedups = coord_storage.get_speedups_ordered().unwrap();
    // CPFP1 was moved out of SpeedupList by replace_on_finalize, so only CPFP2 remains active.
    assert_eq!(
        speedups.len(),
        1,
        "SpeedupList must contain only the active CPFP2 (CPFP1 moved to FundingList)"
    );
    let cpfp2 = &speedups[0];
    let cpfp2_txid = cpfp2.txid;
    let cpfp2_inputs = &cpfp2.speedup_kind().unwrap().context().funding_inputs;
    assert_eq!(
        cpfp2_inputs.len(),
        2,
        "CPFP2 must combine Speedup chain tip + Funding partner (got inputs={:?})",
        cpfp2_inputs
    );
    // Primary = CPFP1 change, partner = funding_extra.
    assert_eq!(cpfp2_inputs[0].txid, cpfp1_txid);
    assert_eq!(cpfp2_inputs[1].txid, funding_extra.txid);
    // The CPFP2 tx must actually spend the combine partner.
    assert!(
        cpfp2
            .tx
            .input
            .iter()
            .any(|i| i.previous_output.txid == funding_extra.txid),
        "CPFP2 tx must include the combine partner as a real input"
    );

    let reached = tick_until_state(
        &coordinator,
        &coord_storage,
        cpfp2_txid,
        TransactionState::InMempool,
        3,
    )
    .unwrap();
    assert!(reached, "CPFP2 must reach InMempool");
    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    let reached = tick_until_all_states(
        &coordinator,
        &coord_storage,
        &[parent2_txid, cpfp2_txid],
        TransactionState::Confirmed,
        10,
        None,
    )
    .unwrap();
    assert!(reached, "parent2 and CPFP2 must reach Confirmed");

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
        coordinator
            .get_news()
            .unwrap()
            .coordinator_news
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

/// `SpeedupContext.spent` is set on the chain-tip CPFP at the moment a newer speedup builds on top of it.
/// After a boost cycle (CPFP1 → CPFP2-boost), CPFP1 must be marked spent and CPFP2 must not be marked spent.
#[test]
fn test_chain_tip_spent_flag_progresses_with_boosts() {
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
        .dispatch_with_speedup(parent_tx, speedup_data, ctx("spent_flag"), None, None)
        .unwrap();

    // CPFP1 built + dispatched. spent flag still false (no boost yet).
    let cpfp1_txid = build_and_dispatch_cpfp(&coordinator, &coord_storage, 3);
    assert!(
        !coord_storage
            .get_tx_by_id(cpfp1_txid)
            .unwrap()
            .unwrap()
            .speedup_kind()
            .unwrap()
            .context()
            .spent,
        "fresh CPFP1 must NOT be marked spent before any boost"
    );

    // Trigger a boost: mine 1 empty block, then tick → CPFP2 built using CPFP1_change.
    mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    coordinator.tick().unwrap();

    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(speedups.len(), 2, "boost must add CPFP2");
    let cpfp2_txid = speedups[1].txid;

    // At save time, mark_funding_consumed sets CPFP1.spent = true.
    assert!(
        coord_storage
            .get_tx_by_id(cpfp1_txid)
            .unwrap()
            .unwrap()
            .speedup_kind()
            .unwrap()
            .context()
            .spent,
        "CPFP1 must be marked spent once CPFP2 reserves its change as funding"
    );
    // CPFP2 itself remains unspent (no boost on top of it yet).
    assert!(
        !coord_storage
            .get_tx_by_id(cpfp2_txid)
            .unwrap()
            .unwrap()
            .speedup_kind()
            .unwrap()
            .context()
            .spent,
        "new chain tip CPFP2 must NOT be marked spent"
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// RBF can pull a combine partner when its inherited (Speedup-derived) funding is insufficient at the
/// bumped fee. Sequence:
///   1. CPFP1 built from a queue funding Fi.
///   2. Boost (1 unconfirmed < 2) → CPFP2 with a single Speedup-derived input (CPFP1's change).
///   3. Boost again (2 unconfirmed = max) → CPFP3 is an RBF replacing CPFP2. RBF inherits CPFP2's
///      funding_inputs = [CPFP1_change]. Because the single input is Speedup-derived, the unified
///       build path is allowed to combine. A queued Fi is pulled in as the second input.
/// Settings are tuned so that CPFP1_change is just enough for CPFP2 but not for the RBF's bumped fee.
#[test]
fn test_rbf_combines_when_inherited_funding_insufficient() {
    init_trace();

    let settings = BitcoinSettings {
        fee: FeeSettings {
            min_safe_fee_rate: 80,
            max_feerate_sat_vb: 1000,
            base_fee_multiplier: 1.0,
        },
        speedup: SpeedupSettings {
            max_unconfirmed_speedups: 2,
            min_blocks_before_resend_speedup: 1,
            bump_fee_percentage: 2.0,
        },
        ..cpfp_settings()
    };

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = TestKeyManager::new();
    let coordinator = create_coordinator_with_km(&setup, key_manager.rc(), settings);
    let coord_storage = get_coord_storage(&setup);

    // Initial funding: large enough for CPFP1 + CPFP2 but its leftover change
    // after CPFP2 is consumed by the RBF cannot cover the bumped RBF fee alone.
    let funding_initial = create_funded_speedup_utxo(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        60_000,
    )
    .unwrap();
    // Extra funding stays unspent in the queue, ready for combine.
    let funding_extra = create_funded_speedup_utxo(
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

    tick_until_ready(&coordinator).unwrap();
    coordinator.add_funding(funding_initial.clone()).unwrap();
    coordinator.add_funding(funding_extra.clone()).unwrap();
    coordinator
        .dispatch_with_speedup(parent_tx, speedup_data, ctx("rbf_combine"), None, None)
        .unwrap();

    // Build + dispatch CPFP1.
    let cpfp1_txid = build_and_dispatch_cpfp(&coordinator, &coord_storage, 3);
    let cpfp1_inputs = coord_storage
        .get_tx_by_id(cpfp1_txid)
        .unwrap()
        .unwrap()
        .speedup_kind()
        .unwrap()
        .context()
        .funding_inputs
        .clone();
    assert_eq!(
        cpfp1_inputs.len(),
        1,
        "CPFP1 must use a single Funding-kind input (only the initial Fi is consumed)"
    );
    assert_eq!(cpfp1_inputs[0].txid, funding_initial.txid);

    // First boost: 1 unconfirmed < max(2) → CPFP. Chain tip primary
    // (Speedup-derived) from CPFP1's change. Must remain single-input.
    mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    coordinator.tick().unwrap();
    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(speedups.len(), 2, "first boost must add CPFP2");
    let cpfp2 = &speedups[1];
    let cpfp2_txid = cpfp2.txid;
    assert!(
        !cpfp2.speedup_kind().unwrap().is_rbf(),
        "first boost must be a CPFP (1 unconfirmed < limit of 2)"
    );
    let cpfp2_inputs = &cpfp2.speedup_kind().unwrap().context().funding_inputs;
    assert_eq!(
        cpfp2_inputs.len(),
        1,
        "CPFP2 must have a single funding input (chain tip CPFP1_change)"
    );
    assert_eq!(cpfp2_inputs[0].txid, cpfp1_txid);
    let reached = tick_until_state(
        &coordinator,
        &coord_storage,
        cpfp2_txid,
        TransactionState::InMempool,
        3,
    )
    .unwrap();
    assert!(reached, "CPFP2 must reach InMempool");

    // Second boost: 2 unconfirmed >= max → RBF replacing CPFP2.  At the bumped fee CPFP1_change
    // alone is insufficient, so `get_combine_funding` pulls in funding_extra as the second input.
    mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    coordinator.tick().unwrap();
    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(speedups.len(), 3, "second boost must add the RBF");
    let rbf = &speedups[2];
    assert!(
        rbf.speedup_kind().unwrap().is_rbf(),
        "second boost must be RBF (unconfirmed limit reached)"
    );
    let rbf_inputs = &rbf.speedup_kind().unwrap().context().funding_inputs;
    assert_eq!(
        rbf_inputs.len(),
        2,
        "RBF must combine inherited input + queue funding (got inputs={:?})",
        rbf_inputs
    );
    // Inherited input first (matches CPFP1's change), extra second.
    assert_eq!(rbf_inputs[0].txid, cpfp1_txid);
    assert_eq!(rbf_inputs[1].txid, funding_extra.txid);
    // The RBF tx must actually spend the extra Fi.
    assert!(
        rbf.tx
            .input
            .iter()
            .any(|i| i.previous_output.txid == funding_extra.txid),
        "RBF tx must include the combine partner as a spent input"
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// Cancel use cases involving a NeedsSpeedup parent and an in-flight CPFP.
///
/// Three phases exercised in one coordinator instance, matching the new
/// cancel contract (only Normal / NeedsSpeedup in ToDispatch is cancellable):
///   A) Cancel a parent BEFORE its first tick, succeeds. The parent never
///      enters the mempool, no CPFP is built, funding stays unspent, PSP self-prunes.
///   B) Cancel a parent AFTER it has been dispatched, REFUSED. `InvalidCancel`
///      news fires, the parent record stays in storage, normal lifecycle continues.
///   C) Register a fresh parent post-rejection and verify the coordinator state
///      is clean for new work.
#[test]
fn test_cancel_parent_edge_cases() {
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
    let funding_txid = funding_utxo.txid;

    tick_until_ready(&coordinator).unwrap();
    coordinator.add_funding(funding_utxo).unwrap();

    // ---------------------------------------------------------------
    // Phase A: cancel BEFORE the first tick
    // ---------------------------------------------------------------
    let (parent_a, sd_a) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        200_000,
    )
    .unwrap();
    let parent_a_txid = parent_a.compute_txid();
    coordinator
        .dispatch_with_speedup(parent_a, sd_a, ctx("cancel_a"), None, None)
        .unwrap();

    coordinator
        .cancel(TypesToMonitor::Transactions(
            vec![parent_a_txid],
            ctx("cancel_a"),
            None,
        ))
        .unwrap();
    assert!(
        coord_storage.get_tx_by_id(parent_a_txid).unwrap().is_none(),
        "parent_a must be gone from storage immediately after cancel",
    );

    // Next tick: PSP lazy-prunes the dangling entry; no CPFP is built.
    coordinator.tick().unwrap();
    assert!(
        coord_storage.get_speedups_ordered().unwrap().is_empty(),
        "no CPFP must be built for a cancelled parent",
    );
    let records = coord_storage.read_funding_records().unwrap();
    assert_eq!(records.len(), 1, "funding queue must be untouched");
    assert_eq!(records[0].utxo.txid, funding_txid);
    assert!(
        !records[0].from_speedup,
        "user funding is not speedup-derived"
    );
    assert!(!records[0].spent, "funding must stay unspent after cancel");

    // ---------------------------------------------------------------
    // Phase B: cancel AFTER dispatch, REFUSED
    // ---------------------------------------------------------------
    let (parent_b, sd_b) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        200_000,
    )
    .unwrap();
    let parent_b_txid = parent_b.compute_txid();
    coordinator
        .dispatch_with_speedup(parent_b, sd_b, ctx("cancel_b"), None, None)
        .unwrap();

    // Tick → parent_b dispatched to InMempool; CPFP built ToDispatch.
    coordinator.tick().unwrap();
    assert_eq!(
        coord_storage
            .get_tx_by_id(parent_b_txid)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::InMempool,
        "parent_b must be InMempool after dispatch",
    );

    // Cancel attempt is refused; news emitted; tx record stays put.
    coordinator
        .cancel(TypesToMonitor::Transactions(
            vec![parent_b_txid],
            ctx("cancel_b"),
            None,
        ))
        .unwrap();
    assert!(
        coord_storage.get_tx_by_id(parent_b_txid).unwrap().is_some(),
        "parent_b must REMAIN in storage; cancel after dispatch is refused",
    );
    let news = coordinator.get_news().unwrap();
    assert!(
        news.coordinator_news.iter().any(|n| matches!(
            n,
            CoordinatorNews::InvalidCancel { txid: id, .. } if *id == parent_b_txid
        )),
        "InvalidCancel news must be emitted for the rejected cancel",
    );
    ack_all_news(&coordinator, &news);

    // The CPFP that was already built keeps progressing normally.
    let speedups = coord_storage.get_speedups_ordered().unwrap();
    assert_eq!(speedups.len(), 1);
    let cpfp_txid = speedups[0].txid;
    let reached = tick_until_state(
        &coordinator,
        &coord_storage,
        cpfp_txid,
        TransactionState::InMempool,
        3,
    )
    .unwrap();
    assert!(
        reached,
        "CPFP keeps making progress despite the refused cancel"
    );

    // Mine through to Finalized so the FundingList is updated.
    mine_blocks(&setup.bitcoin_client, 2, &setup.regtest_wallet).unwrap();
    let reached = tick_until_state(
        &coordinator,
        &coord_storage,
        cpfp_txid,
        TransactionState::Finalized,
        10,
    )
    .unwrap();
    assert!(reached, "CPFP must finalize after blocks are mined");
    ack_all_news(&coordinator, &coordinator.get_news().unwrap());

    // ---------------------------------------------------------------
    // Phase C: register a fresh parent and verify the post-rejection state is clean
    // ---------------------------------------------------------------
    let (parent_c, sd_c) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        200_000,
    )
    .unwrap();
    let parent_c_txid = parent_c.compute_txid();
    coordinator
        .dispatch_with_speedup(parent_c, sd_c, ctx("post_cancel"), None, None)
        .unwrap();

    let cpfp_c_txid = build_and_dispatch_cpfp(&coordinator, &coord_storage, 5);
    assert_ne!(
        cpfp_c_txid, cpfp_txid,
        "fresh CPFP must have a different txid from the previous chain tip",
    );
    assert_eq!(
        coord_storage
            .get_tx_by_id(parent_c_txid)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::InMempool,
        "parent_c must be InMempool after the new CPFP dispatches",
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// Real dispute case, CPFP chain: two parents share one root CPFP and a CPFP-of-CPFP boost sits on top,
/// then an opponent replaces parent1 (double-spends its input) so parent1 settles Failed and its speedup
/// output never exists. The whole chain depends on that dead output; recovery must rebuild a fresh CPFP
/// for the surviving parent, and take the boost down with the root.
#[test]
fn test_cpfp_chain_survivor_after_parent_replaced() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = TestKeyManager::new();
    let settings = rebuild_settings(5);
    let coordinator = create_coordinator_with_km(&setup, key_manager.rc(), settings.clone());
    let coord_storage = get_coord_storage(&setup);

    let funding = create_funded_speedup_utxo(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        1_000_000,
    )
    .unwrap();
    let (p1, sd1) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        200_000,
    )
    .unwrap();
    let (p2, sd2) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        200_000,
    )
    .unwrap();
    let p1_txid = p1.compute_txid();
    let p2_txid = p2.compute_txid();
    let p1_clone = p1.clone();

    tick_until_ready(&coordinator).unwrap();
    coordinator.add_funding(funding).unwrap();
    coordinator
        .dispatch_with_speedup(p1, sd1, ctx("dead"), None, None)
        .unwrap();
    coordinator
        .dispatch_with_speedup(p2, sd2, ctx("live"), None, None)
        .unwrap();

    // Build + dispatch the root CPFP over both parents to InMempool.
    let root_cpfp = build_and_dispatch_cpfp(&coordinator, &coord_storage, 5);

    // Age it one block, then let boost_if_stale add a CPFP-of-CPFP boost on top (slots free -> CPFP, not RBF).
    mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    let mut boost_txid = None;
    for _ in 0..8 {
        coordinator.tick().unwrap();
        ack_all_news(&coordinator, &coordinator.get_news().unwrap());
        for s in coord_storage.get_speedups_ordered().unwrap() {
            if s.txid != root_cpfp && s.state == TransactionState::InMempool {
                boost_txid = Some(s.txid);
            }
        }
        if boost_txid.is_some() {
            break;
        }
    }
    let boost_txid =
        boost_txid.expect("a CPFP-of-CPFP boost must be built and dispatched on top of the root");

    // Opponent replaces parent1 by double-spending its input; parent1 settles Failed, its output never exists.
    replace_parent_by_opponent(&setup.bitcoin_client, &setup.regtest_wallet, &p1_clone, 0).unwrap();

    let survivor_cpfp = drive_dead_parent_recovery(
        &coordinator,
        &coord_storage,
        &setup,
        &settings,
        root_cpfp,
        p1_txid,
        p2_txid,
    );

    assert_ne!(
        survivor_cpfp, root_cpfp,
        "survivor must be a brand-new CPFP"
    );
    assert_ne!(
        survivor_cpfp, boost_txid,
        "survivor must be a brand-new CPFP, not the old boost"
    );
    let parents = coord_storage
        .get_tx_by_id(survivor_cpfp)
        .unwrap()
        .unwrap()
        .speedup_kind()
        .unwrap()
        .parents()
        .to_vec();
    assert!(
        parents.contains(&p2_txid) && !parents.contains(&p1_txid),
        "survivor CPFP must cover only parent2; got {:?}",
        parents
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// Real dispute case, where the parent transaction itself disappears when an opponent replaces it by
/// double-spending its input, so the parent settles Failed and its speedup output never exists. The
/// dispatcher parent-gate then closes, so the root CPFP and its RBF can never re-dispatch. Recovery must
/// come through the parent failing, which fails the child pre-send (ParentFailed) and rebuilds a fresh
/// CPFP for the surviving parent. This checks that an RBF-of-root still lets the survivor recover.
#[test]
fn test_rbf_of_root_survivor_after_parent_replaced() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = TestKeyManager::new();
    let mut settings = rebuild_settings(1);
    settings.fee = FeeSettings {
        min_safe_fee_rate: 10,
        max_feerate_sat_vb: 1000,
        base_fee_multiplier: 1.0,
    };
    settings.speedup.bump_fee_percentage = 2.0;
    let coordinator = create_coordinator_with_km(&setup, key_manager.rc(), settings.clone());
    let coord_storage = get_coord_storage(&setup);

    let funding = create_funded_speedup_utxo(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        1_000_000,
    )
    .unwrap();
    let (p1, sd1) = create_coordinator_parent_tx_with_fee(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        200_000,
        500,
    )
    .unwrap();
    let (p2, sd2) = create_coordinator_parent_tx_with_fee(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        200_000,
        500,
    )
    .unwrap();
    let p1_txid = p1.compute_txid();
    let p2_txid = p2.compute_txid();
    let p1_clone = p1.clone();

    tick_until_ready(&coordinator).unwrap();
    coordinator.add_funding(funding).unwrap();
    coordinator
        .dispatch_with_speedup(p1, sd1, ctx("dead"), None, None)
        .unwrap();
    coordinator
        .dispatch_with_speedup(p2, sd2, ctx("live"), None, None)
        .unwrap();

    let root_cpfp = build_and_dispatch_cpfp(&coordinator, &coord_storage, 5);

    mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    let mut rbf_txid = None;
    for _ in 0..8 {
        coordinator.tick().unwrap();
        ack_all_news(&coordinator, &coordinator.get_news().unwrap());
        for s in coord_storage.get_speedups_ordered().unwrap() {
            if s.txid != root_cpfp
                && s.state == TransactionState::InMempool
                && s.speedup_kind().map(|k| k.is_rbf()).unwrap_or(false)
            {
                rbf_txid = Some(s.txid);
            }
        }
        if rbf_txid.is_some() {
            break;
        }
    }
    let rbf_txid = rbf_txid.expect("an RBF-of-root must replace the root and reach InMempool");

    // Scenario B kill: opponent replaces the parent by double-spending its input; the parent settles Failed.
    replace_parent_by_opponent(&setup.bitcoin_client, &setup.regtest_wallet, &p1_clone, 0).unwrap();

    let survivor_cpfp = drive_dead_parent_recovery(
        &coordinator,
        &coord_storage,
        &setup,
        &settings,
        root_cpfp,
        p1_txid,
        p2_txid,
    );

    let rbf_state = coord_storage
        .get_tx_by_id(rbf_txid)
        .unwrap()
        .map(|t| t.state);
    assert!(
        matches!(rbf_state, None | Some(TransactionState::Failed)),
        "the RBF-of-root must be Failed or evicted; got {:?}",
        rbf_state
    );
    assert_ne!(survivor_cpfp, root_cpfp);
    let parents = coord_storage
        .get_tx_by_id(survivor_cpfp)
        .unwrap()
        .unwrap()
        .speedup_kind()
        .unwrap()
        .parents()
        .to_vec();
    assert!(
        parents.contains(&p2_txid) && !parents.contains(&p1_txid),
        "survivor CPFP must cover only parent2; got {:?}",
        parents
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// A parent that is only transiently absent must keep its acceleration and must not drag the healthy ones.
/// Batch of three parents p1, p2, p3 under one root CPFP. p1 is killed permanently. p2 is evicted a couple
/// blocks later, so when p1's guard window elapses and the root fails, p2 is absent-but-not-yet-Failed (its
/// own window still open). The rescue must: drop p1, build a fresh CPFP over the live p3 immediately without
/// waiting on p2, and leave p2 pending. When p2's disappearance is then reverted by a reorg, p2 comes back
/// and gets its own CPFP on a later pass. This is the state-based survivor test: classifying a survivor by a
/// momentary output probe would wrongly drop p2 here.
#[test]
fn test_batch_transiently_missing_parent_kept_then_recovered() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let key_manager = TestKeyManager::new();
    // Guard window of 3 blocks gives slack to place p2's window a couple blocks behind p1's.
    let mut settings = rebuild_settings(5);
    settings.monitor.max_monitoring_confirmations = Some(3);
    let coordinator = create_coordinator_with_km(&setup, key_manager.rc(), settings.clone());
    let coord_storage = get_coord_storage(&setup);

    let funding = create_funded_speedup_utxo(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        1_000_000,
    )
    .unwrap();
    let (p1, sd1) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        200_000,
    )
    .unwrap();
    let (p2, sd2) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        200_000,
    )
    .unwrap();
    let (p3, sd3) = create_coordinator_parent_tx(
        &setup.bitcoin_client,
        &*key_manager,
        Network::Regtest,
        200_000,
    )
    .unwrap();
    let p1_txid = p1.compute_txid();
    let p2_txid = p2.compute_txid();
    let p3_txid = p3.compute_txid();
    let p1_clone = p1.clone();
    let p2_clone = p2.clone();

    tick_until_ready(&coordinator).unwrap();
    coordinator.add_funding(funding).unwrap();
    coordinator
        .dispatch_with_speedup(p1, sd1, ctx("dead"), None, None)
        .unwrap();
    coordinator
        .dispatch_with_speedup(p2, sd2, ctx("flap"), None, None)
        .unwrap();
    coordinator
        .dispatch_with_speedup(p3, sd3, ctx("live"), None, None)
        .unwrap();

    // One root CPFP over all three parents.
    let root_cpfp = build_and_dispatch_cpfp(&coordinator, &coord_storage, 5);
    let root_parents = coord_storage
        .get_tx_by_id(root_cpfp)
        .unwrap()
        .unwrap()
        .speedup_kind()
        .unwrap()
        .parents()
        .to_vec();
    assert!(
        root_parents.contains(&p1_txid)
            && root_parents.contains(&p2_txid)
            && root_parents.contains(&p3_txid),
        "root CPFP must cover all three parents; got {:?}",
        root_parents
    );

    let max_confs = settings.monitor.max_monitoring_confirmations.unwrap() as usize;
    let retry_ms = settings.coordinator.retry_interval_seconds * 1000;
    let sleep = std::time::Duration::from_millis(retry_ms + 300);

    // Kill p1 permanently in its own block, then let review see it not_found and arm its guard.
    replace_parent_by_opponent(&setup.bitcoin_client, &setup.regtest_wallet, &p1_clone, 0).unwrap();
    std::thread::sleep(sleep);
    coordinator.tick().unwrap();
    ack_all_news(&coordinator, &coordinator.get_news().unwrap());

    // Advance one block so p2's guard will start behind p1's.
    mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    std::thread::sleep(sleep);
    coordinator.tick().unwrap();
    ack_all_news(&coordinator, &coordinator.get_news().unwrap());

    // Kill p2 in its own block (its guard now trails p1's). The conflict pays a low fee (reclaims 99_000 of
    // the parent's 100_000 fee) so that, once we invalidate this block, the higher-fee p2 can RBF-replace the
    // resurrected conflict and recover. Keep the block hash so we can revert just this one.
    let p2_kill_block = replace_parent_by_opponent(
        &setup.bitcoin_client,
        &setup.regtest_wallet,
        &p2_clone,
        99_000,
    )
    .unwrap();
    std::thread::sleep(sleep);
    coordinator.tick().unwrap();
    ack_all_news(&coordinator, &coordinator.get_news().unwrap());

    // Drive until p1's window elapses: the root fails (ParentFailed) and rebuild_survivors runs. p2 must
    // still be inside its own (later) window, so a fresh CPFP is built over the live p3 only, not p2.
    let mut p3_cpfp = None;
    for _ in 0..(max_confs + 4) * 3 {
        std::thread::sleep(sleep);
        coordinator.tick().unwrap();
        ack_all_news(&coordinator, &coordinator.get_news().unwrap());

        let root_failed = coord_storage
            .get_tx_by_id(root_cpfp)
            .unwrap()
            .map_or(false, |t| t.state == TransactionState::Failed);
        if root_failed {
            let p2_state = coord_storage
                .get_tx_by_id(p2_txid)
                .unwrap()
                .map(|t| t.state);
            assert!(
                !matches!(p2_state, Some(TransactionState::Failed)),
                "p2 must NOT be Failed at the rebuild moment (its own window has not elapsed); got {:?}",
                p2_state
            );
            for s in coord_storage.get_speedups_ordered().unwrap() {
                if s.txid == root_cpfp {
                    continue;
                }
                if let Ok(k) = s.speedup_kind() {
                    let ps = k.parents();
                    if ps.contains(&p3_txid) {
                        assert!(
                            !ps.contains(&p1_txid) && !ps.contains(&p2_txid),
                            "fresh CPFP must cover only live p3 (p1 dead, p2 transiently absent); got {:?}",
                            ps
                        );
                        p3_cpfp = Some(s.txid);
                    }
                }
            }
            if p3_cpfp.is_some() {
                break;
            }
        }
        mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    }
    let p3_cpfp = p3_cpfp.expect("root must fail on dead p1 and rebuild a fresh CPFP over live p3");
    assert_ne!(p3_cpfp, root_cpfp);

    // No live speedup may cover p2 yet: it is still transiently absent.
    let p2_covered = coord_storage
        .get_speedups_ordered()
        .unwrap()
        .iter()
        .any(|s| {
            s.txid != root_cpfp
                && s.state != TransactionState::Failed
                && s.speedup_kind()
                    .map(|k| k.parents().contains(&p2_txid))
                    .unwrap_or(false)
        });
    assert!(
        !p2_covered,
        "no live speedup should cover p2 while it is transiently absent"
    );

    // Broadcast the fresh p3 CPFP so no ToDispatch speedup blocks a later p2 CPFP.
    assert!(
        tick_until_state(
            &coordinator,
            &coord_storage,
            p3_cpfp,
            TransactionState::InMempool,
            6
        )
        .unwrap(),
        "the fresh CPFP over p3 must reach the mempool"
    );
    ack_all_news(&coordinator, &coordinator.get_news().unwrap());

    // Revert p2's disappearance: invalidate its kill block. p1's kill block is older and survives, so p1
    // stays dead, but p2's input is free again.
    setup
        .bitcoin_client
        .client
        .invalidate_block(&p2_kill_block)
        .unwrap();

    // p2 must come back (re-dispatched and accepted) and then get its OWN CPFP on a later pass.
    let mut p2_cpfp = None;
    for i in 0..((max_confs + 4) * 3) {
        std::thread::sleep(sleep);
        coordinator.tick().unwrap();
        ack_all_news(&coordinator, &coordinator.get_news().unwrap());
        let p2_state = coord_storage
            .get_tx_by_id(p2_txid)
            .unwrap()
            .map(|t| t.state);
        let p2_in_livepsp = coord_storage
            .get_pending_speedup_parents()
            .unwrap()
            .iter()
            .any(|t| t.txid == p2_txid);
        eprintln!(
            "DBG iter={i} p2_state={:?} p2_in_livepsp={}",
            p2_state, p2_in_livepsp
        );
        for s in coord_storage.get_speedups_ordered().unwrap() {
            if s.txid == root_cpfp || s.state == TransactionState::Failed {
                continue;
            }
            if let Ok(k) = s.speedup_kind() {
                if k.parents().contains(&p2_txid) {
                    p2_cpfp = Some(s.txid);
                }
            }
        }
        if p2_cpfp.is_some() {
            break;
        }
        mine_empty_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    }
    let p2_cpfp =
        p2_cpfp.expect("once p2 reappears after the reorg revert, it must get its own CPFP");
    let p2_cpfp_parents = coord_storage
        .get_tx_by_id(p2_cpfp)
        .unwrap()
        .unwrap()
        .speedup_kind()
        .unwrap()
        .parents()
        .to_vec();
    assert!(
        p2_cpfp_parents.contains(&p2_txid) && !p2_cpfp_parents.contains(&p1_txid),
        "the recovered CPFP must cover p2 and never the permanently-dead p1; got {:?}",
        p2_cpfp_parents
    );

    // p1 stayed permanently dead throughout.
    assert_eq!(
        coord_storage.get_tx_by_id(p1_txid).unwrap().unwrap().state,
        TransactionState::Failed,
        "p1 must remain Failed after the reorg revert"
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}
