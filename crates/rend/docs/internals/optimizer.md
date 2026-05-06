# Optimizer

The optimizer is a small pass over each function's bytecode that pre-computes
runtime structure. It runs once at compile time; its output is baked into the
artifact, so the runtime doesn't repeat the analysis.

## Read clustering

The hot bit. The optimizer scans each function's bytecode for `LoadState` /
`MapGet` / `PMapGet` / `PVecGet` / `LoadStatePath` instructions and groups them into
clusters. A cluster is a set of reads that:

1. Have no dependency on each other (no read in the cluster consumes another's
   result).
2. Are not separated by an instruction that writes state.
3. Are not separated by a `CallExternal` to a non-`view`, non-`pure` callee. The
   compiler stamps the callee's effect bound onto each cross-module call at emission
   time (looking it up in either the same-artifact peer modules' AST or the dep
   artifacts' `BcFn` flags), so a `view` or `pure` cross-module call commutes through
   the cluster.
4. Are not separated by a `CallExternalDyn` unless the dispatch is statically known
   to be `view` or `pure`. Interface methods carry these flags, so the planner
   handles dynamic dispatch by the same mechanism as static cross-module calls.

Each cluster becomes a `read_groups` entry on the `BcFn`. At runtime, the first
read in a cluster issues a `Kv::get_many` for all the cluster's keys; subsequent
reads in the cluster are served from the buffered result.

For a function that reads N independent state slots, this turns N round-trips into
1.

### Why view/pure on dispatch matters

Without the flag, every dynamic dispatch would have to be treated as a write fence
(it might be a non-view method that writes state). With the flag, a `view` dispatch
is provably non-writing, so reads on either side of the dispatch can stay in the
same cluster.

This is one of the concrete examples of the "static facts materialized" idea — the
interface declaration's `view` annotation isn't lint, it's a structural input to
the cluster planner.

## Granular path resolution

For a struct field path like `meta.name`, the compiler walks the type at compile
time and chains `child(parent_key, field_name)` to derive the leaf cell key. The
chain is materialized as a `PathSpec` on the `BcModule`, and the bytecode uses
`LoadStatePath` / `StoreStatePath` instructions that index directly into the
pre-computed key.

The runtime never has to walk a struct shape at access time. The KV fetch is for
the exact leaf cell.

## Constant folding

Currently minimal. Pure functions called with literal arguments are not constant-folded
yet (a future slice). The classifier tags `pure` functions specifically so this is
straightforward to add.

## See also

- [`src/optimize.rs`](../../src/optimize.rs) — clustering pass
- [`src/compile.rs`](../../src/compile.rs) — `PathSpec` construction
