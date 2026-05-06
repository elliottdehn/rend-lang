# Artifacts

An `Artifact` is the compiled, serializable, deterministic output of compiling one or
more modules. The host can store it, hash it, and ship it across processes.

## Binary layout

```
MAGIC[8] | VERSION[2] | n_modules[u32_be] | MODULE+
```

| Field | Bytes | Value |
|---|---|---|
| MAGIC | 8 | `b"REND-BC0"` |
| VERSION_MAJOR | 1 | `0` |
| VERSION_MINOR | 1 | `13` |
| n_modules | 4 (big-endian) | number of modules in this artifact |
| MODULE+ | variable | each module's serialization |

A loader rejects a mismatched magic, rejects mismatched major version, and accepts
any minor version compatible with the current build.

Each module serializes:

- name (length-prefixed string)
- imports (host-fn names — bind at run time)
- state metadata (roots, types, defaults, names)
- struct, enum, event, cap shapes
- interface declarations (so a tx compiled against this module can reference its interfaces)
- function bodies (bytecode, consts, registers, param/return types, `is_entry`/`is_view`/`is_pure`/`is_nore`)
- pre-computed `PathSpec`s for granular field paths
- read-cluster groups produced by the optimizer

See `src/artifact.rs` for the full encoding.

## Content addressing

```rust
let bytes = rend::artifact::serialize(&artifact);
let hash = rend::hashing::child(0, &bytes);   // u128
```

Same source → same bytes → same hash. The hash is a u128 derived by feeding the
artifact bytes through the same content-hash function the runtime uses for
state-cell keys. It's a stable identity for the module set.

When you compile a transaction with `Engine::compile_tx`, it captures the dep
artifacts' hashes. Running the tx requires you to provide the exact same artifacts.

## What's inside vs. outside

**Inside** (frozen at compile time):
- Cross-module call resolution (module name + entry index)
- State cell keys
- Default values
- Read clustering / batched-fetch groups

**Outside** (resolved at run time):
- Host imports (bound through `Engine::bind`)
- Cross-artifact dep resolution (the runtime walks the artifact list passed in)

This split is the reason the artifact is deterministic: nothing in the bytes depends
on the host or on which other artifacts happen to be loaded together. They're
self-contained except for the explicit reference points (host imports by name,
cross-module entries by name).

## Versioning

`VERSION_MINOR` history:

| Minor | Added |
|---|---|
| 0.1 | Initial format |
| 0.2 | Param/return types persisted (cross-artifact compile) |
| 0.3 | `view` / `pure` flags on `BcFn` |
| 0.4 | Interface type support |
| 0.5 | Interface declarations in module section |
| 0.6 | `Instr::CallExternal` carries `is_view`/`is_pure` for cluster planner |
| 0.7 | Whole-collection walks: `PMapEntries`, `PMapKeys`, `PMapValues`, `PVecToArray` |
| 0.8 | `PMapAppendUnique` for multi-index auto-maintenance |
| 0.9 | `PMapPutUnique` enforces unique constraints at write time |
| 0.10 | `PMapWalkInit` / `PMapWalkNext` for streaming HAMT iteration |
| 0.11 | `pbtree<K, V>` sorted persistent map with `PBTreeGet/Put/Contains/Range/WalkInit/WalkNext` |
| 0.12 | `PBTreePutUnique` + `PBTreeAppendUnique` — sorted indexes via pbtree-backed slots |
| 0.13 | `delete state[k]` — primary delete + index back-link cleanup ops |

A future major bump will be required if the layout itself changes. Minor bumps add
fields in a way that older readers can ignore (skip-on-missing or default).

## See also

- [`src/artifact.rs`](../../src/artifact.rs) — the serializer/deserializer
- [`tests/artifact.rs`](../../tests/artifact.rs) — round-trip + version tests
