use crate::types::{
    CoordinatedTx,
    TransactionState::{self, *},
};
use bitvmx_bitcoin_rpc::types::BlockHeight;

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

impl std::fmt::Display for TransactionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToDispatch => write!(f, "ToDispatch"),
            InMempool => write!(f, "InMempool"),
            Confirmed => write!(f, "Confirmed"),
            Finalized => write!(f, "Finalized"),
            Failed => write!(f, "Failed"),
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
    /// Returns `false` if the threshold is disabled (`stuck_in_mempool_blocks
    /// == 0`) or if the transaction has not been broadcast yet
    /// (`broadcast_block_height == 0`).
    pub fn is_stuck_in_mempool(&self, current_height: BlockHeight) -> bool {
        self.stuck_in_mempool_blocks > 0
            && self.broadcast_block_height > 0
            && current_height.saturating_sub(self.broadcast_block_height)
                >= self.stuck_in_mempool_blocks
    }
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
            broadcast_block_height: 0,
            target_block_height: target,
            stuck_in_mempool_blocks: 0,
            confirmation_trigger: 0,
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
        let make_tx = |broadcast: BlockHeight, threshold: u32| CoordinatedTx {
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
            confirmation_trigger: 0,
            retry_count: 0,
            fee_info: FeeInfo {
                fee: 0,
                fee_rate: 1,
                weight: 0,
            },
            context: String::new(),
        };

        // threshold disabled
        assert!(!make_tx(100, 0).is_stuck_in_mempool(200));
        // not yet broadcast
        assert!(!make_tx(0, 10).is_stuck_in_mempool(200));
        // below threshold
        assert!(!make_tx(100, 10).is_stuck_in_mempool(109));
        // exactly at threshold
        assert!(make_tx(100, 10).is_stuck_in_mempool(110));
        // above threshold
        assert!(make_tx(100, 10).is_stuck_in_mempool(200));
    }
}
