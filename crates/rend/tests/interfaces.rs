//! Interfaces + dynamic dispatch.
//!
//! `interface IERC20 { entry fn transfer(...); }` declares an
//! abstract API surface; `IERC20::bind("module_name")` returns a
//! Value::Interface; `$value::method(args)` dispatches at runtime
//! through the bound module.
//!
//! These tests cover compile-time validation (signature match,
//! method existence) and runtime dispatch through the bytecode VM.

use rend::value::Value;
use rend::{Engine, Fuel};

// ---------- declaration syntax ----------

#[test]
fn interface_decl_compiles() {
    let bank = "
        module bank;
        interface ITransfer {
            entry fn transfer(to: Address, amount: u64) -> u64;
            entry view fn balance_of(who: Address) -> u64;
        }
        // Bank exposes its own implementation of these.
        state balances: pmap<Address, u64>;
        entry fn transfer(to: Address, amount: u64) -> u64 {
            balances[to] = balances[to] + amount;
            return balances[to];
        }
        entry view fn balance_of(who: Address) -> u64 {
            return balances[who];
        }
    ";
    Engine::new().compile(bank).unwrap();
}

#[test]
fn interface_method_view_pure_combo_is_rejected() {
    let err = Engine::new().compile(
        "module m;
         interface I {
             entry view pure fn x() -> i64;
         }",
    ).unwrap_err();
    assert!(err.to_string().contains("both"));
}

// ---------- typeck: dispatch shape ----------

