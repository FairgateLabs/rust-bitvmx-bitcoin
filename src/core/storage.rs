use bitcoin::Txid;
use serde::{Deserialize, Serialize};
use std::rc::Rc;
use storage_backend::storage::{KeyValueStore, Storage};

use crate::{
    errors::BitcoinCoordinatorError,
    types::{CoordinatedTx, CoordinatorNews, TransactionState},
};

const TX_PREFIX: &str = "bitcoin_coordinator";

pub struct CoordinatorStorage {
    pub storage: Rc<Storage>,
}

enum StoreKey {
    Tx(Txid),
    News,
}

impl CoordinatorStorage {
    pub fn new(storage: Rc<Storage>) -> Self {
        Self { storage }
    }

    pub fn insert_tx(&self, tx: CoordinatedTx) -> Result<(), BitcoinCoordinatorError> {
        let key = self.get_key(StoreKey::Tx(tx.txid));
        self.storage.set(&key, &tx, None)?;
        Ok(())
    }

    pub fn insert_txs(&self, txs: Vec<CoordinatedTx>) -> Result<(), BitcoinCoordinatorError> {
        for tx in txs {
            self.insert_tx(tx)?;
        }
        Ok(())
    }

    pub fn update_tx(&self, tx: &CoordinatedTx) -> Result<(), BitcoinCoordinatorError> {
        let key = self.get_key(StoreKey::Tx(tx.txid));
        self.storage.set(&key, tx, None)?;
        Ok(())
    }

    pub fn remove_tx(&self, tx_id: Txid) -> Result<(), BitcoinCoordinatorError> {
        let key = self.get_key(StoreKey::Tx(tx_id));
        self.storage.delete(&key)?;
        Ok(())
    }

    pub fn get_tx_by_id(
        &self,
        tx_id: Txid,
    ) -> Result<Option<CoordinatedTx>, BitcoinCoordinatorError> {
        let key = self.get_key(StoreKey::Tx(tx_id));
        Ok(self.storage.get(&key)?)
    }

    /// Get all the txs, but not in insertion order
    pub fn get_all_txs(&self) -> Result<Vec<CoordinatedTx>, BitcoinCoordinatorError> {
        let prefix = self.tx_prefix();

        let entries = self.storage.partial_compare(&prefix)?;

        let mut txs = Vec::with_capacity(entries.len());

        for (_, value) in entries {
            let tx: CoordinatedTx = serde_json::from_str(&value).map_err(|_| {
                //TODO: add fn partial_get<V> in storage
                BitcoinCoordinatorError::StorageBackendError(
                    storage_backend::error::StorageError::SerializationError,
                )
            })?;
            txs.push(tx);
        }

        Ok(txs)
    }

    pub fn get_by_state(
        &self,
        state: TransactionState,
    ) -> Result<Vec<CoordinatedTx>, BitcoinCoordinatorError> {
        let txs = self.get_all_txs()?;

        Ok(txs.into_iter().filter(|tx| tx.state == state).collect())
    }

    pub fn get_active_txs(&self) -> Result<Vec<CoordinatedTx>, BitcoinCoordinatorError> {
        let txs = self.get_all_txs()?;

        Ok(txs
            .into_iter()
            .filter(|tx| {
                matches!(
                    tx.state,
                    TransactionState::ToDispatch
                        | TransactionState::InMempool
                        | TransactionState::Confirmed
                )
            })
            .collect())
    }

    pub fn exists(&self, tx_id: Txid) -> Result<bool, BitcoinCoordinatorError> {
        Ok(self.get_tx_by_id(tx_id)?.is_some())
    }

    pub fn update_tx_state(
        &self,
        tx_id: Txid,
        new_state: TransactionState,
    ) -> Result<(), BitcoinCoordinatorError> {
        let mut tx = match self.get_tx_by_id(tx_id)? {
            Some(tx) => tx,
            None => {
                self.add_news(CoordinatorNews::TxNotFound { txid: tx_id })?;
                return Ok(());
            }
        };

        if !tx.state.can_transition_to(&new_state) {
            self.add_news(CoordinatorNews::InvalidStateTransition {
                txid: tx_id,
                from: tx.state,
                to: new_state,
            })?;
            return Ok(());
        }

        tx.state = new_state;
        self.update_tx(&tx)?;

        Ok(())
    }

