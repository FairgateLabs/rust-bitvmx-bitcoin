use bitcoin::Txid;
use protocol_builder::types::Utxo;
use std::rc::Rc;
use tracing::warn;

use crate::{
    config::config::FundingSettings,
    errors::BitcoinCoordinatorError,
    types::{CoordinatedTx, CoordinatorNews, TransactionState, TxKind},
};

/// Storage operations the funding manager needs.
pub trait FundingStorage {
    /// Read every `TxKind::Funding` record in insertion order.
    fn read_funding_records(&self) -> Result<Vec<CoordinatedTx>, BitcoinCoordinatorError>;

    /// Append a new funding record with `spent = false`.
    fn append_funding(&self, utxo: Utxo) -> Result<(), BitcoinCoordinatorError>;

    /// Toggle the spent flag on a stored record.
    fn set_spent(&self, txid: Txid, spent: bool) -> Result<(), BitcoinCoordinatorError>;

    /// Remove every funding record.
    fn clear_funding_records(&self) -> Result<(), BitcoinCoordinatorError>;

    /// On finalization of the speedup, replace every funding record whose utxo matches one of the speedup's
    /// `funding_inputs` with a single new funding record holding the speedup's change output.
    fn replace_funding_on_finalize(&self, txid: Txid) -> Result<(), BitcoinCoordinatorError>;
}

pub struct FundingManager {
    settings: FundingSettings,
    storage: Rc<dyn FundingStorage>,
}

impl FundingManager {
    pub fn new(settings: FundingSettings, storage: Rc<dyn FundingStorage>) -> Self {
        Self { settings, storage }
    }

    /// Validate `utxo` and append it as an unspent funding record.
    pub fn set_funding(
        &self,
        utxo: Utxo,
    ) -> Result<Option<CoordinatorNews>, BitcoinCoordinatorError> {
        match self.validate(&utxo) {
            Ok(()) => {
                self.storage.append_funding(utxo)?;
                Ok(None)
            }
            Err(news) => {
                warn!("FundingManager: invalid funding utxo: {:?}", utxo);
                Ok(Some(news))
            }
        }
    }

    pub fn clear_funding(&self) -> Result<(), BitcoinCoordinatorError> {
        self.storage.clear_funding_records()
    }

    pub fn has_funding(&self) -> Result<bool, BitcoinCoordinatorError> {
        Ok(!self.storage.read_funding_records()?.is_empty())
    }

    /// First-call. Returns the chosen UTXO and a boolean. The bool is `true` for Speedup-kind
    /// records and `false` for plain Funding-kind records. This is because combine is only valid
    ///  when the primary is Speedup-kind.
    ///
    /// - Pass 1: latest live speedup in SpeedupList that is not being replaced and not already spent.
    /// - Pass 2: first unspent record in FundingList (Funding or Speedup kind).
    /// - `None`: no funding available.
    pub fn get_funding(
        &self,
        speedups: &[CoordinatedTx],
    ) -> Result<Option<(Utxo, bool)>, BitcoinCoordinatorError> {
        // Pass 1: live unspent chain tip from SpeedupList.
        for tx in speedups.iter().rev() {
            if !matches!(
                tx.state,
                TransactionState::InMempool
                    | TransactionState::Confirmed
                    | TransactionState::Finalized
            ) {
                continue;
            }
            let ctx = tx.speedup_kind()?.context();
            if tx.has_live_replacement(speedups) || ctx.spent {
                continue;
            }
            let (utxo, _spent) = tx.get_funding_info()?;
            self.storage.set_spent(tx.txid, true)?;
            return Ok(Some((utxo, true)));
        }

        // Pass 2: first unspent record in FundingList. May be a plain Funding entry (user-provided) or
        // a Speedup entry (finalized chain tip moved into the queue by `replace_funding_on_finalize`).
        for record in self.storage.read_funding_records()? {
            let (utxo, spent) = record.get_funding_info()?;
            if spent {
                continue;
            }
            self.storage.set_spent(record.txid, true)?;
            let is_speedup = matches!(record.kind, TxKind::Speedup(_));
            return Ok(Some((utxo, is_speedup)));
        }

        // No funding available.
        Ok(None)
    }

