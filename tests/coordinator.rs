mod common;
use common::*;

use bitcoin_coordinator::{
    config::config::{BitcoinSettings, CoordinatorSettings, CoordinatorStorageSettings},
    core::funding::FundingStorage,
    types::{AckNews, CoordinatorNews, TransactionState},
};
use bitcoind::bitcoind::BitcoindFlags;
use bitvmx_bitcoin_rpc::bitcoin_client::BitcoinClientApi;
use bitvmx_transaction_monitor::{
    config::MonitorSettingsConfig,
    types::{MonitorNews, TypesToMonitor},
};
use tracing::info;

// =============================================================================
// HAPPY PATH TESTS
// =============================================================================

/// The coordinator starts out unready and becomes ready after the monitor has
/// caught up with the chain.
#[test]
fn test_tick_until_ready() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let coordinator = create_coordinator(&setup);

    assert!(!coordinator.is_ready().unwrap()); // Freshly created coordinator should not be ready before it has ticked
    tick_until_ready(&coordinator).unwrap();
    assert!(coordinator.is_ready().unwrap());

    assert!(coordinator.get_news().unwrap().is_empty());

    drop(coordinator);
    setup.end_all().unwrap();
}

/// Verifies the registration-to-dispatch lifecycle in a single test:
///
/// 1. `dispatch` immediately persists the transaction in storage
/// 2. The first tick broadcasts the transaction, advancing its state to `InMempool`
#[test]
fn test_tx_dispatch_to_mempool() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let coordinator = create_coordinator(&setup);

    tick_until_ready(&coordinator).unwrap();
    let tx = create_signed_tx_to_dispatch(&setup.bitcoin_client).unwrap();
    let txid = tx.compute_txid();
    coordinator
        .dispatch(tx, None, ctx("registration"), None, None)
        .unwrap();

    // `monitor` registers an additional txid for observation without dispatching it. Unnecessary for this test.
    coordinator
        .monitor(TypesToMonitor::Transactions(
            vec![txid],
            ctx("extra_monitor"),
            None,
        ))
        .unwrap();

    // Step 1: tx must be in storage as ToDispatch before any tick.
    let coord_storage = get_coord_storage(&setup);
    let stored = coord_storage.get_tx_by_id(txid).unwrap().unwrap();
    assert_eq!(stored.txid, txid);
    assert_eq!(stored.state, TransactionState::ToDispatch);
    assert_eq!(stored.retry_count, 0);
    assert_eq!(stored.context, ctx("registration"));

    // Step 2: one tick broadcasts the tx → InMempool.
    coordinator.tick().unwrap();
    let stored = coord_storage.get_tx_by_id(txid).unwrap().unwrap();
    assert_eq!(
        stored.state,
        TransactionState::InMempool,
        "Expected InMempool after dispatch tick; got {:?}",
        stored.state
    );
    assert!(
        stored.broadcast_block_height.is_some(),
        "broadcast_block_height must be set after dispatch"
    );

    assert!(coordinator.get_news().unwrap().is_empty());

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// Full lifecycle: ToDispatch → InMempool → Confirmed → Finalized → Evicted,
/// with monitor-news assertions at every transition.
///
/// Settings: `max_monitoring_confirmations = 2`, `max_tracking_confirmations = 1`
/// so the whole sequence completes in 3 regtest blocks.
#[test]
fn test_full_lifecycle() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let settings = BitcoinSettings {
        monitor: MonitorSettingsConfig {
            max_monitoring_confirmations: Some(2),
            ..Default::default()
        },
        storage: CoordinatorStorageSettings {
            max_tracking_confirmations: 1,
        },
        ..Default::default()
    };
    let coordinator = create_coordinator_with_settings(&setup, settings);
    let coord_storage = get_coord_storage(&setup);

    let tx = create_signed_tx_to_dispatch(&setup.bitcoin_client).unwrap();
    let txid = tx.compute_txid();
    info!("Registering tx {}", txid);

    tick_until_ready(&coordinator).unwrap();

    coordinator
        .dispatch_without_speedup(tx, ctx("lifecycle"), None, Some(1), None)
        .unwrap();

    // ── ToDispatch → InMempool ────────────────────────────────────────────────
    coordinator.tick().unwrap();
    assert_eq!(
        coord_storage.get_tx_by_id(txid).unwrap().unwrap().state,
        TransactionState::InMempool,
        "tx must be InMempool after dispatch tick"
    );

    // ── InMempool → Confirmed (1 block) ──────────────────────────────────────
    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    let reached = tick_until_state(
        &coordinator,
        &coord_storage,
        txid,
        TransactionState::Confirmed,
        10,
    )
    .unwrap();
    assert!(reached, "tx must reach Confirmed after 1 block");

    let news = coordinator.get_news().unwrap();
    assert!(
        news.monitor_news.iter().any(|n| {
            matches!(n, MonitorNews::Transaction(id, status, _) if *id == txid && status.is_confirmed())
        }),
        "expected Confirmed monitor news for {txid}; got {:?}",
        news.monitor_news
    );
    assert!(
        news.coordinator_news.is_empty(),
        "no coordinator news expected at Confirmed"
    );
    ack_all_news(&coordinator, &news);

    // ── Confirmed → Finalized (2nd block, max_monitoring_confirmations = 2) ──
    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    let reached = tick_until_state(
        &coordinator,
        &coord_storage,
        txid,
        TransactionState::Finalized,
        10,
    )
    .unwrap();
    assert!(reached, "tx must reach Finalized after 2 confirmations");

    // ── Finalized → Evicted (1 more block, max_tracking_confirmations = 1) ───
    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    let mut evicted = false;
    for _ in 0..10 {
        coordinator.tick().unwrap();
        let news = coordinator.get_news().unwrap();
        if news.coordinator_news.iter().any(
            |n| matches!(n, CoordinatorNews::TransactionEvicted { txid: id, .. } if *id == txid),
        ) {
            evicted = true;
            ack_all_news(&coordinator, &news);
            break;
        }
        ack_all_news(&coordinator, &news);
    }
    assert!(
        evicted,
        "TransactionEvicted news must fire after max_tracking_confirmations blocks"
    );
    assert!(
        coord_storage.get_tx_by_id(txid).unwrap().is_none(),
        "tx must be removed from storage after eviction"
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// A transaction registered with a future `target_block_height` is not
/// dispatched until the chain reaches that height.
#[test]
fn test_height_delays_dispatch() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let coordinator = create_coordinator(&setup);

    let tx = create_signed_tx_to_dispatch(&setup.bitcoin_client).unwrap();
    let txid = tx.compute_txid();
    tick_until_ready(&coordinator).unwrap();

    let current_height = setup.bitcoin_client.get_best_block().unwrap();
    let target_height = current_height + 4;
    info!(
        "current_height={}, target_height={}",
        current_height, target_height
    );

    coordinator
        .dispatch_without_speedup(tx, ctx("delayed_dispatch"), Some(target_height), None, None)
        .unwrap();

    // Ticking now must not dispatch because we are below the target.
    coordinator.tick().unwrap();
    let coord_storage = get_coord_storage(&setup);
    assert_eq!(
        coord_storage.get_tx_by_id(txid).unwrap().unwrap().state,
        TransactionState::ToDispatch,
        "tx must stay ToDispatch while below target_block_height"
    );

    // Mine one block at a time; the tx must stay ToDispatch for blocks 1–3
    // and only enter InMempool once the 4th block reaches target_height.
    for i in 1..=4 {
        mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
        tick_until_ready(&coordinator).unwrap();
        coordinator.tick().unwrap();

        let state = coord_storage.get_tx_by_id(txid).unwrap().unwrap().state;
        if i < 4 {
            assert_eq!(
                state,
                TransactionState::ToDispatch,
                "tx must stay ToDispatch after mining block {i} (target not yet reached)"
            );
        } else {
            assert_eq!(
                state,
                TransactionState::InMempool,
                "tx must be InMempool after mining block {i} (target_height reached)"
            );
        }
    }

    assert!(coordinator.get_news().unwrap().is_empty());

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// Registering multiple transactions and ticking once dispatches all of them
/// in a single pass.  Each independently transitions to `InMempool`.
#[test]
fn test_multiple_txs_dispatched_in_single_tick() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let coordinator = create_coordinator(&setup);

    // Create three independent signed transactions.
    let tx1 = create_signed_tx_to_dispatch(&setup.bitcoin_client).unwrap();
    let tx2 = create_signed_tx_to_dispatch(&setup.bitcoin_client).unwrap();
    let tx3 = create_signed_tx_to_dispatch(&setup.bitcoin_client).unwrap();
    let ids = [tx1.compute_txid(), tx2.compute_txid(), tx3.compute_txid()];

    tick_until_ready(&coordinator).unwrap();

    for tx in [tx1, tx2, tx3] {
        coordinator
            .dispatch_without_speedup(tx, ctx(&format!("batch")), None, None, None)
            .unwrap();
    }

    // A single tick should dispatch all three.
    coordinator.tick().unwrap();

    let coord_storage = get_coord_storage(&setup);
    for txid in ids {
        let state = coord_storage.get_tx_by_id(txid).unwrap().unwrap().state;
        assert_eq!(
            state,
            TransactionState::InMempool,
            "tx {} should be InMempool after batch dispatch",
            txid
        );
    }

    assert!(coordinator.get_news().unwrap().is_empty());

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// Trying to dispatch a parent and its child in the same tick, produces the dispatcher to defers the child
///  to a later tick, where the parent is now in mempool and the child broadcasts.
#[test]
fn test_dispatch_parent_then_child_across_ticks() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let coordinator = create_coordinator(&setup);

    let (parent, child) = create_parent_and_child_signed_txs(&setup.bitcoin_client);
    let parent_id = parent.compute_txid();
    let child_id = child.compute_txid();

    tick_until_ready(&coordinator).unwrap();

    coordinator
        .dispatch_without_speedup(parent, ctx("parent"), None, None, None)
        .unwrap();
    coordinator
        .dispatch_without_speedup(child, ctx("child"), None, None, None)
        .unwrap();

    // Tick 1: parent broadcasts; child is deferred (parent not yet in monitor cache).
    coordinator.tick().unwrap();
    let coord_storage = get_coord_storage(&setup);
    assert_eq!(
        coord_storage
            .get_tx_by_id(parent_id)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::InMempool,
        "parent {parent_id} must be InMempool after first tick"
    );
    assert_eq!(
        coord_storage.get_tx_by_id(child_id).unwrap().unwrap().state,
        TransactionState::ToDispatch,
        "child {child_id} must remain ToDispatch until the monitor sees the parent"
    );

    // Subsequent ticks: child broadcasts once parents_ready passes.
    let reached = tick_until_state(
        &coordinator,
        &coord_storage,
        child_id,
        TransactionState::InMempool,
        5,
    )
    .unwrap();
    assert!(
        reached,
        "child {child_id} must reach InMempool after the parent becomes observable"
    );

    assert!(coordinator.get_news().unwrap().is_empty());

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// Exercises `add_funding`'s validation across all three observable outcomes in a single end-to-end scenario:
///   1. A valid UTXO (above `min_funding_amount_sats`) is accepted with no news.
///   2. A second UTXO below the minimum is rejected and emits exactly one `InvalidFundingUtxo` news item.
///   3. A valid UTXO added after the invalid one is still accepted with no news
#[test]
fn test_add_funding_validation() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let coordinator = create_coordinator(&setup);
    let coord_storage = get_coord_storage(&setup);
    tick_until_ready(&coordinator).unwrap();

    // 1. Valid UTXO (default minimum is 10 000 sats; 20 000 is above it).
    let first_utxo = utxo(20_000);
    coordinator.add_funding(first_utxo.clone()).unwrap();
    assert!(
        coordinator.get_news().unwrap().coordinator_news.is_empty(),
        "no news must fire when a valid funding UTXO is added"
    );

    // 2. Below-minimum UTXO rejected, news emitted.
    coordinator.add_funding(utxo(9_999)).unwrap();
    let news = coordinator.get_news().unwrap();
    assert_eq!(
        news.coordinator_news.len(),
        1,
        "exactly one InvalidFundingUtxo news must fire; got {:?}",
        news.coordinator_news
    );
    assert!(
        matches!(
            &news.coordinator_news[0],
            CoordinatorNews::InvalidFundingUtxo { amount, min_required }
            if *amount == 9_999 && *min_required == 10_000
        ),
        "unexpected news payload: {:?}",
        news.coordinator_news[0]
    );

    // 3. Valid UTXO after invalid one is still accepted with no news.
    let third_utxo = utxo(50_000);
    coordinator.add_funding(third_utxo.clone()).unwrap();
    assert!(
        coordinator.get_news().unwrap().coordinator_news.is_empty(),
        "no news must fire when a valid funding UTXO is added, even after a previous invalid one"
    );

    let fundings = coord_storage.read_funding_records().unwrap();
    assert_eq!(
        fundings.len(),
        2,
        "both valid UTXOs must be present in storage; got {:?}",
        fundings
    );
    assert_eq!(
        fundings[0].get_funding_info().unwrap().0,
        first_utxo,
        "first valid UTXO must be in storage"
    );
    assert_eq!(
        fundings[1].get_funding_info().unwrap().0,
        third_utxo,
        "second valid UTXO must be in storage"
    );

    ack_all_news(&coordinator, &news);
    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// Cancelling a transaction that has not yet been dispatched removes it from
/// coordinator storage entirely.  Subsequent ticks do not dispatch it.
#[test]
fn test_cancel_pending_tx_before_dispatch() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let coordinator = create_coordinator(&setup);
    tick_until_ready(&coordinator).unwrap();

    let tx = dummy_tx();
    let txid = tx.compute_txid();

    coordinator
        .dispatch_without_speedup(tx, ctx("cancel_test"), None, None, None)
        .unwrap();

    // Verify it is registered.
    let coord_storage = get_coord_storage(&setup);
    assert!(coord_storage.get_tx_by_id(txid).unwrap().is_some());

    // Cancel before any dispatch tick.
    coordinator
        .cancel(TypesToMonitor::Transactions(
            vec![txid],
            ctx("cancel_test"),
            None,
        ))
        .unwrap();

    // Tx must no longer be in storage.
    assert!(
        coord_storage.get_tx_by_id(txid).unwrap().is_none(),
        "tx must be gone from storage after cancel"
    );

    // Ticking must not produce any dispatch errors for this tx.
    coordinator.tick().unwrap();

    assert!(coordinator.get_news().unwrap().is_empty());

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// `get_transaction` can query the status of any transaction that the indexer
/// has processed, including transactions confirmed before the coordinator was
/// constructed.
#[test]
fn test_get_transaction_old_tx() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let coordinator = create_coordinator(&setup);

    // Mine 1 block.
    let wallet_addr = setup.bitcoin_client.init_wallet("test_wallet").unwrap();
    setup
        .bitcoin_client
        .mine_blocks_to_address(1, &wallet_addr)
        .unwrap();

    // Use `fund_address` to create a confirmed transaction.
    let (funded_tx, _vout) = setup
        .bitcoin_client
        .fund_address(&wallet_addr, bitcoin::Amount::from_sat(500_000))
        .unwrap();
    let funded_txid = funded_tx.compute_txid();

    // Sync coordinator to the new blocks.
    tick_until_ready(&coordinator).unwrap();

    // Query via coordinator.
    let status = coordinator.get_transaction(funded_txid).unwrap();
    assert!(
        status.confirmations > 0,
        "funded tx must have at least 1 confirmation; got {}",
        status.confirmations
    );

    drop(coordinator);
    setup.end_all().unwrap();
}

