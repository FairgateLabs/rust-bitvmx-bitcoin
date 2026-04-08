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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::config::FundingSettings;
    use bitcoin::hashes::{sha256d, Hash};
    use bitcoin::PublicKey;
    use bitcoin::Txid;
    use std::str::FromStr;

    const MIN: u64 = 10_000;

    fn settings() -> FundingSettings {
        FundingSettings {
            min_funding_amount_sats: MIN,
        }
    }

    fn dummy_pubkey() -> PublicKey {
        PublicKey::from_str("0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798")
            .unwrap()
    }

    fn utxo(amount: u64) -> Utxo {
        let txid = Txid::from_raw_hash(sha256d::Hash::hash(amount.to_le_bytes().as_ref()));
        Utxo::new(txid, 0, amount, &dummy_pubkey())
    }

    #[test]
    fn test_set_valid_funding() {
        let mut mgr = FundingManager::new(settings());
        let news = mgr.set_funding(utxo(MIN));
        assert!(news.is_none());
        assert!(mgr.has_funding());
    }

    #[test]
    fn test_set_invalid_funding_below_min() {
        let mut mgr = FundingManager::new(settings());
        let news = mgr.set_funding(utxo(MIN - 1));
        assert!(matches!(
            news,
            Some(CoordinatorNews::InvalidFundingUtxo { .. })
        ));
        assert!(!mgr.has_funding());
    }

    #[test]
    fn test_get_funding_when_empty() {
        let mgr = FundingManager::new(settings());
        let (utxo, news) = mgr.get_funding();
        assert!(utxo.is_none());
        assert_eq!(news, Some(CoordinatorNews::FundingNotAvailable));
    }

    #[test]
    fn test_get_funding_valid() {
        let mut mgr = FundingManager::new(settings());
        mgr.set_funding(utxo(MIN));
        let (u, news) = mgr.get_funding();
        assert!(u.is_some());
        assert!(news.is_none());
    }

    #[test]
    fn test_consume_updates_utxo() {
        let mut mgr = FundingManager::new(settings());
        mgr.set_funding(utxo(MIN * 2));
        let news = mgr.consume(utxo(MIN));
        assert!(news.is_none());
        assert!(mgr.has_funding());
        let (u, _) = mgr.get_funding();
        assert_eq!(u.unwrap().amount, MIN);
    }

    #[test]
    fn test_consume_invalid_clears_funding() {
        let mut mgr = FundingManager::new(settings());
        mgr.set_funding(utxo(MIN * 2));
        let news = mgr.consume(utxo(MIN - 1));
        assert!(matches!(
            news,
            Some(CoordinatorNews::InvalidFundingUtxo { .. })
        ));
        assert!(!mgr.has_funding());
    }

    #[test]
    fn test_clear_removes_funding() {
        let mut mgr = FundingManager::new(settings());
        mgr.set_funding(utxo(MIN));
        mgr.clear();
        assert!(!mgr.has_funding());
    }
}