    /// Second-call when the primary alone cannot cover fee + dust. Returns the
    /// next unspent Funding-kind record from the FundingList and marks it spent.
    /// Does NOT auto-release on `None`.
    pub fn get_combine_funding(&self) -> Result<Option<Utxo>, BitcoinCoordinatorError> {
        for record in self.storage.read_funding_records()? {
            if !matches!(record.kind, TxKind::Funding(_)) {
                continue;
            }
            let (utxo, spent) = record.get_funding_info()?;
            if spent {
                continue;
            }
            self.storage.set_spent(record.txid, true)?;
            return Ok(Some(utxo));
        }
        Ok(None)
    }

    /// Unmark each UTXO in `utxos` when a build fails partway through.
    pub fn release_marks(&self, utxos: &[Utxo]) -> Result<(), BitcoinCoordinatorError> {
        for u in utxos {
            // Mark as unspent
            self.storage.set_spent(u.txid, false)?;
        }
        Ok(())
    }

    /// Release every funding input of `tx`.
    pub fn mark_parents_unspent(&self, tx: &CoordinatedTx) -> Result<(), BitcoinCoordinatorError> {
        let k = tx.speedup_kind()?;
        self.release_marks(&k.context().funding_inputs)
    }

    /// On finalization of the speedup, replace every funding record whose utxo matches one of the speedup's
    /// `funding_inputs` with a single new funding record holding the speedup's change output.
    pub fn replace_on_finalize(&self, txid: Txid) -> Result<(), BitcoinCoordinatorError> {
        self.storage.replace_funding_on_finalize(txid)
    }