    pub fn mark_as_retry(&self, tx_id: Txid) -> Result<(), BitcoinCoordinatorError> {
        let mut tx = match self.get_tx_by_id(tx_id)? {
            Some(tx) => tx,
            None => {
                self.add_news(CoordinatorNews::TxNotFound { txid: tx_id })?;
                return Ok(());
            }
        };

        if !tx.state.can_transition_to(&TransactionState::ToDispatch) {
            self.add_news(CoordinatorNews::InvalidStateTransition {
                txid: tx_id,
                from: tx.state,
                to: TransactionState::ToDispatch,
            })?;
            return Ok(());
        }

        tx.state = TransactionState::ToDispatch;
        tx.retry_count += 1;

        self.update_tx(&tx)?;
        Ok(())
    }

    // ================================
    //  NEWS
    // ================================

    /// Store `news` if an identical item is not already present.
    pub fn add_news(&self, news: CoordinatorNews) -> Result<(), BitcoinCoordinatorError> {
        let key = self.get_key(StoreKey::News);
        let mut all: Vec<CoordinatorNews> = self.storage.get(&key)?.unwrap_or_default();

        if all.contains(&news) {
            return Ok(()); // exact duplicate already stored
        }

        all.push(news);
        self.storage.set(&key, &all, None)?;
        Ok(())
    }

    /// Return all pending news items.
    pub fn get_news(&self) -> Result<Vec<CoordinatorNews>, BitcoinCoordinatorError> {
        let key = self.get_key(StoreKey::News);
        Ok(self.storage.get(&key)?.unwrap_or_default())
    }

    pub fn clear_news(&self) -> Result<(), BitcoinCoordinatorError> {
        let key = self.get_key(StoreKey::News);
        self.storage.delete(&key)?;
        Ok(())
    }

    pub fn ack_news(&self, news: CoordinatorNews) -> Result<(), BitcoinCoordinatorError> {
        let key = self.get_key(StoreKey::News);
        let mut all: Vec<CoordinatorNews> = self.storage.get(&key)?.unwrap_or_default();
        all.retain(|n| n != &news);
        self.storage.set(&key, &all, None)?;
        Ok(())
    }

    // ================================
    // INTERNAL HELPERS
    // ================================

    fn tx_prefix(&self) -> String {
        format!("{TX_PREFIX}/txs/")
    }

