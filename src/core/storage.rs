use crate::{
    config::config::CoordinatorStorageSettings,
    errors::BitcoinCoordinatorError,
    types::{CoordinatedTx, CoordinatorNews, TransactionState, TxKind},
};
use bitcoin::Txid;
use bitvmx_bitcoin_rpc::types::BlockHeight;
use std::rc::Rc;
use storage_backend::storage::{KeyValueStore, Storage};

const TX_PREFIX: &str = "bitcoin_coordinator";

pub struct CoordinatorStorage {
    pub storage: Rc<Storage>,
    settings: CoordinatorStorageSettings,
}

enum StoreKey {
    /// Individual coordinated transaction record (Normal / NeedsSpeedup / Speedup).
    Tx(Txid),
    /// Ordered list of coordinator and monitor news items awaiting acknowledgement.
    News,
    /// Ordered list of Speedup-kind transaction ids (CPFPs / RBFs), insertion order.
    SpeedupList,
    /// Set of NeedsSpeedup parent txids that still require a CPFP to be built.
    /// Added on dispatch_with_speedup; removed when the covering CPFP is dispatched.
    PendingSpeedupParents,
}

impl CoordinatorStorage {
    pub fn new(storage: Rc<Storage>, settings: CoordinatorStorageSettings) -> Self {
        Self { storage, settings }
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
        self.storage.remove(&key, None)?;
        self.remove_speedup_from_list(tx_id)?;
        Ok(())
    }

    pub fn get_tx_by_id(
        &self,
        tx_id: Txid,
    ) -> Result<Option<CoordinatedTx>, BitcoinCoordinatorError> {
        let key = self.get_key(StoreKey::Tx(tx_id));
        Ok(self.storage.get(&key, None)?)
    }