    // ---------------------------------------------------------------------
    // Private
    // ---------------------------------------------------------------------

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
        config::config::{CoordinatorStorageSettings, FundingSettings},
        core::storage::CoordinatorStorage,
        test_utils::{utxo, StorageTestConfig},
        types::{FeeInfo, SpeedupContext, SpeedupKind},
    };
    use bitcoin::{
        absolute::LockTime,
        hashes::{sha256d, Hash},
        transaction::Version,
        Amount, Transaction, Txid,
    };

    const MIN: u64 = 10_000;

    // -----------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------

    fn settings() -> FundingSettings {
        FundingSettings {
            min_funding_amount_sats: MIN,
        }
    }

    fn make_manager() -> (FundingManager, Rc<CoordinatorStorage>) {
        let backend = StorageTestConfig::new();
        let storage = Rc::new(CoordinatorStorage::new(
            backend.get_raw_storage(),
            CoordinatorStorageSettings::default(),
        ));
        let mgr = FundingManager::new(settings(), Rc::clone(&storage) as Rc<dyn FundingStorage>);
        (mgr, storage)
    }

    fn get_funding(mgr: &FundingManager, storage: &CoordinatorStorage) -> Option<(Utxo, bool)> {
        mgr.get_funding(storage.get_speedups_ordered().unwrap().as_slice())
            .unwrap()
    }

    fn det_txid(seed: u8) -> Txid {
        Txid::from_raw_hash(sha256d::Hash::hash(&[seed; 32]))
    }

    fn speedup_tx(
        seed: u8,
        state: TransactionState,
        funding_input: Utxo,
        change_sats: u64,
    ) -> CoordinatedTx {
        let txid = det_txid(seed);
        let tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![],
            output: vec![bitcoin::TxOut {
                value: Amount::from_sat(change_sats),
                script_pubkey: bitcoin::ScriptBuf::new(),
            }],
        };
        CoordinatedTx {
            txid,
            tx,
            kind: TxKind::Speedup(SpeedupKind::CPFP {
                parents: vec![],
                context: SpeedupContext {
                    funding_inputs: vec![funding_input],
                    replaced_by: None,
                    bump_fee_used: 1.0,
                    parent_data: vec![],
                    spent: false,
                },
            }),
            state,
            broadcast_block_height: None,
            target_block_height: 0,
            stuck_in_mempool_blocks: None,
            confirmation_trigger: None,
            settled_block_height: None,
            fail_guard_until: None,
            retry_count: 0,
            fee_info: FeeInfo {
                fee: 0,
                fee_rate: 1,
                weight: 100,
            },
            context: "test".to_string(),
        }
    }

    fn funding_spent(storage: &CoordinatorStorage, txid: Txid) -> bool {
        match storage.get_tx_by_id(txid).unwrap().unwrap().kind {
            TxKind::Funding(d) => d.spent,
            _ => panic!("expected Funding"),
        }
    }

    fn speedup_spent(storage: &CoordinatorStorage, txid: Txid) -> bool {
        storage
            .get_tx_by_id(txid)
            .unwrap()
            .unwrap()
            .speedup_kind()
            .unwrap()
            .context()
            .spent
    }

    // A user adds funding UTXOs (valid and below-min), the manager hands them out in queue order,
    // marks them spent, exhausts the queue, and correctly resets after clear_funding.
    #[test]
    fn test_funding_lifecycle() {
        let (mgr, storage) = make_manager();

        // Below-min is rejected with news; nothing is stored.
        let news = mgr.set_funding(utxo(MIN - 1)).unwrap();
        assert!(matches!(
            news,
            Some(CoordinatorNews::InvalidFundingUtxo { .. })
        ));
        assert!(!mgr.has_funding().unwrap());

        // Two valid UTXOs are stored in insertion order.
        mgr.set_funding(utxo(MIN * 5)).unwrap();
        mgr.set_funding(utxo(MIN * 3)).unwrap();
        assert!(mgr.has_funding().unwrap());

        // Pass 2 returns the first UTXO and marks it spent; queue order is preserved.
        let (first, is_speedup) = get_funding(&mgr, &storage).unwrap();
        assert!(!is_speedup);
        assert_eq!(first.amount, MIN * 5);
        assert!(funding_spent(&storage, first.txid));

        let (second, is_speedup) = get_funding(&mgr, &storage).unwrap();
        assert!(!is_speedup);
        assert_eq!(second.amount, MIN * 3);
        assert!(funding_spent(&storage, second.txid));

        // Both exhausted.
        assert!(get_funding(&mgr, &storage).is_none());

        // clear_funding wipes the queue; new funding can be added afterwards.
        mgr.clear_funding().unwrap();
        assert!(!mgr.has_funding().unwrap());
        mgr.set_funding(utxo(MIN * 2)).unwrap();
        assert!(mgr.has_funding().unwrap());
    }

    // A live InMempool speedup (chain-tip) is always preferred over the user-provided queue.
    // Once the chain tip is being replaced or is ToDispatch (not yet live), the manager falls
    // through to the queue.
    #[test]
    fn test_chain_tip_preferred_over_queue() {
        let (mgr, storage) = make_manager();

        let q = utxo(MIN * 10);
        mgr.set_funding(q.clone()).unwrap();

        // A ToDispatch speedup is NOT live. Pass 1 must skip it and return the queue.
        let s_pending = speedup_tx(0, TransactionState::ToDispatch, q.clone(), MIN * 9);
        storage.insert_speedup(s_pending.clone()).unwrap();
        let (got, is_speedup) = get_funding(&mgr, &storage).unwrap();
        assert!(
            !is_speedup,
            "ToDispatch speedup must not count as chain tip"
        );
        assert_eq!(got.txid, q.txid);
        storage.set_spent(q.txid, false).unwrap(); // reset for next sub-scenario

        // An InMempool speedup is the chain tip and takes priority over the queue.
        let s1 = speedup_tx(1, TransactionState::InMempool, q.clone(), MIN * 8);
        storage.insert_speedup(s1.clone()).unwrap();

        let (got, is_speedup) = get_funding(&mgr, &storage).unwrap();
        assert!(is_speedup, "InMempool speedup must be preferred over queue");
        assert_eq!(got.txid, s1.txid);
        assert_eq!(got.amount, MIN * 8);
        assert!(speedup_spent(&storage, s1.txid));
        assert!(
            !funding_spent(&storage, q.txid),
            "queue UTXO must be untouched"
        );

        // Mark s1 as being replaced (and reset spent so pass 1 would otherwise pick it).
        let mut s1_replaced = storage.get_tx_by_id(s1.txid).unwrap().unwrap();
        if let TxKind::Speedup(SpeedupKind::CPFP {
            ref mut context, ..
        }) = s1_replaced.kind
        {
            context.replaced_by = Some(det_txid(99));
            context.spent = false;
        }
        storage.update_tx(&s1_replaced).unwrap();

        // Pass 1 skips the replaced tip; pass 2 returns the queue UTXO.
        let (got2, is_speedup2) = get_funding(&mgr, &storage).unwrap();
        assert!(
            !is_speedup2,
            "replaced tip must be skipped; fall through to queue"
        );
        assert_eq!(got2.txid, q.txid);
        assert!(funding_spent(&storage, q.txid));

        // Both sources exhausted.
        assert!(get_funding(&mgr, &storage).is_none());
    }

    // Three CPFP rounds. Each round the chain tip advances as speedups go InMempool and finalize.
    // replace_on_finalize keeps the funding queue pointing at the latest change output. A mismatch
    // on replace_on_finalize is also verified at the end.
    #[test]
    fn test_cpfp_chain_multi_round() {
        let (mgr, storage) = make_manager();

        // Round 1: fresh start, no speedups. Pass 2 returns the user's queue UTXO.
        let q1_amount = MIN * 100;
        mgr.set_funding(utxo(q1_amount)).unwrap();
        let (q1, is_speedup) = get_funding(&mgr, &storage).unwrap();
        assert!(!is_speedup);
        assert_eq!(q1.amount, q1_amount);

        // Build S1 consuming Q1.
        let c1_sats = q1_amount - 1_000;
        let s1 = speedup_tx(1, TransactionState::InMempool, q1.clone(), c1_sats);
        storage.insert_speedup(s1.clone()).unwrap();

        // Round 2: S1 is the live chain tip. Pass 1 returns S1's change.
        let (s1_change, is_speedup) = get_funding(&mgr, &storage).unwrap();
        assert!(is_speedup);
        assert_eq!(s1_change.txid, s1.txid);
        assert_eq!(s1_change.amount, c1_sats);
        assert!(speedup_spent(&storage, s1.txid));

        // Build S2 consuming S1's change.
        let c2_sats = c1_sats - 1_000;
        let s2 = speedup_tx(2, TransactionState::InMempool, s1_change.clone(), c2_sats);
        storage.insert_speedup(s2.clone()).unwrap();

        // S1 finalizes: queue entry for Q1 is replaced with S1's change.
        let mut s1_final = storage.get_tx_by_id(s1.txid).unwrap().unwrap();
        s1_final.state = TransactionState::Finalized;
        storage.update_tx(&s1_final).unwrap();
        mgr.replace_on_finalize(s1.txid).unwrap();

        // Round 3: S1 finalized+spent, S2 live. Pass 1 returns S2's change.
        let (s2_change, is_speedup) = get_funding(&mgr, &storage).unwrap();
        assert!(is_speedup);
        assert_eq!(s2_change.txid, s2.txid);
        assert_eq!(s2_change.amount, c2_sats);
        assert!(speedup_spent(&storage, s2.txid));

        // Build S3 consuming S2's change.
        let c3_sats = c2_sats - 1_000;
        let s3 = speedup_tx(3, TransactionState::InMempool, s2_change.clone(), c3_sats);
        storage.insert_speedup(s3.clone()).unwrap();

        // S2 finalizes: queue advances from S1's change to S2's change.
        let mut s2_final = storage.get_tx_by_id(s2.txid).unwrap().unwrap();
        s2_final.state = TransactionState::Finalized;
        storage.update_tx(&s2_final).unwrap();
        mgr.replace_on_finalize(s2.txid).unwrap();

        // Funding queue now holds S2's change as the single entry.
        let records = storage.read_funding_records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].txid, s2.txid);

        // replace_on_finalize on a tx whose funding_inputs don't match any queue entry
        // is treated defensively: warn + Ok(()), queue untouched.
        let unrelated = speedup_tx(9, TransactionState::Finalized, utxo(MIN * 50), MIN * 40);
        storage.insert_tx(unrelated.clone()).unwrap();
        mgr.replace_on_finalize(unrelated.txid).unwrap();
        let records_after = storage.read_funding_records().unwrap();
        assert_eq!(records_after.len(), 1);
        assert_eq!(records_after[0].txid, s2.txid);
    }

    // A sub-minimum leftover sits at the front of the queue. A second UTXO is added by the user. get_funding
    // picks the leftover as primary; get_combine_funding picks the second as the combine partner. On build
    // failure, release_marks restores both. When there is no combine partner at all, get_combine_funding
    // returns None and auto-releases the primary.
    #[test]
    fn test_combine_leftover_sweep_and_release() {
        let (mgr, storage) = make_manager();

        // A sub-minimum leftover enters the queue directly.
        let leftover = utxo(MIN / 2);
        storage.append_funding(leftover.clone()).unwrap();

        // A proper second UTXO from the user.
        let q2 = utxo(MIN * 5);
        mgr.set_funding(q2.clone()).unwrap();

        // get_funding returns the leftover (first unspent in queue).
        let (primary, is_speedup) = get_funding(&mgr, &storage).unwrap();
        assert!(!is_speedup);
        assert_eq!(primary.txid, leftover.txid);
        assert!(funding_spent(&storage, leftover.txid));
        assert!(!funding_spent(&storage, q2.txid));

        // get_combine_funding skips the already-spent leftover via the spent filter and returns Q2.
        let secondary = mgr.get_combine_funding().unwrap().unwrap();
        assert_eq!(secondary.txid, q2.txid);
        assert!(funding_spent(&storage, q2.txid));

        // Build failure: release_marks restores both entries.
        mgr.release_marks(&[primary, secondary]).unwrap();
        assert!(!funding_spent(&storage, leftover.txid));
        assert!(!funding_spent(&storage, q2.txid));

        // With only one UTXO in the queue, combine returns None and DOES NOT release.
        mgr.clear_funding().unwrap();
        mgr.set_funding(utxo(MIN * 3)).unwrap();
        let (sole, is_speedup) = get_funding(&mgr, &storage).unwrap();
        assert!(!is_speedup);
        let combine = mgr.get_combine_funding().unwrap();
        assert!(
            combine.is_none(),
            "no second entry means no combine partner"
        );
        assert!(
            funding_spent(&storage, sole.txid),
            "primary stays spent on combine None — caller owns the release"
        );
        // Caller releases explicitly.
        mgr.release_marks(&[sole.clone()]).unwrap();
        assert!(!funding_spent(&storage, sole.txid));
    }

    // get_funding claims a UTXO (marks it spent); the build later fails. mark_parents_unspent must
    // restore the spent flag so the next tick can retry with the same UTXO. The cycle then completes:
    // funding is re-claimed and the second build succeeds via get_funding returning the same UTXO again.
    #[test]
    fn test_failed_build_releases_funding() {
        let (mgr, storage) = make_manager();

        mgr.set_funding(utxo(MIN * 4)).unwrap();
        let (claimed, is_speedup) = get_funding(&mgr, &storage).unwrap();
        assert!(!is_speedup);
        assert!(funding_spent(&storage, claimed.txid));

        // A speedup in ToDispatch references the claimed UTXO as its funding input.
        let s = speedup_tx(5, TransactionState::ToDispatch, claimed.clone(), MIN * 2);
        storage.insert_speedup(s.clone()).unwrap();

        // Build fails: mark_parents_unspent releases the funding record.
        mgr.mark_parents_unspent(&s).unwrap();
        assert!(!funding_spent(&storage, claimed.txid));

        // Next round: same UTXO is available again.
        let (reclaimed, is_speedup) = get_funding(&mgr, &storage).unwrap();
        assert!(!is_speedup);
        assert_eq!(reclaimed.txid, claimed.txid);

        // A chain-tip speedup's context.spent is also released correctly via release_marks.
        let tip = speedup_tx(6, TransactionState::InMempool, reclaimed.clone(), MIN * 3);
        storage.insert_speedup(tip.clone()).unwrap();
        let (tip_funding, is_speedup) = get_funding(&mgr, &storage).unwrap();
        assert!(is_speedup);
        assert!(speedup_spent(&storage, tip.txid));
        mgr.release_marks(&[tip_funding]).unwrap();
        assert!(!speedup_spent(&storage, tip.txid));
    }

    // S1 is InMempool with a tiny change that cannot cover the next speedup fee alone. A
    // second user-provided queue UTXO Q2 is present. get_funding returns S1's chain-tip
    // change; get_combine_funding picks Q2. S2 is built with both as funding_inputs. When
    // S2 finalize, replace_on_finalize removes both old funding records and replaces them with
    // a single record pointing to S2's change output.
    #[test]
    fn test_combined_funding_and_replace_on_finalize() {
        let (mgr, storage) = make_manager();

        // A prior speedup's change is already in the funding queue, spent because S1
        // already consumed it when S1 was originally built.
        let s_prev_change = utxo(MIN * 50);
        storage.append_funding(s_prev_change.clone()).unwrap();
        storage.set_spent(s_prev_change.txid, true).unwrap();

        // S1: InMempool, consumes s_prev_change, tiny change output.
        let s1_change_sats = MIN / 3;
        let s1 = speedup_tx(
            1,
            TransactionState::InMempool,
            s_prev_change.clone(),
            s1_change_sats,
        );
        storage.insert_speedup(s1.clone()).unwrap();

        // Q2: fresh user-provided queue UTXO.
        let q2 = utxo(MIN * 20);
        mgr.set_funding(q2.clone()).unwrap();

        // Pass 1: S1's chain-tip change is returned; S1.context.spent is set.
        let (chain_tip, is_speedup) = get_funding(&mgr, &storage).unwrap();
        assert!(is_speedup);
        assert_eq!(chain_tip.txid, s1.txid);
        assert_eq!(chain_tip.amount, s1_change_sats);
        assert!(speedup_spent(&storage, s1.txid));

        // s_prev_change is already spent → skipped. Q2 is the combine partner.
        let secondary = mgr.get_combine_funding().unwrap().unwrap();
        assert_eq!(secondary.txid, q2.txid);
        assert!(funding_spent(&storage, q2.txid));

        // Build S2 with both funding inputs.
        let s2_change_sats = MIN * 10;
        let mut s2 = speedup_tx(
            2,
            TransactionState::InMempool,
            chain_tip.clone(),
            s2_change_sats,
        );
        if let TxKind::Speedup(SpeedupKind::CPFP {
            ref mut context, ..
        }) = s2.kind
        {
            context.funding_inputs = vec![chain_tip.clone(), secondary.clone()];
        }
        storage.insert_speedup(s2.clone()).unwrap();

        // S1 finalizes: s_prev_change funding record is replaced by a new record at
        // S1.txid carrying S1's change output (spent=true because S1.context.spent is true).
        let mut s1_fin = storage.get_tx_by_id(s1.txid).unwrap().unwrap();
        s1_fin.state = TransactionState::Finalized;
        storage.update_tx(&s1_fin).unwrap();
        mgr.replace_on_finalize(s1.txid).unwrap();

        let mid = storage.read_funding_records().unwrap();
        assert_eq!(mid.len(), 2, "S1's change + Q2 must both be in the queue");
        assert_eq!(mid[0].txid, s1.txid);
        assert_eq!(mid[1].txid, q2.txid);

        // S2 finalizes: both S1.txid and Q2.txid funding records are matched by
        // S2.funding_inputs and replaced by a single record for S2's change output.
        let mut s2_fin = storage.get_tx_by_id(s2.txid).unwrap().unwrap();
        s2_fin.state = TransactionState::Finalized;
        storage.update_tx(&s2_fin).unwrap();
        mgr.replace_on_finalize(s2.txid).unwrap();

        let final_records = storage.read_funding_records().unwrap();
        assert_eq!(
            final_records.len(),
            1,
            "both old records must be replaced by one"
        );
        assert_eq!(final_records[0].txid, s2.txid);
        match &final_records[0].kind {
            TxKind::Speedup(k) => {
                assert!(
                    !k.context().spent,
                    "S2 was never claimed; context.spent stays false"
                );
            }
            _ => panic!("expected TxKind::Speedup (kind preserved on finalize)"),
        }
        let (change_out, _) = final_records[0].last_output().unwrap();
        assert_eq!(change_out.value.to_sat(), s2_change_sats);
    }
}
