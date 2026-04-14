mod common;
use common::*;

use bitvmx_bitcoin_rpc::bitcoin_client::BitcoinClientApi;
use bitvmx_transaction_monitor::types::AckMonitorNews;
use bitvmx_transaction_monitor::types::TypesToMonitor;
use rust_bitvmx_bitcoin::{
    config::config::{BitcoinSettings, CoordinatorSettings},
    test_utils::{dummy_tx, init_trace, utxo},
    types::{AckNews, CoordinatorNews, TransactionState},
};
use tracing::info;

// =============================================================================
// Helper: context string for tests
// =============================================================================

fn ctx(label: &str) -> String {
    format!("test_ctx:{}", label)
}

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
/// 1. `dispatch_without_speedup` immediately persists the transaction in storage
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
        .dispatch_without_speedup(tx, ctx("registration"), None, None, 0)
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
        stored.broadcast_block_height > 0,
        "broadcast_block_height must be set after dispatch"
    );

    assert!(coordinator.get_news().unwrap().is_empty());

    drop(coordinator);
    drop(coord_storage);
    setup.end_all().unwrap();
}

/// Full lifecycle from `ToDispatch` → `InMempool` → `Confirmed`.
///
/// Once a valid transaction is dispatched, mining a confirming block and
/// ticking the coordinator must advance the coordinator's state from
/// `InMempool` to `Confirmed`.
/// TODO: when indexer is fixed and state can reach `Finalized`, extend this test to cover that final transition as well.
#[test]
fn test_full_lifecycle() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let coordinator = create_coordinator(&setup);

    let tx = create_signed_tx_to_dispatch(&setup.bitcoin_client).unwrap();
    let txid = tx.compute_txid();
    info!("Registering tx {}", txid);

    tick_until_ready(&coordinator).unwrap();

    coordinator
        .dispatch_without_speedup(tx, ctx("lifecycle"), None, None, 0)
        .unwrap();

    // Dispatch.
    coordinator.tick().unwrap();
    let coord_storage = get_coord_storage(&setup);
    assert_eq!(
        coord_storage.get_tx_by_id(txid).unwrap().unwrap().state,
        TransactionState::InMempool,
        "tx should be InMempool after dispatch"
    );

    // Mine a confirming block.
    mine_blocks(&setup.bitcoin_client, 1, &setup.regtest_wallet).unwrap();

    // Tick until the coordinator sees the confirmation.
    let reached = tick_until_state(
        &coordinator,
        &coord_storage,
        txid,
        TransactionState::Confirmed,
        10,
    )
    .unwrap();

    assert!(
        reached,
        "tx should have transitioned to Confirmed after mining 1 block"
    );

    let final_state = coord_storage.get_tx_by_id(txid).unwrap().unwrap().state;
    assert_eq!(final_state, TransactionState::Confirmed);

    info!(
        "News after confirmation: {:?}",
        coordinator.get_news().unwrap()
    );
    assert!(coordinator.get_news().unwrap().coordinator_news.is_empty()); // Monitor news is expected, because tx confirmation triggers monitor news

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
        .dispatch_without_speedup(tx, ctx("delayed_dispatch"), Some(target_height), None, 0)
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
            .dispatch_without_speedup(tx, ctx(&format!("batch")), None, None, 0)
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

/// Adding a valid funding UTXO (above the minimum threshold) must not
/// generate any coordinator news.
#[test]
fn test_add_valid_funding_utxo() {
    init_trace();

    let setup = TestSetup::new(TestSetupConfig::default()).unwrap();
    let coordinator = create_coordinator(&setup);
    tick_until_ready(&coordinator).unwrap();

    // Default minimum is 10 000 sats; 20 000 is safely above it.
    coordinator.add_funding(utxo(20_000)).unwrap();

    assert!(coordinator.get_news().unwrap().is_empty());

    drop(coordinator);
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
        .dispatch_without_speedup(tx, ctx("cancel_test"), None, None, 0)
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
