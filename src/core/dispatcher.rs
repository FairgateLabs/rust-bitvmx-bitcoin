use crate::{
    config::config::DispatcherSettings,
    errors::BitcoinCoordinatorError,
    types::{CoordinatedTx, TxKind},
};
use bitcoin::Txid;
use bitvmx_bitcoin_rpc::bitcoin_client::{BitcoinClient, BitcoinClientApi};
use bitvmx_transaction_monitor::monitor::Monitor;
use protocol_builder::types::output::MAX_DUST_LIMIT;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use tracing::debug;

pub trait DispatcherStorage {
    fn is_tx_known(&self, txid: &Txid) -> Result<bool, BitcoinCoordinatorError>;
}

/// Per-label counters for everything a single coordinator (one operator) actually broadcasts, protocol
/// transactions and speedups alike. Vbytes are tracked instead of fees because vbytes are feerate independent
#[derive(Debug, Clone, Default)]
pub struct BroadcastStats {
    /// Keyed by label. Label is `<kind>|<context>`, where kind is normal, needs_speedup, or speedup.
    pub by_label: BTreeMap<String, BroadcastEntry>,
}

#[derive(Debug, Clone, Default)]
pub struct BroadcastEntry {
    /// Number of successful broadcasts (an RBF replacement counts once, so does the tx it replaces).
    pub count: u64,
    pub vbytes: u64,
    pub dust_sats: u64,
}

impl BroadcastStats {
    fn record(&mut self, label: String, vbytes: u64, dust_sats: u64) {
        let entry = self.by_label.entry(label).or_default();
        entry.count += 1;
        entry.vbytes += vbytes;
        entry.dust_sats += dust_sats;
    }

    /// Sum across every label.
    pub fn totals(&self) -> BroadcastEntry {
        let mut total = BroadcastEntry::default();
        for entry in self.by_label.values() {
            total.count += entry.count;
            total.vbytes += entry.vbytes;
            total.dust_sats += entry.dust_sats;
        }
        total
    }
}

/// Label a broadcast tx by its kind and its client-supplied context, so protocol dust and speedup dust stay
/// distinguishable in the tally. Speedups may carry an empty context, which is fine, the kind still separates them.
fn stats_label(tx: &CoordinatedTx) -> String {
    let kind = match &tx.kind {
        TxKind::Normal => "normal",
        TxKind::NeedsSpeedup(_) => "needs_speedup",
        TxKind::Speedup(_) => "speedup",
    };
    format!("{kind}|{}", tx.context)
}

/// Gross dust of a broadcast tx: the sats in every output valued at or below the coordinator dust limit.
fn dust_sats(tx: &CoordinatedTx) -> u64 {
    tx.tx
        .output
        .iter()
        .map(|o| o.value.to_sat())
        .filter(|sats| *sats <= MAX_DUST_LIMIT)
        .sum()
}

/// Raw per-transaction outcome returned by [`Dispatcher::dispatch`].
///
/// The dispatcher does not interpret node error strings. A failed broadcast is reported
/// verbatim as [`DispatchOutcome::DispatchError`]; the engine classifies it authoritatively
/// by probing node state over RPC (see `EngineContext::handle_dispatch_result`).
#[derive(Debug)]
pub enum DispatchOutcome {
    /// Transaction accepted by the node.
    Success,
    /// Pre-send validation failure (e.g. transaction weight exceeds the configured max).
    Fatal(String),
    /// Broadcast returned an error. The raw node message is carried unparsed.
    DispatchError(String),
}

pub struct Dispatcher {
    settings: DispatcherSettings,
    bitcoin_client: Rc<BitcoinClient>,
    storage: Rc<dyn DispatcherStorage>,
    stats: RefCell<BroadcastStats>,
}

impl Dispatcher {
    pub fn new(
        settings: DispatcherSettings,
        bitcoin_client: Rc<BitcoinClient>,
        storage: Rc<dyn DispatcherStorage>,
    ) -> Self {
        Self {
            settings,
            bitcoin_client,
            storage,
            stats: RefCell::new(BroadcastStats::default()),
        }
    }

