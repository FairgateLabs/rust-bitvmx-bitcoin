use protocol_builder::types::Utxo;
use std::rc::Rc;
use storage_backend::storage::{KeyValueStore, Storage};
use tracing::warn;

use crate::{
    config::config::FundingSettings, errors::BitcoinCoordinatorError, types::CoordinatorNews,
};

const FUNDING_KEY: &str = "bitcoin_coordinator/funding/utxo";

/// `FundingManager` owns its own storage slice (same underlying [`Storage`]
/// shared with the rest of the coordinator, but under its own key prefix).
/// It does not depend on [`CoordinatorStorage`].
pub struct FundingManager {
    settings: FundingSettings,
    storage: Rc<Storage>,
}

impl FundingManager {
    pub fn new(settings: FundingSettings, storage: Rc<Storage>) -> Self {
        Self { settings, storage }
    }

    /// Validate and persist a new funding UTXO.
    pub fn set_funding(
        &self,
        utxo: Utxo,
    ) -> Result<Option<CoordinatorNews>, BitcoinCoordinatorError> {
        match self.validate(&utxo) {
            Ok(()) => {
                self.storage.set(FUNDING_KEY, &utxo, None)?;
                Ok(None)
            }
            Err(news) => {
                warn!("FundingManager: invalid funding utxo: {:?}", utxo);
                // Clear any stale value so a previously valid UTXO is not
                // accidentally reused after a failed update.
                self.storage.delete(FUNDING_KEY)?;
                Ok(Some(news))
            }
        }
    }

    /// Load the current funding UTXO from storage.
    pub fn get_funding(&self) -> Result<Option<Utxo>, BitcoinCoordinatorError> {
        Ok(self.storage.get(FUNDING_KEY)?)
    }

    /// Remove the funding UTXO from storage.
    pub fn clear_funding(&self) -> Result<(), BitcoinCoordinatorError> {
        self.storage.delete(FUNDING_KEY)?;
        Ok(())
    }

    /// Return `true` when a funding UTXO is currently stored.
    pub fn has_funding(&self) -> Result<bool, BitcoinCoordinatorError> {
        Ok(self.get_funding()?.is_some())
    }

    // -------------------------------------------------------------------------
    // Private helpers
    // -------------------------------------------------------------------------

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
    use crate::{config::config::FundingSettings, helper::StorageTestConfig};
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

    fn make_manager() -> (FundingManager, StorageTestConfig) {
        let config = StorageTestConfig::new();
        let storage = config.get_raw_storage();
        (FundingManager::new(settings(), storage), config)
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
    fn test_set_valid_funding_persists() {
        let (mgr, config) = make_manager();

        let news = mgr.set_funding(utxo(MIN)).unwrap();
        assert!(news.is_none());

        let stored = mgr.get_funding().unwrap();
        assert!(stored.is_some());
        assert_eq!(stored.unwrap().amount, MIN);

        drop(mgr);
        config.remove();
    }

    #[test]
    fn test_set_invalid_funding_below_min_returns_news_and_clears_storage() {
        let (mgr, config) = make_manager();

        // Set a valid UTXO first, then overwrite with an invalid one.
        mgr.set_funding(utxo(MIN)).unwrap();
        let news = mgr.set_funding(utxo(MIN - 1)).unwrap();

        assert!(matches!(
            news,
            Some(CoordinatorNews::InvalidFundingUtxo { .. })
        ));

        // Invalid UTXO must not be stored, and the previous value must be cleared.
        assert!(mgr.get_funding().unwrap().is_none());

        drop(mgr);
        config.remove();
    }

    #[test]
    fn test_get_funding_when_empty() {
        let (mgr, config) = make_manager();

        let stored = mgr.get_funding().unwrap();
        assert!(stored.is_none());

        drop(mgr);
        config.remove();
    }

    #[test]
    fn test_has_funding() {
        let (mgr, config) = make_manager();

        assert!(!mgr.has_funding().unwrap());
        mgr.set_funding(utxo(MIN)).unwrap();
        assert!(mgr.has_funding().unwrap());

        drop(mgr);
        config.remove();
    }

    #[test]
    fn test_clear_funding() {
        let (mgr, config) = make_manager();

        mgr.set_funding(utxo(MIN)).unwrap();
        mgr.clear_funding().unwrap();
        assert!(!mgr.has_funding().unwrap());

        drop(mgr);
        config.remove();
    }

    /// Simulates a coordinator restart: a second `FundingManager` built from
    /// the same storage must see the UTXO set by the first.
    #[test]
    fn test_funding_survives_restart() {
        let config = StorageTestConfig::new();
        let storage = config.get_raw_storage();

        let mgr1 = FundingManager::new(settings(), Rc::clone(&storage));
        mgr1.set_funding(utxo(MIN * 2)).unwrap();
        drop(mgr1);

        let mgr2 = FundingManager::new(settings(), Rc::clone(&storage));
        let stored = mgr2.get_funding().unwrap();
        assert!(stored.is_some());
        assert_eq!(stored.unwrap().amount, MIN * 2);

        drop(mgr2);
        config.remove();
    }
}
