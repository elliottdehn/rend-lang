# Modules

Every rend source file declares a single module:

```rd
module token;
```

The module name is a top-level identifier. Two modules can't share a name in the same
artifact. State, functions, struct/enum/event/cap/interface declarations all live
inside the module that contains them.

## Cross-module calls

Call an `entry` function in another module by `module::function(args)`:

```rd
module main;
fn main() -> u64 {
    return token::balance_of(address("alice"));
}
```

Only functions marked `entry` are reachable across module boundaries. Non-entry
functions are private to their declaring module.

The dep set is implicit — the parser sees `token::balance_of` and adds `token` to the
list of modules this artifact needs. When you compile a transaction against deployed
deps with `Engine::compile_tx`, the compiler resolves the symbol against those
artifacts.

## Imports (host functions)

Host functions are declared explicitly:

```rd
import emit_log(label: string) -> ();
```

The host binds a closure to the import name through `Engine::bind`. Calls go through
`Instr::CallHost`. See [host](../runtime/host.md).

## Multi-module artifacts

A single artifact can hold many modules. Pass an array of source strings to
`Engine::compile_modules` or `Engine::execute_modules`:

```rust
let sources = vec![
    fs::read_to_string("token.rd").unwrap(),
    fs::read_to_string("market.rd").unwrap(),
    fs::read_to_string("main.rd").unwrap(),
];
let artifact = Engine::new().compile_modules(&sources)?;
```

Cross-module calls within an artifact are resolved at compile time. Cross-artifact
calls (e.g. a tx against a deployed token) are resolved when the tx is compiled
against the dep artifacts.

## See also

- [`examples/12_multi_module/`](../../examples/12_multi_module) — module split + cross-module calls
- [`examples/36_modular_market/`](../../examples/36_modular_market) — production-shaped multi-module artifact
