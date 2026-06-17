# Design rules and invariants

The durable rules the coordinator is built on. Each is stated as a fact about the
current system, with a short reason. For how the pieces fit together see
[architecture.md](architecture.md); for the speedup subsystem see
[speedup.md](speedup.md).

## Operating assumptions

These are the conditions the coordinator is designed for. Outside them, behavior
is not guaranteed.

| Assumption | Why it holds |
|---|---|
| A single coordinator instance runs against the storage. | State (slot accounting, funding chain, fee-rate state) assumes one writer. |
| The node runs with `-txindex=1`. | Classification reads live node state for any txid, even when its outputs are already spent. Asserted at construction. |
| The operator does not spend a registered funding UTXO or a registered parent out of band. | The coordinator owns those outputs and is the only intended spender. |
| Reorgs deeper than `max_monitoring_confirmations` do not happen. | A `Finalized` transaction is treated as permanent and is never re-examined. |
| Registered settings do not change across a restart on the same storage. | Persisted state assumes the configuration that produced it. |

## Invariants

Named guarantees that the tick pipeline preserves. They are referenced by name in
the code and the other docs.

### I1: build and dispatch live in different ticks

A speedup or CPFP is saved `ToDispatch` in one tick and broadcast in the next. A
crash between the two leaves a `ToDispatch` record that is simply retried. The
result is that the system never has a transaction on-chain without a local
record of it.

### I2: at most one pre-built speedup is in flight

Only one speedup sits in `ToDispatch` at a time. Reorg edges may briefly leave
two pre-dispatched speedups, which is safe because the network already accepted
them.

### I4: the CPFP builder yields to an existing pre-built speedup

`create_cpfp_batch` (step 6) does nothing if any speedup is already `ToDispatch`.
A boost built in step 5 therefore prevents a duplicate CPFP in step 6 of the same
tick.

## Pipeline ordering

The six tick steps run in a fixed order, and the order carries meaning.

| Rule | Reason |
|---|---|
| Review steps (1, 2) never broadcast. | A tick first reads a consistent chain view, then acts on it. |
| Non-speedups dispatch (3) before speedups (4). | A re-dispatched parent is back in the node mempool before its CPFP is re-broadcast in the same tick. |
| Build/save steps (5, 6) run last and only save. | The broadcast is deferred to the next tick, which is what makes I1 hold. |
| Decisions are made only when `is_ready()` is true. | The monitor advances at most one block per tick; acting only when synced means every classification reads the real chain tip, not a lagging view. |

## Failure classification

Failures are classified by live node state, not by matching the node's error
text. A single `getrawtransaction` answers mempool, confirmed, or absent
directly, which is why `-txindex=1` is required.

| Node state on a rejected broadcast | Verdict |
|---|---|
| In mempool (`Some(0)`) | Accept as `InMempool`. Covers duplicate broadcast and crash-between-send-and-record. |
| Confirmed (`Some(n>=1)`) | Settle `Confirmed`. |
| Absent and a funding input is gone | Settle `Failed` and recreate funding, after the fail guard. |
| Absent and an external or parent input is gone | Settle `Failed`, after the fail guard. |
| Absent but all inputs intact | Transient cause. Retry until the budget is spent, then `Failed`. |

A structural problem decidable from the transaction alone (weight over
`max_tx_weight`, or zero inputs) is settled `Failed` immediately with no node
round-trip.

## The reorg-flap fail guard

An "input consumed" verdict is reversible while a reorg is still unsettled, so it
is not acted on immediately.

- When review finds an already-broadcast transaction `not_found` (gone from both
  chain and mempool), it re-queues the transaction `ToDispatch` and arms
  `fail_guard_until = current_height + max_monitoring_confirmations`. The anchor
  is set once, at the first `not_found`.
- While the chain height is below `fail_guard_until`, an input-consumed verdict is
  deferred: the transaction stays `ToDispatch` and is re-dispatched, paced by the
  retry interval.
- Past the window, the verdict is final and the transaction settles `Failed`.
- Recovery is the re-dispatch itself: once the input frees, the next broadcast is
  accepted and the guard is disarmed.

