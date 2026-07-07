use crate::{
    core::dispatcher::DispatchOutcome,
    errors::BitcoinCoordinatorError,
    types::{
        CoordinatedTx, News, SpeedupContext, SpeedupKind,
        TransactionState::{self, *},
        TxKind,
    },
};
use bitcoin::Txid;
use bitvmx_bitcoin_rpc::types::BlockHeight;
use protocol_builder::types::Utxo;

impl SpeedupKind {
    pub fn context(&self) -> &SpeedupContext {
        match self {
            SpeedupKind::CPFP { context, .. } | SpeedupKind::RBF { context, .. } => context,
        }
    }

    pub fn context_mut(&mut self) -> &mut SpeedupContext {
        match self {
            SpeedupKind::CPFP { context, .. } | SpeedupKind::RBF { context, .. } => context,
        }
    }

    pub fn is_rbf(&self) -> bool {
        matches!(self, SpeedupKind::RBF { .. })
    }

    pub fn parents(&self) -> &[Txid] {
        match self {
            SpeedupKind::CPFP { parents, .. } => parents,
            SpeedupKind::RBF { .. } => &[],
        }
    }
}

impl CoordinatedTx {
    /// Returns `&SpeedupKind` or `InvariantViolation` if this tx is not a Speedup.
    pub fn speedup_kind(&self) -> Result<&SpeedupKind, BitcoinCoordinatorError> {
        match &self.kind {
            TxKind::Speedup(k) => Ok(k),
            _ => Err(BitcoinCoordinatorError::InvariantViolation(format!(
                "expected Speedup, got {:?} for tx {}",
                self.kind, self.txid
            ))),
        }
    }

    /// Mutable variant of `speedup_kind`.
    pub fn speedup_kind_mut(&mut self) -> Result<&mut SpeedupKind, BitcoinCoordinatorError> {
        let txid = self.txid;
        match &mut self.kind {
            TxKind::Speedup(k) => Ok(k),
            _ => Err(BitcoinCoordinatorError::InvariantViolation(format!(
                "expected Speedup (mut) for tx {}",
                txid
            ))),
        }
    }

    /// Returns the last output and its vout index, or `InvariantViolation` if there are no outputs.
    pub fn last_output(&self) -> Result<(&bitcoin::TxOut, u32), BitcoinCoordinatorError> {
        match self.tx.output.last() {
            Some(out) => Ok((out, (self.tx.output.len() - 1) as u32)),
            None => Err(BitcoinCoordinatorError::InvariantViolation(format!(
                "tx {} has no outputs",
                self.txid
            ))),
        }
    }

    /// Build the funding UTXO backed by this speedup's change output (its last output), reusing the
    /// speedup's own funding pub_key. `InvariantViolation` if this tx is not a Speedup or has no outputs.
    pub fn speedup_change_utxo(&self) -> Result<Utxo, BitcoinCoordinatorError> {
        let k = self.speedup_kind()?;
        let (out, vout) = self.last_output()?;
        let pub_key = &k.context().funding_inputs[0].pub_key;
        Ok(Utxo::new(self.txid, vout, out.value.to_sat(), pub_key))
    }

    pub fn verify_tx_id(&self, txid: Txid) -> Result<(), BitcoinCoordinatorError> {
        if self.txid != txid {
            return Err(BitcoinCoordinatorError::InvariantViolation(format!(
                "expected txid {}, got {}",
                txid, self.txid
            )));
        }
        Ok(())
    }
}

/// Validates that `results` contains exactly one entry with a txid matching `expected_txid`.
pub fn verify_single_dispatch_result(
    expected_txid: Txid,
    results: Vec<(Txid, DispatchOutcome)>,
) -> Result<(Txid, DispatchOutcome), BitcoinCoordinatorError> {
    if results.len() != 1 {
        return Err(BitcoinCoordinatorError::InvariantViolation(format!(
            "dispatch returned {} results for single tx {}",
            results.len(),
            expected_txid
        )));
    }
    let result = results.into_iter().next().ok_or_else(|| {
        BitcoinCoordinatorError::InvariantViolation(format!(
            "dispatch returned no results for tx {}",
            expected_txid
        ))
    })?;
    if result.0 != expected_txid {
        return Err(BitcoinCoordinatorError::InvariantViolation(format!(
            "dispatch returned txid {} but expected {}",
            result.0, expected_txid
        )));
    }
    Ok(result)
}

