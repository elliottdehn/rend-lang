# Engine API

`rend::Engine` is the public surface for embedding rend. Every code path — running
sources, deploying, executing transactions, querying — goes through it.

## Construction

```rust
use rend::{Engine, Fuel};

let engine = Engine::new();
```

`Engine` is cheap to construct and cheap to clone in spirit (you can build one per
call if you like). It holds a `Host` for binding host functions; default host has no
imports bound.

## Host bindings

```rust
let mut engine = Engine::new();
engine.bind("log", |args| {
    println!("{:?}", args);
    Ok(rend::Value::Unit)
});
```

The bound name must match an `import` declaration in the source. See
[host functions](host.md).

## Execution surface

Three kinds of operation, three matching code paths:

### Run-and-discard (development convenience)

```rust
fn run(&self, src: &str, fuel: Fuel) -> Result<Value, Error>
```

Compile, run `main()`, return its value. No persistent state, no transaction context.
Equivalent to throwaway calculation; not the embedding path.

### Execute (writes to a KV)

```rust
fn execute(&self, src: &str, fuel: Fuel, kv: &dyn Kv) -> Result<ExecOutcome, Error>
fn execute_with_context(&self, src: &str, ctx: TxContext, fuel: Fuel, kv: &dyn Kv) -> Result<ExecOutcome, Error>
fn execute_modules(&self, sources: &[String], main_module: &str, fuel: Fuel, kv: &dyn Kv) -> Result<ExecOutcome, Error>
fn execute_main(&self, sources: &HashMap<String, String>, main_module: &str, fuel: Fuel, kv: &dyn Kv) -> Result<ExecOutcome, Error>
fn execute_main_with_context(&self, sources: &HashMap<String, String>, main_module: &str, ctx: TxContext, fuel: Fuel, kv: &dyn Kv) -> Result<ExecOutcome, Error>
```

`execute` is the read-write transaction path. The runtime materializes a write set,
runs the program against it, and returns:

```rust
pub struct ExecOutcome {
    pub result: Value,
    pub reads:  Vec<(u128, Value)>,
    pub writes: Vec<(u128, Value)>,
    pub events: Vec<EventRecord>,
    pub pmap_types: Vec<(u128, Type)>,
    pub pvec_types: Vec<(u128, Type)>,
    pub node_cells_written: usize,
}
```

The host applies `writes` to the KV when (and if) it accepts the tx. `reads` is the
OCC read set — the host can compare it against the live KV to detect conflicts.

### Query (no writes, view/pure only)

```rust
fn query(&self, tx: &Artifact, deps: &[Artifact], fuel: Fuel, kv: &dyn Kv) -> Result<QueryOutcome, Error>
fn query_with_context(&self, tx: &Artifact, deps: &[Artifact], ctx: TxContext, fuel: Fuel, kv: &dyn Kv) -> Result<QueryOutcome, Error>
```

`query` runs against a tx artifact (typically produced by `compile_tx`) plus a slice
of dep artifacts. It's a separate code path: no write log, no OCC bookkeeping. The
tx's `main` must be declared `view` or `pure` — anything else is rejected before
the VM runs.

```rust
let q_tx = engine.compile_tx(view_src, &[token.clone()])?;
let result = engine.query(&q_tx, &[token], Fuel::new(20_000), &kv)?;
```

## Artifact-based execution

For deployed contracts, compile once and execute many times against the same artifact:

```rust
fn compile(&self, src: &str) -> Result<Artifact, Error>
fn compile_modules(&self, sources: &[String]) -> Result<Artifact, Error>
fn compile_tx(&self, src: &str, deps: &[Artifact]) -> Result<Artifact, Error>

fn deploy(&self, artifact: &Artifact, fuel: Fuel, kv: &dyn Kv) -> Result<Option<ExecOutcome>, Error>
fn deploy_with_context(&self, artifact: &Artifact, ctx: TxContext, fuel: Fuel, kv: &dyn Kv) -> Result<Option<ExecOutcome>, Error>

fn execute_tx(&self, tx: &Artifact, deps: &[Artifact], fuel: Fuel, kv: &dyn Kv) -> Result<ExecOutcome, Error>
fn execute_tx_with_context(&self, tx: &Artifact, deps: &[Artifact], ctx: TxContext, fuel: Fuel, kv: &dyn Kv) -> Result<ExecOutcome, Error>

fn query(&self, tx: &Artifact, deps: &[Artifact], fuel: Fuel, kv: &dyn Kv) -> Result<QueryOutcome, Error>
```

See [lifecycle](lifecycle.md) for the deploy/tx/query flow.

## Free-standing helpers

```rust
rend::run(src: &str) -> Result<Value, Error>            // tree-walk interpreter, no fuel
rend::run_bc(src: &str, fuel: Fuel) -> Result<Value, Error>  // bytecode VM, fueled
rend::frontend(src: &str) -> Result<Module, Error>      // parse + typecheck + affine, no exec
```

`frontend` is useful if you want to inspect the AST before compilation — e.g. to
extract the module's interface declarations or state types for tooling.

## Error handling

Every method returns `Result<_, rend::Error>`. The error type carries:

- `kind`: `ErrorKind::Lex`, `Parse`, `Type`, `Affine`, `Compile`, `Runtime`
- `message`: a description
- `span`: source position when known

Out-of-fuel is `ErrorKind::Runtime` with message `"out of fuel"`.

## See also

- [lifecycle](lifecycle.md) — when to use which entry point
- [transactions](transactions.md) — `TxContext` and OCC details
- [`tests/end_to_end.rs`](../../tests/end_to_end.rs) — full embedding flows
