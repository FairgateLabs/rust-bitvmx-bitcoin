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

    /// Build a `FeeInfo` for a freshly-built speedup whose actual fee is known. The `vsize == 0`
    ///  branch is defensive, although every well-formed `Transaction` has `vsize >0`.
    pub fn fee_info_for_paid_tx(&self, tx: &Transaction, fee_paid: u64) -> FeeInfo {
        let vsize = tx.vsize() as u64;
        FeeInfo {
            fee: fee_paid,
            fee_rate: if vsize == 0 { 0 } else { fee_paid / vsize },
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

    /// Compute the total fee (in sats) a CPFP/RBF speedup must pay. The speedup is responsible for
    /// bringing the package (parents + child) up to `fee_rate` sat/vB. We conservatively assume the
    /// parents contributed nothing: the CPFP overpays by the parents' already-paid fee.
    ///
    /// Returns `(fee, capped)`. `capped == true` means the final fee would have exceeded
    /// `max_feerate_sat_vb * child_vsize` and was clamped down to that limit.
    pub fn compute_speedup_fee(
        &self,
        parent_vsizes: &[usize],
        child_vsize: usize,
        bump_fee: f64,
        fee_rate: u64,
        is_rbf: bool,
        chain_diff_fee: u64,
    ) -> (u64, bool) {
        let parent_vbytes: usize = parent_vsizes.iter().sum();
        let child_total_sats = child_vsize * fee_rate as usize;
        let mut total_fee = (parent_vbytes + child_vsize) * fee_rate as usize;

        // Bitcoin RBF policy: replacement must pay at least the bandwidth cost.
        // (https://github.com/bitcoin/bitcoin/blob/master/doc/policy/mempool-replacements.md?plain=1#L32)
        if is_rbf && total_fee < child_total_sats * 2 {
            total_fee = child_total_sats * 2;
        }

        total_fee += chain_diff_fee as usize;

        let final_fee = (total_fee as f64 * bump_fee).ceil() as u64;
        let cap = self
            .settings
            .max_feerate_sat_vb
            .saturating_mul(child_vsize as u64);
        if final_fee > cap {
            (cap, true)
        } else {
            (final_fee, false)
        }
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

    /// A CPFP must pay for `parent_vsize + child_vsize` bytes at `fee_rate`. The parents' already-paid
    /// fee is intentionally NOT credited: the speedup overpays slightly rather than risk under-paying.
    #[test]
    fn test_compute_speedup_fee_basic_cpfp() {
        let manager = FeeManager::new(settings(1, 10_000));

        // Parent vsize 100, child 50, rate 5, bump 1.0:
        //   total_fee = (100 + 50) * 5 = 750.
        let (fee, capped) = manager.compute_speedup_fee(&[100], 50, 1.0, 5, false, 0);
        assert_eq!(fee, 750);
        assert!(!capped);

        // No parents (boost CPFP-of-CPFP): only the child's vsize counts.
        //   total_fee = 50 * 5 = 250.
        let (fee, _) = manager.compute_speedup_fee(&[], 50, 1.0, 5, false, 0);
        assert_eq!(fee, 250);

        // Multiple parents (batched CPFP):
        //   total_fee = (40 + 60 + 50) * 5 = 750.
        let (fee, _) = manager.compute_speedup_fee(&[40, 60], 50, 1.0, 5, false, 0);
        assert_eq!(fee, 750);
    }

    /// BIP-125 rule 4: an RBF replacement must pay at least
    /// `2 × child_total_sats`. When the natural package fee falls below
    /// that floor, the floor takes over.
    #[test]
    fn test_compute_speedup_fee_rbf_bandwidth_floor() {
        let manager = FeeManager::new(settings(1, 10_000));

        // Natural package fee dominates the floor:
        //   parent_vsize=100, child=50, rate=5 → natural 750, floor 500.
        let (fee, _) = manager.compute_speedup_fee(&[100], 50, 1.0, 5, true, 0);
        assert_eq!(fee, 750);

        // Floor takes over when parents are small relative to the child:
        //   parent_vsize=10, child=50, rate=1 → natural 60, floor 100.
        let (fee, _) = manager.compute_speedup_fee(&[10], 50, 1.0, 1, true, 0);
        assert_eq!(fee, 100, "RBF bandwidth floor takes over the natural fee");
    }

    /// `chain_diff_fee` is added before `bump_fee` multiplies, so both
    /// channels stack as expected.
    #[test]
    fn test_compute_speedup_fee_bump_and_chain_diff() {
        let manager = FeeManager::new(settings(1, 100_000));

        // bump 2.0 on 750 → 1500.
        let (fee, _) = manager.compute_speedup_fee(&[100], 50, 2.0, 5, false, 0);
        assert_eq!(fee, 1500);

        // bump 1.5 → ceil(750 * 1.5) = 1125.
        let (fee, _) = manager.compute_speedup_fee(&[100], 50, 1.5, 5, false, 0);
        assert_eq!(fee, 1125);

        // chain_diff_fee is added pre-multiplier:
        //   (750 + 100) * 1.0 = 850.
        let (fee, _) = manager.compute_speedup_fee(&[100], 50, 1.0, 5, false, 100);
        assert_eq!(fee, 850);

        // chain_diff_fee combined with bump:
        //   (750 + 100) * 2.0 = 1700.
        let (fee, _) = manager.compute_speedup_fee(&[100], 50, 2.0, 5, false, 100);
        assert_eq!(fee, 1700);
    }

    /// `max_feerate_sat_vb * child_vsize` is a hard ceiling. When the computed fee exceeds it,
    /// the result is clamped and `capped` is returned `true`.
    #[test]
    fn test_compute_speedup_fee_caps_at_max() {
        // Cap = 2 × 50 = 100 sats. Unclamped fee = 750 → clamped to 100.
        let manager = FeeManager::new(settings(1, 2));
        let (fee, capped) = manager.compute_speedup_fee(&[100], 50, 1.0, 5, false, 0);
        assert_eq!(fee, 100, "fee must be clamped to max * child_vsize");
        assert!(capped, "cap flag must be set when clamping occurs");

        // Cap = 5 × 50 = 250. Unclamped 750 → clamped to 250.
        let manager_5 = FeeManager::new(settings(1, 5));
        let (fee, capped) = manager_5.compute_speedup_fee(&[100], 50, 1.0, 5, false, 0);
        assert_eq!(fee, 250);
        assert!(capped);

        // Cap = 15 × 50 = 750. Unclamped 750 hits the cap exactly — not flagged.
        let manager_15 = FeeManager::new(settings(1, 15));
        let (fee, capped) = manager_15.compute_speedup_fee(&[100], 50, 1.0, 5, false, 0);
        assert_eq!(fee, 750);
        assert!(!capped, "cap flag must be clear when final <= cap");
    }
}