/// Finds a tx by txid in a dispatch batch, or returns `InvariantViolation`.
pub fn find_tx_in_batch<'a>(
    txs: &'a [CoordinatedTx],
    txid: Txid,
) -> Result<&'a CoordinatedTx, BitcoinCoordinatorError> {
    txs.iter().find(|t| t.txid == txid).ok_or_else(|| {
        BitcoinCoordinatorError::InvariantViolation(format!(
            "dispatcher returned txid {} not present in dispatch batch",
            txid
        ))
    })
}

impl TransactionState {
    /// Returns `true` when transitioning from `self` to `next` is a valid
    /// lifecycle step.
    pub fn can_transition_to(&self, next: &TransactionState) -> bool {
        match (self, next) {
            // Normal forward flow
            (ToDispatch, InMempool) => true,
            (InMempool, Confirmed) => true,
            (Confirmed, Finalized) => true,

            // Crash recovery: tx already on-chain when we restart
            (ToDispatch, Confirmed) => true, // crash before InMempool record, or a confirmed tx transiently re-queued to ToDispatch found confirmed on resend
            (ToDispatch, Finalized) => true, // crash before Confirmed record, or a confirmed tx transiently re-queued to ToDispatch found finalized on resend
            (InMempool, Finalized) => true,  // confirmed so fast we never saw Confirmed

            // Requeue after not-found in mempool
            (InMempool, ToDispatch) => true,

            // Reorg: confirmed block rolled back
            (Confirmed, InMempool) => true,

            // Deep reorg of a previously-Confirmed speedup whose tx is now
            (Confirmed, ToDispatch) => true,

            // Settle to Failed only from live states.
            //   - `ToDispatch → Failed`: `handle_dispatch_result` on Fatal or retries-exhausted Retryable.
            //   - `InMempool → Failed`: `remove_replaced_rbf` walks the `replaces` chain when an RBF finalizes.
            (ToDispatch, Failed) => true,
            (InMempool, Failed) => true,

            // Idempotency
            (a, b) if a == b => true,

            _ => false,
        }
    }
}

impl CoordinatedTx {
    /// True when this speedup is being replaced by an RBF. Two signals, because they are written at different times:
    ///   (a) `replaced_by` is set, written only when the RBF is dispatched (`mark_accepted` or `mark_already_confirmed`).
    ///   (b) A live (`state != Failed`) RBF record whose `replaces == self.txid` exists. Set at RBF build time, before (a),
    ///       and it survives a restart where (a) was not yet written.
    pub fn has_live_replacement(&self, all_speedups: &[CoordinatedTx]) -> bool {
        matches!(&self.kind, TxKind::Speedup(k) if k.context().is_being_replaced())
            || all_speedups.iter().any(|s| {
                s.state != TransactionState::Failed
                    && matches!(&s.kind, TxKind::Speedup(SpeedupKind::RBF { replaces, .. }) if *replaces == self.txid)
            })
    }

    /// Returns `true` when the transaction is due to be dispatched at `current_height`.
    pub fn is_ready_to_dispatch(&self, current_height: BlockHeight) -> bool {
        current_height >= self.target_block_height
    }

    /// Returns `true` when the transaction has been waiting in the mempool for
    /// longer than its `stuck_in_mempool_blocks` threshold.
    ///
    /// Returns `false` if the threshold is disabled (`stuck_in_mempool_blocks`
    /// is `None`) or if the transaction has not been broadcast yet
    /// (`broadcast_block_height` is `None`).
    pub fn is_stuck_in_mempool(&self, current_height: BlockHeight) -> bool {
        match (self.stuck_in_mempool_blocks, self.broadcast_block_height) {
            (Some(threshold), Some(broadcast)) => {
                current_height.saturating_sub(broadcast) >= threshold
            }
            _ => false,
        }
    }
}

