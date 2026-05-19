use crate::types::CoordinatorNews;
use crate::{
    config::config::FeeSettings,
    errors::BitcoinCoordinatorError,
    types::{CoordinatedTx, FeeInfo},
};
use bitcoin::Transaction;
use bitvmx_transaction_monitor::monitor::Monitor;
use tracing::warn;

pub struct FeeManager {
    pub settings: FeeSettings,
}

impl FeeManager {
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
        // `min_safe_fee_rate` doubles as the fallback when bitcoind's
        // estimate is unavailable and as a hard lower clamp on the rate.
        let mut network_fee_rate = monitor
            .get_estimated_fee_rate()
            .ok()
            .filter(|rate| *rate >= self.settings.min_safe_fee_rate)
            .unwrap_or(self.settings.min_safe_fee_rate);

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

    /// Returns the base fee multiplier (used as the first `bump_fee` for a new speedup).
    pub fn base_fee_multiplier(&self) -> f64 {
        self.settings.base_fee_multiplier
    }

    /// Compute the extra fee needed to bring all `unconfirmed_speedups` up to
    /// `new_fee_rate`, along with the total virtual size of that chain.
    pub fn chain_fee_diff(
        &self,
        new_fee_rate: u64,
        unconfirmed_speedups: &[CoordinatedTx],
    ) -> (u64, usize) {
        let last_fee_rate_used = match unconfirmed_speedups.last() {
            // All previous speedups in the chain are assumed to have used the same fee rate
            // (the last one's fee_rate is representative).
            Some(tx) => tx.fee_info.fee_rate,
            None => return (0, 0),
        };

        let mut fee_diff = 0u64;
        let mut chain_vsize = 0usize;

        for tx in unconfirmed_speedups {
            let vsize = tx.tx.vsize();
            let rate_diff = new_fee_rate.saturating_sub(last_fee_rate_used);
            fee_diff += vsize as u64 * rate_diff;
            chain_vsize += vsize;
        }

        (fee_diff, chain_vsize)
    }

    /// Compute the total fee (in sats) required for a CPFP/RBF speedup transaction.
    ///
    /// `parent_entries` is a slice of `(output_amount_sats, parent_vsize)` pairs,
    /// one per parent transaction being included in this speedup.
    pub fn compute_speedup_fee(
        &self,
        parent_entries: &[(u64, usize)],
        child_vsize: usize,
        bump_fee: f64,
        fee_rate: u64,
        is_rbf: bool,
        chain_diff_fee: u64,
        chain_vsize: usize,
    ) -> u64 {
        // Minimum relay fee that each parent already paid (1 sat/vB).
        let min_relay_fee_rate: usize = 1; //ASK: why 1 sat/vB? Cant assume a constant value for this

        let mut parent_amount_outputs: usize = 0;
        let mut parent_vbytes: usize = 0;

        for (amount, vsize) in parent_entries {
            parent_amount_outputs += *amount as usize;
            parent_vbytes += vsize;
        }

        let parent_total_sats = parent_vbytes * fee_rate as usize;
        let child_total_sats = child_vsize * fee_rate as usize;
        let total_sats = parent_total_sats + child_total_sats;

        let mut total_fee = total_sats
            .saturating_sub(parent_amount_outputs)
            .saturating_sub(parent_vbytes * min_relay_fee_rate);

        // Bitcoin RBF policy: replacement must pay at least the bandwidth cost.
        // (https://github.com/bitcoin/bitcoin/blob/master/doc/policy/mempool-replacements.md?plain=1#L32)
        if is_rbf && total_fee < child_total_sats * 2 {
            total_fee = child_total_sats * 2;
        }

        total_fee += chain_diff_fee as usize;

        // If we're bumping above the base multiplier, add chain vsize as extra incentive. //ASK: why? is this necessary?
        if chain_vsize > 0 && bump_fee > self.settings.base_fee_multiplier {
            total_fee += chain_vsize * min_relay_fee_rate;
        }

        (total_fee as f64 * bump_fee).ceil() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::config::FeeSettings,
        test_utils::{cpfp_coordinated_tx, dummy_tx, StorageTestConfig, TestBitcoind},
    };

    fn settings(min_safe: u64, max: u64) -> FeeSettings {
        FeeSettings {
            max_feerate_sat_vb: max,
            base_fee_multiplier: 1.0,
            min_safe_fee_rate: min_safe,
        }
    }