#[test]
fn dyn_call_typechecks_against_interface() {
    let src = "
        module m;
        interface IDoubler {
            entry fn double(x: i64) -> i64;
        }
        state d: IDoubler;
        fn main() -> i64 {
            d = IDoubler::bind(\"impl\");
            return $d::double(7);
        }
    ";
    Engine::new().compile(src).unwrap();
}

#[test]
fn dyn_call_rejects_unknown_method() {
    let err = Engine::new().compile(
        "module m;
         interface IDoubler {
             entry fn double(x: i64) -> i64;
         }
         state d: IDoubler;
         fn main() -> i64 {
             d = IDoubler::bind(\"impl\");
             return $d::triple(7);
         }",
    ).unwrap_err();
    assert!(err.to_string().contains("triple"));
}

#[test]
fn dyn_call_rejects_wrong_arg_types() {
    let err = Engine::new().compile(
        "module m;
         interface IDoubler {
             entry fn double(x: i64) -> i64;
         }
         state d: IDoubler;
         fn main() -> i64 {
             d = IDoubler::bind(\"impl\");
             return $d::double(\"not a number\");
         }",
    ).unwrap_err();
    assert!(
        err.to_string().contains("expected") || err.to_string().contains("got"),
        "got: {err}",
    );
}

#[test]
fn dyn_call_on_non_interface_is_compile_error() {
    let err = Engine::new().compile(
        "module m;
         state d: i64;
         fn main() -> i64 { return $d::method(); }",
    ).unwrap_err();
    assert!(err.to_string().contains("interface"));
}

#[test]
fn bind_requires_string_arg() {
    let err = Engine::new().compile(
        "module m;
         interface I { entry fn x() -> i64; }
         state i: I;
         fn main() -> i64 {
             i = I::bind(42);
             return $i::x();
         }",
    ).unwrap_err();
    assert!(err.to_string().contains("string"));
}

// ---------- end-to-end runtime dispatch ----------

#[test]
fn dyn_dispatch_runs_via_bytecode_vm() {
    // Two modules: `doubler` defines a `double` entry.
    // `main` declares an interface, binds it to `doubler`, and
    // calls it dynamically. The runtime must route to doubler's
    // entry through the world's module index.
    let sources = vec![
        "module doubler;
         entry fn double(x: i64) -> i64 { return x * 2; }
        ".to_string(),
        "module main;
         interface IDoubler {
             entry fn double(x: i64) -> i64;
         }
         state d: IDoubler;
         fn main() -> i64 {
             d = IDoubler::bind(\"doubler\");
             return $d::double(21);
         }
        ".to_string(),
    ];
    let kv = rend::kv::InMemoryKv::new();
    let out = Engine::new()
        .execute_modules(&sources, "main", Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::Int(42));
}

#[test]
fn dyn_dispatch_can_swap_implementations() {
    // The same interface, bound to two different modules. The
    // host can choose which implementation to dispatch to at
    // runtime — that's the point of interfaces.
    let common = "
        interface IGreeter {
            entry fn hello() -> i64;
        }";
    let sources = vec![
        "module impl_a;
         entry fn hello() -> i64 { return 1; }
        ".to_string(),
        "module impl_b;
         entry fn hello() -> i64 { return 2; }
        ".to_string(),
        format!(
            "module main;
             {common}
             state g: IGreeter;
             entry fn run(which: string) -> i64 {{
                 g = IGreeter::bind(which);
                 return $g::hello();
             }}
             fn main() -> i64 {{
                 return run(\"impl_a\") + run(\"impl_b\") * 10;
             }}
            ",
        ),
    ];
    let kv = rend::kv::InMemoryKv::new();
    let out = Engine::new()
        .execute_modules(&sources, "main", Fuel::new(50_000), &kv)
        .unwrap();
    // 1 + 2*10 = 21
    assert_eq!(out.result, Value::Int(21));
}

#[test]
fn dyn_dispatch_unbound_interface_is_runtime_error() {
    // Reading an unbound interface from state and dispatching
    // through it should fail with a clean runtime error rather
    // than e.g. routing to the empty module name.
    let src = "
        module m;
        interface I { entry fn x() -> i64; }
        state i: I;
        fn main() -> i64 { return $i::x(); }
    ";
    let kv = rend::kv::InMemoryKv::new();
    let err = Engine::new()
        .execute(src, Fuel::new(20_000), &kv)
        .unwrap_err();
    assert!(err.to_string().contains("unbound"));
}

#[test]
fn dyn_dispatch_to_unloaded_module_is_runtime_error() {
    // Bound to a module that isn't in the world.
    let src = "
        module m;
        interface I { entry fn x() -> i64; }
        state i: I;
        fn main() -> i64 {
            i = I::bind(\"ghost\");
            return $i::x();
        }
    ";
    let kv = rend::kv::InMemoryKv::new();
    let err = Engine::new()
        .execute(src, Fuel::new(20_000), &kv)
        .unwrap_err();
    assert!(
        err.to_string().contains("not loaded") || err.to_string().contains("ghost"),
        "got: {err}",
    );
}

// ---------- bind-time conformance ----------

/// Helper: run the given module set with `main` as the entry,
/// expecting an error. Returns the stringified error so individual
/// tests can assert on substrings.
fn expect_run_err(sources: Vec<String>) -> String {
    let kv = rend::kv::InMemoryKv::new();
    Engine::new()
        .execute_modules(&sources, "main", Fuel::new(20_000), &kv)
        .unwrap_err()
        .to_string()
}

#[test]
fn bind_rejects_module_missing_method() {
    // `IGreeter` declares `hello`, `goodbye`. The target only
    // implements `hello`. Bind must fail — the consumer would
    // have crashed on first `$g::goodbye()`, so we'd rather
    // catch it at the bind site.
    let sources = vec![
        "module bad_impl;
         entry fn hello() -> i64 { return 1; }
        ".to_string(),
        "module main;
         interface IGreeter {
             entry fn hello() -> i64;
             entry fn goodbye() -> i64;
         }
         state g: IGreeter;
         fn main() -> i64 {
             g = IGreeter::bind(\"bad_impl\");
             return $g::hello();
         }
        ".to_string(),
    ];
    let err = expect_run_err(sources);
    assert!(err.contains("goodbye") && err.contains("does not define"), "got: {err}");
}

#[test]
fn bind_rejects_method_with_wrong_arity() {
    let sources = vec![
        "module bad_impl;
         entry fn transfer(to: Address) -> u64 { return 0u64; }
        ".to_string(),
        "module main;
         interface ITransfer {
             entry fn transfer(to: Address, amount: u64) -> u64;
         }
         state t: ITransfer;
         fn main() -> u64 {
             t = ITransfer::bind(\"bad_impl\");
             return 0u64;
         }
        ".to_string(),
    ];
    let err = expect_run_err(sources);
    assert!(err.contains("transfer") && err.contains("param"), "got: {err}");
}

#[test]
fn bind_rejects_method_with_wrong_param_type() {
    let sources = vec![
        "module bad_impl;
         entry fn ping(x: i64) -> i64 { return x; }
        ".to_string(),
        "module main;
         interface IPing {
             entry fn ping(x: u64) -> i64;
         }
         state p: IPing;
         fn main() -> i64 {
             p = IPing::bind(\"bad_impl\");
             return 0;
         }
        ".to_string(),
    ];
    let err = expect_run_err(sources);
    assert!(err.contains("ping") && err.contains("param"), "got: {err}");
}

#[test]
fn bind_rejects_method_with_wrong_return_type() {
    let sources = vec![
        "module bad_impl;
         entry fn answer() -> i64 { return 42; }
        ".to_string(),
        "module main;
         interface IAnswer {
             entry fn answer() -> u64;
         }
         state a: IAnswer;
         fn main() -> u64 {
             a = IAnswer::bind(\"bad_impl\");
             return 0u64;
         }
        ".to_string(),
    ];
    let err = expect_run_err(sources);
    assert!(err.contains("answer") && err.contains("returns"), "got: {err}");
}

#[test]
fn bind_rejects_non_entry_method() {
    // The target's `hello` is not declared `entry`, so it can't
    // be reached via cross-module dispatch — the interface
    // contract requires it.
    let sources = vec![
        "module bad_impl;
         fn hello() -> i64 { return 1; }
         entry fn touch() -> i64 { return hello(); }
        ".to_string(),
        "module main;
         interface IGreeter {
             entry fn hello() -> i64;
         }
         state g: IGreeter;
         fn main() -> i64 {
             g = IGreeter::bind(\"bad_impl\");
             return 0;
         }
        ".to_string(),
    ];
    let err = expect_run_err(sources);
    assert!(err.contains("hello") && err.contains("entry"), "got: {err}");
}

#[test]
fn bind_rejects_view_iface_with_impure_impl() {
    // The interface promises `view` (read-only). The impl writes
    // state, which classifies as Impure. Conformance must fail.
    let sources = vec![
        "module bad_impl;
         state log: pmap<i64, i64>;
         entry fn read() -> i64 {
             log[0] = log[0] + 1;
             return log[0];
         }
        ".to_string(),
        "module main;
         interface IReader {
             entry view fn read() -> i64;
         }
         state r: IReader;
         fn main() -> i64 {
             r = IReader::bind(\"bad_impl\");
             return 0;
         }
        ".to_string(),
    ];
    let err = expect_run_err(sources);
    assert!(err.contains("read") && err.contains("view"), "got: {err}");
}

#[test]
fn bind_rejects_pure_iface_with_view_impl() {
    // Iface says `pure` — the impl reads state (so it's view at
    // best), which is more relaxed than the interface promises.
    let sources = vec![
        "module bad_impl;
         state ctr: i64;
         entry view fn answer() -> i64 { return ctr; }
        ".to_string(),
        "module main;
         interface IAnswer {
             entry pure fn answer() -> i64;
         }
         state a: IAnswer;
         fn main() -> i64 {
             a = IAnswer::bind(\"bad_impl\");
             return 0;
         }
        ".to_string(),
    ];
    let err = expect_run_err(sources);
    assert!(err.contains("answer") && err.contains("pure"), "got: {err}");
}

#[test]
fn bind_accepts_stricter_impl_than_iface_declares() {
    // Interface declares `view`; impl is `pure` — strictly
    // stricter. Should bind cleanly and dispatch.
    let sources = vec![
        "module strict_impl;
         entry pure fn read() -> i64 { return 7; }
        ".to_string(),
        "module main;
         interface IReader {
             entry view fn read() -> i64;
         }
         state r: IReader;
         fn main() -> i64 {
             r = IReader::bind(\"strict_impl\");
             return $r::read();
         }
        ".to_string(),
    ];
    let kv = rend::kv::InMemoryKv::new();
    let out = Engine::new()
        .execute_modules(&sources, "main", Fuel::new(20_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::Int(7));
}

#[test]
fn bind_accepts_any_impl_when_iface_has_no_effect_annotation() {
    // Iface declares no `view`/`pure` — anything the impl does
    // is fine. Useful as a regression: the conformance check
    // should NOT bake assumptions about default classifications.
    let sources = vec![
        "module loose_impl;
         state ctr: i64;
         entry fn touch() -> i64 {
             ctr = ctr + 1;
             return ctr;
         }
        ".to_string(),
        "module main;
         interface ITouch {
             entry fn touch() -> i64;
         }
         state t: ITouch;
         fn main() -> i64 {
             t = ITouch::bind(\"loose_impl\");
             return $t::touch();
         }
        ".to_string(),
    ];
    let kv = rend::kv::InMemoryKv::new();
    let out = Engine::new()
        .execute_modules(&sources, "main", Fuel::new(20_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::Int(1));
}

// ---------- local-bound view-fn dispatch ----------

#[test]
fn view_fn_can_call_view_method_through_local_interface() {
    // The annotation pass should stamp the local-bound DynCall
    // with the method's view flag, letting the calling fn
    // verify as `view`. Without that pass, this would fail
    // verification because dyncalls would be considered Impure.
    let sources = vec![
        "module impl_;
         entry view fn read() -> i64 { return 42; }
        ".to_string(),
        "module main;
         interface IReader {
             entry view fn read() -> i64;
         }
         entry view fn outer() -> i64 {
             // Locally-bound interface — used to require the
             // tighter classification only for state-bound.
             let r: IReader = IReader::bind(\"impl_\");
             return $r::read();
         }
         fn main() -> i64 { return outer(); }
        ".to_string(),
    ];
    let kv = rend::kv::InMemoryKv::new();
    let out = Engine::new()
        .execute_modules(&sources, "main", Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::Int(42));
}

#[test]
fn view_fn_calling_non_view_method_through_local_is_rejected() {
    // Mirror of the test above, but the impl method writes
    // (no annotation). The outer view fn should fail because
    // its DynCall now classifies as Impure.
    let sources = vec![
        "module impl_;
         state n: i64;
         entry fn touch() -> i64 { n = n + 1; return n; }
        ".to_string(),
        "module main;
         interface IPoke {
             entry fn touch() -> i64;
         }
         entry view fn bad() -> i64 {
             let p: IPoke = IPoke::bind(\"impl_\");
             return $p::touch();
         }
         fn main() -> i64 { return bad(); }
        ".to_string(),
    ];
    let kv = rend::kv::InMemoryKv::new();
    let err = Engine::new()
        .execute_modules(&sources, "main", Fuel::new(50_000), &kv)
        .unwrap_err();
    assert!(err.to_string().contains("view"));
}

// ---------- cross-artifact interface use ----------

#[test]
fn tx_can_use_interface_declared_in_dep_artifact() {
    // The `bank` dep declares `interface IERC20`. The tx
    // (separate artifact, separately compiled) doesn't redeclare
    // it — it just uses `IERC20::bind(...)` and `$t::method(...)`.
    let bank_src = "
        module bank;
        interface IERC20 {
            entry fn transfer(from: Address, to: Address, amount: u64) -> u64;
            entry view fn balance_of(who: Address) -> u64;
        }
        state balances: pmap<Address, u64>;
        entry fn transfer(from: Address, to: Address, amount: u64) -> u64 {
            assert(balances[from] >= amount);
            balances[from] = balances[from] - amount;
            balances[to]   = balances[to]   + amount;
            return balances[from];
        }
        entry view fn balance_of(who: Address) -> u64 {
            return balances[who];
        }
        entry fn mint(to: Address, amount: u64) -> u64 {
            balances[to] = balances[to] + amount;
            return balances[to];
        }
    ";
    let bank = Engine::new().compile(bank_src).unwrap();

    // Tx references `IERC20` from bank's artifact. No re-declaration.
    let tx_src = "
        module main;
        fn main() -> u64 {
            // Seed alice via direct call (skipping IERC20 here
            // because mint isn't on it).
            bank::mint(address(\"alice\"), 100u64);
            // Now operate via the interface — this proves the
            // type name resolved from the dep.
            let t: IERC20 = IERC20::bind(\"bank\");
            $t::transfer(address(\"alice\"), address(\"bob\"), 30u64);
            return $t::balance_of(address(\"bob\"));
        }
    ";
    let tx = Engine::new().compile_tx(tx_src, &[bank.clone()]).unwrap();
    let kv = rend::kv::InMemoryKv::new();
    let out = Engine::new()
        .execute_tx(&tx, &[bank], Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::U64(30));
}

#[test]
fn cross_artifact_view_dispatch_works_in_query_path() {
    // Read-only query that goes through a dep-declared interface.
    // The annotation pass should stamp the dyncall as view, so
    // the tx's own `view` verification passes.
    let bank = Engine::new().compile(
        "module bank;
         interface IBalance {
             entry view fn balance_of(who: Address) -> u64;
         }
         state balances: pmap<Address, u64>;
         entry view fn balance_of(who: Address) -> u64 {
             return balances[who];
         }
         entry fn mint(to: Address, amount: u64) -> u64 {
             balances[to] = balances[to] + amount;
             return balances[to];
         }",
    ).unwrap();
    let mut kv = rend::kv::InMemoryKv::new();
    // Seed via a normal tx.
    let setup = Engine::new().compile_tx(
        "module main;
         fn main() -> u64 { return bank::mint(address(\"alice\"), 500u64); }",
        &[bank.clone()],
    ).unwrap();
    let out = Engine::new()
        .execute_tx(&setup, &[bank.clone()], Fuel::new(20_000), &kv)
        .unwrap();
    kv.apply(&out.writes);

    // Query through the dep's interface.
    let q_tx = Engine::new().compile_tx(
        "module main;
         view fn main() -> u64 {
             let b: IBalance = IBalance::bind(\"bank\");
             return $b::balance_of(address(\"alice\"));
         }",
        &[bank.clone()],
    ).unwrap();
    let q = Engine::new()
        .query(&q_tx, &[bank], Fuel::new(20_000), &kv)
        .unwrap();
    assert_eq!(q.result, Value::U64(500));
}

// ---------- showcase example ----------

#[test]
fn example_37_router_dispatches_through_two_impls() {
    let mut sources = Vec::new();
    for e in std::fs::read_dir("examples/37_interface_dispatch").unwrap() {
        let p = e.unwrap().path();
        if p.extension().and_then(|s| s.to_str()) == Some("rd") {
            sources.push(std::fs::read_to_string(p).unwrap());
        }
    }
    let kv = rend::kv::InMemoryKv::new();
    let out = Engine::new()
        .execute_modules(&sources, "main", Fuel::new(200_000), &kv)
        .unwrap();
    // alice: usd 900, eur 899 → 900 * 10000 + 899
    assert_eq!(out.result, Value::U64(9_000_899));
}

// ---------- artifact round-trip ----------

#[test]
fn interface_artifact_round_trips() {
    let src = "
        module m;
        interface IFoo {
            entry view fn read() -> i64;
            entry fn write(x: i64) -> i64;
        }
        state f: IFoo;
        fn main() -> i64 {
            f = IFoo::bind(\"impl\");
            return 0;
        }
    ";
    let a = Engine::new().compile(src).unwrap();
    let b = rend::artifact::Artifact::from_bytes(a.bytes.clone()).unwrap();
    assert_eq!(a.content_hash, b.content_hash);
    assert_eq!(a.modules.len(), b.modules.len());
}
