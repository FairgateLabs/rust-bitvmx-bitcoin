use bitcoin::{Transaction, Txid};
use bitvmx_bitcoin_rpc::types::BlockHeight;
use protocol_builder::types::Utxo;
use serde::{Deserialize, Serialize};

struct CoordinatedTx {
    txid: Txid,
    tx: Transaction,

    kind: TxKind,

    state: TransactionState,

    // lifecycle
    broadcast_block_height: BlockHeight,
    target_block_height: BlockHeight,
    stuck_in_mempool_blocks: u32,
    confirmation_trigger: u32,

    // retry
    retry_count: u32,
    last_retry_timestamp: Option<u64>,

    fee_info: FeeInfo,

    context: String,
}

struct FeeInfo {
    fee: u64,
    fee_rate: u64,
    weight: u64,
}

enum TxKind {
    Normal,
    Speedup(SpeedupKind),
}

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

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub enum TransactionState {
    ToDispatch,
    InMempool,
    Confirmed,
    Finalized,
    Failed,
}