The guard is block-paced and bounded. Within `max_monitoring_confirmations`
blocks exactly one competing branch survives, so the transaction is either back or
genuinely gone; the window cannot be extended.

## Reorgs and orphans

| Event | Handling |
|---|---|
| A `Confirmed` transaction reappears in the mempool. | Reset to `InMempool` and refresh the broadcast height. |
| The block of a transaction is orphaned. | Keep the transaction `InMempool`. |
| A deep reorg evicts both blocks and mempool. | Caught by the not_found path; the same txid is re-dispatched. |

## Retry budget

Transient failures are retried on two independent limits:

| Limit | Unit | Purpose |
|---|---|---|
| `retry_attempts_sending_tx` | Count | How many transient failures before settling `Failed`. |
| `retry_interval_seconds` | Wall clock | Minimum gap between retry batches, shared across both engines. |

The fail guard is separate from this budget: it is block-paced, so a guarded
transaction can outlive the retry budget without being failed.

## Funding rules

| Rule | Reason |
|---|---|
| A funding UTXO below `min_funding_amount_sats` is rejected with `InvalidFundingUtxo`. | Dust cannot pay a useful fee. |
| The funding walk skips `Failed` and `replaced_by` speedups. | The next boost must chain off the live tip, not a dead branch. |
| Funding marks are released when a speedup is failed or replaced. | A released UTXO becomes spendable again for the next attempt. |
| `add_funding` UTXOs must be effectively final on-chain. | The funding chain assumes its base output is not reorgable in practice. |

## Cancel contract

`cancel` removes monitoring and storage only for client-registered transactions
still in `ToDispatch`.

| Target | Result |
|---|---|
| `Normal` or `NeedsSpeedup` in `ToDispatch` | Cancelled: removed from storage, monitoring, and the pending-speedup set. |
| Already dispatched (`InMempool` / `Confirmed` / `Finalized` / `Failed`) | Refused with `InvalidCancel`. |
| `Speedup` or `Funding` kind, or an unknown txid | Refused with `InvalidCancel`. |
| A non-`Transactions` monitoring entry | Passed straight through to the monitor. |

## News rules

| Rule | Reason |
|---|---|
| News is deduplicated by value within a block. | Acknowledge only after acting; acking early drops a same-block repeat. |
| Internal CPFP/RBF transactions are filtered from client transaction news. | The client's context does not distinguish speedup variants. |
| A settled record emits `TransactionEvicted` when removed. | The client gets the full lifecycle, including teardown. |

## Application obligations

| Obligation | Reason |
|---|---|
| Register transaction B only after transaction A it depends on is `Finalized`. | An in-flight A can disappear and be re-dispatched; the coordinator preserves no ordering between independently registered transactions. |
| External (untracked) parents must already be confirmed. | The dispatcher gates only on tracked parents; an untracked, unconfirmed parent makes the node reject the child. |
| Acknowledge news only after acting on it. | See the news deduplication rule above. |

## Glossary

| Term | Meaning |
|---|---|
| Tick | One call to `tick()`. Advances the monitor and runs the six-step pipeline. |
| `EngineContext` | The shared service bundle (monitor, dispatcher, fee, funding, storage) both engines hold. |
| `CoordinatedTx` | The coordinator's record for one transaction: its kind, state, and lifecycle metadata. |
| Anchor input | The CPFP input that spends a parent output, tying the child to the parent. |
| Funding input | The CPFP/RBF input that pays the extra fee, taken from the funding chain. |
| Funding chain | The sequence of change outputs the funding flows through, from the operator UTXO onward. |
| Speedup tip | The latest live speedup in the chain, the one a new boost builds on. |
| Boost | A new speedup (CPFP or RBF) built because the current tip is stale. |
| PSP (pending speedup parents) | The set of `NeedsSpeedup` parents still waiting for a CPFP to be built. |
| Fail guard | `fail_guard_until`: the block height before which an input-consumed verdict is deferred. |
| `replaced_by` | Marker on a speedup that has been superseded by an RBF replacement. |
| News | A structured lifecycle event the application pulls and acknowledges. |
