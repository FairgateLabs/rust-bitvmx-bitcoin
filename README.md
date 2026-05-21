# rust-bitvmx-bitcoin

`rust-bitvmx-bitcoin` is the Bitcoin coordinator library for the BitVMX client. It
sits between application code and a Bitcoin node, taking care of broadcasting
transactions, monitoring their lifecycle on-chain, and keeping stuck transactions
moving via Child-Pays-For-Parent (CPFP) and Replace-By-Fee (RBF) speedups.

It is a modular rewrite of `rust-bitcoin-coordinator` with a clearer separation
between the transaction engine, the speedup engine, the dispatcher, the fee
manager, and the funding manager.

## ⚠️ Disclaimer

This library is currently under development and may not be fully stable.
It is not production-ready, has not been audited, and future updates may
introduce breaking changes without preserving backward compatibility.

## Key Features

- 📤 **Transaction Dispatch**: Broadcasts signed transactions through the
  Bitcoin RPC, with retry, target-block-height gating, and topological ordering
  for parent/child pairs sent in the same tick.
- 🕵️ **Transaction Monitoring**: Tracks the full lifecycle of each registered
  transaction (`ToDispatch` → `InMempool` → `Confirmed` → `Finalized`), including
  reorg detection, orphan handling, and stuck-in-mempool detection.
- 🚀 **Automatic Speedups**: Builds and dispatches CPFP or RBF transactions
  automatically when parents linger in the mempool, escalating fees on each
  attempt.
- 💰 **Funding Management**: Maintains a funding UTXO chain that survives
  reorgs and mempool evictions, so speedup fees always come from a known
  spendable output.
- 📰 **News Stream**: Emits structured news for state transitions, dispatch
  errors, stuck transactions, and other lifecycle events that the client
  acknowledges explicitly.
- 💾 **Persistent Storage**: All coordinator state is persisted through
  `rust-bitvmx-storage-backend`, so a restart resumes work without losing
  in-flight transactions.

## Architecture

The coordinator is composed of small, focused components wired together inside
`BitcoinCoordinator`:

| Component | Responsibility |
|---|---|
| `Monitor` (external) | Indexes blocks, mempool, and exposes per-tx status. |
| `TransactionEngine` | Reviews active txs, retries, dispatches `ToDispatch` txs. |
| `SpeedupEngine` | Builds CPFP/RBF speedups for stuck parents. |
| `Dispatcher` | Validates and broadcasts txs through the RPC. |
| `FeeManager` | Computes fee rates and speedup fees. |
| `FundingManager` | Tracks the spendable funding UTXO. |
| `CoordinatorStorage` | Persists `CoordinatedTx` records and news. |

Each call to `tick()` runs one pass: it advances the monitor, reviews
in-flight transactions, dispatches anything ready, and schedules speedups for
parents that need them. Build/save of a new speedup happens in one tick and
the broadcast happens in the next, with at most one pre-built speedup in
flight at a time.

## Public API

> ⚠️ **Indirect dependencies must wait for finality.** If transaction B
> indirectly depends on transaction A, do NOT register B until A has reached
> `Finalized`. While A is still in flight it may disappear from the mempool
> and be re-dispatched, and the coordinator does not preserve any ordering
> between independently registered transactions.

The `BitcoinCoordinator` struct exposes the following methods (see
`src/coordinator.rs` for full Rustdoc):

| Method | Purpose |
|---|---|
| `new_with_paths` | Construct a coordinator with RPC config, storage, key manager, optional settings. |
| `is_ready` | Returns `true` once the monitor has caught up with the chain. |
| `tick` | Periodic processing: advance the monitor, review and dispatch active txs. |
| `dispatch` | Dispatch a tx with optional speedup support. |
| `dispatch_without_speedup` | Dispatch a plain tx with optional stuck-in-mempool detection. |
| `dispatch_with_speedup` | Dispatch a tx and enable CPFP/RBF speedups. |
| `cancel` | Cancel monitoring and remove tracked txs from storage. |
| `add_funding` | Register a funding UTXO available for future speedups. |
| `get_transaction` | Query the on-chain / mempool status of a tx. |
| `get_news` | Retrieve all unacknowledged monitor and coordinator news. |
| `ack_news` | Acknowledge a news item so it is not returned again. |
| `monitor` | Register data to be monitored without scheduling a dispatch. |