// Implement is_empty for News.
impl News {
    pub fn is_empty(&self) -> bool {
        self.monitor_news.is_empty() && self.coordinator_news.is_empty()
    }
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FeeInfo;
    use crate::{
        core::dispatcher::DispatchOutcome,
        test_utils::{cpfp_coordinated_tx, normal_coordinated_tx},
    };
    use bitcoin::{
        absolute::LockTime,
        hashes::{sha256d, Hash},
        transaction::Version,
        Amount, ScriptBuf, Transaction, TxOut,
    };

    #[test]
    fn test_is_ready_to_dispatch() {
        let make_tx = |target: BlockHeight| CoordinatedTx {
            txid: Txid::from_raw_hash(sha256d::Hash::hash(&[0u8; 32])),
            tx: Transaction {
                version: Version::TWO,
                lock_time: LockTime::ZERO,
                input: vec![],
                output: vec![],
            },
            kind: TxKind::Normal,
            state: ToDispatch,
            broadcast_block_height: None,
            target_block_height: target,
            stuck_in_mempool_blocks: None,
            confirmation_trigger: None,
            settled_block_height: None,
            fail_guard_until: None,
            retry_count: 0,
            fee_info: FeeInfo {
                fee: 0,
                fee_rate: 1,
                package_fee_rate: 1,
                weight: 0,
            },
            context: String::new(),
        };

        assert!(make_tx(100).is_ready_to_dispatch(100));
        assert!(make_tx(100).is_ready_to_dispatch(101));
        assert!(!make_tx(100).is_ready_to_dispatch(99));
    }

    #[test]
    fn test_is_stuck_in_mempool() {
        let make_tx = |broadcast: Option<BlockHeight>, threshold: Option<u32>| CoordinatedTx {
            txid: Txid::from_raw_hash(sha256d::Hash::hash(&[1u8; 32])),
            tx: Transaction {
                version: Version::TWO,
                lock_time: LockTime::ZERO,
                input: vec![],
                output: vec![],
            },
            kind: TxKind::Normal,
            state: InMempool,
            broadcast_block_height: broadcast,
            target_block_height: 0,
            stuck_in_mempool_blocks: threshold,
            confirmation_trigger: None,
            settled_block_height: None,
            fail_guard_until: None,
            retry_count: 0,
            fee_info: FeeInfo {
                fee: 0,
                fee_rate: 1,
                package_fee_rate: 1,
                weight: 0,
            },
            context: String::new(),
        };

        // Threshold disabled.
        assert!(!make_tx(Some(100), None).is_stuck_in_mempool(200));
        // Not yet broadcast.
        assert!(!make_tx(None, Some(10)).is_stuck_in_mempool(200));
        // Below threshold.
        assert!(!make_tx(Some(100), Some(10)).is_stuck_in_mempool(109));
        // Exactly at threshold.
        assert!(make_tx(Some(100), Some(10)).is_stuck_in_mempool(110));
        // Above threshold.
        assert!(make_tx(Some(100), Some(10)).is_stuck_in_mempool(200));
    }

    #[test]
    fn test_speedup_kind_helpers() {
        // Normal tx returns Err for both variants.
        let normal = normal_coordinated_tx(1);
        assert!(normal.speedup_kind().is_err());

        let mut normal_mut = normal_coordinated_tx(2);
        assert!(normal_mut.speedup_kind_mut().is_err());

        // Speedup tx returns Ok.
        let speedup = cpfp_coordinated_tx(3, 1);
        assert!(speedup.speedup_kind().is_ok());

        let mut speedup_mut = cpfp_coordinated_tx(4, 1);
        let k = speedup_mut.speedup_kind_mut().unwrap();
        assert!(!k.is_rbf());

        // RBF: exercises context_mut and parents RBF arms.
        let mut rbf = cpfp_coordinated_tx(5, 1);
        let replaced_txid = rbf.txid;
        let context = match &rbf.kind {
            TxKind::Speedup(SpeedupKind::CPFP { context, .. }) => context.clone(),
            _ => panic!("expected CPFP"),
        };
        rbf.kind = TxKind::Speedup(SpeedupKind::RBF {
            replaces: replaced_txid,
            new_funding_inputs: vec![],
            context,
        });
        let k = rbf.speedup_kind_mut().unwrap();
        assert!(k.is_rbf());
        assert!(k.parents().is_empty());
        k.context_mut().replaced_by = Some(replaced_txid);
    }

