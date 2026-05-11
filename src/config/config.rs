use crate::{config::settings::*, errors::BitcoinCoordinatorError};
use bitvmx_bitcoin_rpc::rpc_config::RpcConfig;
use bitvmx_settings::settings::load_config_file;
use bitvmx_transaction_monitor::config::MonitorSettingsConfig;
use key_manager::config::KeyManagerConfig;
use serde::Deserialize;
use storage_backend::storage_config::StorageConfig;

macro_rules! ensure {
    ($cond:expr, $msg:expr) => {
        if !($cond) {
            return Err(BitcoinCoordinatorError::InvalidConfiguration(
                $msg.to_string(),
            ));
        }
    };
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)] // enforce strict field compliance
pub struct Config {
    pub storage: StorageConfig,
    pub rpc: RpcConfig,
    pub key_manager: KeyManagerConfig,

    #[serde(default)]
    pub settings: BitcoinSettings,
}

impl Config {
    pub fn load_config(path: &str) -> Result<Self, BitcoinCoordinatorError> {
        let config = load_config_file::<Self>(Some(path.to_string()))
            .map_err(|e| BitcoinCoordinatorError::InvalidConfiguration(e.to_string()))?;
        config.settings.validate()?;

        Ok(config)
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct BitcoinSettings {
    #[serde(default)]
    pub coordinator: CoordinatorSettings,

    #[serde(default)]
    pub dispatcher: DispatcherSettings,

    #[serde(default)]
    pub fee: FeeSettings,

    #[serde(default)]
    pub speedup: SpeedupSettings,

    #[serde(default)]
    pub funding: FundingSettings,

    #[serde(default)]
    pub storage: CoordinatorStorageSettings,

    #[serde(default)]
    pub monitor: MonitorSettingsConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct CoordinatorSettings {
    pub retry_interval_seconds: u64,
    pub retry_attempts_sending_tx: u32,
}

impl Default for CoordinatorSettings {
    fn default() -> Self {
        Self {
            retry_interval_seconds: DEFAULT_RETRY_INTERVAL_SECONDS,
            retry_attempts_sending_tx: DEFAULT_RETRY_ATTEMPTS_SENDING_TX,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct DispatcherSettings {
    pub max_tx_weight: u64,
}

impl Default for DispatcherSettings {
    fn default() -> Self {
        Self {
            max_tx_weight: DEFAULT_MAX_TX_WEIGHT,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct FeeSettings {
    pub max_feerate_sat_vb: u64,
    pub min_network_fee_rate: u64,
    pub base_fee_multiplier: f64,
}

impl Default for FeeSettings {
    fn default() -> Self {
        Self {
            max_feerate_sat_vb: DEFAULT_MAX_FEERATE_SAT_VB,
            min_network_fee_rate: DEFAULT_MIN_NETWORK_FEE_RATE,
            base_fee_multiplier: DEFAULT_BASE_FEE_MULTIPLIER,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct SpeedupSettings {
    pub max_unconfirmed_speedups: u32,
    pub max_rbf_attempts: u32,
    pub min_blocks_before_resend_speedup: u32,
    pub rbf_fee_multiplier: f64,
    pub bump_fee_percentage: f64,
}

impl Default for SpeedupSettings {
    fn default() -> Self {
        Self {
            max_unconfirmed_speedups: DEFAULT_MAX_UNCONFIRMED_SPEEDUPS,
            max_rbf_attempts: DEFAULT_MAX_RBF_ATTEMPTS,
            min_blocks_before_resend_speedup: DEFAULT_MIN_BLOCKS_BEFORE_RESEND_SPEEDUP,
            rbf_fee_multiplier: DEFAULT_RBF_FEE_MULTIPLIER,
            bump_fee_percentage: DEFAULT_BUMP_FEE_PERCENTAGE,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct FundingSettings {
    pub min_funding_amount_sats: u64,
}

impl Default for FundingSettings {
    fn default() -> Self {
        Self {
            min_funding_amount_sats: DEFAULT_MIN_FUNDING_AMOUNT_SATS,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct CoordinatorStorageSettings {
    pub max_tracking_confirmations: u32,
}

impl Default for CoordinatorStorageSettings {
    fn default() -> Self {
        Self {
            max_tracking_confirmations: DEFAULT_MAX_TRACKING_CONFIRMATIONS,
        }
    }
}

impl BitcoinSettings {
    pub fn validate(&self) -> Result<(), BitcoinCoordinatorError> {
        ensure!(
            self.speedup.max_unconfirmed_speedups > 0
                && self.speedup.max_unconfirmed_speedups <= MAX_LIMIT_UNCONFIRMED_PARENTS,
            "invalid max_unconfirmed_speedups"
        );

        ensure!(
            self.dispatcher.max_tx_weight > 0 && self.dispatcher.max_tx_weight <= MAX_MAX_TX_WEIGHT,
            "invalid max_tx_weight"
        );

        ensure!(
            self.speedup.max_rbf_attempts > 0
                && self.speedup.max_rbf_attempts <= MAX_MAX_RBF_ATTEMPTS,
            "invalid max_rbf_attempts"
        );

        ensure!(
            self.funding.min_funding_amount_sats >= MIN_MIN_FUNDING_AMOUNT_SATS,
            "funding below dust threshold"
        );

        ensure!(
            self.speedup.rbf_fee_multiplier >= 1.0
                && self.speedup.rbf_fee_multiplier <= MAX_RBF_FEE_MULTIPLIER,
            "invalid rbf_fee_multiplier"
        );

        ensure!(
            self.speedup.min_blocks_before_resend_speedup > 0
                && self.speedup.min_blocks_before_resend_speedup
                    <= MAX_MIN_BLOCKS_BEFORE_RESEND_SPEEDUP,
            "invalid min_blocks_before_resend_speedup"
        );

        ensure!(
            self.fee.max_feerate_sat_vb > 0
                && self.fee.max_feerate_sat_vb <= MAX_MAX_FEERATE_SAT_VB,
            "invalid max_feerate_sat_vb"
        );

        ensure!(
            self.fee.base_fee_multiplier > 0.0
                && self.fee.base_fee_multiplier <= MAX_BASE_FEE_MULTIPLIER,
            "invalid base_fee_multiplier"
        );

        ensure!(
            self.speedup.bump_fee_percentage >= 1.0
                && self.speedup.bump_fee_percentage <= MAX_BUMP_FEE_PERCENTAGE,
            "invalid bump_fee_percentage"
        );

        ensure!(
            self.coordinator.retry_interval_seconds > 0
                && self.coordinator.retry_interval_seconds <= MAX_RETRY_INTERVAL_SECONDS,
            "invalid retry_interval_seconds"
        );

        ensure!(
            self.fee.min_network_fee_rate >= 1,
            "min_network_fee_rate must be at least 1"
        );

        ensure!(
            self.fee.max_feerate_sat_vb >= self.fee.min_network_fee_rate,
            "max_feerate_sat_vb must be greater than min_network_fee_rate"
        );
        Ok(())
    }
}