## Usage Example

```rust
use bitcoin_coordinator::{
    coordinator::BitcoinCoordinator,
    types::AckNews,
};
use bitvmx_transaction_monitor::types::{AckMonitorNews, TypesToMonitor};
use protocol_builder::types::{output::SpeedupData, Utxo};

// Construct the coordinator.
let coordinator = BitcoinCoordinator::new_with_paths(
    &rpc_config,
    storage.clone(),
    key_manager.clone(),
    None,
)?;

// Synchronize the coordinator with the blockchain (e.g., after startup or new blocks).
coordinator.tick()?;

// Bail out until the monitor is synced with the chain.
if !coordinator.is_ready()? {
    return Ok(());
}

// Track an external transaction without dispatching it.
let ctx = "ctx".to_string();
coordinator.monitor(TypesToMonitor::Transactions(
    vec![external_txid],
    ctx.clone(),
    None,
))?;

// Dispatch a transaction with CPFP support enabled.
let speedup_data = SpeedupData::new(speedup_utxo);
coordinator.dispatch(
    transaction.clone(),
    Some(speedup_data),
    ctx.clone(),
    None, // target_block_height
    None, // confirmation_trigger
)?;

// Or dispatch without speedup, opting into stuck-in-mempool detection.
coordinator.dispatch_without_speedup(
    transaction,
    ctx.clone(),
    None,
    None,
    Some(10),
)?;

// Provide a funding UTXO that speedups can spend.
coordinator.add_funding(Utxo::new(txid, vout, amount.to_sat(), &pubkey))?;

// Pull and acknowledge news.
let news = coordinator.get_news()?;
for item in news.monitor_news.iter() {
    // ... handle item ...
}
coordinator.ack_news(AckNews::Monitor(AckMonitorNews::Transaction(some_txid)))?;

// Query the current status of any tx.
let status = coordinator.get_transaction(some_txid)?;
```

## Configuration

Tuning constants live in `BitcoinSettings`. A sample YAML used by the test
suite is in `config/coordinator_config.yaml`; every field is optional and falls
back to the defaults in `src/config/settings.rs`. The main groups are:

- `coordinator`: retry interval and retry attempts for failed dispatches.
- `dispatcher`: maximum allowed transaction weight.
- `fee`: minimum, maximum, and base fee multipliers used by `FeeManager`.
- `speedup`: maximum unconfirmed speedups, RBF attempts, and bump-fee
  percentages.
- `funding`: minimum sat amount required for a funding UTXO.
- `storage`: how long settled txs are tracked before eviction.
- `monitor`: max confirmations to track and indexer settings (forwarded to
  `bitvmx-transaction-monitor`).

## Development Setup

Prerequisites:

- Rust
- Docker, used by integration tests

Common commands:

```bash
# Build everything (lib + tests).
cargo build --tests

# Run the unit test suite.
cargo test --lib

# Run integration tests (require Docker running; one bitcoind per test).
cargo test
```
## Contributing

Contributions are welcome! Please open an issue or submit a pull request on
GitHub.

## License

This project is licensed under the MIT License. See [LICENSE](LICENSE) for
details.

---

## 🧩 Part of the BitVMX Ecosystem

This repository is a component of the **BitVMX Ecosystem**, an open platform
for disputable computation secured by Bitcoin.
You can find the index of all BitVMX open-source components at
[**FairgateLabs/BitVMX**](https://github.com/FairgateLabs/BitVMX).

---
