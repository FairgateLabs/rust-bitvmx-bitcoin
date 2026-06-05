use bitcoin::{Transaction, Txid};
use bitvmx_bitcoin_rpc::types::BlockHeight;
use bitvmx_transaction_monitor::types::{AckMonitorNews, MonitorNews};
use protocol_builder::types::{output::SpeedupData, Utxo};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub enum TransactionState {
    ToDispatch,
    InMempool,
    Confirmed,
    Finalized,
    Failed,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct CoordinatedTx {
    pub txid: Txid,
    pub tx: Transaction,

    pub kind: TxKind,

    pub state: TransactionState,

    // Lifecycle
    pub target_block_height: BlockHeight, // Earliest block at which to dispatch
    pub confirmation_trigger: Option<u32>, // Emit confirmation news at this confirmation count; None = disabled
    pub stuck_in_mempool_blocks: Option<u32>, // Emit StuckInMempool news after this many mempool blocks; None = disabled
    pub settled_block_height: Option<BlockHeight>, // Block when tx reached Finalized/Failed; None = not yet settled
    pub broadcast_block_height: Option<BlockHeight>, // Block when tx was broadcast; None = not yet broadcast

    // Retry
    pub retry_count: u32,
    pub fee_info: FeeInfo,

    pub context: String,
}

// Compare by txid since txids uniquely identify transactions.
impl PartialEq for CoordinatedTx {
    fn eq(&self, other: &Self) -> bool {
        self.txid == other.txid
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct FeeInfo {
    pub fee: u64,
    pub fee_rate: u64,
    pub weight: u64,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub enum TxKind {
    Normal,
    /// Parent transaction awaiting CPFP creation. Carries the signing/UTXO
    /// metadata until the coordinator builds a CPFP in the same tick.
    NeedsSpeedup(SpeedupData),
    Speedup(SpeedupKind),
    /// User-provided funding UTXO tracked as a coordinator transaction.
    Funding(FundingData),
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct FundingData {
    pub utxo: Utxo,
    #[serde(default)]
    pub spent: bool,
}

/// Shared context stored in every speedup transaction regardless of CPFP/RBF variant.
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct SpeedupContext {
    /// UTXOs that funded this tx (1 entry normally, 2 when a leftover is swept).
    pub funding_inputs: Vec<Utxo>,
    /// Set when this transaction has been superseded by an RBF replacement.
    pub replaced_by: Option<Txid>,
    /// Bump-fee multiplier used; escalated multiplicatively for each subsequent boost/RBF.
    pub bump_fee_used: f64,
    /// (signing_data, output_amount_sats, parent_vsize) for each parent included.
    /// Stored here because the RBF variant (`SpeedupKind::RBF`) only records `replaces`,
    /// not the original parents, so reconstruction would require a chain walk.
    pub parent_data: Vec<(SpeedupData, u64, usize)>,
    /// True when this speedup's change output has been consumed by a newer speedup as its funding input.
    #[serde(default)]
    pub spent: bool,
}

impl SpeedupContext {
    pub fn is_being_replaced(&self) -> bool {
        self.replaced_by.is_some()
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub enum SpeedupKind {
    CPFP {
        parents: Vec<Txid>,
        context: SpeedupContext,
    },
    RBF {
        replaces: Txid,
        /// Funding inputs newly claimed by this RBF beyond what the replaced predecessor already consumed.
        new_funding_inputs: Vec<Utxo>,
        context: SpeedupContext,
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
    /// Transaction has been evicted from coordinator storage after exceeding
    /// `max_tracking_confirmations` blocks in the `Finalized` or `Failed` state.
    TransactionEvicted {
        txid: Txid,
        context: String,
    },
    /// Funding UTXO has insufficient balance to cover a speedup fee.
    InsufficientFunds {
        available: u64,
        required: u64,
    },
    /// A speedup transaction could not be dispatched.
    SpeedupDispatchError {
        txid: Txid,
        context: String,
    },
    /// A speedup transaction was saved at the `max_feerate_sat_vb` cap.
    /// No further boosts will be applied to it.
    MaxFeeRateReached {
        txid: Txid,
        effective_fee_rate: u64,
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
