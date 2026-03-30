use protocol_builder::types::Utxo;

use crate::config::config::FundingSettings;

pub struct FundingManager {
    settings: FundingSettings,
    current_utxo: Option<Utxo>,
}

impl FundingManager {
    pub fn new(settings: FundingSettings) -> Self {
        Self {
            settings,
            current_utxo: None,
        }
    }
}