    /// Snapshot of everything this dispatcher has broadcast so far, per label. Cheap clone, safe to call anytime.
    pub fn broadcast_stats(&self) -> BroadcastStats {
        self.stats.borrow().clone()
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

    /// Broadcast each tx whose coordinator-tracked input parents are in mempool / chain.
    /// Parents the coordinator does not track are assumed ready and the broadcast proceeds;
    /// bitcoind validates on receive.
    pub fn dispatch(
        &self,
        txs: Vec<CoordinatedTx>,
        monitor: &Monitor,
    ) -> Result<Vec<(Txid, DispatchOutcome)>, BitcoinCoordinatorError> {
        let (valid, mut results) = self.validate(txs);
        for tx in valid {
            let txid = tx.txid;
            if !self.parents_ready(&tx, monitor)? {
                debug!(
                    txid = %txid,
                    "dispatcher: deferring — at least one tracked parent is not yet in mempool/chain",
                );
                continue;
            }
            let outcome = match self.bitcoin_client.send_transaction(&tx.tx) {
                Ok(_) => {
                    // Count only accepted broadcasts, so the tally reflects what actually reached the node.
                    self.stats.borrow_mut().record(
                        stats_label(&tx),
                        tx.tx.vsize() as u64,
                        dust_sats(&tx),
                    );
                    DispatchOutcome::Success
                }
                Err(e) => DispatchOutcome::DispatchError(e.to_string()),
            };
            results.push((txid, outcome));
        }
        Ok(results)
    }

    /// For each input of `tx`: if the parent txid is coordinator-tracked, require it to be
    /// in mempool / confirmed / finalized; otherwise (external) assume it is on chain.
    fn parents_ready(
        &self,
        tx: &CoordinatedTx,
        monitor: &Monitor,
    ) -> Result<bool, BitcoinCoordinatorError> {
        let max_confs = monitor.settings.max_monitoring_confirmations;
        for input in &tx.tx.input {
            let parent_txid = input.previous_output.txid;
            if !self.storage.is_tx_known(&parent_txid)? {
                continue;
            }
            let status = monitor
                .get_tx_status(&parent_txid, true)
                .map_err(|e| BitcoinCoordinatorError::Internal(e.to_string()))?;
            let ready =
                status.is_in_mempool() || status.is_confirmed() || status.is_finalized(max_confs);
            if !ready {
                return Ok(false);
            }
        }
        Ok(true)
    }

    // -------------------------------------------------------------------------
    // Private helpers
    // -------------------------------------------------------------------------

    /// Split `txs` into those that pass structural pre-send checks and those that don't.
    /// Deterministically-invalid transactions receive a `Fatal` outcome immediately. Valid
    /// ones are returned. Two checks, both decidable from the tx alone:
    ///   - oversized (weight exceeds `max_tx_weight`);
    ///   - zero inputs, since a tx with no inputs can never be valid.
    fn validate(
        &self,
        txs: Vec<CoordinatedTx>,
    ) -> (Vec<CoordinatedTx>, Vec<(Txid, DispatchOutcome)>) {
        let mut valid = Vec::new();
        let mut failures = Vec::new();
        for tx in txs {
            if tx.tx.input.is_empty() {
                failures.push((
                    tx.txid,
                    DispatchOutcome::Fatal("transaction has no inputs".to_string()),
                ));
                continue;
            }
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

    /// A dummy `CoordinatedTx` carrying a single input, so it passes the structural validity check in `validate`.
    fn tx_with_input(seed: u8) -> CoordinatedTx {
        let mut tx = get_dummy_tx();
        let base_txid = tx.txid;
        tx.tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: base_txid,
                    vout: seed as u32,
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
        tx.txid = tx.tx.compute_txid();
        tx
    }

    struct AllUnknown;
    impl DispatcherStorage for AllUnknown {
        fn is_tx_known(&self, _txid: &Txid) -> Result<bool, BitcoinCoordinatorError> {
            Ok(false)
        }
    }
    struct AllKnown;
    impl DispatcherStorage for AllKnown {
        fn is_tx_known(&self, _txid: &Txid) -> Result<bool, BitcoinCoordinatorError> {
            Ok(true)
        }
    }

    fn dispatcher(max_weight: u64) -> (Dispatcher, TestBitcoind) {
        let bitcoind = TestBitcoind::default();
        let client = Rc::new(
            BitcoinClient::new_from_config(&bitcoind.rpc_config)
                .expect("BitcoinClient::new_from_config failed"),
        );
        let storage: Rc<dyn DispatcherStorage> = Rc::new(AllUnknown);
        let d = Dispatcher::new(
            DispatcherSettings {
                max_tx_weight: max_weight,
            },
            client,
            storage,
        );
        (d, bitcoind)
    }

    fn dispatcher_with_client(max_weight: u64, client: Rc<BitcoinClient>) -> Dispatcher {
        let storage: Rc<dyn DispatcherStorage> = Rc::new(AllUnknown);
        Dispatcher::new(
            DispatcherSettings {
                max_tx_weight: max_weight,
            },
            client,
            storage,
        )
    }

    #[test]
    fn test_valid_tx_passes_partition() {
        let tx = tx_with_input(1);
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
        let weight = tx_with_input(1).tx.weight().to_wu();
        let (d, bitcoind) = dispatcher(weight); // exactly at limit

        // Slightly under limit → both txs fail validation as overweight.
        let d2 = dispatcher_with_client(weight - 1, Rc::clone(&d.bitcoin_client));
        let (valid, failures) = d2.validate(vec![tx_with_input(1), tx_with_input(2)]);
        assert!(valid.is_empty());
        assert_eq!(failures.len(), 2);
        drop(d2);

        // Now both fit exactly at the limit.
        let (valid, failures) = d.validate(vec![tx_with_input(1), tx_with_input(2)]);
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

        // Case A, weight overflow: two parents each of `single_weight` but max_tx_weight = single_weight + 1
        // (fits the first, overflows on the second). Expect two batches of 1 each (up to max_batches=10).
        let (d, bitcoind) = dispatcher(single_weight + 1);
        let two = vec![p1.clone(), p2.clone()];
        let batches = d.batch_by_weight(&two, 10);
        assert_eq!(batches.len(), 2, "weight overflow must open a new batch");
        assert_eq!(batches[0].len(), 1);
        assert_eq!(batches[1].len(), 1);

        // Case B, max_batches limit: three parents, max_batches = 2.
        // The third parent triggers `batches.len() >= max_batches` and breaks.
        let three = vec![p1.clone(), p2.clone(), p3.clone()];
        let batches = d.batch_by_weight(&three, 2);
        assert_eq!(batches.len(), 2, "batch count must not exceed max_batches");
        assert_eq!(batches[0].len(), 1);
        assert_eq!(batches[1].len(), 1);

        drop(d);
        bitcoind.stop().unwrap();
    }

    /// A tx with no inputs is trivially ready and the dispatcher proceeds to send.
    #[test]
    fn test_parents_ready_empty_inputs_is_ready() {
        let (d, bitcoind) = dispatcher(u64::MAX);
        let storage_cfg = StorageTestConfig::new();
        let monitor = bitcoind.create_monitor(storage_cfg.get_raw_storage());

        let tx = get_dummy_tx();
        assert!(tx.tx.input.is_empty(), "dummy tx must have no inputs");
        assert!(
            d.parents_ready(&tx, &monitor).unwrap(),
            "parents_ready must return true when there are no inputs to gate on",
        );

        drop(d);
        drop(monitor);
        bitcoind.stop().unwrap();
        storage_cfg.remove().unwrap();
    }

    /// A coordinator-tracked parent that the monitor has not yet observed makes the gate
    /// defer the child. An untracked (external) parent passes the gate.
    #[test]
    fn test_tracked_parent_defers_child_external_passes() {
        let storage_cfg = StorageTestConfig::new();
        let bitcoind = TestBitcoind::default();
        let client = Rc::new(
            BitcoinClient::new_from_config(&bitcoind.rpc_config)
                .expect("BitcoinClient::new_from_config failed"),
        );
        let monitor = bitcoind.create_monitor(storage_cfg.get_raw_storage());

        let parent = get_dummy_tx();
        let child = get_child_tx(&parent);

        // (a) Tracked parent + monitor knows nothing → child deferred.
        let tracked: Rc<dyn DispatcherStorage> = Rc::new(AllKnown);
        let d_tracked = Dispatcher::new(
            DispatcherSettings {
                max_tx_weight: u64::MAX,
            },
            Rc::clone(&client),
            tracked,
        );
        let txids = d_tracked.dispatch(vec![child.clone()], &monitor).unwrap();
        assert!(
            txids.is_empty(),
            "child must be deferred when the tracked parent is not in mempool/chain",
        );

        // (b) Untracked parent (treated as external) → child dispatched (send fails because
        // the dummy tx has no real funding, so the result carries a DispatchError outcome).
        let untracked: Rc<dyn DispatcherStorage> = Rc::new(AllUnknown);
        let d_untracked = Dispatcher::new(
            DispatcherSettings {
                max_tx_weight: u64::MAX,
            },
            client,
            untracked,
        );
        let txids = d_untracked.dispatch(vec![child.clone()], &monitor).unwrap();
        assert_eq!(txids.len(), 1, "external parent must let the gate pass");
        assert_eq!(txids[0].0, child.txid);

        drop(d_tracked);
        drop(d_untracked);
        drop(monitor);
        bitcoind.stop().unwrap();
        storage_cfg.remove().unwrap();
    }
}
