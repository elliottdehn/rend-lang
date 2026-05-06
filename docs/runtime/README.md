# Runtime reference

The embedding side of rend: how to drive the language from a Rust host.

- [Engine API](engine.md) — the public surface (`compile`, `execute`, `query`, `deploy`, ...)
- [lifecycle](lifecycle.md) — when to use deploy vs tx vs query
- [artifacts](artifacts.md) — binary format, content addressing, versioning
- [transactions](transactions.md) — `TxContext`, OCC, 3-way merge, write log
- [fuel](fuel.md) — fuel metering and sandboxing
- [host functions](host.md) — binding closures to `import` declarations
- [KV backend](kv.md) — `Kv` trait, `InMemoryKv`, what cells the runtime stores

## Reading order

1. **Engine API + lifecycle** — what calls to make and when.
2. **Transactions** — what `ExecOutcome` carries and how to commit.
3. **Fuel** — sandbox bounds.
4. **KV + artifacts** — the storage model and the deterministic compile output.
5. **Host functions** — extending the language with custom builtins.

If you only want to run a one-off rend script, [quick-start](../quick-start.md) is
enough. Come back here when you need to embed it for real.