/// Full news lifecycle: coordinator news is produced, retrievable, and
/// accurately removed after acknowledgement.
#[test]
fn test_news_ack() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let coordinator = create_coordinator(&setup);
    tick_until_ready(&coordinator).unwrap();

    // Add a UTXO below the minimum to generate InvalidFundingUtxo news.
    coordinator.add_funding(utxo(500)).unwrap(); // min is 10 000

    let news = coordinator.get_news().unwrap();
    assert_eq!(
        news.coordinator_news.len(),
        1,
        "Expected exactly one coordinator news item; got {:?}",
        news.coordinator_news
    );

    let news_item = news.coordinator_news[0].clone();
    assert!(
        matches!(news_item, CoordinatorNews::InvalidFundingUtxo { .. }),
        "Expected InvalidFundingUtxo; got {:?}",
        news_item
    );

    // Acknowledge the news item.
    coordinator
        .ack_news(AckNews::Coordinator(news_item))
        .unwrap();

    // After ack the coordinator news list must be empty.
    let news_after_ack = coordinator.get_news().unwrap();
    assert!(
        news_after_ack.coordinator_news.is_empty(),
        "Coordinator news must be empty after ack; got {:?}",
        news_after_ack.coordinator_news
    );

    drop(coordinator);
    setup.end_all().unwrap();
}

