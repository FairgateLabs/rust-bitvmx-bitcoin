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
            (ToDispatch, Confirmed) => true, // dispatched but crash before InMempool record or someone else broadcast the tx
            (ToDispatch, Finalized) => true, // dispatched but crash before Confirmed record or someone else broadcast the tx
            (InMempool, Finalized) => true,  // confirmed so fast we never saw Confirmed

            // Requeue after not-found in mempool
            (InMempool, ToDispatch) => true,

            // Reorg: confirmed block rolled back
            (Confirmed, InMempool) => true,

            // Any state can fail
            (_, Failed) => true,

            // Idempotency
            (a, b) if a == b => true,

            _ => false,
        }
    }
}

impl CoordinatedTx {
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

//implement is_empty for News
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
            retry_count: 0,
            fee_info: FeeInfo {
                fee: 0,
                fee_rate: 1,
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
            retry_count: 0,
            fee_info: FeeInfo {
                fee: 0,
                fee_rate: 1,
                weight: 0,
            },
            context: String::new(),
        };

        // threshold disabled
        assert!(!make_tx(Some(100), None).is_stuck_in_mempool(200));
        // not yet broadcast
        assert!(!make_tx(None, Some(10)).is_stuck_in_mempool(200));
        // below threshold
        assert!(!make_tx(Some(100), Some(10)).is_stuck_in_mempool(109));
        // exactly at threshold
        assert!(make_tx(Some(100), Some(10)).is_stuck_in_mempool(110));
        // above threshold
        assert!(make_tx(Some(100), Some(10)).is_stuck_in_mempool(200));
    }

    #[test]
    fn test_speedup_kind_helpers() {
        // Normal tx → Err for both variants
        let normal = normal_coordinated_tx(1);
        assert!(normal.speedup_kind().is_err());

        let mut normal_mut = normal_coordinated_tx(2);
        assert!(normal_mut.speedup_kind_mut().is_err());

        // Speedup tx → Ok
        let speedup = cpfp_coordinated_tx(3, 1);
        assert!(speedup.speedup_kind().is_ok());

        let mut speedup_mut = cpfp_coordinated_tx(4, 1);
        let k = speedup_mut.speedup_kind_mut().unwrap();
        assert!(!k.is_rbf());
    }

    #[test]
    fn test_last_output() {
        // No outputs → Err
        let tx = normal_coordinated_tx(1);
        assert!(tx.last_output().is_err());

        // One output → vout = 0
        let mut tx = normal_coordinated_tx(2);
        tx.tx.output.push(TxOut {
            value: Amount::from_sat(1_000),
            script_pubkey: ScriptBuf::new(),
        });
        let (out, vout) = tx.last_output().unwrap();
        assert_eq!(vout, 0);
        assert_eq!(out.value.to_sat(), 1_000);

        // Two outputs → vout = 1 (last)
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

        // Correct single result → Ok
        assert!(
            verify_single_dispatch_result(txid, vec![(txid, DispatchOutcome::Success)]).is_ok()
        );

        // Empty → Err
        assert!(verify_single_dispatch_result(txid, vec![]).is_err());

        // Two results → Err
        assert!(verify_single_dispatch_result(
            txid,
            vec![
                (txid, DispatchOutcome::Success),
                (other, DispatchOutcome::Success)
            ]
        )
        .is_err());

        // Wrong txid → Err
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
}
