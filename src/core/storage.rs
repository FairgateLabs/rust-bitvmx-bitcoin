use crate::{
    config::config::CoordinatorStorageSettings,
    core::{dispatcher::DispatcherStorage, funding::FundingStorage},
    errors::BitcoinCoordinatorError,
    types::{CoordinatedTx, CoordinatorNews, FundingData, SpeedupKind, TransactionState, TxKind},
};
use bitcoin::Txid;
use bitvmx_bitcoin_rpc::types::BlockHeight;
use protocol_builder::types::Utxo;
use serde::{Deserialize, Serialize};
use std::rc::Rc;
use storage_backend::storage::{KeyValueStore, Storage};
use tracing::warn;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredNewsItem {
    news: CoordinatorNews,
    acked_at_block: Option<BlockHeight>,
    shown_at_block: Option<BlockHeight>,
}

const TX_PREFIX: &str = "bitcoin_coordinator";

pub struct CoordinatorStorage {
    pub storage: Rc<Storage>,
    settings: CoordinatorStorageSettings,
}

enum StoreKey {
    /// Individual coordinated transaction record
    /// (Normal / NeedsSpeedup / Speedup / Funding).
    Tx(Txid),
    /// Ordered list of coordinator and monitor news items awaiting acknowledgement.
    News,
    /// Ordered list of Speedup-kind transaction ids (CPFPs / RBFs), insertion order.
    SpeedupList,
    /// Set of NeedsSpeedup parent txids that still require a CPFP to be built.
    /// Added on dispatch_with_speedup; removed when the covering CPFP is dispatched.
    PendingSpeedupParents,
    /// Ordered list of TxKind::Funding record txids, insertion order. It may contain
    /// TxKind::Funding records and TxKind::Speedup records, when speedups are finalized.
    FundingList,
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
        Ok(self.storage.partial_get(&prefix, None)?)
    }

    pub fn get_by_state(
        &self,
        state: TransactionState,
    ) -> Result<Vec<CoordinatedTx>, BitcoinCoordinatorError> {
        let txs = self.get_all_txs()?;

        Ok(txs.into_iter().filter(|tx| tx.state == state).collect())
    }

    /// Returns the non-terminal transactions: the ToDispatch, InMempool, and Confirmed records. These are the only
    /// states the tick pipeline acts on, so Failed and Finalized records are deliberately excluded (review never
    /// re-examines a terminal tx). Several invariants in the engines rely on this filtered set.
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

    /// Re-queue a tx that the chain reported `not_found` back to `ToDispatch`, and arm the reorg-flap
    /// fail guard. `fail_guard_until` is set to `current_height + max_monitoring_confirmations`.After
    /// the window, exactly one chain branch survives, so the verdict is final.
    pub fn requeue_not_found(
        &self,
        tx_id: Txid,
        guard_deadline: BlockHeight,
    ) -> Result<(), BitcoinCoordinatorError> {
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
        // Anchor at the first not_found; do not extend on subsequent ones. Reorgs deeper than max_confs are impossible
        tx.fail_guard_until.get_or_insert(guard_deadline);
        self.update_tx(&tx)?;
        Ok(())
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

    /// Re-queues a tx for another dispatch attempt: moves it back to ToDispatch and increments retry_count. Used both
    /// by the normal retry path and by the reorg-flap guard's deferral. Emits news instead of erroring if the tx is
    /// missing or the transition to ToDispatch is not allowed from its current state.
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

    /// Remove transactions that have been in a terminal state for at least `max_tracking_confirmations`
    /// blocks, emitting a `TransactionEvicted` news item for each one.
    ///
    /// Eviction protection for `NeedsSpeedup` parents (so a later rebuild-by-prepend can still find them):
    ///   1. The parent is currently listed in `PendingSpeedupParents`, or
    ///   2. Any non-Failed-and-non-Finalized speedup references the parent
    ///      in `SpeedupKind::CPFP.parents` (live coverage).
    /// Eviction also protects speedups that are part of the FundingList
    pub fn evict_stale_txs(
        &self,
        current_height: BlockHeight,
    ) -> Result<(), BitcoinCoordinatorError> {
        let settled = self.get_settled_txs()?;
        let psp: Vec<Txid> = {
            let key = self.get_key(StoreKey::PendingSpeedupParents);
            self.storage.get(&key, None)?.unwrap_or_default()
        };
        let funding_list: Vec<Txid> = {
            let key = self.get_key(StoreKey::FundingList);
            self.storage.get(&key, None)?.unwrap_or_default()
        };
        // Build the set of NeedsSpeedup parent txids that are referenced by a non-Failed/non-Finalized speedup ("live coverage").
        let mut live_covered: std::collections::HashSet<Txid> = std::collections::HashSet::new();
        for s in self.get_speedups_ordered()? {
            if matches!(
                s.state,
                TransactionState::Failed | TransactionState::Finalized
            ) {
                continue;
            }
            if let TxKind::Speedup(SpeedupKind::CPFP { parents, .. }) = &s.kind {
                for p in parents {
                    live_covered.insert(*p);
                }
            }
        }
        for tx in settled {
            if let Some(settled_height) = tx.settled_block_height {
                if current_height.saturating_sub(settled_height)
                    >= self.settings.max_tracking_confirmations
                {
                    // NeedsSpeedup parent: protected while a CPFP is still pending or live.
                    if matches!(tx.kind, TxKind::NeedsSpeedup(_))
                        && (psp.contains(&tx.txid) || live_covered.contains(&tx.txid))
                    {
                        continue;
                    }
                    // Finalized speedup: protected while its change output still backs an entry in the funding queue.
                    if funding_list.contains(&tx.txid) {
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

    /// Insert `txids` at the front of the pending speedup parents list, preserving
    /// their internal order, deduplicating against entries already present.
    pub fn prepend_pending_speedup_parents(
        &self,
        txids: &[Txid],
    ) -> Result<(), BitcoinCoordinatorError> {
        if txids.is_empty() {
            return Ok(());
        }
        let key = self.get_key(StoreKey::PendingSpeedupParents);
        let mut list: Vec<Txid> = self.storage.get(&key, None)?.unwrap_or_default();
        let existing: std::collections::HashSet<Txid> = list.iter().copied().collect();
        let to_prepend: Vec<Txid> = txids
            .iter()
            .copied()
            .filter(|id| !existing.contains(id))
            .collect();
        if to_prepend.is_empty() {
            return Ok(());
        }
        let mut new_list = to_prepend;
        new_list.append(&mut list);
        self.storage.set(&key, &new_list, None)?;
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

    /// Re-add `failed_cpfp`'s NeedsSpeedup parents to PendingSpeedupParents (prepend with dedup).
    pub fn requeue_protocol_parents(
        &self,
        failed_cpfp: &CoordinatedTx,
    ) -> Result<(), BitcoinCoordinatorError> {
        let parents = match &failed_cpfp.kind {
            TxKind::Speedup(SpeedupKind::CPFP { parents, .. }) => parents.clone(),
            _ => return Ok(()),
        };
        let mut to_prepend = Vec::with_capacity(parents.len());
        for p in parents {
            if let Some(parent) = self.get_tx_by_id(p)? {
                if matches!(parent.kind, TxKind::NeedsSpeedup(_)) {
                    to_prepend.push(p);
                }
            }
        }
        self.prepend_pending_speedup_parents(&to_prepend)?;
        Ok(())
    }

    // ================================
    //  NEWS
    // ================================

    /// Store `news` unless any item with the same value already exists (acked or not).
    /// This prevents duplicate entries even while a previously-acked copy is still
    /// waiting for its next-block cleanup.
    /// Returns `true` if the item was inserted, `false` if it was a duplicate.
    pub fn add_news(&self, news: CoordinatorNews) -> Result<bool, BitcoinCoordinatorError> {
        let key = self.get_key(StoreKey::News);
        let mut all: Vec<StoredNewsItem> = self.storage.get(&key, None)?.unwrap_or_default();
        if all.iter().any(|item| item.news == news) {
            return Ok(false);
        }
        all.push(StoredNewsItem {
            news,
            acked_at_block: None,
            shown_at_block: None,
        });
        self.storage.set(&key, &all, None)?;
        Ok(true)
    }

    /// Return unacked news not yet shown at `current_height` and mark them shown.
    /// A second call with the same `current_height` returns empty (already shown this block).
    /// When the next block arrives, unacked items become visible again.
    pub fn get_and_mark_news(
        &self,
        current_height: BlockHeight,
    ) -> Result<Vec<CoordinatorNews>, BitcoinCoordinatorError> {
        let key = self.get_key(StoreKey::News);
        let mut all: Vec<StoredNewsItem> = self.storage.get(&key, None)?.unwrap_or_default();
        let mut pending = Vec::new();
        let mut changed = false;
        for item in &mut all {
            if item.acked_at_block.is_some() {
                continue;
            }
            if item.shown_at_block == Some(current_height) {
                continue;
            }
            item.shown_at_block = Some(current_height);
            pending.push(item.news.clone());
            changed = true;
        }
        if changed {
            self.storage.set(&key, &all, None)?;
        }
        Ok(pending)
    }

    /// Mark `news` as acknowledged at `current_height`. The item is hidden from
    /// `get_and_mark_news` immediately but stays in storage until `cleanup_news`
    /// runs at a strictly later block.
    pub fn ack_news(
        &self,
        news: CoordinatorNews,
        current_height: BlockHeight,
    ) -> Result<(), BitcoinCoordinatorError> {
        let key = self.get_key(StoreKey::News);
        let mut all: Vec<StoredNewsItem> = self.storage.get(&key, None)?.unwrap_or_default();
        for item in &mut all {
            if item.acked_at_block.is_none() && item.news == news {
                item.acked_at_block = Some(current_height);
                break;
            }
        }
        self.storage.set(&key, &all, None)?;
        Ok(())
    }

    /// Remove items that were acknowledged in a strictly earlier block
    /// (`acked_at_block < current_height`). Called at the start of each tick.
    pub fn cleanup_news(&self, current_height: BlockHeight) -> Result<(), BitcoinCoordinatorError> {
        let key = self.get_key(StoreKey::News);
        let mut all: Vec<StoredNewsItem> = self.storage.get(&key, None)?.unwrap_or_default();
        all.retain(|item| item.acked_at_block.map_or(true, |h| h >= current_height));
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
            StoreKey::SpeedupList => format!("{prefix}/speedup/list"),
            StoreKey::PendingSpeedupParents => format!("{prefix}/speedup/pending_parents"),
            StoreKey::FundingList => format!("{prefix}/funding/list"),
        }
    }
}

// ================================
// FUNDING STORAGE
// ================================
impl FundingStorage for CoordinatorStorage {
    fn read_funding_records(&self) -> Result<Vec<CoordinatedTx>, BitcoinCoordinatorError> {
        let list_key = self.get_key(StoreKey::FundingList);
        let list: Vec<Txid> = self.storage.get(&list_key, None)?.unwrap_or_default();
        let mut out = Vec::with_capacity(list.len());
        for id in list {
            if let Some(tx) = self.get_tx_by_id(id)? {
                out.push(tx);
            }
        }
        Ok(out)
    }

    fn append_funding(&self, utxo: Utxo) -> Result<(), BitcoinCoordinatorError> {
        let txid = utxo.txid;
        let mut record = CoordinatedTx::new_empty();
        record.txid = txid;
        record.kind = TxKind::Funding(FundingData { utxo, spent: false });
        self.insert_tx(record)?;
        let list_key = self.get_key(StoreKey::FundingList);
        let mut list: Vec<Txid> = self.storage.get(&list_key, None)?.unwrap_or_default();
        if !list.contains(&txid) {
            list.push(txid);
            self.storage.set(&list_key, &list, None)?;
        }
        Ok(())
    }

    fn set_spent(&self, txid: Txid, spent: bool) -> Result<(), BitcoinCoordinatorError> {
        if let Some(mut tx) = self.get_tx_by_id(txid)? {
            let changed = match tx.kind {
                TxKind::Funding(ref mut d) if d.spent != spent => {
                    d.spent = spent;
                    true
                }
                TxKind::Speedup(ref mut k) if k.context().spent != spent => {
                    k.context_mut().spent = spent;
                    true
                }
                _ => false,
            };
            if changed {
                self.update_tx(&tx)?;
            }
        }
        Ok(())
    }

    fn clear_funding_records(&self) -> Result<(), BitcoinCoordinatorError> {
        let list_key = self.get_key(StoreKey::FundingList);
        let list: Vec<Txid> = self.storage.get(&list_key, None)?.unwrap_or_default();
        for id in list {
            let tx_key = self.get_key(StoreKey::Tx(id));
            self.storage.remove(&tx_key, None)?;
        }
        self.storage.remove(&list_key, None)?;
        Ok(())
    }

    fn replace_funding_on_finalize(&self, txid: Txid) -> Result<(), BitcoinCoordinatorError> {
        let tx = match self.get_tx_by_id(txid)? {
            Some(t) => t,
            None => {
                warn!("replace_funding_on_finalize: tx {} not found", txid);
                return Ok(());
            }
        };
        let k = match tx.speedup_kind() {
            Ok(k) => k,
            Err(_) => {
                warn!(
                    "replace_funding_on_finalize: tx {} is not a Speedup (kind={:?})",
                    tx.txid, tx.kind
                );
                return Ok(());
            }
        };
        let ctx = k.context();

        let list_key = self.get_key(StoreKey::FundingList);
        let mut list: Vec<Txid> = self.storage.get(&list_key, None)?.unwrap_or_default();

        // Find positions in `list` whose record's change output matches one of tx.funding_inputs.
        // Records may be Funding (utxo field) or Speedup (its last output is the change consumed by the next speedup).
        let mut to_remove_idx: Vec<usize> = Vec::new();
        let mut insert_pos: Option<usize> = None;
        for (idx, id) in list.iter().enumerate() {
            let Some(rec) = self.get_tx_by_id(*id)? else {
                continue;
            };
            let (rec_txid, rec_vout) = match &rec.kind {
                TxKind::Funding(d) => (d.utxo.txid, d.utxo.vout),
                TxKind::Speedup(_) => {
                    let (_, v) = rec.last_output()?;
                    (rec.txid, v)
                }
                _ => continue,
            };
            let matched = ctx
                .funding_inputs
                .iter()
                .any(|fi| fi.txid == rec_txid && fi.vout == rec_vout);
            if matched {
                to_remove_idx.push(idx);
                if insert_pos.map_or(true, |p| idx < p) {
                    insert_pos = Some(idx);
                }
            }
        }

        if to_remove_idx.is_empty() {
            warn!(
                "replace_funding_on_finalize: tx {} has no matching funding records",
                tx.txid
            );
            return Ok(());
        }

        // Remove (list entry + tx record) from highest to lowest index. Emit a
        // `TransactionEvicted` news entry for every removed record.
        for idx in to_remove_idx.iter().rev() {
            let removed_id = list.remove(*idx);
            let removed_ctx = self
                .get_tx_by_id(removed_id)?
                .map(|rec| rec.context.clone());
            let tx_key = self.get_key(StoreKey::Tx(removed_id));
            self.storage.remove(&tx_key, None)?;
            self.remove_speedup_from_list(removed_id)?;
            if let Some(ctx_str) = removed_ctx {
                self.add_news(CoordinatorNews::TransactionEvicted {
                    txid: removed_id,
                    context: ctx_str,
                })?;
            }
        }

        // Insert the finalized speedup's txid at the smallest removed position.
        let pos = insert_pos.unwrap();
        if pos <= list.len() {
            list.insert(pos, tx.txid);
        } else {
            list.push(tx.txid);
        }
        self.storage.set(&list_key, &list, None)?;
        self.remove_speedup_from_list(tx.txid)?;
        Ok(())
    }
}

// ================================
// DISPATCHER STORAGE
// ================================
impl DispatcherStorage for CoordinatorStorage {
    /// Funding-kind records are operator-supplied external UTXOs (must already be
    /// confirmed per the README disclaimer). Treat them as untracked so the
    /// dispatcher does NOT gate on them.
    fn is_tx_known(&self, txid: &Txid) -> Result<bool, BitcoinCoordinatorError> {
        Ok(self
            .get_tx_by_id(*txid)?
            .is_some_and(|tx| !matches!(tx.kind, TxKind::Funding(_))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::dummy_pubkey;
    use crate::types::{SpeedupContext, SpeedupKind};
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
            fail_guard_until: None,
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
        assert!(storage.get_and_mark_news(1).unwrap().is_empty());

        // Valid: InMempool -> Confirmed
        storage
            .update_tx_state(txid, TransactionState::Confirmed)
            .unwrap();
        let updated = storage.get_tx_by_id(txid).unwrap().unwrap();
        assert_eq!(updated.state, TransactionState::Confirmed);
        assert!(storage.get_and_mark_news(2).unwrap().is_empty());

        // Valid: Confirmed -> ToDispatch (deep-reorg recovery, where the speedup engine re-queues a not_found Confirmed speedup for re-dispatch).
        storage
            .update_tx_state(txid, TransactionState::ToDispatch)
            .unwrap();
        let updated = storage.get_tx_by_id(txid).unwrap().unwrap();
        assert_eq!(updated.state, TransactionState::ToDispatch);
        assert!(storage.get_and_mark_news(3).unwrap().is_empty());

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
        assert!(storage.get_and_mark_news(1).unwrap().is_empty());

        // ToDispatch -> Confirmed (restart after dispatch, tx already on-chain)
        let txid2 = random_txid();
        storage
            .insert_tx(dummy_tx(txid2, TransactionState::ToDispatch))
            .unwrap();
        storage
            .update_tx_state(txid2, TransactionState::Confirmed)
            .unwrap();
        assert!(storage.get_and_mark_news(2).unwrap().is_empty());

        // ToDispatch -> Finalized (restart after dispatch, tx already finalized)
        let txid3 = random_txid();
        storage
            .insert_tx(dummy_tx(txid3, TransactionState::ToDispatch))
            .unwrap();
        storage
            .settle_tx(txid3, TransactionState::Finalized, 0)
            .unwrap();
        assert!(storage.get_and_mark_news(3).unwrap().is_empty());

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

        let news = storage.get_and_mark_news(1).unwrap();
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
    fn test_add_and_get_news() {
        let storage_backend = StorageTestConfig::new();
        let storage = new_storage(&storage_backend);

        let news_item = CoordinatorNews::TxNotFound {
            txid: random_txid(),
        };
        storage.add_news(news_item.clone()).unwrap();

        // First call at block 10 shows the item.
        let news = storage.get_and_mark_news(10).unwrap();
        assert_eq!(news.len(), 1);
        assert_eq!(news[0], news_item);

        // Second call at same block returns empty (already shown this block).
        assert!(storage.get_and_mark_news(10).unwrap().is_empty());

        // Next block shows it again (unacked).
        let news2 = storage.get_and_mark_news(11).unwrap();
        assert_eq!(news2.len(), 1);
        assert_eq!(news2[0], news_item);

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

        // Acking item1 hides it immediately.
        storage.ack_news(news_item1.clone(), 10).unwrap();

        let news = storage.get_and_mark_news(10).unwrap();
        assert_eq!(news.len(), 1);
        assert_eq!(news[0], news_item2);

        // item1 is still in storage (not yet cleaned up), not returned.
        assert!(storage.get_and_mark_news(10).unwrap().is_empty());

        // Cleanup at block 11 removes item1 (acked_at=10 < 11).
        storage.cleanup_news(11).unwrap();
        // item2 was shown at 10, re-appears at 11.
        let news3 = storage.get_and_mark_news(11).unwrap();
        assert_eq!(news3.len(), 1);
        assert_eq!(news3[0], news_item2);

        drop(storage);
        storage_backend.remove().unwrap();
    }

    /// Adding the same item multiple times stores it only once.
    /// Repeated get_and_mark_news at the same block returns empty after the first call.
    #[test]
    fn test_add_news_dedup() {
        let storage_backend = StorageTestConfig::new();
        let storage = new_storage(&storage_backend);

        let news = CoordinatorNews::FundingNotAvailable;

        storage.add_news(news.clone()).unwrap();
        storage.add_news(news.clone()).unwrap();
        storage.add_news(news.clone()).unwrap();

        let returned = storage.get_and_mark_news(5).unwrap();
        assert_eq!(returned.len(), 1);

        // Same block: already shown.
        assert!(storage.get_and_mark_news(5).unwrap().is_empty());

        // Next block: shows again.
        assert_eq!(storage.get_and_mark_news(6).unwrap().len(), 1);

        drop(storage);
        storage_backend.remove().unwrap();
    }

    /// Two distinct items are both stored and returned together in the same call.
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

        let returned = storage.get_and_mark_news(1).unwrap();
        assert_eq!(returned.len(), 2);
        assert!(returned.contains(&item1));
        assert!(returned.contains(&item2));

        drop(storage);
        storage_backend.remove().unwrap();
    }

    /// Acked item blocks re-add until cleanup; after cleanup the same news
    /// can be stored and shown again.
    #[test]
    fn test_ack_blocks_readd_until_cleanup() {
        let storage_backend = StorageTestConfig::new();
        let storage = new_storage(&storage_backend);

        let news = CoordinatorNews::FundingNotAvailable;
        storage.add_news(news.clone()).unwrap();
        storage.get_and_mark_news(10).unwrap();
        storage.ack_news(news.clone(), 10).unwrap();

        // Acked item no longer shown.
        assert!(storage.get_and_mark_news(10).unwrap().is_empty());
        assert!(storage.get_and_mark_news(11).unwrap().is_empty());

        // Re-add is blocked: the acked record still exists in storage.
        storage.add_news(news.clone()).unwrap();
        assert!(storage.get_and_mark_news(11).unwrap().is_empty());

        // Cleanup at block 11 removes the acked item (acked_at=10 < 11).
        storage.cleanup_news(11).unwrap();
        assert!(storage.get_and_mark_news(12).unwrap().is_empty());

        // Now re-add succeeds and the item is visible.
        storage.add_news(news.clone()).unwrap();
        assert_eq!(storage.get_and_mark_news(12).unwrap().len(), 1);

        drop(storage);
        storage_backend.remove().unwrap();
    }

    /// cleanup_news at the same block as ack does not remove the item;
    /// only a strictly later block triggers removal.
    #[test]
    fn test_cleanup_same_block_keeps_item() {
        let storage_backend = StorageTestConfig::new();
        let storage = new_storage(&storage_backend);

        let news = CoordinatorNews::FundingNotAvailable;
        storage.add_news(news.clone()).unwrap();
        storage.ack_news(news.clone(), 5).unwrap();

        // Cleanup at same block: acked_at (5) >= current (5) → retained.
        storage.cleanup_news(5).unwrap();
        // Item is acked, not shown.
        assert!(storage.get_and_mark_news(5).unwrap().is_empty());

        // Cleanup at block 6: acked_at (5) < 6 → removed.
        storage.cleanup_news(6).unwrap();

        // Now re-add works.
        storage.add_news(news.clone()).unwrap();
        assert_eq!(storage.get_and_mark_news(7).unwrap().len(), 1);

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
        assert!(storage.get_and_mark_news(0).unwrap().is_empty());

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
        let news = storage.get_and_mark_news(1).unwrap();
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

        // The non-Failed parents are still retained for future CPFP construction.
        // A second call returns the same three.
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
            storage.get_and_mark_news(1).unwrap().is_empty(),
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

    /// Live-coverage protection: a NeedsSpeedup parent that has been removed from PSP (because its CPFP was built)
    /// must still be protected from eviction as long as any non-Failed-and-non-Finalized speedup references it
    #[test]
    fn test_evict_stale_keeps_parent_under_live_coverage() {
        use crate::types::{SpeedupContext, SpeedupKind};
        use protocol_builder::types::{output::SpeedupData, Utxo};

        let storage_backend = StorageTestConfig::new();
        let storage = new_storage(&storage_backend);

        // Insert a NeedsSpeedup parent, settle it Finalized so it would otherwise be a candidate for eviction.
        let parent_id = random_txid();
        let mut parent = dummy_tx(parent_id, TransactionState::Finalized);
        let pub_key = crate::test_utils::dummy_pubkey();
        parent.kind =
            TxKind::NeedsSpeedup(SpeedupData::new(Utxo::new(parent_id, 0, 100_000, &pub_key)));
        parent.settled_block_height = Some(1);
        storage.insert_tx(parent).unwrap();

        // Build a covering CPFP speedup in InMempool state (NOT in PSP since
        // the build flow removes it).
        let cpfp_id = random_txid();
        let mut cpfp = dummy_tx(cpfp_id, TransactionState::InMempool);
        cpfp.kind = TxKind::Speedup(SpeedupKind::CPFP {
            parents: vec![parent_id],
            context: SpeedupContext {
                funding_inputs: vec![Utxo::new(parent_id, 0, 100_000, &pub_key)],
                replaced_by: None,
                bump_fee_used: 1.0,
                parent_data: vec![],
                spent: false,
            },
        });
        storage.insert_speedup(cpfp).unwrap();

        // Eviction should NOT remove the parent, since live coverage protects it.
        storage.evict_stale_txs(100).unwrap();
        assert!(
            storage.get_tx_by_id(parent_id).unwrap().is_some(),
            "live-covered NeedsSpeedup parent must not be evicted"
        );

        // Once the covering speedup is Failed, the parent is no longer protected by live coverage and gets evicted on the next call.
        storage
            .settle_tx(cpfp_id, TransactionState::Failed, 100)
            .unwrap();
        storage.evict_stale_txs(200).unwrap();
        assert!(
            storage.get_tx_by_id(parent_id).unwrap().is_none(),
            "parent must be evicted once no live speedup references it"
        );

        drop(storage);
        storage_backend.remove().unwrap();
    }

    //Verifies prepend order + dedup then layered requeue semantics on the same PSP.
    #[test]
    fn test_pending_speedup_parents_prepend_and_requeue() {
        use crate::types::{SpeedupContext, SpeedupKind};
        use protocol_builder::types::{output::SpeedupData, Utxo};

        let storage_backend = StorageTestConfig::new();
        let storage = new_storage(&storage_backend);
        let pub_key = crate::test_utils::dummy_pubkey();
        let key = storage.get_key(StoreKey::PendingSpeedupParents);
        let read_list =
            || -> Vec<Txid> { storage.storage.get(&key, None).unwrap().unwrap_or_default() };

        let p1 = random_txid();
        let p2 = random_txid();
        let p3 = random_txid();
        let p4 = random_txid();
        storage.add_pending_speedup_parent(p3).unwrap();
        // [p3] + prepend [p1, p2] → [p1, p2, p3].
        storage.prepend_pending_speedup_parents(&[p1, p2]).unwrap();
        // [p1, p2, p3] + prepend [p2, p4] → only p4 inserted (p2 already present) → [p4, p1, p2, p3].
        storage.prepend_pending_speedup_parents(&[p2, p4]).unwrap();
        assert_eq!(read_list(), vec![p4, p1, p2, p3]);
        // Empty input is a no-op.
        storage.prepend_pending_speedup_parents(&[]).unwrap();
        assert_eq!(read_list(), vec![p4, p1, p2, p3]);

        // Two NeedsSpeedup parents + one Speedup parent + one missing.
        let np1 = random_txid();
        let mut np1_tx = dummy_tx(np1, TransactionState::Finalized);
        np1_tx.kind = TxKind::NeedsSpeedup(SpeedupData::new(Utxo::new(np1, 0, 100, &pub_key)));
        storage.insert_tx(np1_tx).unwrap();

        let np2 = random_txid();
        let mut np2_tx = dummy_tx(np2, TransactionState::Finalized);
        np2_tx.kind = TxKind::NeedsSpeedup(SpeedupData::new(Utxo::new(np2, 0, 100, &pub_key)));
        storage.insert_tx(np2_tx).unwrap();

        let p_speedup = random_txid();
        let mut p_sp = dummy_tx(p_speedup, TransactionState::InMempool);
        p_sp.kind = TxKind::Speedup(SpeedupKind::CPFP {
            parents: vec![],
            context: SpeedupContext {
                funding_inputs: vec![Utxo::new(p_speedup, 0, 100, &pub_key)],
                replaced_by: None,
                bump_fee_used: 1.0,
                parent_data: vec![],
                spent: false,
            },
        });
        storage.insert_tx(p_sp).unwrap();

        let p_missing = random_txid();
        let failed_cpfp_id = random_txid();
        let mut failed = dummy_tx(failed_cpfp_id, TransactionState::Failed);
        failed.kind = TxKind::Speedup(SpeedupKind::CPFP {
            parents: vec![np1, np2, p_speedup, p_missing],
            context: SpeedupContext {
                funding_inputs: vec![Utxo::new(failed_cpfp_id, 0, 100, &pub_key)],
                replaced_by: None,
                bump_fee_used: 1.0,
                parent_data: vec![],
                spent: false,
            },
        });

        // Only np1 + np2 are NeedsSpeedup → prepended in front; existing PSP preserved.
        storage.requeue_protocol_parents(&failed).unwrap();
        assert_eq!(read_list(), vec![np1, np2, p4, p1, p2, p3]);

        // Non-CPFP failed tx (RBF) is a no-op.
        let mut failed_rbf = dummy_tx(random_txid(), TransactionState::Failed);
        failed_rbf.kind = TxKind::Speedup(SpeedupKind::RBF {
            replaces: failed_cpfp_id,
            new_funding_inputs: vec![],
            context: SpeedupContext {
                funding_inputs: vec![],
                replaced_by: None,
                bump_fee_used: 1.0,
                parent_data: vec![],
                spent: false,
            },
        });
        storage.requeue_protocol_parents(&failed_rbf).unwrap();
        assert_eq!(read_list(), vec![np1, np2, p4, p1, p2, p3]);

        drop(storage);
        storage_backend.remove().unwrap();
    }

    #[test]
    fn test_get_by_state_and_exists() {
        let storage_backend = StorageTestConfig::new();
        let storage = new_storage(&storage_backend);

        let id1 = random_txid();
        let id2 = random_txid();
        storage
            .insert_tx(dummy_tx(id1, TransactionState::ToDispatch))
            .unwrap();
        storage
            .insert_tx(dummy_tx(id2, TransactionState::InMempool))
            .unwrap();

        assert_eq!(
            storage
                .get_by_state(TransactionState::ToDispatch)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            storage
                .get_by_state(TransactionState::InMempool)
                .unwrap()
                .len(),
            1
        );
        assert!(storage
            .get_by_state(TransactionState::Confirmed)
            .unwrap()
            .is_empty());

        assert!(storage.exists(id1).unwrap());
        assert!(storage.exists(id2).unwrap());
        storage.remove_tx(id1).unwrap();
        assert!(!storage.exists(id1).unwrap());

        drop(storage);
        storage_backend.remove().unwrap();
    }

    #[test]
    fn test_invalid_state_and_retry_error_paths() {
        let storage_backend = StorageTestConfig::new();
        let storage = new_storage(&storage_backend);

        // update_tx_state: missing tx emits TxNotFound
        let missing = random_txid();
        storage
            .update_tx_state(missing, TransactionState::InMempool)
            .unwrap();
        let news = storage.get_and_mark_news(1).unwrap();
        assert!(matches!(&news[0], CoordinatorNews::TxNotFound { txid } if *txid == missing));
        storage.ack_news(news[0].clone(), 1).unwrap();
        storage.cleanup_news(2).unwrap();

        // update_tx_state: invalid transition emits InvalidStateTransition and leaves state unchanged
        let txid = random_txid();
        let mut tx = dummy_tx(txid, TransactionState::Finalized);
        tx.settled_block_height = Some(1);
        storage.insert_tx(tx).unwrap();
        storage
            .update_tx_state(txid, TransactionState::InMempool)
            .unwrap();
        let news = storage.get_and_mark_news(2).unwrap();
        assert!(matches!(&news[0],
            CoordinatorNews::InvalidStateTransition { txid: id, from, to }
            if *id == txid && *from == TransactionState::Finalized && *to == TransactionState::InMempool
        ));
        assert_eq!(
            storage.get_tx_by_id(txid).unwrap().unwrap().state,
            TransactionState::Finalized
        );
        storage.ack_news(news[0].clone(), 2).unwrap();
        storage.cleanup_news(3).unwrap();

        // mark_as_retry: missing tx emits TxNotFound
        let missing2 = random_txid();
        storage.mark_as_retry(missing2).unwrap();
        let news = storage.get_and_mark_news(3).unwrap();
        assert!(matches!(&news[0], CoordinatorNews::TxNotFound { txid } if *txid == missing2));
        storage.ack_news(news[0].clone(), 3).unwrap();
        storage.cleanup_news(4).unwrap();

        // mark_as_retry: invalid transition (Finalized → ToDispatch) emits InvalidStateTransition
        storage.mark_as_retry(txid).unwrap();
        let news = storage.get_and_mark_news(4).unwrap();
        assert!(matches!(&news[0],
            CoordinatorNews::InvalidStateTransition { txid: id, from, to }
            if *id == txid && *from == TransactionState::Finalized && *to == TransactionState::ToDispatch
        ));
        assert_eq!(storage.get_tx_by_id(txid).unwrap().unwrap().retry_count, 0);

        drop(storage);
        storage_backend.remove().unwrap();
    }

    // ===============================================================
    // FundingStorage tests
    // ===============================================================

    /// Build a Speedup-kind CoordinatedTx with a single change output.
    fn cpfp_with_change(
        funding_inputs: Vec<protocol_builder::types::Utxo>,
        change_sats: u64,
        state: TransactionState,
    ) -> CoordinatedTx {
        let txid = random_txid();
        let tx = bitcoin::Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![bitcoin::TxOut {
                value: bitcoin::Amount::from_sat(change_sats),
                script_pubkey: bitcoin::ScriptBuf::new(),
            }],
        };
        let mut t = dummy_tx(txid, state);
        t.tx = tx;
        t.kind = TxKind::Speedup(SpeedupKind::CPFP {
            parents: vec![],
            context: SpeedupContext {
                funding_inputs,
                replaced_by: None,
                bump_fee_used: 1.0,
                parent_data: vec![],
                spent: false,
            },
        });
        t
    }

    /// Surface check: append → read order → set_spent on both kinds → no-op semantics on wrong-kind / missing → clear wipes everything.
    #[test]
    fn test_funding_storage_surface() {
        let storage_backend = StorageTestConfig::new();
        let storage = new_storage(&storage_backend);
        let pub_key = dummy_pubkey();

        // Empty queue.
        assert!(FundingStorage::read_funding_records(&storage)
            .unwrap()
            .is_empty());

        // Append two Funding records; insertion order preserved, spent=false initially.
        let u1 = Utxo::new(random_txid(), 0, 1_000, &pub_key);
        let u2 = Utxo::new(random_txid(), 1, 2_000, &pub_key);
        FundingStorage::append_funding(&storage, u1.clone()).unwrap();
        FundingStorage::append_funding(&storage, u2.clone()).unwrap();
        let records = FundingStorage::read_funding_records(&storage).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].txid, u1.txid);
        assert_eq!(records[1].txid, u2.txid);
        match (&records[0].kind, &records[1].kind) {
            (TxKind::Funding(a), TxKind::Funding(b)) => {
                assert_eq!(a.utxo.amount, 1_000);
                assert_eq!(b.utxo.amount, 2_000);
                assert!(!a.spent && !b.spent);
            }
            _ => panic!("both must be Funding kind"),
        }

        // set_spent toggles Funding.spent.
        FundingStorage::set_spent(&storage, u1.txid, true).unwrap();
        if let TxKind::Funding(d) = &storage.get_tx_by_id(u1.txid).unwrap().unwrap().kind {
            assert!(d.spent);
        } else {
            panic!("expected Funding");
        }
        FundingStorage::set_spent(&storage, u1.txid, false).unwrap();
        if let TxKind::Funding(d) = &storage.get_tx_by_id(u1.txid).unwrap().unwrap().kind {
            assert!(!d.spent);
        } else {
            panic!("expected Funding");
        }

        // set_spent toggles SpeedupContext.spent on a stored Speedup.
        let s_id = random_txid();
        let mut s = dummy_tx(s_id, TransactionState::InMempool);
        s.kind = TxKind::Speedup(SpeedupKind::CPFP {
            parents: vec![],
            context: SpeedupContext {
                funding_inputs: vec![Utxo::new(s_id, 0, 100_000, &pub_key)],
                replaced_by: None,
                bump_fee_used: 1.0,
                parent_data: vec![],
                spent: false,
            },
        });
        storage.insert_tx(s).unwrap();
        FundingStorage::set_spent(&storage, s_id, true).unwrap();
        assert!(
            storage
                .get_tx_by_id(s_id)
                .unwrap()
                .unwrap()
                .speedup_kind()
                .unwrap()
                .context()
                .spent
        );
        FundingStorage::set_spent(&storage, s_id, false).unwrap();
        assert!(
            !storage
                .get_tx_by_id(s_id)
                .unwrap()
                .unwrap()
                .speedup_kind()
                .unwrap()
                .context()
                .spent
        );

        // Wrong-kind (Normal) and missing txids → silent no-op.
        let normal_id = random_txid();
        storage
            .insert_tx(dummy_tx(normal_id, TransactionState::Finalized))
            .unwrap();
        FundingStorage::set_spent(&storage, normal_id, true).unwrap();
        FundingStorage::set_spent(&storage, random_txid(), true).unwrap();

        // clear wipes list AND underlying tx records.
        FundingStorage::clear_funding_records(&storage).unwrap();
        assert!(FundingStorage::read_funding_records(&storage)
            .unwrap()
            .is_empty());
        assert!(storage.get_tx_by_id(u1.txid).unwrap().is_none());
        assert!(storage.get_tx_by_id(u2.txid).unwrap().is_none());

        drop(storage);
        storage_backend.remove().unwrap();
    }

    /// End-to-end exercise of `replace_funding_on_finalize` along a real chain. Verifies:
    ///   1. A Funding-kind entry is replaced by the finalized speedup
    ///   2. The finalizing speedup is removed from SpeedupList.
    ///   3. A second finalize matches a Speedup-kind queue entry via its `last_output`.
    ///   4. Multi-match (Speedup + Funding) removes both stale entries and inserts the new speedup.
    ///   5. The tx records of removed entries are dropped from storage.
    #[test]
    fn test_replace_funding_on_finalize_lifecycle() {
        let storage_backend = StorageTestConfig::new();
        let storage = new_storage(&storage_backend);
        let pub_key = dummy_pubkey();

        // Seed FundingList with U1 (Funding kind).
        let u1 = Utxo::new(random_txid(), 0, 100_000, &pub_key);
        FundingStorage::append_funding(&storage, u1.clone()).unwrap();

        // S1 consumes U1; insert via insert_speedup so SpeedupList tracks it.
        let s1 = cpfp_with_change(vec![u1.clone()], 90_000, TransactionState::InMempool);
        storage.insert_speedup(s1.clone()).unwrap();
        storage
            .settle_tx(s1.txid, TransactionState::Finalized, 1)
            .unwrap();
        FundingStorage::replace_funding_on_finalize(&storage, s1.txid).unwrap();

        // FundingList = [S1] (Speedup, preserved). U1 tx record gone. S1 removed from SpeedupList.
        let records = FundingStorage::read_funding_records(&storage).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].txid, s1.txid);
        assert!(matches!(records[0].kind, TxKind::Speedup(_)));
        assert_eq!(records[0].state, TransactionState::Finalized);
        assert!(storage.get_tx_by_id(u1.txid).unwrap().is_none());
        assert!(!storage
            .get_speedups_ordered()
            .unwrap()
            .iter()
            .any(|t| t.txid == s1.txid));

        // Append U2 after S1's entry. FundingList = [S1, U2].
        let u2 = Utxo::new(random_txid(), 0, 50_000, &pub_key);
        FundingStorage::append_funding(&storage, u2.clone()).unwrap();
        assert_eq!(
            FundingStorage::read_funding_records(&storage)
                .unwrap()
                .len(),
            2
        );

        // S2 consumes S1's change AND U2. S1's change is matched by the Speedup record's last_output (txid=S1, vout=0).
        let s1_change = Utxo::new(s1.txid, 0, 90_000, &pub_key);
        let s2 = cpfp_with_change(
            vec![s1_change, u2.clone()],
            130_000,
            TransactionState::InMempool,
        );
        storage.insert_speedup(s2.clone()).unwrap();
        storage
            .settle_tx(s2.txid, TransactionState::Finalized, 2)
            .unwrap();
        FundingStorage::replace_funding_on_finalize(&storage, s2.txid).unwrap();

        // Both S1 entry and U2 entry removed; S2 inserted at smallest matched pos.
        let records = FundingStorage::read_funding_records(&storage).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].txid, s2.txid);
        assert!(matches!(records[0].kind, TxKind::Speedup(_)));
        assert!(storage.get_tx_by_id(s1.txid).unwrap().is_none());
        assert!(storage.get_tx_by_id(u2.txid).unwrap().is_none());
        let sl = storage.get_speedups_ordered().unwrap();
        assert!(!sl.iter().any(|t| t.txid == s1.txid));
        assert!(!sl.iter().any(|t| t.txid == s2.txid));

        drop(storage);
        storage_backend.remove().unwrap();
    }

    /// Defensive paths of `replace_funding_on_finalize`. None are fatal:
    ///   1. Missing txid.
    ///   2. Speedup whose funding_inputs match no queue entry.
    ///   3. Non-Speedup tx (Normal / Funding / NeedsSpeedup).
    #[test]
    fn test_replace_funding_on_finalize_defensive_paths() {
        let storage_backend = StorageTestConfig::new();
        let storage = new_storage(&storage_backend);
        let pub_key = dummy_pubkey();

        // Seed a queue entry that must remain untouched across all 3 calls.
        let untouched = Utxo::new(random_txid(), 0, 7_777, &pub_key);
        FundingStorage::append_funding(&storage, untouched.clone()).unwrap();
        let snapshot = || FundingStorage::read_funding_records(&storage).unwrap();
        assert_eq!(snapshot().len(), 1);

        // 1. Missing txid.
        FundingStorage::replace_funding_on_finalize(&storage, random_txid()).unwrap();
        assert_eq!(snapshot().len(), 1);

        // 2. Speedup with funding_inputs that don't match any queue entry.
        let orphan = cpfp_with_change(
            vec![Utxo::new(random_txid(), 0, 1_000, &pub_key)],
            500,
            TransactionState::Finalized,
        );
        storage.insert_speedup(orphan.clone()).unwrap();
        FundingStorage::replace_funding_on_finalize(&storage, orphan.txid).unwrap();
        assert_eq!(snapshot().len(), 1);
        // Orphan speedup tx record left intact (not moved into FundingList).
        assert!(storage.get_tx_by_id(orphan.txid).unwrap().is_some());

        // 3. Wrong-kind tx (Normal).
        let normal_id = random_txid();
        storage
            .insert_tx(dummy_tx(normal_id, TransactionState::Finalized))
            .unwrap();
        FundingStorage::replace_funding_on_finalize(&storage, normal_id).unwrap();
        assert_eq!(snapshot().len(), 1);
        assert_eq!(snapshot()[0].txid, untouched.txid);

        drop(storage);
        storage_backend.remove().unwrap();
    }
}