    fn get_key(&self, key: StoreKey) -> String {
        let prefix = TX_PREFIX;
        match key {
            StoreKey::Tx(tx_id) => format!("{prefix}/txs/{tx_id}"),
            StoreKey::News => format!("{prefix}/news"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test_utils::StorageTestConfig,
        types::{FeeInfo, TxKind},
    };
    use bitcoin::{transaction::Version, Transaction, Txid};

    fn dummy_tx(txid: Txid, state: TransactionState) -> CoordinatedTx {
        CoordinatedTx {
            txid,
            tx: Transaction {
                version: Version::TWO,
                lock_time: bitcoin::absolute::LockTime::Blocks(
                    bitcoin::absolute::Height::from_consensus(0).unwrap(),
                ),
                input: vec![],
                output: vec![],
            },
            kind: TxKind::Normal,
            state,
            broadcast_block_height: 0,
            target_block_height: 0,
            stuck_in_mempool_blocks: 0,
            confirmation_trigger: 0,
            retry_count: 0,
            fee_info: FeeInfo {
                fee: 1000,
                fee_rate: 1,
                weight: 100,
            },
            context: "test".to_string(),
        }
    }

    fn random_txid() -> Txid {
        use bitcoin::hashes::{sha256d, Hash};
        Txid::from_raw_hash(sha256d::Hash::hash(&rand::random::<[u8; 32]>()))
    }

    #[test]
    fn test_insert_get_remove_tx() {
        let storage_backend = StorageTestConfig::new();
        let storage = CoordinatorStorage::new(storage_backend.get_raw_storage());

        let txid = random_txid();
        let tx = dummy_tx(txid, TransactionState::ToDispatch);

        storage.insert_tx(tx.clone()).unwrap();
        let fetched = storage.get_tx_by_id(txid).unwrap().unwrap();
        assert_eq!(fetched.txid, tx.txid);
        assert_eq!(fetched.state, tx.state);

        storage.remove_tx(txid).unwrap();
        let fetched = storage.get_tx_by_id(txid).unwrap();
        assert!(fetched.is_none());

        drop(storage);
        storage_backend.remove().unwrap();
    }

    #[test]
    fn test_get_all_txs() {
        let storage_backend = StorageTestConfig::new();
        let storage = CoordinatorStorage::new(storage_backend.get_raw_storage());

        let tx1 = dummy_tx(random_txid(), TransactionState::ToDispatch);
        let tx2 = dummy_tx(random_txid(), TransactionState::InMempool);

        storage.insert_txs(vec![tx1.clone(), tx2.clone()]).unwrap();
        let all = storage.get_all_txs().unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.contains(&tx1));
        assert!(all.contains(&tx2));

        drop(storage);
        storage_backend.remove().unwrap();
    }

    #[test]
    fn test_update_tx_state() {
        let storage_backend = StorageTestConfig::new();
        let storage = CoordinatorStorage::new(storage_backend.get_raw_storage());

        let txid = random_txid();
        let tx = dummy_tx(txid, TransactionState::ToDispatch);
        storage.insert_tx(tx).unwrap();

        // Valid: ToDispatch -> InMempool
        storage
            .update_tx_state(txid, TransactionState::InMempool)
            .unwrap();
        let updated = storage.get_tx_by_id(txid).unwrap().unwrap();
        assert_eq!(updated.state, TransactionState::InMempool);
        assert!(storage.get_news().unwrap().is_empty());

        // Valid: InMempool -> Confirmed
        storage
            .update_tx_state(txid, TransactionState::Confirmed)
            .unwrap();
        let updated = storage.get_tx_by_id(txid).unwrap().unwrap();
        assert_eq!(updated.state, TransactionState::Confirmed);
        assert!(storage.get_news().unwrap().is_empty());

        // Invalid: Confirmed -> ToDispatch (not a valid transition)
        storage
            .update_tx_state(txid, TransactionState::ToDispatch)
            .unwrap();
        let news = storage.get_news().unwrap();
        assert_eq!(news.len(), 1);
        assert_eq!(
            news[0],
            CoordinatorNews::InvalidStateTransition {
                from: TransactionState::Confirmed,
                to: TransactionState::ToDispatch,
                txid,
            }
        );

        drop(storage);
        storage_backend.remove().unwrap();
    }

    #[test]
    fn test_crash_recovery_state_transitions() {
        let storage_backend = StorageTestConfig::new();
        let storage = CoordinatorStorage::new(storage_backend.get_raw_storage());

        // InMempool -> Finalized (fast confirmation, skipped Confirmed)
        let txid1 = random_txid();
        storage
            .insert_tx(dummy_tx(txid1, TransactionState::InMempool))
            .unwrap();
        storage
            .update_tx_state(txid1, TransactionState::Finalized)
            .unwrap();
        assert!(storage.get_news().unwrap().is_empty());

        // ToDispatch -> Confirmed (restart after dispatch, tx already on-chain)
        let txid2 = random_txid();
        storage
            .insert_tx(dummy_tx(txid2, TransactionState::ToDispatch))
            .unwrap();
        storage
            .update_tx_state(txid2, TransactionState::Confirmed)
            .unwrap();
        assert!(storage.get_news().unwrap().is_empty());

        // ToDispatch -> Finalized (restart after dispatch, tx already finalized)
        let txid3 = random_txid();
        storage
            .insert_tx(dummy_tx(txid3, TransactionState::ToDispatch))
            .unwrap();
        storage
            .update_tx_state(txid3, TransactionState::Finalized)
            .unwrap();
        assert!(storage.get_news().unwrap().is_empty());

        drop(storage);
        storage_backend.remove().unwrap();
    }

    #[test]
    fn test_update_tx_state_not_found() {
        let storage_backend = StorageTestConfig::new();
        let storage = CoordinatorStorage::new(storage_backend.get_raw_storage());

        let txid = random_txid();

        storage
            .update_tx_state(txid, TransactionState::InMempool)
            .unwrap();

        let news = storage.get_news().unwrap();
        assert_eq!(news.len(), 1);
        assert_eq!(news[0], CoordinatorNews::TxNotFound { txid: txid });

        drop(storage);
        storage_backend.remove().unwrap();
    }

    #[test]
    fn test_mark_as_retry_success() {
        let storage_backend = StorageTestConfig::new();
        let storage = CoordinatorStorage::new(storage_backend.get_raw_storage());

        let txid = random_txid();
        let tx = dummy_tx(txid, TransactionState::InMempool);

        storage.insert_tx(tx).unwrap();

        storage.mark_as_retry(txid).unwrap();

        let updated = storage.get_tx_by_id(txid).unwrap().unwrap();

        assert_eq!(updated.state, TransactionState::ToDispatch);
        assert_eq!(updated.retry_count, 1);

        drop(storage);
        storage_backend.remove().unwrap();
    }

    #[test]
    fn test_add_get_clear_news() {
        let storage_backend = StorageTestConfig::new();
        let storage = CoordinatorStorage::new(storage_backend.get_raw_storage());

        let news_item = CoordinatorNews::TxNotFound {
            txid: random_txid(),
        };

        storage.add_news(news_item.clone()).unwrap();

        let news = storage.get_news().unwrap();
        assert_eq!(news.len(), 1);
        assert_eq!(news[0], news_item);

        storage.clear_news().unwrap();
        assert!(storage.get_news().unwrap().is_empty());

        drop(storage);
        storage_backend.remove().unwrap();
    }

    #[test]
    fn test_clear_news() {
        let storage_backend = StorageTestConfig::new();
        let storage = CoordinatorStorage::new(storage_backend.get_raw_storage());

        storage
            .add_news(CoordinatorNews::TxNotFound {
                txid: random_txid(),
            })
            .unwrap();
        storage.clear_news().unwrap();

        assert!(storage.get_news().unwrap().is_empty());

        drop(storage);
        storage_backend.remove().unwrap();
    }

    #[test]
    fn test_ack_news() {
        let storage_backend = StorageTestConfig::new();
        let storage = CoordinatorStorage::new(storage_backend.get_raw_storage());

        let news_item1 = CoordinatorNews::TxNotFound {
            txid: random_txid(),
        };
        let news_item2 = CoordinatorNews::InvalidStateTransition {
            txid: random_txid(),
            from: TransactionState::InMempool,
            to: TransactionState::Finalized,
        };

        storage.add_news(news_item1.clone()).unwrap();
        storage.add_news(news_item2.clone()).unwrap();

        storage.ack_news(news_item1.clone()).unwrap();

        let news = storage.get_news().unwrap();
        assert_eq!(news.len(), 1);
        assert_eq!(news[0], news_item2);

        drop(storage);
        storage_backend.remove().unwrap();
    }

    // -- add_news dedup -----------------------------------------------------------

    /// Adding the same item multiple times stores it only once.
    #[test]
    fn test_add_news_dedup() {
        let storage_backend = StorageTestConfig::new();
        let storage = CoordinatorStorage::new(storage_backend.get_raw_storage());

        let news = CoordinatorNews::FundingNotAvailable;

        storage.add_news(news.clone()).unwrap();
        storage.add_news(news.clone()).unwrap();
        storage.add_news(news.clone()).unwrap();

        let returned = storage.get_news().unwrap();
        assert_eq!(returned.len(), 1);

        // get_news is idempotent — calling it again returns the same items.
        let returned_again = storage.get_news().unwrap();
        assert_eq!(returned_again.len(), 1);

        drop(storage);
        storage_backend.remove().unwrap();
    }

    /// Two distinct items are both stored and always returned together.
    #[test]
    fn test_add_news_distinct_items_both_stored() {
        let storage_backend = StorageTestConfig::new();
        let storage = CoordinatorStorage::new(storage_backend.get_raw_storage());

        let item1 = CoordinatorNews::FundingNotAvailable;
        let item2 = CoordinatorNews::EstimateFeerateTooHigh {
            estimated_fee_rate: 50,
            max_fee_rate: 10,
        };

        storage.add_news(item1.clone()).unwrap();
        storage.add_news(item2.clone()).unwrap();

        let returned = storage.get_news().unwrap();
        assert_eq!(returned.len(), 2);
        assert!(returned.contains(&item1));
        assert!(returned.contains(&item2));

        drop(storage);
        storage_backend.remove().unwrap();
    }

    /// After ack the item is removed; re-adding it makes it visible again.
    #[test]
    fn test_ack_then_readd_is_visible() {
        let storage_backend = StorageTestConfig::new();
        let storage = CoordinatorStorage::new(storage_backend.get_raw_storage());

        let news = CoordinatorNews::FundingNotAvailable;
        storage.add_news(news.clone()).unwrap();
        assert_eq!(storage.get_news().unwrap().len(), 1);

        storage.ack_news(news.clone()).unwrap();
        assert!(storage.get_news().unwrap().is_empty());

        // Re-add: no longer a duplicate, so it is stored and returned.
        storage.add_news(news.clone()).unwrap();
        assert_eq!(storage.get_news().unwrap().len(), 1);

        drop(storage);
        storage_backend.remove().unwrap();
    }
}
