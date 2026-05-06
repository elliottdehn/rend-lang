//! Capability tokens — non-Copy, struct-shaped, constructable only
//! inside the declaring module.

use rend::value::Value;
use rend::{run, Engine, Fuel};

// ---------- construction ----------

#[test]
fn cap_construct_and_field_access() {
    let v = run("
        cap MintCap {
            max_per_call: u64,
        }
        entry fn make() -> MintCap {
            return MintCap { max_per_call: 1000u64 };
        }
        fn main() -> u64 {
            let c = make();
            return c.max_per_call;
        }
    ").unwrap();
    assert_eq!(v, Value::U64(1000));
}

#[test]
fn cap_with_multiple_fields() {
    let v = run("
        cap MintCap {
            max_per_call: u64,
            uses_left: u64,
        }
        fn main() -> u64 {
            let c = MintCap { max_per_call: 500u64, uses_left: 7u64 };
            return c.max_per_call + c.uses_left;
        }
    ").unwrap();
    assert_eq!(v, Value::U64(507));
}

// ---------- affine semantics ----------

#[test]
fn cap_move_then_use_after_move_is_compile_error() {
    let err = run("
        cap MintCap {
            max_per_call: u64,
        }
        entry fn consume(c: MintCap) -> u64 {
            return c.max_per_call;
        }
        fn main() -> u64 {
            let c = MintCap { max_per_call: 100u64 };
            let a = consume(c);
            let b = consume(c);     // c was moved on the first call
            return a + b;
        }
    ").unwrap_err();
    assert!(err.to_string().contains("used after move") || err.to_string().contains("moved"));
}

#[test]
fn cap_as_function_param_and_return_works() {
    // A function taking a cap and returning it is the standard "use
    // and reissue" pattern: caller hands over the cap, we do the
    // privileged thing, we hand it back so the caller can act again.
    let v = run("
        cap MintCap {
            uses_left: u64,
        }
        entry fn use_one(c: MintCap) -> MintCap {
            assert(c.uses_left > 0u64);
            return MintCap { uses_left: c.uses_left - 1u64 };
        }
        fn main() -> u64 {
            let c = MintCap { uses_left: 3u64 };
            let c = use_one(c);
            let c = use_one(c);
            return c.uses_left;
        }
    ").unwrap();
    assert_eq!(v, Value::U64(1));
}

#[test]
fn cap_field_value_is_copy_even_if_cap_is_not() {
    // Reading a Copy field of a cap doesn't move the cap — the field
    // value is duplicated. The cap itself remains in place for further
    // reads.
    let v = run("
        cap LimitCap {
            max: u64,
        }
        fn main() -> u64 {
            let c = LimitCap { max: 50u64 };
            let a = c.max;     // copies the u64 — does not move c
            let b = c.max;
            return a + b;
        }
    ").unwrap();
    assert_eq!(v, Value::U64(100));
}

// ---------- storage ----------

#[test]
fn cap_in_state_round_trips() {
    // A cap stored in state then read back yields the same field
    // values. State reads return a fresh value (no aliasing), so
    // affine doesn't fight us here.
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        cap MintCap {
            uses_left: u64,
        }
        state root: MintCap;
        entry fn install() -> u64 {
            root = MintCap { uses_left: 42u64 };
            return 0u64;
        }
        entry fn read() -> u64 {
            let c = root;
            return c.uses_left;
        }
        fn main() -> u64 {
            install();
            return read();
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(50_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(42));
}

// ---------- delegation pattern ----------

// ---------- showcase example ----------

#[test]
fn example_32_capabilities_runs_end_to_end() {
    let kv = rend::kv::InMemoryKv::new();
    let src = std::fs::read_to_string("examples/32_capabilities.rd").unwrap();
    let out = Engine::new().execute(&src, Fuel::new(50_000), &kv).unwrap();
    // alice gets 30 (via bob's derived cap), bob gets 100 (via admin_mint
    // on root). Total 130.
    assert_eq!(out.result, Value::U64(130));
}

// ---------- collision rules ----------

#[test]
fn cap_with_same_name_as_struct_is_compile_error() {
    let err = run("
        struct MintCap { uses_left: u64 }
        cap MintCap { uses_left: u64 }
        fn main() -> u64 { return 0u64; }
    ").unwrap_err();
    assert!(err.to_string().contains("duplicate") || err.to_string().contains("shadows"));
}

#[test]
fn cap_split_into_parent_and_child() {
    // The "derive" pattern: split a parent cap into (smaller parent,
    // narrower child). Both come back in a tuple so the affine system
    // sees both halves consumed.
    let v = run("
        cap MintCap {
            uses_left: u64,
        }
        entry fn split(parent: MintCap, give: u64) -> (MintCap, MintCap) {
            assert(give <= parent.uses_left);
            let kept = parent.uses_left - give;
            return (
                MintCap { uses_left: kept },
                MintCap { uses_left: give },
            );
        }
        fn main() -> u64 {
            let p = MintCap { uses_left: 10u64 };
            let (parent, child) = split(p, 3u64);
            return parent.uses_left * 100u64 + child.uses_left;
        }
    ").unwrap();
    assert_eq!(v, Value::U64(703)); // 7 * 100 + 3
}