    #[test]
    fn test_compute_fee_for_tx() {
        let manager = FeeManager::new(settings(1, 1000));
        let tx = dummy_tx();
        let vsize = tx.vsize() as u64;
        let fee_info = manager.compute_fee_for_tx(&tx, 10);
        assert_eq!(fee_info.fee, vsize * 10);
        assert_eq!(fee_info.fee_rate, 10);
        assert_eq!(fee_info.weight, tx.weight().to_wu() as u64);
    }

    #[test]
    fn test_get_network_fee_rate_below_max() {
        let manager = FeeManager::new(settings(10, 100));
        let storage = StorageTestConfig::new();
        let bitcoind = TestBitcoind::default();
        let monitor = bitcoind.create_monitor(storage.get_raw_storage());

        let (fee_rate, news) = manager.get_network_fee_rate(&monitor).unwrap();
        assert!(fee_rate <= 100);
        assert!(news.is_none());

        drop(monitor);
        storage.remove().unwrap();
        bitcoind.stop().unwrap();
    }

    /// `get_network_fee_rate` lifts a low network estimate (or the
    /// fallback when no estimate is available) up to `min_safe_fee_rate`.
    #[test]
    fn test_get_network_fee_rate_honors_min_safe_floor() {
        let manager = FeeManager::new(FeeSettings {
            max_feerate_sat_vb: 1000,
            base_fee_multiplier: 1.0,
            min_safe_fee_rate: 25,
        });
        let storage = StorageTestConfig::new();
        let bitcoind = TestBitcoind::default();
        let monitor = bitcoind.create_monitor(storage.get_raw_storage());

        let (fee_rate, _) = manager.get_network_fee_rate(&monitor).unwrap();
        assert!(
            fee_rate >= 25,
            "get_network_fee_rate must clamp up to min_safe_fee_rate; got {}",
            fee_rate
        );

        drop(monitor);
        storage.remove().unwrap();
        bitcoind.stop().unwrap();
    }

    #[test]
    fn test_chain_fee_diff() {
        let manager = FeeManager::new(settings(1, 1000));

        // Empty input returns (0, 0).
        assert_eq!(manager.chain_fee_diff(10, &[]), (0, 0));

        let tx = cpfp_coordinated_tx(1, 10);
        let tx_vsize = tx.tx.vsize();

        // Same rate: 0 fee diff, correct chain vsize.
        let (diff, vsize) = manager.chain_fee_diff(10, &[tx.clone()]);
        assert_eq!(diff, 0);
        assert_eq!(vsize, tx_vsize);

        // Rate increase 5 to 10: diff = vsize * (10 - 5).
        let tx2 = cpfp_coordinated_tx(2, 5);
        let (diff, chain_vsize) = manager.chain_fee_diff(10, &[tx2]);
        assert_eq!(diff, tx_vsize as u64 * (10 - 5));
        assert_eq!(chain_vsize, tx_vsize);

        // Two txs at old rate: cumulative diff and vsize.
        let tx3 = cpfp_coordinated_tx(3, 5);
        let tx4 = cpfp_coordinated_tx(4, 5); // Both are expected to have the same fee rate
        let (diff, chain_vsize) = manager.chain_fee_diff(10, &[tx3, tx4]);
        assert_eq!(diff, 2 * tx_vsize as u64 * (10 - 5));
        assert_eq!(chain_vsize, 2 * tx_vsize);
    }

    #[test]
    fn test_compute_speedup_fee() {
        let manager = FeeManager::new(FeeSettings {
            max_feerate_sat_vb: 1000,
            base_fee_multiplier: 1.0,
            min_safe_fee_rate: 1,
        });

        // Basic CPFP: parent 100 vB / 500 sat output; child 50 vB; rate 5; bump 1.0.
        // parent_total=500, child_total=250, total=750; fee = 750-500-100 = 150.
        let fee = manager.compute_speedup_fee(&[(500, 100)], 50, 1.0, 5, false, 0, 0);
        assert_eq!(fee, 150);

        // RBF bandwidth policy: total_fee(150) < child_total*2(500) -> floor lifted to 500.
        let fee = manager.compute_speedup_fee(&[(500, 100)], 50, 1.0, 5, true, 0, 0);
        assert_eq!(fee, 500);

        // Bump multiplier 1.5: ceil(150 * 1.5) = 225
        let fee = manager.compute_speedup_fee(&[(500, 100)], 50, 1.5, 5, false, 0, 0);
        assert_eq!(fee, 225);
    }
}
