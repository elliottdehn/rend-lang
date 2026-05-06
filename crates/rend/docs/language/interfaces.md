# Interfaces and dynamic dispatch

An interface declares an abstract method surface. Values of interface type are runtime
bindings to a target module. Calls go through the binding via `$value::method(args)`.

## Declaration

```rd
interface IPriceFeed {
    entry view fn price_of(symbol: string) -> u64;
}

interface IERC20 {
    entry fn transfer(to: Address, amount: u64) -> u64;
    entry view fn balance_of(who: Address) -> u64;
}
```

Each method must be `entry` (because cross-module dispatch only reaches `entry` fns).
Methods may also carry `view` or `pure` annotations — these are part of the contract
the bound module's implementation must satisfy.

## Binding

```rd
state feed: IPriceFeed;

entry fn switch_feed(name: string) -> u64 {
    feed = IPriceFeed::bind(name);
    return 1u64;
}
```

`IPriceFeed::bind("oracle_simple")` returns a value of type `IPriceFeed` that wraps
the runtime module name. The bind is checked at runtime: if the target module doesn't
implement every method in the interface (matching name, arity, types, return type, and
effect bound at-least-as-strict as the interface declares), the bind fails immediately.

## Dispatch

```rd
view fn portfolio_value(holder: Address) -> u64 {
    let units = holdings[holder];
    let price = $feed::price_of("ETH");
    return units * price;
}
```

The `$` prefix makes dynamic dispatch syntactically loud. You can scan code and tell
exactly which calls are static (`module::fn(...)`) and which are dispatched
(`$value::method(...)`).

The compiler stamps the method's `is_view` and `is_pure` flags onto the dispatch
instruction at compile time, so the read-cluster planner knows whether a dispatch
fences a batch of reads.

## Bind-time conformance

When `IFace::bind("name")` runs, the VM walks the interface's method list and verifies
that each method exists in the target module with:

- Matching name
- Matching parameter arity
- Matching parameter types
- Matching return type
- An effect bound at-least-as-strict as the interface declares
  - iface `pure` ⇒ impl must be `pure`
  - iface `view` ⇒ impl must be `view` or `pure`
  - iface unannotated ⇒ no constraint
- The implementation function must be `entry`

A failure is a runtime error pointing at the bind site, not the dispatch — so the
problem surfaces at bind time, not at first call.

## Cross-artifact interfaces

An interface declared in one artifact can be bound to a module in another artifact, as
long as both are loaded into the world together. The compiler threads the dep
artifacts through parsing/typeck/compile so that `IExternal::bind(...)` from a tx can
reference an interface declared in a deployed module.

## Local-bound interfaces

```rd
view fn read_through(name: string) -> u64 {
    let r: IReader = IReader::bind(name);
    return $r::read();
}
```

The compiler tracks the local's type through let-bindings (annotated lets, the
`IFace::bind` constructor, and ident copies) so the dispatch through `r` carries the
correct effect classification. This is what lets a `view` function locally bind and
dispatch through an interface without classifier downgrade to `Impure`.

## See also

- [`examples/37_interface_dispatch/`](../../examples/37_interface_dispatch) — router + multiple impls
- [`examples/38_swappable_oracle/`](../../examples/38_swappable_oracle) — swap oracle implementations at runtime
- [`tests/interfaces.rs`](../../tests/interfaces.rs) — full conformance test surface
