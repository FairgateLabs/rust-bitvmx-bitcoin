use crate::{
    config::config::DispatcherSettings,
    types::{BitcoinBroadcastErrorKind, CoordinatorNews},
};
use bitcoin::{Transaction, Txid};
use bitvmx_bitcoin_rpc::bitcoin_client::BitcoinClientApi;
use bitvmx_transaction_monitor::monitor::Monitor;

pub struct Dispatcher {
    settings: DispatcherSettings,
}

impl Dispatcher {
    pub fn new(settings: DispatcherSettings) -> Self {
        Self { settings }
    }
}

impl Dispatcher {
    pub fn dispatch(
        &self,
        monitor: &Monitor,
        txs: Vec<Transaction>,
    ) -> Vec<(Txid, Result<(), CoordinatorNews>)> {
        let batches = self.build_batches(txs);

        let mut results = Vec::new();

        for batch in batches {
            for tx in batch {
                let txid = tx.compute_txid();

                match monitor.indexer.bitcoin_client.send_transaction(&tx) {
                    Ok(_) => {
                        results.push((txid, Ok(())));
                    }

                    Err(e) => {
                        results.push((
                            txid,
                            Err(CoordinatorNews::BitcoinClientError {
                                tx_id: tx.compute_txid(),
                                error: BitcoinBroadcastErrorKind::from_error_message(
                                    &e.to_string(),
                                ),
                            }),
                        ));
                    }
                }
            }
        }

        results
    }

    fn build_batches(&self, txs: Vec<Transaction>) -> Vec<Vec<Transaction>> {
        let mut batches = Vec::new();
        let mut current_batch = Vec::new();
        let mut current_weight = 0;

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
