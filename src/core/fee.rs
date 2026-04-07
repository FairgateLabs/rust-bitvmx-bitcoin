use crate::types::CoordinatorNews;
use crate::{
    config::config::FeeSettings,
    errors::BitcoinCoordinatorError,
    types::{CoordinatedTx, FeeInfo, TransactionState},
};
use bitcoin::Txid;
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
        let fee = tx.tx.vsize() as u64 * network_fee_rate;

        FeeInfo {
            fee,
            fee_rate: network_fee_rate,
            weight: tx.tx.weight().to_wu() as u64,
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
