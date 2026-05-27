use protocol_builder::types::Utxo;
use std::rc::Rc;
use storage_backend::storage::{KeyValueStore, Storage};
use tracing::warn;

use crate::{
    config::config::FundingSettings,
    errors::BitcoinCoordinatorError,
    types::{CoordinatedTx, CoordinatorNews, TransactionState},
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

    /// Validate `utxo` and append it to the back of the funding queue. The
    /// head of the queue (index 0) is the active base; subsequent entries are
    /// pending fallbacks consumed in FIFO order by `advance_funding`.
    pub fn set_funding(
        &self,
        utxo: Utxo,
    ) -> Result<Option<CoordinatorNews>, BitcoinCoordinatorError> {
        match self.validate(&utxo) {
            Ok(()) => {
                let mut queue = self.read_queue()?;
                queue.push(utxo);
                self.write_queue(&queue)?;
                Ok(None)
            }
            Err(news) => {
                warn!("FundingManager: invalid funding utxo: {:?}", utxo);
                Ok(Some(news))
            }
        }
    }

    /// Unified funding query. Returns the correct spendable UTXO for the current
    /// state of the speedup chain.
    ///
    /// Pass 1 (newest to oldest): last live speedup (`InMempool | Confirmed | Finalized`) whose
    /// change output meets `min_funding_amount_sats`. If the chain tip exists but is too small,
    /// older live txs are already spent by it, so Pass 1 stops and falls through.
    ///
    /// Pass 2: `get_base_funding()`, the head of the funding queue.
    pub fn get_funding(
        &self,
        speedups: &[CoordinatedTx],
    ) -> Result<Option<Utxo>, BitcoinCoordinatorError> {
        // Pass 1: live chain tip
        for tx in speedups.iter().rev() {
            if !matches!(
                tx.state,
                TransactionState::InMempool
                    | TransactionState::Confirmed
                    | TransactionState::Finalized
            ) {
                continue;
            }
            let k = tx.speedup_kind()?;
            // Skip a speedup that has been superseded by an RBF: its change UTXO
            // will be invalidated once the replacement lands.
            if k.context().is_being_replaced() {
                continue;
            }
            if let Some(out) = tx.tx.output.last() {
                let amount = out.value.to_sat();
                if amount >= self.settings.min_funding_amount_sats {
                    let vout = tx.tx.output.len().saturating_sub(1) as u32;
                    return Ok(Some(Utxo::new(
                        tx.txid,
                        vout,
                        amount,
                        &k.context().funding_input.pub_key,
                    )));
                }
            }
            // Chain tip is live but unusable (amount too small or no output).
            // Older live txs are already spent, so stop Pass 1.
            break;
        }
        // Pass 2: queue head (last finalized chain output, or user-provided funding)
        self.get_base_funding()
    }

    /// Return the head of the funding queue, or `None` if empty.
    pub fn get_base_funding(&self) -> Result<Option<Utxo>, BitcoinCoordinatorError> {
        Ok(self.read_queue()?.into_iter().next())
    }

    /// Pop the head of the queue and return the new head, or `None` if the
    /// queue is now empty.
    pub fn advance_funding(&self) -> Result<Option<Utxo>, BitcoinCoordinatorError> {
        let mut queue = self.read_queue()?;
        if queue.is_empty() {
            return Ok(None);
        }
        queue.remove(0);
        self.write_queue(&queue)?;
        Ok(queue.into_iter().next())
    }

    // Update the head of the queue with a new UTXO derived from a finalized speedup tx
    pub fn update_funding_from_tx(
        &self,
        tx: &CoordinatedTx,
    ) -> Result<(), BitcoinCoordinatorError> {
        let k = tx.speedup_kind()?;
        let (out, vout) = tx.last_output()?;
        self.update_funding(Utxo::new(
            tx.txid,
            vout,
            out.value.to_sat(),
            &k.context().funding_input.pub_key,
        ))?;
        Ok(())
    }

    /// Remove every funding UTXO from storage.
    pub fn clear_funding(&self) -> Result<(), BitcoinCoordinatorError> {
        self.storage.remove(FUNDING_KEY, None)?;
        Ok(())
    }

    /// Return `true` when at least one funding UTXO is currently queued.
    pub fn has_funding(&self) -> Result<bool, BitcoinCoordinatorError> {
        Ok(!self.read_queue()?.is_empty())
    }

    // -------------------------------------------------------------------------
    // Private helpers
    // -------------------------------------------------------------------------

    /// Overwrite the head of the funding queue without validation. Only
    /// for `Finalized` txs, so the head always holds a confirmed UTXO and is
    /// resilient to mempool evictions and reorgs.
    fn update_funding(&self, utxo: Utxo) -> Result<(), BitcoinCoordinatorError> {
        let mut queue = self.read_queue()?;
        if queue.is_empty() {
            queue.push(utxo);
        } else {
            queue[0] = utxo;
        }
        self.write_queue(&queue)?;
        Ok(())
    }

    fn read_queue(&self) -> Result<Vec<Utxo>, BitcoinCoordinatorError> {
        Ok(self.storage.get(FUNDING_KEY, None)?.unwrap_or_default())
    }

    fn write_queue(&self, queue: &[Utxo]) -> Result<(), BitcoinCoordinatorError> {
        self.storage.set(FUNDING_KEY, &queue, None)?;
        Ok(())
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
    use crate::{
        config::config::FundingSettings,
        test_utils::{utxo, StorageTestConfig},
        types::{FeeInfo, SpeedupContext, SpeedupKind, TxKind},
    };
    use bitcoin::{
        absolute::LockTime,
        hashes::{sha256d, Hash},
        transaction::Version,
        Amount, Transaction, Txid,
    };

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

    fn det_txid(seed: u8) -> Txid {
        Txid::from_raw_hash(sha256d::Hash::hash(&[seed; 32]))
    }

    /// Build a CPFP speedup tx with a deterministic txid (seed), given state,
    /// funding_input, and change amount.  Each step in a chain should pass
    /// `change_of(prev)` as the funding_input.
    fn speedup_tx(
        seed: u8,
        state: TransactionState,
        funding_input: Utxo,
        change_sats: u64,
    ) -> CoordinatedTx {
        let txid = det_txid(seed);
        let change = bitcoin::TxOut {
            value: Amount::from_sat(change_sats),
            script_pubkey: bitcoin::ScriptBuf::new(),
        };
        let tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![],
            output: vec![change],
        };
        let context = SpeedupContext {
            funding_input,
            replaced_by: None,
            bump_fee_used: 1.0,
            parent_data: vec![],
        };
        CoordinatedTx {
            txid,
            tx,
            kind: TxKind::Speedup(SpeedupKind::CPFP {
                parents: vec![],
                context,
            }),
            state,
            broadcast_block_height: None,
            target_block_height: 0,
            stuck_in_mempool_blocks: None,
            confirmation_trigger: None,
            settled_block_height: None,
            retry_count: 0,
            fee_info: FeeInfo {
                fee: 0,
                fee_rate: 1,
                weight: 100,
            },
            context: "test".to_string(),
        }
    }

    /// Derive the UTXO that the next tx in the chain should use as its
    /// funding_input
    fn change_of(tx: &CoordinatedTx) -> Utxo {
        let out = tx.tx.output.last().unwrap();
        let vout = (tx.tx.output.len() - 1) as u32;
        let pub_key = tx.speedup_kind().unwrap().context().funding_input.pub_key;
        Utxo::new(tx.txid, vout, out.value.to_sat(), &pub_key)
    }

    #[test]
    fn test_set_valid_funding_persists() {
        let (mgr, config) = make_manager();

        let news = mgr.set_funding(utxo(MIN)).unwrap();
        assert!(news.is_none());

        let stored = mgr.get_base_funding().unwrap();
        assert!(stored.is_some());
        assert_eq!(stored.unwrap().amount, MIN);

        drop(mgr);
        config.remove().unwrap();
    }

    #[test]
    fn test_invalid_funding_keeps_queue() {
        let (mgr, config) = make_manager();

        let valid = utxo(MIN);
        mgr.set_funding(valid.clone()).unwrap();

        let news = mgr.set_funding(utxo(MIN - 1)).unwrap();
        assert!(matches!(
            news,
            Some(CoordinatorNews::InvalidFundingUtxo { .. })
        ));

        // The invalid set_funding leaves the queue untouched.
        let head = mgr.get_base_funding().unwrap().unwrap();
        assert_eq!(head.txid, valid.txid);
        assert_eq!(head.amount, MIN);
        assert!(mgr.advance_funding().unwrap().is_none());

        drop(mgr);
        config.remove().unwrap();
    }

    #[test]
    fn test_add_funding_appends_to_queue() {
        let (mgr, config) = make_manager();

        let a = utxo(MIN);
        let b = utxo(MIN * 2);
        mgr.set_funding(a.clone()).unwrap();
        mgr.set_funding(b.clone()).unwrap();

        // Head is the first added.
        assert_eq!(mgr.get_base_funding().unwrap().unwrap().txid, a.txid);

        // After advancing, the second entry becomes the head.
        let new_head = mgr.advance_funding().unwrap().unwrap();
        assert_eq!(new_head.txid, b.txid);
        assert_eq!(mgr.get_base_funding().unwrap().unwrap().txid, b.txid);

        drop(mgr);
        config.remove().unwrap();
    }

    #[test]
    fn test_advance_funding_returns_none_when_empty() {
        let (mgr, config) = make_manager();

        // Empty queue.
        assert!(mgr.advance_funding().unwrap().is_none());

        // Single-entry queue: advancing leaves the queue empty.
        mgr.set_funding(utxo(MIN)).unwrap();
        assert!(mgr.advance_funding().unwrap().is_none());
        assert!(!mgr.has_funding().unwrap());

        drop(mgr);
        config.remove().unwrap();
    }

    #[test]
    fn test_update_funding_replaces_head_only() {
        let (mgr, config) = make_manager();

        let a = utxo(MIN);
        let b = utxo(MIN * 2);
        let c = utxo(MIN * 3);
        mgr.set_funding(a).unwrap();
        mgr.set_funding(b.clone()).unwrap();

        // Replace the head; the tail (b) must remain.
        mgr.update_funding(c.clone()).unwrap();
        assert_eq!(mgr.get_base_funding().unwrap().unwrap().txid, c.txid);
        assert_eq!(
            mgr.advance_funding().unwrap().unwrap().txid,
            b.txid,
            "second queue entry must be preserved across update_funding"
        );

        drop(mgr);
        config.remove().unwrap();
    }

    #[test]
    fn test_funding_queue_survives_restart() {
        let config = StorageTestConfig::new();
        let storage = config.get_raw_storage();

        let a = utxo(MIN);
        let b = utxo(MIN * 2);

        let mgr1 = FundingManager::new(settings(), Rc::clone(&storage));
        mgr1.set_funding(a.clone()).unwrap();
        mgr1.set_funding(b.clone()).unwrap();
        drop(mgr1);

        let mgr2 = FundingManager::new(settings(), Rc::clone(&storage));
        assert_eq!(mgr2.get_base_funding().unwrap().unwrap().txid, a.txid);
        assert_eq!(mgr2.advance_funding().unwrap().unwrap().txid, b.txid);
        drop(mgr2);

        drop(storage);
        config.remove().unwrap();
    }

    #[test]
    fn test_get_base_funding_when_empty() {
        let (mgr, config) = make_manager();

        let stored = mgr.get_base_funding().unwrap();
        assert!(stored.is_none());

        drop(mgr);
        config.remove().unwrap();
    }

    #[test]
    fn test_has_funding() {
        let (mgr, config) = make_manager();

        assert!(!mgr.has_funding().unwrap());
        mgr.set_funding(utxo(MIN)).unwrap();
        assert!(mgr.has_funding().unwrap());

        drop(mgr);
        config.remove().unwrap();
    }

    #[test]
    fn test_update_funding_overwrites_without_validation() {
        let (mgr, config) = make_manager();

        // Below-minimum amount is accepted by update_funding (no validation).
        mgr.update_funding(utxo(MIN - 1)).unwrap();
        let stored = mgr.get_base_funding().unwrap().unwrap();
        assert_eq!(stored.amount, MIN - 1);

        // Overwrite with a different UTXO.
        mgr.update_funding(utxo(MIN * 3)).unwrap();
        let stored = mgr.get_base_funding().unwrap().unwrap();
        assert_eq!(stored.amount, MIN * 3);

        drop(mgr);
        config.remove().unwrap();
    }

    #[test]
    fn test_clear_funding() {
        let (mgr, config) = make_manager();

        mgr.set_funding(utxo(MIN)).unwrap();
        mgr.clear_funding().unwrap();
        assert!(!mgr.has_funding().unwrap());

        drop(mgr);
        config.remove().unwrap();
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
        let stored = mgr2.get_base_funding().unwrap();
        assert!(stored.is_some());
        assert_eq!(stored.unwrap().amount, MIN * 2);
        drop(mgr2);

        drop(storage);
        config.remove().unwrap();
    }

    // -------------------------------------------------------------------------
    // get_funding tests
    // -------------------------------------------------------------------------

    // Pass 2: no speedups, no stored UTXO returns None.
    #[test]
    fn test_get_funding_empty() {
        let (mgr, config) = make_manager();
        assert!(mgr.get_funding(&[]).unwrap().is_none());
        drop(mgr);
        config.remove().unwrap();
    }

    // Pass 2: no speedups, stored UTXO returns the stored UTXO.
    #[test]
    fn test_get_funding_no_speedups() {
        let (mgr, config) = make_manager();
        mgr.set_funding(utxo(MIN * 4)).unwrap();
        assert_eq!(mgr.get_funding(&[]).unwrap().unwrap().amount, MIN * 4);
        drop(mgr);
        config.remove().unwrap();
    }

    // Pass 1: InMempool speedup returns its change output.
    #[test]
    fn test_get_funding_in_mempool() {
        let (mgr, config) = make_manager();
        let root = utxo(MIN * 4);
        let cpfp1 = speedup_tx(1, TransactionState::InMempool, root, MIN * 3);
        let result = mgr.get_funding(&[cpfp1.clone()]).unwrap().unwrap();
        assert_eq!(result.txid, cpfp1.txid);
        assert_eq!(result.vout, 0);
        assert_eq!(result.amount, MIN * 3);
        drop(mgr);
        config.remove().unwrap();
    }

    // Pass 1: newest live wins (CPFP2 Finalized over CPFP1 Confirmed).
    // Regression: derive_funding skipped Finalized, returning CPFP1's change (already spent).
    #[test]
    fn test_get_funding_confirmed_then_finalized() {
        let (mgr, config) = make_manager();
        let root = utxo(MIN * 4);
        let cpfp1 = speedup_tx(1, TransactionState::Confirmed, root, MIN * 3);
        let cpfp2 = speedup_tx(2, TransactionState::Finalized, change_of(&cpfp1), MIN * 2);
        let result = mgr.get_funding(&[cpfp1, cpfp2.clone()]).unwrap().unwrap();
        assert_eq!(result.txid, cpfp2.txid);
        assert_eq!(result.amount, MIN * 2);
        drop(mgr);
        config.remove().unwrap();
    }

    // Pass 1: both Finalized returns newer one's change.
    #[test]
    fn test_get_funding_all_finalized() {
        let (mgr, config) = make_manager();
        let root = utxo(MIN * 4);
        let cpfp1 = speedup_tx(1, TransactionState::Finalized, root, MIN * 3);
        let cpfp2 = speedup_tx(2, TransactionState::Finalized, change_of(&cpfp1), MIN * 2);
        let result = mgr.get_funding(&[cpfp1, cpfp2.clone()]).unwrap().unwrap();
        assert_eq!(result.txid, cpfp2.txid);
        assert_eq!(result.amount, MIN * 2);
        drop(mgr);
        config.remove().unwrap();
    }

    // Pass 2: all ToDispatch (chain evicted from mempool) returns stored base UTXO.
    #[test]
    fn test_get_funding_all_evicted() {
        let (mgr, config) = make_manager();
        let root = utxo(MIN * 4);
        mgr.set_funding(root.clone()).unwrap();
        let cpfp1 = speedup_tx(1, TransactionState::ToDispatch, root.clone(), MIN * 3);
        let cpfp2 = speedup_tx(2, TransactionState::ToDispatch, change_of(&cpfp1), MIN * 2);
        let result = mgr.get_funding(&[cpfp1, cpfp2]).unwrap().unwrap();
        assert_eq!(result.txid, root.txid);
        assert_eq!(result.amount, MIN * 4);
        drop(mgr);
        config.remove().unwrap();
    }

    // Pass 2: Failed speedup returns stored base UTXO (same invariant as above).
    #[test]
    fn test_get_funding_failed() {
        let (mgr, config) = make_manager();
        let root = utxo(MIN * 4);
        mgr.set_funding(root.clone()).unwrap();
        let cpfp1 = speedup_tx(1, TransactionState::Failed, root.clone(), MIN * 3);
        let result = mgr.get_funding(&[cpfp1]).unwrap().unwrap();
        assert_eq!(result.txid, root.txid);
        assert_eq!(result.amount, MIN * 4);
        drop(mgr);
        config.remove().unwrap();
    }

    // Pass 1 skips chain tip below min_funding_amount_sats; Pass 2 returns stored UTXO.
    // Ensures a fresh set_funding call is honoured even while a dust chain tip is live.
    #[test]
    fn test_get_funding_chain_tip_too_small_uses_stored() {
        let (mgr, config) = make_manager();
        let fresh = utxo(MIN * 4);
        mgr.set_funding(fresh.clone()).unwrap();
        let root = utxo(MIN * 2);
        let cpfp1 = speedup_tx(1, TransactionState::InMempool, root, MIN - 1);
        let result = mgr.get_funding(&[cpfp1]).unwrap().unwrap();
        assert_eq!(result.txid, fresh.txid);
        assert_eq!(result.amount, MIN * 4);
        drop(mgr);
        config.remove().unwrap();
    }

    // Pass 1 skips a live speedup whose `replaced_by` is set (`is_being_replaced()` == true)
    // and falls back to the next older live entry.
    #[test]
    fn test_get_funding_skips_being_replaced() {
        let (mgr, config) = make_manager();
        let root = utxo(MIN * 4);
        let cpfp1 = speedup_tx(1, TransactionState::InMempool, root, MIN * 3);
        let mut cpfp2 = speedup_tx(2, TransactionState::InMempool, change_of(&cpfp1), MIN * 2);
        // Mark cpfp2 as being replaced by an RBF.
        if let TxKind::Speedup(SpeedupKind::CPFP { ref mut context, .. }) = cpfp2.kind {
            context.replaced_by = Some(det_txid(99));
        }
        // Pass 1 (newest first): cpfp2 is_being_replaced → skip; cpfp1 is live → use its change.
        let result = mgr.get_funding(&[cpfp1.clone(), cpfp2]).unwrap().unwrap();
        assert_eq!(result.txid, cpfp1.txid);
        assert_eq!(result.amount, MIN * 3);
        drop(mgr);
        config.remove().unwrap();
    }

    #[test]
    fn test_skip_being_replaced() {
        let (mgr, config) = make_manager();
        let root = utxo(MIN * 4);
        let cpfp1 = speedup_tx(1, TransactionState::InMempool, root.clone(), MIN * 3);
        let mut cpfp2 = speedup_tx(2, TransactionState::InMempool, change_of(&cpfp1), MIN * 2);
        // Simulate cpfp1 being replaced by an RBF before it confirms.
        cpfp2.kind = TxKind::Speedup(SpeedupKind::RBF {
            replaces: cpfp1.txid,
            context: SpeedupContext {
                funding_input: change_of(&cpfp1),
                replaced_by: None,
                bump_fee_used: 1.0,
                parent_data: vec![],
            },
        });
        let result = mgr
            .get_funding(&[cpfp1.clone(), cpfp2.clone()])
            .unwrap()
            .unwrap();
        assert_eq!(result.txid, cpfp2.txid);
        assert_eq!(result.amount, MIN * 2);
        drop(mgr);
        config.remove().unwrap();
    }
}