    #[test]
    fn test_last_output() {
        // No outputs returns Err.
        let tx = normal_coordinated_tx(1);
        assert!(tx.last_output().is_err());

        // One output: vout = 0.
        let mut tx = normal_coordinated_tx(2);
        tx.tx.output.push(TxOut {
            value: Amount::from_sat(1_000),
            script_pubkey: ScriptBuf::new(),
        });
        let (out, vout) = tx.last_output().unwrap();
        assert_eq!(vout, 0);
        assert_eq!(out.value.to_sat(), 1_000);

        // Two outputs: vout = 1 (last).
        tx.tx.output.push(TxOut {
            value: Amount::from_sat(2_000),
            script_pubkey: ScriptBuf::new(),
        });
        let (out, vout) = tx.last_output().unwrap();
        assert_eq!(vout, 1);
        assert_eq!(out.value.to_sat(), 2_000);
    }

    #[test]
    fn test_verify_single_dispatch_result() {
        let txid = Txid::from_raw_hash(sha256d::Hash::hash(&[1u8; 32]));
        let other = Txid::from_raw_hash(sha256d::Hash::hash(&[2u8; 32]));

        // Correct single result returns Ok.
        assert!(
            verify_single_dispatch_result(txid, vec![(txid, DispatchOutcome::Success)]).is_ok()
        );

        // Empty returns Err.
        assert!(verify_single_dispatch_result(txid, vec![]).is_err());

        // Two results returns Err.
        assert!(verify_single_dispatch_result(
            txid,
            vec![
                (txid, DispatchOutcome::Success),
                (other, DispatchOutcome::Success)
            ]
        )
        .is_err());

        // Wrong txid returns Err.
        assert!(
            verify_single_dispatch_result(txid, vec![(other, DispatchOutcome::Success)]).is_err()
        );
    }

    #[test]
    fn test_find_tx_in_batch() {
        let tx1 = normal_coordinated_tx(1);
        let tx2 = normal_coordinated_tx(2);
        let batch = vec![tx1.clone(), tx2.clone()];

        assert!(find_tx_in_batch(&batch, tx1.txid).is_ok());
        assert!(find_tx_in_batch(&batch, tx2.txid).is_ok());

        let missing = Txid::from_raw_hash(sha256d::Hash::hash(&[99u8; 32]));
        assert!(find_tx_in_batch(&batch, missing).is_err());
    }

    #[test]
    fn test_verify_tx_id() {
        let tx = normal_coordinated_tx(1);
        assert!(tx.verify_tx_id(tx.txid).is_ok());

        let other = Txid::from_raw_hash(sha256d::Hash::hash(&[99u8; 32]));
        assert!(tx.verify_tx_id(other).is_err());
    }

    #[test]
    fn test_can_transition_to() {
        use crate::types::TransactionState::*;

        // InMempool → Failed is valid (RBF chain cleanup).
        assert!(InMempool.can_transition_to(&Failed));

        // Idempotent transitions are always valid.
        assert!(ToDispatch.can_transition_to(&ToDispatch));
        assert!(Finalized.can_transition_to(&Finalized));

        // Invalid transitions return false.
        assert!(!Finalized.can_transition_to(&ToDispatch));
        assert!(!Finalized.can_transition_to(&InMempool));
        assert!(!Failed.can_transition_to(&InMempool));
        assert!(!Confirmed.can_transition_to(&Failed));
    }
}