/// Dispatching a transaction that is already in the mempool is treated as a
/// success (`AlreadyKnown`).  The coordinator sets the state to `InMempool`
///  without generating any error news.
#[test]
fn test_dispatch_already_in_mempool() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let coordinator = create_coordinator(&setup);
    tick_until_ready(&coordinator).unwrap();

    let tx = create_signed_tx_to_dispatch(&setup.bitcoin_client).unwrap();
    let txid = tx.compute_txid();

    // Put the tx directly into the mempool before the coordinator dispatches it.
    setup.bitcoin_client.send_transaction(&tx).unwrap();

    // Now register the same tx with the coordinator.
    coordinator
        .dispatch_without_speedup(tx, ctx("already_known"), None, None, None)
        .unwrap();

    // The coordinator tries to broadcast → node returns "already in mempool" →
    // AlreadyKnown → treated as success → InMempool.
    coordinator.tick().unwrap();

    let coord_storage = get_coord_storage(&setup);
    let stored = coord_storage.get_tx_by_id(txid).unwrap().unwrap();
    assert_eq!(
        stored.state,
        TransactionState::InMempool,
        "AlreadyKnown must be treated as a successful dispatch"
    );

    assert!(coordinator.get_news().unwrap().is_empty());

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

// =============================================================================
// ERROR CONDITION TESTS
// =============================================================================

