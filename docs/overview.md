# Overview

`rend` is a small programming language for on-chain logic, designed to be embedded
inside a host process (a node, a sequencer, a sidecar) rather than run on its own VM.
It targets a register-bytecode interpreter with per-instruction fuel metering, persists
state through a host-provided key-value store, and ships every static guarantee as
runtime structure rather than a typecheck-and-discard verdict.

## What's in the language

- **Affine type system.** Capabilities (`cap`) and resources are non-Copy: holding one
  is the proof of permission, so duplicating it would defeat the model.
- **Effect classifier.** Every function is classified as `Pure`, `ReadOnly`, `WriteOnly`,
  `ReadWrite`, or `Impure`. The `view` and `pure` annotations are *verified* against
  this classification; mismatches are compile errors.
- **Persistent collections.** `pmap` (HAMT) and `pvec` (indexed trie) store one node per
  KV cell, so concurrent transactions touching disjoint subtrees commit in parallel via
  3-way CRDT merge on the root pointer.
- **Capabilities.** `cap Foo { ... }` declares an unforgeable, non-Copy permission token.
  Literals are only legal inside the declaring module.
- **Interfaces & dynamic dispatch.** `interface IFoo { ... }` declares a method surface.
  `IFoo::bind("module_name")` returns a runtime-bound interface value; `$value::method(...)`
  dispatches through it. Conformance is checked at bind time.
- **Sum types.** `enum`s with variant payloads, exhaustive `match`.
- **Modifiers.** Solidity-style function wrappers with parameter binding and `_;`
  placeholder for the wrapped body.
- **Events.** `emit Name(args)` appends to a per-tx log scoped by module name.

## What's in the runtime

- **Register VM** with fuel metering; out-of-fuel is the actual sandbox enforcement.
- **OCC + 3-way merge.** Transactions commit if their read set is unchanged. When two
  txs touch disjoint subtrees of a `pmap` or `pvec`, the merge is non-conflicting.
- **Multi-artifact deploy/tx/query.** Modules compile to deterministic, content-addressed
  artifacts. A tx can be compiled against a frozen set of dep artifacts so its references
  don't drift.
- **`Engine::query`** is a separate code path that runs `view` or `pure` entries with no
  OCC bookkeeping and no write log. The annotation buys real performance, not just
  type-check-time confidence.
- **Granular state.** Struct fields and map/`pmap` cells live in distinct KV cells, so
  reads and writes are scoped to the smallest unit that participates in conflict checks.
- **Read clustering.** The bytecode optimizer batches independent state reads into
  `Kv::get_many` calls. The cluster planner consumes interface effect flags so dynamic
  dispatch through a `view` method doesn't fence a batch.

## The unifying idea

Static facts in rend become physical runtime structure:

| Static fact | Runtime materialization |
|---|---|
| `view` / `pure` | Different VM entry path (`Engine::query` skips OCC and write log) |
| `cap` non-Copy | No copy instruction emitted; cap is moved or re-read from state |
| Interface effect flag | Cluster planner uses it to decide whether to fence a read batch |
| Interface conformance | Bind fails fast if target module doesn't satisfy signatures |
| Persistent collection shape | One KV cell per trie node — disjoint subtree edits don't conflict |
| Content-addressed artifact | Tx commits to the exact dep set it was compiled against |

The compiler isn't approving a program; it's building a different one for each set of
static facts it discovered. There's no layer where the type system "goes away" — it
stays in the artifact as physical structure.
