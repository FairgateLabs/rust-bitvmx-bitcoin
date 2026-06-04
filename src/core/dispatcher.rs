use crate::{config::config::DispatcherSettings, types::CoordinatedTx};
use bitcoin::Txid;
use bitvmx_bitcoin_rpc::bitcoin_client::{BitcoinClient, BitcoinClientApi};
use bitvmx_transaction_monitor::monitor::Monitor;
use std::rc::Rc;
use tracing::debug;

/// Typed outcome returned per-transaction by [`Dispatcher::dispatch`].
#[derive(Debug)]
pub enum DispatchOutcome {
    /// Transaction accepted by the node.
    Success,
    /// Node already has the transaction in the mempool.
    AlreadyKnown,
    /// Transaction is already confirmed on-chain (outputs already in UTXO set).
    /// This is definitive: no indexer query needed.
    AlreadyConfirmed,
    /// Transient error (fee/mempool policy, network). Coordinator may retry.
    Retryable(String),
    /// Permanent error (e.g. transaction too heavy, script invalid). Mark as failed.
    Fatal(String),
    /// `bad-txns-inputs-missingorspent`: at least one input is gone or spent from the UTXO set / mempool.
    MissingInput(String),
}

pub struct Dispatcher {
    settings: DispatcherSettings,
    bitcoin_client: Rc<BitcoinClient>,
}

impl Dispatcher {
    pub fn new(settings: DispatcherSettings, bitcoin_client: Rc<BitcoinClient>) -> Self {
        Self {
            settings,
            bitcoin_client,
        }
    }

    /// Group `parents` into batches whose cumulative weight stays within
    /// `max_tx_weight`, returning at most `max_batches` groups.
    pub fn batch_by_weight<'a>(
        &self,
        parents: &'a [CoordinatedTx],
        max_batches: u32,
    ) -> Vec<Vec<&'a CoordinatedTx>> {
        let mut batches: Vec<Vec<&CoordinatedTx>> = Vec::new();
        let mut current_batch: Vec<&CoordinatedTx> = Vec::new();
        let mut current_weight = 0u64;

        for parent in parents {
            if batches.len() as u32 >= max_batches {
                break;
            }
            let weight = parent.tx.weight().to_wu();
            if !current_batch.is_empty() && current_weight + weight > self.settings.max_tx_weight {
                batches.push(current_batch);
                current_batch = Vec::new();
                current_weight = 0;
            }
            current_batch.push(parent);
            current_weight += weight;
        }

        if !current_batch.is_empty() && (batches.len() as u32) < max_batches {
            batches.push(current_batch);
        }

        batches
    }

    /// Broadcast each tx whose input parents are observable in the mempool / chain via the (cached) `monitor.get_tx_status`.
    /// Txs with at least one parent in `NotFound` / `Orphan` state are skipped.
    pub fn dispatch(
        &self,
        txs: Vec<CoordinatedTx>,
        monitor: &Monitor,
    ) -> Vec<(Txid, DispatchOutcome)> {
        let (valid, mut results) = self.validate(txs);
        for tx in valid {
            let txid = tx.txid;
            if !parents_ready(&tx, monitor) {
                debug!(
                    txid = %txid,
                    "dispatcher: deferring — at least one parent is not yet in mempool/chain",
                );
                continue;
            }
            let outcome = match self.bitcoin_client.send_transaction(&tx.tx) {
                Ok(_) => DispatchOutcome::Success,
                Err(e) => classify_error(&e.to_string()),
            };
            results.push((txid, outcome));
        }
        results
    }

    // -------------------------------------------------------------------------
    // Private helpers
    // -------------------------------------------------------------------------

    /// Split `txs` into those that pass the weight limit and those that don't. Oversized
    /// transactions receive a `Fatal` outcome immediately; valid ones are returned.
    fn validate(
        &self,
        txs: Vec<CoordinatedTx>,
    ) -> (Vec<CoordinatedTx>, Vec<(Txid, DispatchOutcome)>) {
        let mut valid = Vec::new();
        let mut failures = Vec::new();
        for tx in txs {
            let weight = tx.tx.weight().to_wu();
            if weight > self.settings.max_tx_weight {
                failures.push((
                    tx.txid,
                    DispatchOutcome::Fatal(format!(
                        "transaction weight {} wu exceeds max {} wu",
                        weight, self.settings.max_tx_weight
                    )),
                ));
            } else {
                valid.push(tx);
            }
        }
        (valid, failures)
    }
}

