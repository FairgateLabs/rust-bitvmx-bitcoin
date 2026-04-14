use bitcoin::{Transaction, Txid};
use bitvmx_bitcoin_rpc::types::BlockHeight;
use bitvmx_transaction_monitor::types::{AckMonitorNews, MonitorNews};
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
pub enum SpeedupKind {
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
    /// No funding UTXO is available.
    FundingNotAvailable,
    /// Transaction has been in the mempool longer than `stuck_in_mempool_blocks` threshold.
    TransactionStuckInMempool {
        txid: Txid,
        context: String,
    },
    /// Transaction could not be dispatched after exhausting all retry attempts.
    DispatchError {
        txid: Txid,
        context: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct News {
    pub monitor_news: Vec<MonitorNews>,
    pub coordinator_news: Vec<CoordinatorNews>,
}

pub enum AckNews {
    Monitor(AckMonitorNews),
    Coordinator(CoordinatorNews),
}
