use crate::types::CoordinatorNews;
use crate::{
    config::config::FeeSettings,
    errors::BitcoinCoordinatorError,
    types::{CoordinatedTx, FeeInfo},
};
use bitcoin::Transaction;
use bitvmx_transaction_monitor::monitor::Monitor;
use tracing::warn;

/// Bitcoin's default minimum relay fee rate (sat/vB). Any transaction that reached the mempool
/// must have paid at least this much.
const MIN_RELAY_FEE_RATE: u64 = 1;

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

    /// Returns the current network fee rate, clamped to `max_feerate_sat_vb` and with a floor of
    /// `min_safe_fee_rate`.
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

    /// Boost-rate floor: every boost (RBF and CPFP-of-CPFP) must pay strictly more per vbyte than its predecessor.
    /// For RBF this satisfies BIP-125 rule 6; for CPFP-of-CPFP it prevents a lower-rate child from dragging the
    /// package's effective rate down Returns `max(network_rate, predecessor_rate + 1)`.
    pub fn boost_fee_rate(network_rate: u64, predecessor_rate: u64) -> u64 {
        network_rate.max(predecessor_rate.saturating_add(1))
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
    /// bringing the package (parents + child) up to `fee_rate` sat/vB. Parents are credited with
    /// `MIN_RELAY_FEE_RATE` sat/vB (the minimum any mempool transaction must have paid).
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
        let parent_already_paid = parent_vbytes * MIN_RELAY_FEE_RATE as usize;
        let mut total_fee =
            ((parent_vbytes + child_vsize) * fee_rate as usize).saturating_sub(parent_already_paid);

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
    fn test_compute_fee() {
        let manager = FeeManager::new(settings(1, 1000));
        let tx = cpfp_coordinated_tx(1, 5);
        let via_wrapper = manager.compute_fee(&tx, 10);
        let direct = manager.compute_fee_for_tx(&tx.tx, 10);
        assert_eq!(via_wrapper.fee, direct.fee);
        assert_eq!(via_wrapper.fee_rate, direct.fee_rate);
        assert_eq!(via_wrapper.weight, direct.weight);
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

    /// When `min_safe_fee_rate` exceeds `max_feerate_sat_vb`, the network rate
    /// is clamped down to the cap and `EstimateFeerateTooHigh` news is emitted.
    /// This is just to ensure the manager behaves predictably even with a misconfigured fee range.
    #[test]
    fn test_get_network_fee_rate_clamps_above_max() {
        let manager = FeeManager::new(FeeSettings {
            min_safe_fee_rate: 20,
            max_feerate_sat_vb: 10,
            base_fee_multiplier: 1.0,
        });
        let storage = StorageTestConfig::new();
        let bitcoind = TestBitcoind::default();
        let monitor = bitcoind.create_monitor(storage.get_raw_storage());

        let (fee_rate, news) = manager.get_network_fee_rate(&monitor).unwrap();
        assert_eq!(
            fee_rate, 10,
            "fee rate must be clamped to max_feerate_sat_vb"
        );
        assert!(
            matches!(
                news,
                Some(CoordinatorNews::EstimateFeerateTooHigh {
                    max_fee_rate: 10,
                    ..
                })
            ),
            "EstimateFeerateTooHigh must be emitted when clamping; got {:?}",
            news
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

    /// Parents are credited with `MIN_RELAY_FEE_RATE` sat/vB; the child pays only the remainder.
    #[test]
    fn test_compute_speedup_fee_basic_cpfp() {
        let manager = FeeManager::new(settings(1, 10_000));

        // Parent vsize 100, child 50, rate 5, min_relay 1, bump 1.0:
        //   package_needed = (100+50)*5 = 750; parent_credit = 100*1 = 100; fee = 650.
        let (fee, capped) = manager.compute_speedup_fee(&[100], 50, 1.0, 5, false, 0);
        assert_eq!(fee, 650);
        assert!(!capped);

        // No parents: no credit, only child vsize counts.
        //   fee = 50 * 5 = 250.
        let (fee, _) = manager.compute_speedup_fee(&[], 50, 1.0, 5, false, 0);
        assert_eq!(fee, 250);

        // Multiple parents (batched CPFP): parent_credit = (40+60)*1 = 100; fee = 650.
        let (fee, _) = manager.compute_speedup_fee(&[40, 60], 50, 1.0, 5, false, 0);
        assert_eq!(fee, 650);
    }

    /// BIP-125 rule 4: an RBF replacement must pay at least
    /// `2 × child_total_sats`. When the natural package fee falls below
    /// that floor, the floor takes over.
    #[test]
    fn test_compute_speedup_fee_rbf_bandwidth_floor() {
        let manager = FeeManager::new(settings(1, 10_000));

        // Natural package fee dominates the floor:
        //   parent_vsize=100, child=50, rate=5 → package 750, parent_credit 100, natural 650, floor 500.
        let (fee, _) = manager.compute_speedup_fee(&[100], 50, 1.0, 5, true, 0);
        assert_eq!(fee, 650);

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

        // bump 2.0 on 650 → 1300.
        let (fee, _) = manager.compute_speedup_fee(&[100], 50, 2.0, 5, false, 0);
        assert_eq!(fee, 1300);

        // bump 1.5 → ceil(650 * 1.5) = 975.
        let (fee, _) = manager.compute_speedup_fee(&[100], 50, 1.5, 5, false, 0);
        assert_eq!(fee, 975);

        // chain_diff_fee is added pre-multiplier:
        //   (650 + 100) * 1.0 = 750.
        let (fee, _) = manager.compute_speedup_fee(&[100], 50, 1.0, 5, false, 100);
        assert_eq!(fee, 750);

        // chain_diff_fee combined with bump:
        //   (650 + 100) * 2.0 = 1500.
        let (fee, _) = manager.compute_speedup_fee(&[100], 50, 2.0, 5, false, 100);
        assert_eq!(fee, 1500);
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

        // Cap = 15 × 50 = 750. Unclamped 650 is below the cap — not flagged.
        let manager_15 = FeeManager::new(settings(1, 15));
        let (fee, capped) = manager_15.compute_speedup_fee(&[100], 50, 1.0, 5, false, 0);
        assert_eq!(fee, 650);
        assert!(!capped, "cap flag must be clear when final <= cap");
    }

    /// `boost_fee_rate` floors at `predecessor + 1`. When the network rate is already
    /// higher, the network rate wins. Saturating arithmetic protects against u64::MAX.
    #[test]
    fn test_boost_fee_rate_floor() {
        assert_eq!(
            FeeManager::boost_fee_rate(5, 10),
            11,
            "floor wins when network below"
        );
        assert_eq!(
            FeeManager::boost_fee_rate(15, 10),
            15,
            "network wins when above floor"
        );
        assert_eq!(
            FeeManager::boost_fee_rate(11, 10),
            11,
            "equal-to-floor passes through"
        );
        assert_eq!(
            FeeManager::boost_fee_rate(0, 0),
            1,
            "predecessor 0 still floors at 1"
        );
        assert_eq!(
            FeeManager::boost_fee_rate(5, u64::MAX),
            u64::MAX,
            "saturating add prevents overflow",
        );
    }
}