/// Return `true` when every input of `tx` references a parent that the monitor reports
/// as in mempool / confirmed / finalized. External parents naturally satisfy this.
fn parents_ready(tx: &CoordinatedTx, monitor: &Monitor) -> bool {
    let max_confs = monitor.settings.max_monitoring_confirmations;
    for input in &tx.tx.input {
        let parent_txid = input.previous_output.txid;
        match monitor.get_tx_status(&parent_txid, true) {
            Ok(status) => {
                let ready = status.is_in_mempool()
                    || status.is_confirmed()
                    || status.is_finalized(max_confs);
                if !ready {
                    return false;
                }
            }
            Err(_) => {
                // If the monitor errors on this txid, defer.
                return false;
            }
        }
    }
    true
}

/// Map a raw Bitcoin RPC error message to a [`DispatchOutcome`].
fn classify_error(msg: &str) -> DispatchOutcome {
    if msg.contains("Transaction outputs already in utxo set")
        || msg.contains("already in block chain")
    {
        return DispatchOutcome::AlreadyConfirmed;
    }

    if msg.contains("already in mempool") {
        return DispatchOutcome::AlreadyKnown;
    }

    if msg.contains("missing-or-spent") || msg.contains("missing-inputs") {
        return DispatchOutcome::MissingInput(msg.to_string());
    }

    if msg.contains("mempool full")
        || msg.contains("insufficient priority")
        || msg.contains("min relay fee")
        || msg.contains("mempool min fee not met")
        || msg.contains("too-long-mempool-chain")
        || msg.contains("network")
        || msg.contains("connection")
        || msg.contains("timeout")
    {
        return DispatchOutcome::Retryable(msg.to_string());
    }

    DispatchOutcome::Fatal(msg.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::config::DispatcherSettings,
        test_utils::{normal_coordinated_tx, StorageTestConfig, TestBitcoind},
    };
    use bitcoin::{
        absolute::LockTime, transaction::Version, Amount, OutPoint, ScriptBuf, Sequence,
        Transaction, TxIn, TxOut, Witness,
    };
    use bitvmx_transaction_monitor::types::TypesToMonitor;

    // Create a dummy CoordinatedTx
    fn get_dummy_tx() -> CoordinatedTx {
        normal_coordinated_tx(1)
    }

    fn get_child_tx(parent: &CoordinatedTx) -> CoordinatedTx {
        let parent_txid = parent.txid;
        let mut child = get_dummy_tx();
        child.tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: parent_txid,
                    vout: 0,
                },
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(1),
                script_pubkey: ScriptBuf::new(),
            }],
        };
        child.txid = child.tx.compute_txid();
        child
    }

    fn dispatcher(max_weight: u64) -> (Dispatcher, TestBitcoind) {
        let bitcoind = TestBitcoind::default();
        let client = Rc::new(
            BitcoinClient::new_from_config(&bitcoind.rpc_config)
                .expect("BitcoinClient::new_from_config failed"),
        );
        let d = Dispatcher::new(
            DispatcherSettings {
                max_tx_weight: max_weight,
            },
            client,
        );
        (d, bitcoind)
    }

    fn dispatcher_with_client(max_weight: u64, client: Rc<BitcoinClient>) -> Dispatcher {
        Dispatcher::new(
            DispatcherSettings {
                max_tx_weight: max_weight,
            },
            client,
        )
    }

    #[test]
    fn test_valid_tx_passes_partition() {
        let tx = get_dummy_tx();
        let weight = tx.tx.weight().to_wu();
        let (d, bitcoind) = dispatcher(weight + 100);
        let (valid, failures) = d.validate(vec![tx]);
        assert_eq!(valid.len(), 1);
        assert!(failures.is_empty());

        drop(d);
        bitcoind.stop().unwrap();
    }

    #[test]
    fn test_overweight_tx_is_fatal() {
        let tx = get_dummy_tx();
        let weight = tx.tx.weight().to_wu();
        let (d, bitcoind) = dispatcher(weight - 1);
        let (valid, failures) = d.validate(vec![tx]);
        assert!(valid.is_empty());
        assert_eq!(failures.len(), 1);
        assert!(matches!(failures[0].1, DispatchOutcome::Fatal(_)));

        drop(d);
        bitcoind.stop().unwrap();
    }

    #[test]
    fn test_mixed_txs_partitioned_correctly() {
        let weight = get_dummy_tx().tx.weight().to_wu();
        let (d, bitcoind) = dispatcher(weight); // exactly at limit

        // Slightly under limit → both txs fail validation as overweight.
        let d2 = dispatcher_with_client(weight - 1, Rc::clone(&d.bitcoin_client));
        let (valid, failures) = d2.validate(vec![get_dummy_tx(), get_dummy_tx()]);
        assert!(valid.is_empty());
        assert_eq!(failures.len(), 2);
        drop(d2);

        // Now both fit exactly at the limit.
        let (valid, failures) = d.validate(vec![get_dummy_tx(), get_dummy_tx()]);
        assert_eq!(valid.len(), 2);
        assert!(failures.is_empty());
        drop(d);

        bitcoind.stop().unwrap();
    }

    #[test]
    fn test_batch_by_weight_limits_and_splits() {
        let p1 = normal_coordinated_tx(1);
        let p2 = normal_coordinated_tx(2);
        let p3 = normal_coordinated_tx(3);

        // Weight of an empty tx in wu; used to set max_tx_weight just below 2×.
        let single_weight = p1.tx.weight().to_wu();

        // Case A — weight overflow: two parents each of `single_weight` but
        // max_tx_weight = single_weight + 1 (fits first, overflows on second).
        // Expect: two batches of 1 each (up to max_batches=10).
        let (d, bitcoind) = dispatcher(single_weight + 1);
        let two = vec![p1.clone(), p2.clone()];
        let batches = d.batch_by_weight(&two, 10);
        assert_eq!(batches.len(), 2, "weight overflow must open a new batch");
        assert_eq!(batches[0].len(), 1);
        assert_eq!(batches[1].len(), 1);

        // Case B — max_batches limit: three parents, max_batches = 2.
        // The third parent triggers `batches.len() >= max_batches` → break.
        let three = vec![p1.clone(), p2.clone(), p3.clone()];
        let batches = d.batch_by_weight(&three, 2);
        assert_eq!(batches.len(), 2, "batch count must not exceed max_batches");
        assert_eq!(batches[0].len(), 1);
        assert_eq!(batches[1].len(), 1);

        drop(d);
        bitcoind.stop().unwrap();
    }

    /// A tx with no inputs is trivially ready, so `parents_ready` short-circuits through the loop and returns true.
    #[test]
    fn test_parents_ready_empty_inputs_is_ready() {
        let bitcoind = TestBitcoind::default();
        let storage_cfg = StorageTestConfig::new();
        let monitor = bitcoind.create_monitor(storage_cfg.get_raw_storage());
        while !monitor.is_ready().unwrap() {
            monitor.tick().unwrap();
        }

        let tx = get_dummy_tx();
        assert!(tx.tx.input.is_empty(), "dummy tx must have no inputs");
        assert!(
            parents_ready(&tx, &monitor),
            "parents_ready must return true when there are no inputs to gate on",
        );

        drop(monitor);
        bitcoind.stop().unwrap();
        storage_cfg.remove().unwrap();
    }

    // Test the full flow of a parent and child tx through the dispatcher.
    // Disclaimer: the parent doesn't actually have real funding, so the dispatch will fail.
    #[test]
    fn test_parents_became_ready() {
        let storage_cfg = StorageTestConfig::new();
        let (d, bitcoind) = dispatcher(u64::MAX);
        let monitor = bitcoind.create_monitor(storage_cfg.get_raw_storage());
        while !monitor.is_ready().unwrap() {
            monitor.tick().unwrap();
        }
        let parent = get_dummy_tx();
        let child = get_child_tx(&parent);
        monitor
            .monitor(
                TypesToMonitor::Transactions(
                    vec![parent.txid, child.txid],
                    "test".to_string(),
                    None,
                ),
                true,
            )
            .unwrap();

        // Initially both parent and child are not dispatched, so if we try to dispatch the child, it should be deferred.
        let txids = d.dispatch(vec![child.clone()], &monitor);
        assert!(
            txids.is_empty(),
            "child must be deferred when parent is not yet in mempool/chain",
        );

        // If both are dispatched together, only the parent should be dispatched, and the child should be deferred.
        let txids = d.dispatch(vec![parent.clone(), child.clone()], &monitor);
        assert_eq!(txids.len(), 1);
        assert_eq!(txids[0].0, parent.txid);

        drop(d);
        drop(monitor);
        bitcoind.stop().unwrap();
        storage_cfg.remove().unwrap();
    }
}