    /// Get all the txs, but not in insertion order
    pub fn get_all_txs(&self) -> Result<Vec<CoordinatedTx>, BitcoinCoordinatorError> {
        let prefix = self.tx_prefix();

        let entries = self.storage.partial_compare(&prefix, None)?;

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

    fn update_tx_state_impl(
        &self,
        tx_id: Txid,
        new_state: TransactionState,
        block_height: Option<BlockHeight>,
    ) -> Result<(), BitcoinCoordinatorError> {
        let is_terminal = matches!(
            new_state,
            TransactionState::Finalized | TransactionState::Failed
        );

        if is_terminal && block_height.is_none() {
            return Err(BitcoinCoordinatorError::Internal(
                "settle_tx must be used for terminal states (Finalized/Failed)".to_string(),
            ));
        }

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

        if is_terminal {
            tx.settled_block_height = block_height;
        }

        tx.state = new_state;
        self.update_tx(&tx)?;

        Ok(())
    }

    /// Transition a tx to a non-terminal state (`ToDispatch`, `InMempool`, `Confirmed`).
    /// Returns an error if called with a terminal state; use `settle_tx` instead.
    pub fn update_tx_state(
        &self,
        tx_id: Txid,
        new_state: TransactionState,
    ) -> Result<(), BitcoinCoordinatorError> {
        self.update_tx_state_impl(tx_id, new_state, None)
    }

    /// Transition a tx to a terminal state (`Finalized` or `Failed`) and record
    /// the block height at which it settled.
    /// Returns an error if called with a non-terminal state; use `update_tx_state` instead.
    pub fn settle_tx(
        &self,
        tx_id: Txid,
        new_state: TransactionState,
        block_height: BlockHeight,
    ) -> Result<(), BitcoinCoordinatorError> {
        if !matches!(
            new_state,
            TransactionState::Finalized | TransactionState::Failed
        ) {
            return Err(BitcoinCoordinatorError::Internal(
                "settle_tx must only be called with Finalized or Failed".to_string(),
            ));
        }
        self.update_tx_state_impl(tx_id, new_state, Some(block_height))
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

    /// Return all transactions that have reached a terminal state (`Finalized` or `Failed`).
    pub fn get_settled_txs(&self) -> Result<Vec<CoordinatedTx>, BitcoinCoordinatorError> {
        let txs = self.get_all_txs()?;
        Ok(txs
            .into_iter()
            .filter(|tx| {
                matches!(
                    tx.state,
                    TransactionState::Finalized | TransactionState::Failed
                )
            })
            .collect())
    }

    /// Remove transactions that have been in a terminal state for at least `max_tracking_confirmations` blocks, emitting
    /// a `TransactionEvicted` news item for each one. `NeedsSpeedup` parents are protected from eviction while they are
    /// still in the pending-speedup-parents set (no CPFP built yet).
    pub fn evict_stale_txs(
        &self,
        current_height: BlockHeight,
    ) -> Result<(), BitcoinCoordinatorError> {
        let settled = self.get_settled_txs()?;
        let psp: Vec<Txid> = {
            let key = self.get_key(StoreKey::PendingSpeedupParents);
            self.storage.get(&key, None)?.unwrap_or_default()
        };
        for tx in settled {
            if let Some(settled_height) = tx.settled_block_height {
                if current_height.saturating_sub(settled_height)
                    >= self.settings.max_tracking_confirmations
                {
                    if matches!(tx.kind, TxKind::NeedsSpeedup(_)) && psp.contains(&tx.txid) {
                        // CPFP not yet built, or a covering CPFP is still in flight.
                        // Keep the parent so its SpeedupData survives for a potential rebuild.
                        continue;
                    }
                    self.remove_tx(tx.txid)?;
                    self.add_news(CoordinatorNews::TransactionEvicted {
                        txid: tx.txid,
                        context: tx.context.clone(),
                    })?;
                }
            }
        }
        Ok(())
    }

    // ================================
    //  Speedup
    // ================================

    /// Insert a speedup transaction and append its txid to the ordered SpeedupList.
    /// Use this instead of `insert_tx` for all CPFP/RBF transactions so that
    /// `get_speedups_ordered` returns them in creation order.
    pub fn insert_speedup(&self, tx: CoordinatedTx) -> Result<(), BitcoinCoordinatorError> {
        let txid = tx.txid;
        self.insert_tx(tx)?;
        let key = self.get_key(StoreKey::SpeedupList);
        let mut list: Vec<Txid> = self.storage.get(&key, None)?.unwrap_or_default();
        if !list.contains(&txid) {
            list.push(txid);
            self.storage.set(&key, &list, None)?;
        }
        Ok(())
    }

    /// Return in-flight speedups (`InMempool` or `Confirmed`) in creation order.
    pub fn get_active_speedups(&self) -> Result<Vec<CoordinatedTx>, BitcoinCoordinatorError> {
        Ok(self
            .get_speedups_ordered()?
            .into_iter()
            .filter(|tx| {
                matches!(
                    tx.state,
                    TransactionState::InMempool | TransactionState::Confirmed
                )
            })
            .collect())
    }

    /// Return speedup transactions in creation order (oldest first).
    /// Txids that no longer exist in storage are skipped as a safety net against
    /// any inconsistency between the list and the tx store.
    pub fn get_speedups_ordered(&self) -> Result<Vec<CoordinatedTx>, BitcoinCoordinatorError> {
        let key = self.get_key(StoreKey::SpeedupList);
        let list: Vec<Txid> = self.storage.get(&key, None)?.unwrap_or_default();
        let mut result = Vec::new();
        for txid in list {
            if let Some(tx) = self.get_tx_by_id(txid)? {
                result.push(tx);
            }
        }
        Ok(result)
    }

    fn remove_speedup_from_list(&self, txid: Txid) -> Result<(), BitcoinCoordinatorError> {
        let list_key = self.get_key(StoreKey::SpeedupList);
        let mut list: Vec<Txid> = self.storage.get(&list_key, None)?.unwrap_or_default();
        let before = list.len();
        list.retain(|id| id != &txid);
        if list.len() != before {
            self.storage.set(&list_key, &list, None)?;
        }
        Ok(())
    }

    // ================================
    // PENDING SPEEDUP PARENTS
    // ================================

    /// Record that `txid` (a `NeedsSpeedup` parent) is waiting for a CPFP.
    pub fn add_pending_speedup_parent(&self, txid: Txid) -> Result<(), BitcoinCoordinatorError> {
        let key = self.get_key(StoreKey::PendingSpeedupParents);
        let mut list: Vec<Txid> = self.storage.get(&key, None)?.unwrap_or_default();
        if !list.contains(&txid) {
            list.push(txid);
            self.storage.set(&key, &list, None)?;
        }
        Ok(())
    }

    /// Remove `txid` from the pending set (CPFP dispatched or parent no longer active).
    pub fn remove_pending_speedup_parent(&self, txid: Txid) -> Result<(), BitcoinCoordinatorError> {
        let key = self.get_key(StoreKey::PendingSpeedupParents);
        let mut list: Vec<Txid> = self.storage.get(&key, None)?.unwrap_or_default();
        let before = list.len();
        list.retain(|id| id != &txid);
        if list.len() != before {
            self.storage.set(&key, &list, None)?;
        }
        Ok(())
    }

    /// Return the `CoordinatedTx` records for pending speedup parents that are eligible for CPFP construction.
    ///
    /// Pruning rules:
    ///   - `Failed`     → remove. The parent will never confirm; no CPFP can help.
    ///   - missing      → remove. Missing parents will re-dispatch as new `ToDispatch`,
    ///                    and will be re-added to PendingSpeedupParents.
    ///   - `ToDispatch` → keep in PendingSpeedupParents but exclude from this call's result.
    ///   - `InMempool`, `Confirmed`, `Finalized` → keep in PendingSpeedupParents and return.
    pub fn get_pending_speedup_parents(
        &self,
    ) -> Result<Vec<CoordinatedTx>, BitcoinCoordinatorError> {
        let list: Vec<Txid> = {
            let key = self.get_key(StoreKey::PendingSpeedupParents);
            self.storage.get(&key, None)?.unwrap_or_default()
        };
        let mut live = Vec::new();
        for txid in list {
            match self.get_tx_by_id(txid)? {
                Some(tx)
                    if matches!(
                        tx.state,
                        TransactionState::InMempool
                            | TransactionState::Confirmed
                            | TransactionState::Finalized
                    ) =>
                {
                    live.push(tx)
                }
                Some(tx) if tx.state == TransactionState::Failed => {
                    self.remove_pending_speedup_parent(txid)?;
                }
                None => {
                    self.remove_pending_speedup_parent(txid)?;
                }
                _ => {} // ToDispatch: parent not yet broadcast, defer.
            }
        }
        Ok(live)
    }

    // ================================
    //  NEWS
    // ================================

    /// Store `news` if an identical item is not already present.
    pub fn add_news(&self, news: CoordinatorNews) -> Result<(), BitcoinCoordinatorError> {
        let key = self.get_key(StoreKey::News);
        let mut all: Vec<CoordinatorNews> = self.storage.get(&key, None)?.unwrap_or_default();

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
        Ok(self.storage.get(&key, None)?.unwrap_or_default())
    }

    pub fn clear_news(&self) -> Result<(), BitcoinCoordinatorError> {
        let key = self.get_key(StoreKey::News);
        self.storage.remove(&key, None)?;
        Ok(())
    }

    pub fn ack_news(&self, news: CoordinatorNews) -> Result<(), BitcoinCoordinatorError> {
        let key = self.get_key(StoreKey::News);
        let mut all: Vec<CoordinatorNews> = self.storage.get(&key, None)?.unwrap_or_default();
        all.retain(|n| n != &news);
        self.storage.set(&key, &all, None)?;
        Ok(())
    }

    // ================================
    // INTERNAL HELPERS
    // ================================

    /// Return the set of `NeedsSpeedup` parent txids that are directly listed
    /// in `SpeedupKind::CPFP::parents` of at least one speedup record whose
    /// state is neither `Failed` nor `Finalized`.
    // fn parents_with_live_cpfp(&self) -> Result<HashSet<Txid>, BitcoinCoordinatorError> {
    //     let mut out = HashSet::new();
    //     for s in self.get_speedups_ordered()? {
    //         if matches!(
    //             s.state,
    //             TransactionState::Failed | TransactionState::Finalized
    //         ) {
    //             continue;
    //         }
    //         if let TxKind::Speedup(SpeedupKind::CPFP { parents, .. }) = &s.kind {
    //             for p in parents {
    //                 if let Some(tx) = self.get_tx_by_id(*p)? {
    //                     if matches!(tx.kind, TxKind::NeedsSpeedup(_)) {
    //                         out.insert(*p);
    //                     }
    //                 }
    //             }
    //         }
    //     }
    //     Ok(out)
    // }

    fn tx_prefix(&self) -> String {
        format!("{TX_PREFIX}/txs/")
    }

    fn get_key(&self, key: StoreKey) -> String {
        let prefix = TX_PREFIX;
        match key {
            StoreKey::Tx(tx_id) => format!("{prefix}/txs/{tx_id}"),
            StoreKey::News => format!("{prefix}/news"),
            StoreKey::SpeedupList => format!("{prefix}/speedup/list"),
            StoreKey::PendingSpeedupParents => format!("{prefix}/speedup/pending_parents"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::config::CoordinatorStorageSettings,
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
            broadcast_block_height: None,
            target_block_height: 0,
            stuck_in_mempool_blocks: None,
            confirmation_trigger: None,
            settled_block_height: None,
            retry_count: 0,
            fee_info: FeeInfo {
                fee: 1000,
                fee_rate: 1,
                weight: 100,
            },
            context: "test".to_string(),
        }
    }

    fn new_storage(storage_backend: &StorageTestConfig) -> CoordinatorStorage {
        CoordinatorStorage::new(
            storage_backend.get_raw_storage(),
            CoordinatorStorageSettings::default(),
        )
    }

    fn random_txid() -> Txid {
        use bitcoin::hashes::{sha256d, Hash};
        Txid::from_raw_hash(sha256d::Hash::hash(&rand::random::<[u8; 32]>()))
    }

    #[test]
    fn test_insert_get_remove_tx() {
        let storage_backend = StorageTestConfig::new();
        let storage = new_storage(&storage_backend);

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
        let storage = new_storage(&storage_backend);

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
        let storage = new_storage(&storage_backend);

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

        // Valid: Confirmed -> ToDispatch (deep-reorg recovery — the speedup
        // engine re-queues a not_found Confirmed speedup for re-dispatch).
        storage
            .update_tx_state(txid, TransactionState::ToDispatch)
            .unwrap();
        let updated = storage.get_tx_by_id(txid).unwrap().unwrap();
        assert_eq!(updated.state, TransactionState::ToDispatch);
        assert!(storage.get_news().unwrap().is_empty());

        drop(storage);
        storage_backend.remove().unwrap();
    }

    #[test]
    fn test_crash_recovery_state_transitions() {
        let storage_backend = StorageTestConfig::new();
        let storage = new_storage(&storage_backend);

        // InMempool -> Finalized (fast confirmation, skipped Confirmed)
        let txid1 = random_txid();
        storage
            .insert_tx(dummy_tx(txid1, TransactionState::InMempool))
            .unwrap();
        storage
            .settle_tx(txid1, TransactionState::Finalized, 0)
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
            .settle_tx(txid3, TransactionState::Finalized, 0)
            .unwrap();
        assert!(storage.get_news().unwrap().is_empty());

        drop(storage);
        storage_backend.remove().unwrap();
    }

    #[test]
    fn test_update_tx_state_not_found() {
        let storage_backend = StorageTestConfig::new();
        let storage = new_storage(&storage_backend);

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
        let storage = new_storage(&storage_backend);

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
        let storage = new_storage(&storage_backend);

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
        let storage = new_storage(&storage_backend);

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
        let storage = new_storage(&storage_backend);

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

    /// Adding the same item multiple times stores it only once.
    #[test]
    fn test_add_news_dedup() {
        let storage_backend = StorageTestConfig::new();
        let storage = new_storage(&storage_backend);

        let news = CoordinatorNews::FundingNotAvailable;

        storage.add_news(news.clone()).unwrap();
        storage.add_news(news.clone()).unwrap();
        storage.add_news(news.clone()).unwrap();

        let returned = storage.get_news().unwrap();
        assert_eq!(returned.len(), 1);

        // get_news is idempotent: calling it again returns the same items.
        let returned_again = storage.get_news().unwrap();
        assert_eq!(returned_again.len(), 1);

        drop(storage);
        storage_backend.remove().unwrap();
    }

    /// Two distinct items are both stored and always returned together.
    #[test]
    fn test_add_news_distinct_items_both_stored() {
        let storage_backend = StorageTestConfig::new();
        let storage = new_storage(&storage_backend);

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
        let storage = new_storage(&storage_backend);

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

    /// `settle_tx` records `settled_block_height` when transitioning to a terminal state.
    #[test]
    fn test_settle_tx_records_height() {
        let storage_backend = StorageTestConfig::new();
        let storage = new_storage(&storage_backend);

        let txid = random_txid();
        storage
            .insert_tx(dummy_tx(txid, TransactionState::InMempool))
            .unwrap();

        storage
            .settle_tx(txid, TransactionState::Finalized, 42)
            .unwrap();

        let updated = storage.get_tx_by_id(txid).unwrap().unwrap();
        assert_eq!(updated.state, TransactionState::Finalized);
        assert_eq!(updated.settled_block_height, Some(42));
        assert!(storage.get_news().unwrap().is_empty());

        drop(storage);
        storage_backend.remove().unwrap();
    }

    /// `evict_stale_txs` removes txs whose settled height exceeds the threshold
    /// and emits `TransactionEvicted` news.
    #[test]
    fn test_evict_stale_txs() {
        let storage_backend = StorageTestConfig::new();
        let storage = new_storage(&storage_backend);

        let txid_stale = random_txid();
        let txid_fresh = random_txid();

        // Stale: settled at height 5, current height = 16 -> 11 blocks ago >= 10
        let mut stale = dummy_tx(txid_stale, TransactionState::Finalized);
        stale.settled_block_height = Some(5);
        storage.insert_tx(stale).unwrap();

        // Fresh: settled at height 10, current height = 16 -> 6 blocks ago < 10
        let mut fresh = dummy_tx(txid_fresh, TransactionState::Finalized);
        fresh.settled_block_height = Some(10);
        storage.insert_tx(fresh).unwrap();

        storage.evict_stale_txs(16).unwrap();

        // Stale tx removed, fresh still present
        assert!(storage.get_tx_by_id(txid_stale).unwrap().is_none());
        assert!(storage.get_tx_by_id(txid_fresh).unwrap().is_some());

        // One eviction news item for the stale tx
        let news = storage.get_news().unwrap();
        assert_eq!(news.len(), 1);
        assert!(matches!(
            &news[0],
            CoordinatorNews::TransactionEvicted { txid, .. } if *txid == txid_stale
        ));

        drop(storage);
        storage_backend.remove().unwrap();
    }

    /// `remove_tx` removes the txid from the SpeedupList when the tx is a speedup.
    #[test]
    fn test_remove_tx_cleans_speedup_list() {
        let storage_backend = StorageTestConfig::new();
        let storage = new_storage(&storage_backend);

        let txid1 = random_txid();
        let txid2 = random_txid();

        storage
            .insert_speedup(dummy_tx(txid1, TransactionState::InMempool))
            .unwrap();
        storage
            .insert_speedup(dummy_tx(txid2, TransactionState::InMempool))
            .unwrap();

        assert_eq!(storage.get_speedups_ordered().unwrap().len(), 2);

        storage.remove_tx(txid1).unwrap();

        let remaining = storage.get_speedups_ordered().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].txid, txid2);

        drop(storage);
        storage_backend.remove().unwrap();
    }

    /// `evict_stale_txs` removes settled speedups from both the tx store and the SpeedupList.
    #[test]
    fn test_evict_speedup_removes_from_list() {
        let storage_backend = StorageTestConfig::new();
        let storage = new_storage(&storage_backend);

        let txid_stale = random_txid();
        let txid_active = random_txid();

        let mut stale = dummy_tx(txid_stale, TransactionState::Finalized);
        stale.settled_block_height = Some(5);
        storage.insert_speedup(stale).unwrap();

        storage
            .insert_speedup(dummy_tx(txid_active, TransactionState::InMempool))
            .unwrap();

        assert_eq!(storage.get_speedups_ordered().unwrap().len(), 2);

        // current_height = 16: stale settled at 5 -> 11 blocks ago >= max_tracking_confirmations(10)
        storage.evict_stale_txs(16).unwrap();

        let remaining = storage.get_speedups_ordered().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].txid, txid_active);

        drop(storage);
        storage_backend.remove().unwrap();
    }

    /// After a reorg a speedup stays in the SpeedupList with its updated state.
    #[test]
    fn test_reorg_does_not_remove_from_speedup_list() {
        let storage_backend = StorageTestConfig::new();
        let storage = new_storage(&storage_backend);

        let txid = random_txid();
        let mut tx = dummy_tx(txid, TransactionState::Confirmed);
        tx.broadcast_block_height = Some(10);
        storage.insert_speedup(tx.clone()).unwrap();

        // Simulate reorg: update state back to InMempool
        storage
            .update_tx_state(txid, TransactionState::InMempool)
            .unwrap();

        let speedups = storage.get_speedups_ordered().unwrap();
        assert_eq!(speedups.len(), 1);
        assert_eq!(speedups[0].state, TransactionState::InMempool);

        drop(storage);
        storage_backend.remove().unwrap();
    }

    /// `get_pending_speedup_parents` keeps Confirmed and Finalized parents in PendingSpeedupParents.
    /// They are still eligible for CPFP construction; only the parent hitting Failed or being missing
    ///  should evict from PendingSpeedupParents.
    #[test]
    fn test_pending_parents_keep_confirmed_and_finalized() {
        let storage_backend = StorageTestConfig::new();
        let storage = new_storage(&storage_backend);

        let in_mempool_id = random_txid();
        let confirmed_id = random_txid();
        let finalized_id = random_txid();
        let failed_id = random_txid();

        storage
            .insert_tx(dummy_tx(in_mempool_id, TransactionState::InMempool))
            .unwrap();
        storage
            .insert_tx(dummy_tx(confirmed_id, TransactionState::Confirmed))
            .unwrap();
        let mut finalized_tx = dummy_tx(finalized_id, TransactionState::Finalized);
        finalized_tx.settled_block_height = Some(1);
        storage.insert_tx(finalized_tx).unwrap();
        let mut failed_tx = dummy_tx(failed_id, TransactionState::Failed);
        failed_tx.settled_block_height = Some(1);
        storage.insert_tx(failed_tx).unwrap();

        for id in [in_mempool_id, confirmed_id, finalized_id, failed_id] {
            storage.add_pending_speedup_parent(id).unwrap();
        }

        // First read returns the 3 still-CPFP-eligible parents and lazily prunes
        // the Failed one.
        let returned = storage.get_pending_speedup_parents().unwrap();
        let returned_ids: Vec<Txid> = returned.iter().map(|t| t.txid).collect();
        assert_eq!(returned.len(), 3);
        assert!(returned_ids.contains(&in_mempool_id));
        assert!(returned_ids.contains(&confirmed_id));
        assert!(returned_ids.contains(&finalized_id));
        assert!(!returned_ids.contains(&failed_id));

        // The non-Failed parents are still retained for future CPFP construction:
        // a second call returns the same three.
        let again = storage.get_pending_speedup_parents().unwrap();
        let again_ids: Vec<Txid> = again.iter().map(|t| t.txid).collect();
        assert_eq!(again_ids.len(), 3);
        assert!(again_ids.contains(&in_mempool_id));
        assert!(again_ids.contains(&confirmed_id));
        assert!(again_ids.contains(&finalized_id));

        drop(storage);
        storage_backend.remove().unwrap();
    }

    /// `evict_stale_txs` refuses to evict a `NeedsSpeedup` parent that is still in PendingSpeedupParents,
    /// even after `max_tracking_confirmations` have elapsed.
    #[test]
    fn test_evict_stale_keeps_needs_speedup_parent() {
        use protocol_builder::types::{output::SpeedupData, Utxo};

        let storage_backend = StorageTestConfig::new();
        let storage = new_storage(&storage_backend);

        let txid = random_txid();
        let mut tx = dummy_tx(txid, TransactionState::Finalized);
        let pub_key = crate::test_utils::dummy_pubkey();
        tx.kind = TxKind::NeedsSpeedup(SpeedupData::new(Utxo::new(txid, 0, 100_000, &pub_key)));
        tx.settled_block_height = Some(1);
        storage.insert_tx(tx).unwrap();
        storage.add_pending_speedup_parent(txid).unwrap();

        // Past the eviction threshold but parent is still in PendingSpeedupParents.
        storage.evict_stale_txs(100).unwrap();

        assert!(
            storage.get_tx_by_id(txid).unwrap().is_some(),
            "NeedsSpeedup parent in PendingSpeedupParents must not be evicted (SpeedupData must survive)"
        );
        assert!(
            storage.get_news().unwrap().is_empty(),
            "no TransactionEvicted news while the parent is still in PendingSpeedupParents"
        );

        // After the CPFP is built, parent is removed from PendingSpeedupParents and eviction
        // proceeds normally on the next tick.
        storage.remove_pending_speedup_parent(txid).unwrap();
        storage.evict_stale_txs(100).unwrap();

        assert!(
            storage.get_tx_by_id(txid).unwrap().is_none(),
            "parent must be evicted once removed from PendingSpeedupParents"
        );

        drop(storage);
        storage_backend.remove().unwrap();
    }
}
