use crate::config::config::DispatcherSettings;
use bitcoin::{Transaction, Txid};
use bitvmx_bitcoin_rpc::bitcoin_client::BitcoinClientApi;
use bitvmx_transaction_monitor::monitor::Monitor;

/// Typed outcome returned per-transaction by [`Dispatcher::dispatch`].
#[derive(Debug)]
pub enum DispatchOutcome {
    /// Transaction accepted by the node.
    Success,
    /// Node already knows the transaction (mempool or confirmed). Treat as success.
    AlreadyKnown,
    /// Transient error (fee/mempool policy, network). Coordinator may retry.
    Retryable(String),
    /// Permanent error (e.g. transaction too heavy, script invalid). Mark as failed.
    Fatal(String),
}

pub struct Dispatcher {
    settings: DispatcherSettings,
}

impl Dispatcher {
    pub fn new(settings: DispatcherSettings) -> Self {
        Self { settings }
    }

    /// Broadcast `txs` to the Bitcoin node via `monitor`.
    pub fn dispatch(
        &self,
        monitor: &Monitor,
        txs: Vec<Transaction>,
    ) -> Vec<(Txid, DispatchOutcome)> {
        let (valid_txs, mut results) = self.validate_and_partition(txs);
        let batches = self.build_batches(valid_txs);

        for batch in batches {
            for tx in batch {
                let txid = tx.compute_txid();
                let outcome = match monitor.indexer.bitcoin_client.send_transaction(&tx) {
                    Ok(_) => DispatchOutcome::Success,
                    Err(e) => classify_error(&e.to_string()),
                };
                results.push((txid, outcome));
            }
        }

        results
    }

    // -------------------------------------------------------------------------
    // Private helpers
    // -------------------------------------------------------------------------

    /// Split `txs` into those that pass the weight limit and those that don't.
    /// Oversized transactions receive a `Fatal` outcome immediately; valid ones
    /// are returned for batching.
    fn validate_and_partition(
        &self,
        txs: Vec<Transaction>,
    ) -> (Vec<Transaction>, Vec<(Txid, DispatchOutcome)>) {
        let mut valid = Vec::new();
        let mut failures = Vec::new();

        for tx in txs {
            let weight = tx.weight().to_wu();
            if weight > self.settings.max_tx_weight {
                failures.push((
                    tx.compute_txid(),
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

    /// Group `txs` (already weight-validated) into batches whose cumulative
    /// weight stays within `max_tx_weight`.
    fn build_batches(&self, txs: Vec<Transaction>) -> Vec<Vec<Transaction>> {
        let mut batches = Vec::new();
        let mut current_batch = Vec::new();
        let mut current_weight = 0u64;

        for tx in txs {
            let weight = tx.weight().to_wu();

            if current_weight + weight > self.settings.max_tx_weight {
                batches.push(current_batch);
                current_batch = Vec::new();
                current_weight = 0;
            }

            current_weight += weight;
            current_batch.push(tx);
        }

        if !current_batch.is_empty() {
            batches.push(current_batch);
        }

        batches
    }
}

/// Map a raw Bitcoin RPC error message to a [`DispatchOutcome`].
fn classify_error(msg: &str) -> DispatchOutcome {
    if msg.contains("already in mempool") || msg.contains("Transaction outputs already in utxo set")
    {
        return DispatchOutcome::AlreadyKnown;
    }

    if msg.contains("mempool full")
        || msg.contains("insufficient priority")
        || msg.contains("min relay fee")
        || msg.contains("mempool min fee not met")
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
    use crate::config::config::DispatcherSettings;
    use bitcoin::{absolute::LockTime, transaction::Version, Transaction};

    fn empty_tx() -> Transaction {
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![],
            output: vec![],
        }
    }

    fn dispatcher(max_weight: u64) -> Dispatcher {
        Dispatcher::new(DispatcherSettings {
            max_tx_weight: max_weight,
        })
    }

    // -- validate_and_partition -----------------------------------------------

    #[test]
    fn test_valid_tx_passes_partition() {
        let tx = empty_tx();
        let weight = tx.weight().to_wu();
        let d = dispatcher(weight + 100);
        let (valid, failures) = d.validate_and_partition(vec![tx]);
        assert_eq!(valid.len(), 1);
        assert!(failures.is_empty());
    }

    #[test]
    fn test_overweight_tx_is_fatal() {
        let tx = empty_tx();
        let weight = tx.weight().to_wu();
        let d = dispatcher(weight - 1);
        let (valid, failures) = d.validate_and_partition(vec![tx]);
        assert!(valid.is_empty());
        assert_eq!(failures.len(), 1);
        assert!(matches!(failures[0].1, DispatchOutcome::Fatal(_)));
    }

    #[test]
    fn test_mixed_txs_partitioned_correctly() {
        let tx = empty_tx();
        let weight = tx.weight().to_wu();
        let d = dispatcher(weight); // exactly at limit

        let valid_tx = empty_tx();
        let heavy_tx = empty_tx(); // same weight, limit is exactly weight so this equals max - fine

        let d2 = dispatcher(weight - 1);
        let (valid, failures) = d2.validate_and_partition(vec![valid_tx, heavy_tx]);
        assert!(valid.is_empty());
        assert_eq!(failures.len(), 2);

        // Now both fit exactly at the limit
        let tx1 = empty_tx();
        let tx2 = empty_tx();
        let (valid, failures) = d.validate_and_partition(vec![tx1, tx2]);
        assert_eq!(valid.len(), 2);
        assert!(failures.is_empty());
    }

    // -- build_batches --------------------------------------------------------

    #[test]
    fn test_build_batches_single_tx() {
        let tx = empty_tx();
        let weight = tx.weight().to_wu();
        let d = dispatcher(weight * 10);
        let batches = d.build_batches(vec![tx]);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 1);
    }

    #[test]
    fn test_build_batches_splits_when_overweight() {
        let tx1 = empty_tx();
        let tx2 = empty_tx();
        let weight = tx1.weight().to_wu();
        // Max fits exactly one tx per batch
        let d = dispatcher(weight);
        let batches = d.build_batches(vec![tx1, tx2]);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 1);
        assert_eq!(batches[1].len(), 1);
    }

    #[test]
    fn test_build_batches_empty_input() {
        let d = dispatcher(400_000);
        let batches = d.build_batches(vec![]);
        assert!(batches.is_empty());
    }
}
