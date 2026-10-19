# rust-bitvmx-bitcoin

`rust-bitvmx-bitcoin` is the Bitcoin coordinator library for the BitVMX client. It sits between application code and a Bitcoin node, taking care of broadcasting transactions, monitoring their lifecycle on-chain, and keeping stuck transactions moving via Child-Pays-For-Parent (CPFP) and Replace-By-Fee (RBF) speedups.

## Documentation

The `docs/` folder explains the concepts a new developer needs without reading the source:

- [`docs/architecture.md`](docs/architecture.md): components, the tick pipeline, the transaction lifecycle, dispatch classification, and the news stream.
- [`docs/speedup.md`](docs/speedup.md): CPFP vs RBF, the funding chain, and the fee model.
- [`docs/design.md`](docs/design.md): the durable rules and invariants the coordinator is built on, plus a glossary.

## ⚠️ Disclaimer

This library is currently under development and may not be fully stable. It is not production-ready, has not been audited, and future updates may introduce breaking changes without preserving backward compatibility.

## Key Features

- 📤 **Transaction Dispatch**: Broadcasts signed transactions through the Bitcoin RPC, with retry, target-block-height gating, and topological ordering for parent/child pairs sent in the same tick.
- 🕵️ **Transaction Monitoring**: Tracks the full lifecycle of each registered transaction (`ToDispatch` → `InMempool` → `Confirmed` → `Finalized`), including reorg detection and stuck-in-mempool detection.
- 🚀 **Automatic Speedups**: Builds and dispatches CPFP or RBF transactions automatically when parents linger in the mempool, escalating fees on each attempt.
- 💰 **Funding Management**: Maintains a funding UTXO chain that survives reorgs and mempool evictions, so speedup fees always come from a known spendable output. Funding is keyed by `OutPoint` (txid + vout), so several UTXOs from the same funding transaction can be registered without collision.
- 📰 **News Stream**: Emits structured news for state transitions, dispatch errors, stuck transactions, and other lifecycle events that the client acknowledges explicitly.
- 💾 **Persistent Storage**: All coordinator state is persisted through `rust-bitvmx-storage-backend`, so a restart resumes work without losing in-flight transactions.

## Architecture

The coordinator is composed of small, focused components wired together inside `BitcoinCoordinator`:

| Component | Responsibility |
|---|---|
| `Monitor` (external) | Indexes blocks, mempool, and exposes per-tx status. |
| `TransactionEngine` | Reviews active txs, retries, dispatches `ToDispatch` txs. |
| `SpeedupEngine` | Builds CPFP/RBF speedups for stuck parents. |
| `Dispatcher` | Validates and broadcasts txs through the RPC. |
| `FeeManager` | Computes fee rates and speedup fees. |
| `FundingManager` | Tracks the spendable funding UTXO. |
| `CoordinatorStorage` | Persists `CoordinatedTx` records and news. |

Each call to `tick()` runs one pass: it advances the monitor, reviews in-flight transactions, dispatches anything ready, and schedules speedups for parents that need them. Build/save of a new speedup happens in one tick and the broadcast happens in the next, with at most one pre-built speedup in flight at a time.

See [`docs/architecture.md`](docs/architecture.md) for the full tick pipeline and component diagram, and [`docs/design.md`](docs/design.md) for the invariants behind the build/dispatch split.

## Public API

> ⚠️ **Indirect dependencies must wait for finality.** If transaction B indirectly depends on transaction A, do not register B until A has reached `Finalized`. While A is still in flight it may disappear from the mempool and be re-dispatched, and the coordinator does not preserve any ordering between independently registered transactions.

> ⚠️ **`add_funding` UTXOs must be effectively final.** Only pass UTXOs whose funding transaction is deep enough on-chain that you accept it as `Finalized` (i.e., no longer reorgable in practice).

> ⚠️ **Reorgs deeper than the monitor's `finality` are assumed impossible.** A tx is `Finalized` at `finality` confirmations and treated as permanent; a reorg past it is out of scope and not detected.

> ⚠️ **Tick once before any other call.** The coordinator's height is the monitor's indexed height, which is unknown until the first `tick()`. A `dispatch*` call before it returns an error.

> ⚠️ **`confirmation_trigger` must be within `finality..=max_monitoring_confirmations`, or `None`.** Out of range, the `dispatch*` call returns an error and nothing is stored.

> ⚠️ **External (non-coordinator-tracked) parents must already be confirmed.** The dispatcher only gates on tracked parents; untracked ones are assumed on-chain. Registering a tx whose input references an unregistered, not-yet-confirmed tx will be rejected by bitcoind with a missing-input error.

> 💡 **Stuck plain transaction.** `TransactionStuckInMempool` fires once per block past the threshold. To unblock: dispatch a CPFP spending one of its outputs via `dispatch_without_speedup`. The coordinator tracks both and mines them together.

> ⚠️ **`cancel` only works pre-dispatch.** Only `Normal` / `NeedsSpeedup` txs still in `ToDispatch` and never broadcast are cancellable. Once a tx has been broadcast (`InMempool` / `Confirmed` / `Finalized` / `Failed`, or back in `ToDispatch` after a broadcast), or if the txid is internal (Speedup) or a funding UTXO, the request is refused and a news item is emitted. A txid the coordinator does not know under the given context (for example one registered only through `monitor`, or one it tracks under another context) is passed to the monitor's cancel, and the coordinator's own record is left untouched.