/// Dispatching a structurally invalid (empty) transaction results in a
/// `Fatal` dispatch outcome.  The coordinator marks the tx as `Failed` and
/// generates a `DispatchError` coordinator news item.
#[test]
fn test_dispatch_invalid_empty_tx() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();

    // One retry means the first failure marks the tx Failed immediately.
    let settings = BitcoinSettings {
        coordinator: CoordinatorSettings {
            retry_attempts_sending_tx: 1,
            retry_interval_seconds: 5,
        },
        ..BitcoinSettings::default()
    };
    let coordinator = create_coordinator_with_settings(&setup, settings);
    tick_until_ready(&coordinator).unwrap();

    let tx = dummy_tx();
    let txid = tx.compute_txid();

    coordinator
        .dispatch_without_speedup(tx, ctx("bad_tx"), None, None, None)
        .unwrap();

    // Tick: the coordinator attempts to broadcast the empty tx.
    coordinator.tick().unwrap();

    let coord_storage = get_coord_storage(&setup);
    let stored = coord_storage.get_tx_by_id(txid).unwrap().unwrap();
    assert_eq!(
        stored.state,
        TransactionState::Failed,
        "Invalid tx must be Failed after one dispatch attempt; got {:?}",
        stored.state
    );

    let news = coordinator.get_news().unwrap();
    assert_eq!(
        news.coordinator_news.len(),
        1,
        "Expected exactly one DispatchError news for txid {}; got {:?}",
        txid,
        news.coordinator_news
    );
    assert!(
        matches!(
            &news.coordinator_news[0],
            CoordinatorNews::DispatchError { txid: id, .. } if *id == txid
        ),
        "Expected DispatchError news for txid {}; got {:?}",
        txid,
        news.coordinator_news
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// When one valid and one invalid transaction are registered and the
/// coordinator ticks, they fail independently: the valid one reaches
/// `InMempool` while the invalid one reaches `Failed` with a `DispatchError`
/// news item.  The system remains consistent and the valid tx is unaffected.
#[test]
fn test_valid_and_invalid_tx() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();

    let settings = BitcoinSettings {
        ..BitcoinSettings::default()
    };
    let coordinator = create_coordinator_with_settings(&setup, settings);

    let valid_tx = create_signed_tx_to_dispatch(&setup.bitcoin_client).unwrap();
    let invalid_tx = dummy_tx();
    let valid_txid = valid_tx.compute_txid();
    let invalid_txid = invalid_tx.compute_txid();

    tick_until_ready(&coordinator).unwrap();

    coordinator
        .dispatch_without_speedup(valid_tx, ctx("valid"), None, None, None)
        .unwrap();
    coordinator
        .dispatch_without_speedup(invalid_tx, ctx("invalid"), None, None, None)
        .unwrap();

    coordinator.tick().unwrap();

    let coord_storage = get_coord_storage(&setup);
    assert_eq!(
        coord_storage
            .get_tx_by_id(valid_txid)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::InMempool,
        "valid tx should be InMempool"
    );
    assert_eq!(
        coord_storage
            .get_tx_by_id(invalid_txid)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::Failed,
        "invalid tx should be Failed"
    );

    let news = coordinator.get_news().unwrap();
    assert_eq!(
        news.coordinator_news.len(),
        1,
        "Expected exactly one coordinator news for the invalid tx; got {:?}",
        news.coordinator_news
    );
    assert!(
        matches!(
            &news.coordinator_news[0],
            CoordinatorNews::DispatchError { txid, .. } if *txid == invalid_txid
        ),
        "Expected DispatchError news for the invalid tx; got {:?}",
        news.coordinator_news[0]
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// Cancelling a transaction that is already in the mempool removes it from
/// coordinator storage and cancels monitor tracking.  Subsequent ticks do not
/// produce any errors or spurious news for that txid.
#[test]
fn test_cancel_dispatched_tx() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let coordinator = create_coordinator(&setup);

    let tx = create_signed_tx_to_dispatch(&setup.bitcoin_client).unwrap();
    let txid = tx.compute_txid();

    tick_until_ready(&coordinator).unwrap();

    coordinator
        .dispatch_without_speedup(tx, ctx("cancel_after_dispatch"), None, None, None)
        .unwrap();

    // Dispatch.
    coordinator.tick().unwrap();
    let coord_storage = get_coord_storage(&setup);
    assert_eq!(
        coord_storage.get_tx_by_id(txid).unwrap().unwrap().state,
        TransactionState::InMempool,
        "tx must be InMempool before cancel"
    );

    // Cancel after dispatch.
    coordinator
        .cancel(TypesToMonitor::Transactions(
            vec![txid],
            ctx("cancel_after_dispatch"),
            None,
        ))
        .unwrap();

    assert!(
        coord_storage.get_tx_by_id(txid).unwrap().is_none(),
        "tx must be removed from storage after cancel"
    );

    // Several subsequent ticks must not produce news related to this tx.
    for _ in 0..10 {
        coordinator.tick().unwrap();
    }
    let news = coordinator.get_news().unwrap();
    assert!(
        news.is_empty(),
        "No coordinator news expected after cancelling a dispatched tx; got {:?}",
        news.coordinator_news
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// When multiple coordinator news items are present, `ack_news` removes only
/// the acknowledged item and leaves the rest untouched.
#[test]
fn test_selective_ack() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();

    let settings = BitcoinSettings {
        ..BitcoinSettings::default()
    };
    let coordinator = create_coordinator_with_settings(&setup, settings);
    tick_until_ready(&coordinator).unwrap();

    // Produce one InvalidFundingUtxo news item.
    coordinator.add_funding(utxo(1_000)).unwrap();

    // Produce one DispatchError news item.
    let tx = dummy_tx();
    let txid = tx.compute_txid();
    coordinator
        .dispatch_without_speedup(tx, ctx("multi_news"), None, None, None)
        .unwrap();
    coordinator.tick().unwrap();

    let news = coordinator.get_news().unwrap();
    assert_eq!(
        news.coordinator_news.len(),
        2,
        "Expected 2 coordinator news items (funding + dispatch error); got {:?}",
        news.coordinator_news
    );

    // Identify the DispatchError item.
    let dispatch_err = news
        .coordinator_news
        .iter()
        .find(|n| matches!(n, CoordinatorNews::DispatchError { txid: id, .. } if *id == txid))
        .cloned()
        .expect("DispatchError news must be present");

    // Acknowledge only the DispatchError.
    coordinator
        .ack_news(AckNews::Coordinator(dispatch_err))
        .unwrap();

    // Advance to a new block so unacknowledged items become visible again.
    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();
    coordinator.tick().unwrap();

    // Only the InvalidFundingUtxo must remain.
    let remaining = coordinator.get_news().unwrap().coordinator_news;
    assert_eq!(
        remaining.len(),
        1,
        "One news item must remain after selective ack; got {:?}",
        remaining
    );
    assert!(
        matches!(remaining[0], CoordinatorNews::InvalidFundingUtxo { .. }),
        "Remaining item must be InvalidFundingUtxo; got {:?}",
        remaining[0]
    );

    drop(coordinator);
    setup.end_all().unwrap();
}

/// Registering multiple transactions and then querying storage directly
/// confirms that each transaction's metadata (context, state, fee_info) is
/// persisted correctly and independently.
#[test]
fn test_tx_metadata_persisted() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let coordinator = create_coordinator(&setup);
    tick_until_ready(&coordinator).unwrap();

    // Use a real signed tx so the two txids are distinct.
    let tx1 = create_signed_tx_to_dispatch(&setup.bitcoin_client).unwrap();
    let tx2 = dummy_tx();

    // Compute txids **before** moving the transactions.
    let txid1 = tx1.compute_txid();
    let txid2 = tx2.compute_txid();

    assert_ne!(
        txid1, txid2,
        "tx1 (real) and tx2 (dummy) must have distinct txids"
    );

    coordinator
        .dispatch_without_speedup(tx1, ctx("meta_tx1"), None, Some(2), Some(5))
        .unwrap();
    coordinator
        .dispatch_without_speedup(tx2, ctx("meta_tx2"), None, None, None)
        .unwrap();

    let coord_storage = get_coord_storage(&setup);

    let stored1 = coord_storage.get_tx_by_id(txid1).unwrap().unwrap();
    assert_eq!(stored1.context, ctx("meta_tx1"));
    assert_eq!(stored1.confirmation_trigger, Some(2));
    assert_eq!(stored1.stuck_in_mempool_blocks, Some(5));
    assert_eq!(stored1.state, TransactionState::ToDispatch);

    let stored2 = coord_storage.get_tx_by_id(txid2).unwrap().unwrap();
    assert_eq!(stored2.context, ctx("meta_tx2"));
    assert_eq!(stored2.confirmation_trigger, None);
    assert_eq!(stored2.stuck_in_mempool_blocks, None);
    assert_eq!(stored2.state, TransactionState::ToDispatch);

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// Verifies that the coordinator handles being re-created from existing
/// storage (simulating a restart) without losing previously registered
/// transactions.
#[test]
fn test_coordinator_restart() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();

    let valid_tx = create_signed_tx_to_dispatch(&setup.bitcoin_client).unwrap();
    let txid = valid_tx.compute_txid();

    // ── First coordinator instance ────────────────────────────────────────
    {
        let coordinator_v1 = create_coordinator(&setup);
        tick_until_ready(&coordinator_v1).unwrap();

        coordinator_v1
            .dispatch_without_speedup(valid_tx, ctx("restart_test"), None, None, None)
            .unwrap();

        // Verify the tx is stored as ToDispatch.
        let coord_storage = get_coord_storage(&setup);
        assert_eq!(
            coord_storage.get_tx_by_id(txid).unwrap().unwrap().state,
            TransactionState::ToDispatch
        );
        // Do not tick – the tx was never dispatched.
    }
    // coordinator_v1 dropped here (simulates process crash/restart).

    // ── Second coordinator instance from the same storage ─────────────────
    {
        let coordinator_v2 = create_coordinator(&setup);

        // The tx must still be in storage from the previous instance.
        let coord_storage = get_coord_storage(&setup);
        let recovered = coord_storage.get_tx_by_id(txid).unwrap();
        assert!(recovered.is_some(), "tx must survive a coordinator restart");
        assert_eq!(recovered.unwrap().state, TransactionState::ToDispatch);

        // Dispatch on the new coordinator.
        coordinator_v2.tick().unwrap();

        assert_eq!(
            coord_storage.get_tx_by_id(txid).unwrap().unwrap().state,
            TransactionState::InMempool,
            "tx must be dispatched by the restarted coordinator"
        );
    }

    setup.end_all().unwrap();
}

/// A transaction that stays in the mempool for more than `stuck_in_mempool_blocks`
/// blocks emits a `TransactionStuckInMempool` news item.
#[test]
fn test_stuck_in_mempool_news() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let coordinator = create_coordinator(&setup);
    tick_until_ready(&coordinator).unwrap();

    let tx = create_signed_tx_to_dispatch(&setup.bitcoin_client).unwrap();
    let txid = tx.compute_txid();

    coordinator
        .dispatch_without_speedup(tx, ctx("stuck"), None, None, Some(1))
        .unwrap();
    coordinator.tick().unwrap(); // dispatches → InMempool

    let coord_storage = get_coord_storage(&setup);
    assert_eq!(
        coord_storage.get_tx_by_id(txid).unwrap().unwrap().state,
        TransactionState::InMempool
    );

    // Set broadcast_block_height = 0 so the current chain height always exceeds
    // the stuck threshold of 1 block without needing to mine extra blocks.
    let mut record = coord_storage.get_tx_by_id(txid).unwrap().unwrap();
    record.broadcast_block_height = Some(0);
    coord_storage.update_tx(&record).unwrap();

    coordinator.tick().unwrap();

    let news = coordinator.get_news().unwrap();
    assert!(
        news.coordinator_news.iter().any(|n| {
            matches!(n, CoordinatorNews::TransactionStuckInMempool { txid: id, .. } if *id == txid)
        }),
        "expected TransactionStuckInMempool news; got {:?}",
        news.coordinator_news
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// A stuck transaction can be unblocked by dispatching a CPFP via
/// `dispatch_without_speedup`. The coordinator tracks both independently,
/// dispatches them in topological order, and both reach `Confirmed` once a
/// block is mined.
#[test]
fn test_stuck_in_mempool_cpfp_resolution() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let coordinator = create_coordinator(&setup);
    tick_until_ready(&coordinator).unwrap();

    // Parent (will be stuck) + CPFP spending parent:0.
    let (parent_tx, cpfp_tx) = create_parent_and_child_signed_txs(&setup.bitcoin_client);
    let parent_txid = parent_tx.compute_txid();
    let cpfp_txid = cpfp_tx.compute_txid();

    coordinator
        .dispatch_without_speedup(parent_tx, ctx("stuck_parent"), None, None, Some(1))
        .unwrap();
    coordinator.tick().unwrap(); // dispatches → InMempool

    let coord_storage = get_coord_storage(&setup);
    assert_eq!(
        coord_storage
            .get_tx_by_id(parent_txid)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::InMempool
    );

    // Force stuck: set broadcast_block_height to 0 so the threshold of 1 block
    // is always exceeded without needing to mine extra blocks.
    let mut record = coord_storage.get_tx_by_id(parent_txid).unwrap().unwrap();
    record.broadcast_block_height = Some(0);
    coord_storage.update_tx(&record).unwrap();

    coordinator.tick().unwrap();

    let news = coordinator.get_news().unwrap();
    assert!(
        news.coordinator_news.iter().any(|n| {
            matches!(n, CoordinatorNews::TransactionStuckInMempool { txid: id, .. } if *id == parent_txid)
        }),
        "expected TransactionStuckInMempool before CPFP; got {:?}",
        news.coordinator_news
    );

    // Another tick without the CPFP should not change anything.
    let status = tick_until_state(
        &coordinator,
        &coord_storage,
        parent_txid,
        TransactionState::Confirmed,
        5,
    )
    .unwrap();
    assert!(
        !status,
        "parent should not reach Confirmed without CPFP resolution"
    );

    // Dispatch the CPFP spending parent:0. Both are now ToDispatch / InMempool.
    coordinator
        .dispatch_without_speedup(cpfp_tx, ctx("cpfp"), None, None, None)
        .unwrap();
    coordinator.tick().unwrap(); // dispatches CPFP → InMempool

    assert_eq!(
        coord_storage
            .get_tx_by_id(cpfp_txid)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::InMempool,
        "CPFP must reach InMempool after dispatch"
    );

    // Mine one block: parent + CPFP confirm together.
    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();

    let both_confirmed = tick_until_all_states(
        &coordinator,
        &coord_storage,
        &[parent_txid, cpfp_txid],
        TransactionState::Confirmed,
        5,
    )
    .unwrap();
    assert!(both_confirmed, "both parent and CPFP must reach Confirmed");

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// A transaction in retry state is not dispatched until `retry_interval_seconds`
/// has elapsed since the last retry batch.
#[test]
fn test_retry_dispatches_after_rate_limit() {
    init_trace();
    let retry_interval_seconds = 5u64;

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let settings = BitcoinSettings {
        coordinator: CoordinatorSettings {
            retry_interval_seconds,
            ..CoordinatorSettings::default()
        },
        ..Default::default()
    };
    let coordinator = create_coordinator_with_settings(&setup, settings);
    tick_until_ready(&coordinator).unwrap();

    let coord_storage = get_coord_storage(&setup);

    // --- Prime last_retry_at ---
    // Dispatch tx_prime, mark it as retry, then tick.  Because last_retry_at is
    // None the first retry is always allowed → tx_prime reaches InMempool and
    // last_retry_at is recorded as "now".
    let tx_prime = create_signed_tx_to_dispatch(&setup.bitcoin_client).unwrap();
    let txid_prime = tx_prime.compute_txid();
    coordinator
        .dispatch_without_speedup(tx_prime, ctx("prime"), None, None, None)
        .unwrap();
    coord_storage.mark_as_retry(txid_prime).unwrap();
    coordinator.tick().unwrap();
    assert_eq!(
        coord_storage
            .get_tx_by_id(txid_prime)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::InMempool,
        "tx_prime must be InMempool after the priming tick"
    );

    // --- Subject under test ---
    // tx_subject is placed in retry state right after the priming tick, so
    // the rate-limiter window is active.
    let tx_subject = create_signed_tx_to_dispatch(&setup.bitcoin_client).unwrap();
    let txid_subject = tx_subject.compute_txid();
    coordinator
        .dispatch_without_speedup(tx_subject, ctx("subject"), None, None, None)
        .unwrap();
    coord_storage.mark_as_retry(txid_subject).unwrap();

    // Ticks 1 and 2 must be blocked. The interval has not elapsed yet.
    for tick in 1..=2 {
        coordinator.tick().unwrap();
        assert_eq!(
            coord_storage
                .get_tx_by_id(txid_subject)
                .unwrap()
                .unwrap()
                .state,
            TransactionState::ToDispatch,
            "tx_subject must still be ToDispatch on tick {tick} (rate-limited)"
        );
    }

    // Tick 3: After the interval, must dispatch successfully.
    std::thread::sleep(std::time::Duration::from_secs(retry_interval_seconds + 1));
    coordinator.tick().unwrap();
    assert_eq!(
        coord_storage
            .get_tx_by_id(txid_subject)
            .unwrap()
            .unwrap()
            .state,
        TransactionState::InMempool,
        "tx_subject must be InMempool after the retry interval has elapsed"
    );

    assert!(coordinator.get_news().unwrap().coordinator_news.is_empty());

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// Each time a transaction fails with a retryable error the coordinator calls
/// `mark_as_retry`, which increments `retry_count`.  Once `retry_count + 1`
/// reaches `retry_attempts_sending_tx` the transaction is permanently marked
/// `Failed` and a `DispatchError` news item is stored.
#[test]
fn test_retry_failure() {
    init_trace();
    // Long enough that consecutive ticks are reliably below the rate-limit window.
    let retry_interval_seconds = 8u64;

    // Non-zero min_relay_tx_fee ensures our zero-fee tx is always rejected.
    let setup = TestSetup::new(TestSetupConfig {
        bitcoind_flags: Some(BitcoindFlags {
            min_relay_tx_fee: 0.00002, // 2 sat/vbyte. Zero-fee tx will fail
            ..BitcoindFlags::default()
        }),
        ..TestSetupConfig::default()
    })
    .unwrap();

    let settings = BitcoinSettings {
        coordinator: CoordinatorSettings {
            retry_interval_seconds,
            retry_attempts_sending_tx: 3,
            ..CoordinatorSettings::default()
        },
        ..Default::default()
    };
    let coordinator = create_coordinator_with_settings(&setup, settings);
    tick_until_ready(&coordinator).unwrap();

    let tx = create_zero_fee_tx(&setup.bitcoin_client).unwrap();
    let txid = tx.compute_txid();
    coordinator
        .dispatch_without_speedup(tx, ctx("retry_inc"), None, None, None)
        .unwrap();

    let coord_storage = get_coord_storage(&setup);

    // Tick 1: first dispatch attempt (retry_count = 0, not rate-limited).
    // The tx fails → mark_as_retry → retry_count = 1.
    coordinator.tick().unwrap();
    let stored = coord_storage.get_tx_by_id(txid).unwrap().unwrap();
    assert_eq!(
        stored.retry_count, 1,
        "retry_count must be 1 after first failure"
    );
    assert_eq!(stored.state, TransactionState::ToDispatch);

    // Tick 2 immediately: `last_retry_at` is still None so the first retry is
    // allowed.  The tx fails again → mark_as_retry → retry_count = 2.
    // `last_retry_at` is now set.
    coordinator.tick().unwrap();
    let stored = coord_storage.get_tx_by_id(txid).unwrap().unwrap();
    assert_eq!(
        stored.retry_count, 2,
        "retry_count must be 2 after second failure"
    );
    assert_eq!(stored.state, TransactionState::ToDispatch);

    // Tick 3 immediately: rate-limiter blocks the retry (interval not elapsed).
    coordinator.tick().unwrap();
    let stored = coord_storage.get_tx_by_id(txid).unwrap().unwrap();
    assert_eq!(
        stored.retry_count, 2,
        "retry_count must not change while rate-limited"
    );
    assert_eq!(stored.state, TransactionState::ToDispatch);
    assert!(
        coordinator.get_news().unwrap().coordinator_news.is_empty(),
        "No news should be generated while rate-limited"
    );

    // Wait for the retry interval, then tick.  Third attempt: retry_count + 1 = 3
    // >= retry_attempts_sending_tx (3) → tx is marked Failed + DispatchError news.
    std::thread::sleep(std::time::Duration::from_secs(retry_interval_seconds + 5));
    coordinator.tick().unwrap();
    let stored = coord_storage.get_tx_by_id(txid).unwrap().unwrap();
    assert_eq!(
        stored.state,
        TransactionState::Failed,
        "tx must be Failed after exhausting all retry attempts"
    );

    let news = coordinator.get_news().unwrap();
    assert!(
        news.coordinator_news
            .iter()
            .any(|n| matches!(n, CoordinatorNews::DispatchError { txid: id, .. } if *id == txid)),
        "DispatchError news must be present after retry exhaustion; got {:?}",
        news.coordinator_news
    );

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}
