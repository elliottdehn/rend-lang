# Register VM

`rend` runs on a register-based VM, not a stack machine. Each function carries a
fixed-size register file allocated at compile time; instructions read from and write
to specific register indices.

## Why registers

Three reasons:

1. **Decoder simplicity.** Each instruction names its operands; no implicit stack
   discipline.
2. **Static analysis.** The optimizer can reason about live ranges and clustering
   reads/writes by inspecting register usage directly.
3. **No allocation in the inner loop.** A pre-sized `Vec<Value>` per call frame; no
   `push`/`pop` allocations.

## Calls

A call frame is `Vec<Value>` of size `n_regs`. Arguments are written into the first
`n_params` slots; the rest are scratch. Returning is `regs[ret_reg]`.

## Fuel ticks

Every iteration of the main loop checks the fuel counter and decrements. Out-of-fuel
returns `ErrorKind::Runtime` with message `"out of fuel"`. Host calls and KV reads
incur their own ticks at entry.

## Lazy reads

`Value::Pending` is a cell key that hasn't been forced yet. Most reads produce a
`Pending`; only when an instruction needs the actual value (arithmetic, branch on
bool, etc.) does the runtime call `force`, which dispatches a `Kv::get_many` for any
queued cluster.

This is the read-batching mechanism: the optimizer groups independent reads at
compile time, the VM defers each read until forced, and the first force in a cluster
issues one batched fetch for all of them.

## Instruction set sketch

The bytecode language has instructions for:

- **Constants** (`LoadConst`)
- **Arithmetic** (`Add`, `Sub`, `Mul`, ..., one variant per type)
- **Comparison and logical**
- **Control flow** (`Jmp`, `JmpIf`, `JmpIfNot`)
- **Calls** (`Call`, `CallExternal`, `CallExternalDyn`, `CallHost`)
- **State** (`LoadState`, `StoreState`, `LoadStatePath`, `StoreStatePath`)
- **Collections** (`MapGet`, `MapSet`, `PMapGet`, `PMapSet`, `PVecPush`, `PVecGet`, `PVecSet`)
- **Structs/enums/tuples** (`MakeStruct`, `MakeEnum`, `MakeTuple`, `FieldGet`, `EnumTag`, ...)
- **Interfaces** (`MakeInterface`, `CallExternalDyn`)
- **Events** (`Emit`)
- **Capabilities/affinity** (`Move`, conventionally — affine handling is mostly typeck-side)

See `src/bc.rs` for the complete `Instr` enum.

## Read clusters

The optimizer (`src/optimize.rs`) walks each function body and groups state-read
instructions into clusters that:

- Have no dependency on each other
- Have no instruction between them that writes state
- Have no `CallExternal` (which is treated as a write fence)
- Have no `CallExternalDyn` *unless* its `is_view` or `is_pure` flag is set

Each cluster becomes a `read_groups` entry on the function. At runtime, the first
read in a cluster triggers a single `Kv::get_many` for all keys in the cluster; the
rest of the cluster's reads are served from the same buffer.

## See also

- [`src/vm.rs`](../../src/vm.rs) — the main loop
- [`src/optimize.rs`](../../src/optimize.rs) — cluster planner
- [`src/bc.rs`](../../src/bc.rs) — instruction set