> ⚠️ **A txid can be registered only once while it is live.** A `dispatch*` call for a txid the coordinator still stores is discarded with a warning log, except when that tx settled `Failed`: then it is registered again from scratch (state `ToDispatch`, full retry budget).

> ⚠️ **Ack news only AFTER acting on it.** News is deduplicated by value. Acking early means a second occurrence of the same event within the same block will NOT re-fire.

The `BitcoinCoordinator` struct exposes the following methods:

| Method | Purpose |
|---|---|
| `new_with_paths` | Construct a coordinator with RPC config, storage, key manager, optional settings. |
| `is_ready` | Returns `true` once the monitor has caught up with the chain. |
| `tick` | Periodic processing: advance the monitor, review and dispatch active txs. |
| `dispatch` | Dispatch a tx with optional speedup support. |
| `dispatch_without_speedup` | Dispatch a plain tx with optional stuck-in-mempool detection. |
| `dispatch_with_speedup` | Dispatch a tx and enable CPFP/RBF speedups. |
| `cancel` | Takes monitoring targets and their context. Transaction targets: cancel monitoring + storage for txs still in `ToDispatch` and never broadcast (refused once broadcast). Unknown txids and other targets are forwarded to the monitor. |
| `add_funding` | Register a funding UTXO available for future speedups. |
| `get_transaction` | Query the on-chain / mempool status of a tx. `InMempool` is only reported for txs the coordinator dispatched. |
| `get_block` | Fetch a block by height and hash. `None` when the block is above the indexed height or was replaced by a reorg. |
| `get_news` | Retrieve unacknowledged monitor and coordinator news. `max_keys` limits the monitor news to the first `max_keys` transactions and block heights (`None` for all). |
| `ack_news` | Acknowledge a news item so it is not returned again. |
| `monitor` | Register monitoring targets under a context, with an optional confirmation trigger, without scheduling a dispatch. |

## Usage Example

```rust
use bitcoin_coordinator::{
    coordinator::BitcoinCoordinator,
    types::AckNews,
    MonitorTarget, TransactionStatus,
};
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
coordinator.monitor(
    &[MonitorTarget::Transaction(external_txid)],
    ctx.clone(),
    None, // confirmation_trigger
)?;

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
let news = coordinator.get_news(None)?;
for item in news.monitor_news {
    // ... handle item ...
    coordinator.ack_news(AckNews::Monitor(item))?;
}
for item in news.coordinator_news {
    // ... handle item ...
    coordinator.ack_news(AckNews::Coordinator(item))?;
}

// Query the current status of any tx, and the block it was confirmed in.
let status = coordinator.get_transaction(some_txid)?;
if let TransactionStatus::Confirmed { block_height, block_hash, .. } = &status {
    let block = coordinator.get_block(*block_height, block_hash)?;
}
```

## Configuration

Tuning constants live in `BitcoinSettings`. A sample YAML used by the test suite is in `config/coordinator_config.yaml`; every field is optional and falls back to the defaults in `src/config/settings.rs`.

> ⚠️ **Do not change `BitcoinSettings` values across a restart on the same storage.** Several invariants (ancestor accounting, fee-rate state, retention bounds) assume the configuration that produced the persisted state is the one used to resume it. Reload the coordinator with the same settings, or start from clean storage.

The main groups are:

- `coordinator`: retry interval and retry attempts for failed dispatches.
- `dispatcher`: maximum allowed transaction weight.
- `fee`: minimum, maximum, and base fee multipliers used by `FeeManager`. The `max_feerate_sat_vb` ceiling bounds the speedup PACKAGE effective fee rate (parents + child together), not the child transaction alone.
- `speedup`: maximum unconfirmed ancestors of a speedup package, resend spacing, and bump-fee percentage.
- `funding`: minimum sat amount required for a funding UTXO.
- `storage`: how long settled txs are tracked before eviction.
- `monitor`: `finality` (the `Finalized` depth and the fail-guard window), max confirmations to track, and indexer settings (forwarded to `bitvmx-transaction-monitor`).

The full settings table and the behavior each field drives are in [`docs/architecture.md`](docs/architecture.md#configuration).

## Development Setup

Prerequisites:

- Rust
- Docker, used by integration tests
- A Bitcoin node running with `-txindex=1` (required: see [`docs/design.md`](docs/design.md#failure-classification) for more detail)

Common commands:

```bash
# Build everything (lib + tests).
cargo build --tests

# Run the unit test suite.
cargo test --lib

# Run integration tests (require Docker running; one bitcoind per test binary).
cargo test
```

The tests of each test binary share one regtest node in the `bitcoin-regtest` Docker container, restarted only when a test needs different node flags. Each test gets its own storage. The container keeps running after the tests finish and the next run replaces it; remove it by hand with `docker rm -f bitcoin-regtest`.

## Contributing

Contributions are welcome! Please open an issue or submit a pull request on GitHub.

## License

This project is licensed under the MIT License. See [LICENSE](LICENSE) for details.

---

## 🧩 Part of the BitVMX Ecosystem

This repository is a component of the **BitVMX Ecosystem**, an open platform for disputable computation secured by Bitcoin. You can find the index of all BitVMX open-source components at [**FairgateLabs/BitVMX**](https://github.com/FairgateLabs/BitVMX).

---
