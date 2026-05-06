# Internals

How the compiler and runtime are put together. Useful if you're working on rend
itself, or if you need to reason about performance characteristics from first
principles.

- [VM](vm.md) — register file, instruction set, fuel ticks, lazy reads
- [persistent structures](persistent-structures.md) — HAMT (`pmap`) and indexed trie (`pvec`)
- [optimizer](optimizer.md) — read clusters, granular path resolution

## Compilation pipeline

```
source
  ↓ lexer
tokens
  ↓ parser
AST
  ↓ modifier::expand     (desugar [mod] wrappers)
AST'
  ↓ typeck::resolve_types
AST''                    (types fully resolved)
  ↓ typeck::check
  ↓ affine::check        (non-Copy ownership)
  ↓ typeck::annotate_dyn_calls   (stamp view/pure on $-dispatches)
  ↓ effects::classify
  ↓ effects::verify_purity_annotations
AST''' (fully checked, annotated)
  ↓ compile::compile     (lower to BcModule)
BcModule
  ↓ optimize             (cluster reads, etc.)
BcModule' (final)
  ↓ artifact::serialize  (deterministic bytes)
Artifact
```

The frontend is `rend::frontend(src)` — useful for tooling that wants the AST without
running anything. `Engine::compile` does the full pipeline through artifact
serialization.

## See also

- [`src/`](../../src) — the sources, organized one module per pipeline stage
- [`tests/`](../../tests) — one test file per major feature, used as both correctness
  proof and documentation of expected behavior
