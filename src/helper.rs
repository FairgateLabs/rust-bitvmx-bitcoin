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
    let result = results.into_iter().next().unwrap();
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
    use crate::types::{CoordinatedTx, FeeInfo, TxKind};
    use bitcoin::hashes::{sha256d, Hash};
    use bitcoin::{absolute::LockTime, transaction::Version, Transaction, Txid};

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
}
