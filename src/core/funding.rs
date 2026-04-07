use protocol_builder::types::Utxo;
use tracing::warn;

use crate::{config::config::FundingSettings, types::CoordinatorNews};

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

    pub fn set_funding(&mut self, utxo: Utxo) -> Option<CoordinatorNews> {
        match self.validate(&utxo) {
            Ok(()) => {
                self.current_utxo = Some(utxo);
                None
            }
            Err(news) => {
                warn!("FundingManager: invalid funding utxo: {:?}", utxo);
                self.current_utxo = None;
                Some(news)
            }
        }
    }

    pub fn get_funding(&self) -> (Option<Utxo>, Option<CoordinatorNews>) {
        match &self.current_utxo {
            None => (None, Some(CoordinatorNews::FundingNotAvailable)),

            Some(utxo) => match self.validate(utxo) {
                Ok(()) => (Some(utxo.clone()), None),

                Err(news) => {
                    warn!("FundingManager: stored utxo became invalid: {:?}", utxo);
                    (None, Some(news))
                }
            },
        }
    }

    pub fn consume(&mut self, new_change: Utxo) -> Option<CoordinatorNews> {
        match self.validate(&new_change) {
            Ok(()) => {
                self.current_utxo = Some(new_change);
                None
            }
            Err(news) => {
                warn!("FundingManager: new change utxo invalid: {:?}", new_change);
                self.current_utxo = None;
                Some(news)
            }
        }
    }

    pub fn clear(&mut self) {
        self.current_utxo = None;
    }

    pub fn has_funding(&self) -> bool {
        self.current_utxo
            .as_ref()
            .map(|u| self.validate(u).is_ok())
            .unwrap_or(false)
    }

    fn validate(&self, utxo: &Utxo) -> Result<(), CoordinatorNews> {
        if utxo.amount < self.settings.min_funding_amount_sats {
            Err(CoordinatorNews::InvalidFundingUtxo {
                amount: utxo.amount,
                min_required: self.settings.min_funding_amount_sats,
            })
        } else {
            Ok(())
        }
    }
}
