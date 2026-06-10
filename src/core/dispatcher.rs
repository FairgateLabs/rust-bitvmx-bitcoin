use crate::{
    config::config::DispatcherSettings, errors::BitcoinCoordinatorError, types::CoordinatedTx,
};
use bitcoin::Txid;
use bitvmx_bitcoin_rpc::bitcoin_client::{BitcoinClient, BitcoinClientApi};
use bitvmx_transaction_monitor::monitor::Monitor;
use std::rc::Rc;
use tracing::debug;

pub trait DispatcherStorage {
    fn is_tx_known(&self, txid: &Txid) -> Result<bool, BitcoinCoordinatorError>;
}

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
    storage: Rc<dyn DispatcherStorage>,
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
                Ok(_) => DispatchOutcome::Success,
                Err(e) => classify_error(&e.to_string()),
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

    // Bitcoind reports the policy code `bad-txns-inputs-missingorspent`. Also accept the older `missing-inputs` form.
    if msg.contains("missingorspent") || msg.contains("missing-inputs") {
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
        // the dummy tx has no real funding, so the result carries a Fatal/MissingInput outcome).
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
