use bitcoin::{Transaction, Txid};
use bitvmx_bitcoin_rpc::types::BlockHeight;
use protocol_builder::types::Utxo;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub enum TransactionState {
    ToDispatch,
    InMempool,
    Confirmed,
    Finalized,
    Failed,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct CoordinatedTx {
    pub txid: Txid,
    pub tx: Transaction,

    pub kind: TxKind,

    pub state: TransactionState,

    // lifecycle
    pub broadcast_block_height: BlockHeight,
    pub target_block_height: BlockHeight,
    pub stuck_in_mempool_blocks: u32,
    pub confirmation_trigger: u32,

    // retry
    pub retry_count: u32,
    // pub last_retry_timestamp: Option<u64>,
    pub fee_info: FeeInfo,

    pub context: String,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct FeeInfo {
    pub fee: u64,
    pub fee_rate: u64,
    pub weight: u64,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub enum TxKind {
    Normal,
    Speedup(SpeedupKind),
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
enum SpeedupKind {
    CPFP {
        parents: Vec<Txid>,
        funding_input: Utxo,
        change_output: Utxo,
    },

    RBF {
        replaces_txid: Txid, //TODO: Add a REPLACED to possible status //TODO: possible bug: if block mines in the middle
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
//TODO: complete
pub enum CoordinatorNews {
    TxNotFound {
        txid: Txid,
    },
    InvalidStateTransition {
        txid: Txid,
        from: TransactionState,
        to: TransactionState,
    },
    EstimateFeerateTooHigh {
        estimated_fee_rate: u64,
        max_fee_rate: u64,
    },
    InvalidFundingUtxo {
        amount: u64,
        min_required: u64,
    },
    FundingNotAvailable,
    BitcoinClientError {
        tx_id: Txid,
        error: BitcoinBroadcastErrorKind,
    },
}

/// High–level categorization of errors returned by the Bitcoin node when
/// attempting to broadcast a transaction.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum BitcoinBroadcastErrorKind {
    /// The transaction is already known by the node (in mempool or confirmed).
    AlreadyKnown,
    /// The transaction was rejected by mempool policy (fee too low, mempool full, etc.).
    MempoolRejection,
    /// A network/connection/timeout error occurred while talking to the node.
    NetworkError,
    /// Any other unexpected error.
    Other,
}

impl BitcoinBroadcastErrorKind {
    pub fn from_error_message(error_msg: &str) -> Self {
        let msg = error_msg;

        // Already-known / already-confirmed transaction
        if msg.contains("already in mempool")
            || msg.contains("Transaction outputs already in utxo set")
        {
            return BitcoinBroadcastErrorKind::AlreadyKnown;
        }

        // Mempool policy / fee issues
        if msg.contains("mempool full")
            || msg.contains("insufficient priority")
            || msg.contains("min relay fee")
            || msg.contains("mempool min fee not met")
        {
            return BitcoinBroadcastErrorKind::MempoolRejection;
        }

        // Infrastructure / connectivity issues
        if msg.contains("network") || msg.contains("connection") || msg.contains("timeout") {
            return BitcoinBroadcastErrorKind::NetworkError;
        }

        BitcoinBroadcastErrorKind::Other
    }
}
