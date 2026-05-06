# rend documentation

`rend` is an embeddable, sandboxed, affine-typed language for writing on-chain logic
that runs against a key-value store with optimistic concurrency.

The unifying design idea: **static facts are materialized, not checked.** Every static
guarantee — `view`, `pure`, capability affinity, interface conformance, content-addressed
artifacts — produces a different runtime artifact (different VM path, different storage
layout, different optimization). The type system isn't a gate on top of a uniform
runtime; it determines the runtime's shape.

## Start here

- [overview](overview.md) — what rend is and the design thesis
- [quick-start](quick-start.md) — minimal embedding example

## Language reference

- [lexical](language/lexical.md) — keywords, comments, literals
- [modules](language/modules.md) — module declarations, cross-module calls, imports
- [types](language/types.md) — primitives, composites, type inference
- [functions and effects](language/functions-and-effects.md) — `fn`, `entry`, `view`, `pure`, `nore`
- [state and persistence](language/state-and-persistence.md) — `state`, `pmap`, `pvec`, granular fields
- [indexes](language/indexes.md) — auto-maintained derived pmap indexes
- [capabilities](language/capabilities.md) — `cap`, affine ownership, privileged construction
- [interfaces](language/interfaces.md) — `interface`, dynamic dispatch, `IFace::bind`
- [events](language/events.md) — `event` declarations and `emit`
- [pattern matching](language/pattern-matching.md) — `match`, exhaustiveness, sum types
- [comprehensions](language/comprehensions.md) — list, set, and dict comprehensions
- [modifiers](language/modifiers.md) — Solidity-style function wrappers
- [operators](language/operators.md) — arithmetic, comparison, logical, bitwise
- [builtins](language/builtins.md) — host-provided functions

## Runtime reference

- [Engine API](runtime/engine.md) — public surface for embedding rend
- [lifecycle](runtime/lifecycle.md) — compile / deploy / tx / query
- [artifacts](runtime/artifacts.md) — binary format, content addressing
- [transactions](runtime/transactions.md) — `TxContext`, OCC, 3-way merge, write log
- [fuel](runtime/fuel.md) — fuel metering and sandboxing
- [host](runtime/host.md) — binding host functions
- [KV backend](runtime/kv.md) — `Kv` trait, in-memory backend

## Internals

- [VM](internals/vm.md) — register file, instruction set, fuel ticks
- [persistent structures](internals/persistent-structures.md) — HAMT (`pmap`) and indexed trie (`pvec`)
- [optimizer](internals/optimizer.md) — read clusters, granular paths

## Examples

The [`examples/`](../examples) directory has working programs for every major feature.
See [examples-index](examples-index.md) for a one-line description of each.
