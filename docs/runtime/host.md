# Host functions

Host functions let rend call into the embedding process. The source declares an
`import`; the host binds a closure to that name; the runtime routes calls through
the binding.

## Source side

```rd
module main;

import log_string(s: string) -> ();
import current_height() -> u64;

fn main() -> u64 {
    log_string("starting");
    return current_height();
}
```

`import` declarations sit at the top of a module. The signature must match what the
host binds; a mismatch is a runtime error on first call.

## Host side

```rust
use rend::{Engine, Fuel};
use rend::value::Value;
use rend::host::HostError;

let mut engine = Engine::new();

engine.bind("log_string", |args| {
    if let [Value::Str(s)] = args {
        println!("[contract] {s}");
        Ok(Value::Unit)
    } else {
        Err(HostError::new("log_string expects (string)"))
    }
});

engine.bind("current_height", |_args| Ok(Value::U64(42)));

engine.execute(src, Fuel::new(10_000), &kv)?;
```

`Engine::bind` takes a closure of type
`Fn(&[Value]) -> Result<Value, HostError> + Send + Sync + 'static`.

## Effect classification

Host imports default to `Impure`. A function that calls one is classified `Impure`
and can't be `view` or `pure`. If you want a `view`-callable host function, the
binding mechanism doesn't currently mark imports as pure — keep them out of view
paths.

## When to use host functions vs. cross-module calls

Use host functions for things rend can't compute internally:

- Logging to the host's tracing infrastructure
- Reading from auxiliary indexes the host maintains
- Cryptographic primitives (signatures, hashes outside the built-in set)
- Real-world side effects (RPC fanout, etc.)

Use cross-module calls for everything else. A host function is opaque to the
optimizer and the effects classifier; a rend module is fully analyzed.

## See also

- [`src/host.rs`](../../src/host.rs) — `Host` struct and `HostError`
- [`tests/imports.rs`](../../tests/imports.rs) — host binding and call flow
- [`examples/05_host_imports.rd`](../../examples/05_host_imports.rd) — minimal example
