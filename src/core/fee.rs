use crate::types::CoordinatorNews;
use crate::{
    config::config::FeeSettings,
    errors::BitcoinCoordinatorError,
    types::{CoordinatedTx, FeeInfo},
};
use bitcoin::Transaction;
use bitvmx_transaction_monitor::monitor::Monitor;
use tracing::warn;

pub struct FeeEngine {
    settings: FeeSettings,
}

impl FeeEngine {
    pub fn new(settings: FeeSettings) -> Self {
        Self { settings }
    }

    pub fn compute_fee(&self, tx: &CoordinatedTx, network_fee_rate: u64) -> FeeInfo {
        self.compute_fee_for_tx(&tx.tx, network_fee_rate)
    }

    pub fn compute_fee_for_tx(&self, tx: &Transaction, fee_rate: u64) -> FeeInfo {
        let fee = tx.vsize() as u64 * fee_rate;
        FeeInfo {
            fee,
            fee_rate,
            weight: tx.weight().to_wu() as u64,
        }
    }

    pub fn get_network_fee_rate(
        &self,
        monitor: &Monitor,
    ) -> Result<(u64, Option<CoordinatorNews>), BitcoinCoordinatorError> {
        let mut network_fee_rate = match monitor.get_estimated_fee_rate() {
            Ok(rate) => rate,
            Err(_) => self.settings.min_network_fee_rate,
        };

        let mut news = None;

        if network_fee_rate > self.settings.max_feerate_sat_vb {
            news = Some(CoordinatorNews::EstimateFeerateTooHigh {
                estimated_fee_rate: network_fee_rate,
                max_fee_rate: self.settings.max_feerate_sat_vb,
            });
            warn!("Network fee rate clamped to maximum value");
            network_fee_rate = self.settings.max_feerate_sat_vb;
        }

        Ok((network_fee_rate, news))
    }

    // pub fn get_diff_fee_for_unconfirmed_chain(
    //     &self,
    //     new_network_fee_rate: u64,
    //     chain: &[CoordinatedTx],
    // ) -> Result<(u64, usize), BitcoinCoordinatorError> {
    //     if chain.is_empty() {
    //         return Ok((0, 0));
    //     }

    //     // Assumes all previous speedups used the same fee rate
    //     let last_fee_rate_used = chain.last().unwrap().fee_info.fee_rate;

    //     let mut fee_chain_difference = 0; // total missing fee to bring chain to current fee rate
    //     let mut chain_vsize = 0; // total virtual size of the unconfirmed chain

    //     for tx in chain {
    //         let fee_rate_to_pay = new_network_fee_rate.saturating_sub(last_fee_rate_used);
    //         let vsize = tx.tx.vsize();
    //         let expected_fee = vsize as u64 * fee_rate_to_pay;
    //         fee_chain_difference += expected_fee;
    //         chain_vsize += vsize;
    //     }

    //     Ok((fee_chain_difference, chain_vsize))
    // }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::config::FeeSettings,
        test_utils::{dummy_tx, StorageTestConfig, TestBitcoind},
    };

    fn settings(min: u64, max: u64) -> FeeSettings {
        FeeSettings {
            min_network_fee_rate: min,
            max_feerate_sat_vb: max,
            base_fee_multiplier: 1.0,
        }
    }

    #[test]
    fn test_compute_fee_for_tx() {
        let engine = FeeEngine::new(settings(1, 1000));
        let tx = dummy_tx();
        let vsize = tx.vsize() as u64;
        let fee_info = engine.compute_fee_for_tx(&tx, 10);
        assert_eq!(fee_info.fee, vsize * 10);
        assert_eq!(fee_info.fee_rate, 10);
        assert_eq!(fee_info.weight, tx.weight().to_wu() as u64);
    }

    #[test]
    fn test_get_network_fee_rate() {
        let engine = FeeEngine::new(settings(10, 100));
        let storage = StorageTestConfig::new();
        let bitcoind = TestBitcoind::default();
        let monitor = bitcoind.create_monitor(storage.get_raw_storage());

        let (fee_rate, news) = engine.get_network_fee_rate(&monitor).unwrap();
        assert!(fee_rate <= 100);
        assert!(news.is_none());

        drop(monitor);
        storage.remove().unwrap();
        bitcoind.stop().unwrap();
    }
}
