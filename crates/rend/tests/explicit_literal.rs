//! `` `<expr>` `` — explicit literal. Parses the inner expression
//! normally, then walks the AST to ensure every node is in the
//! inert-literal subset: primitive literals, struct / array /
//! tuple / set / dict literals whose elements are themselves
//! inert, JSON literals, and the `-<numeric>` ergonomic
//! shorthand. Identifier references, function calls, computing
//! operators, control flow — anything that requires runtime
//! evaluation — is rejected at parse time.
//!
//! No type-level marker: once validated, the inner expression is
//! a normal literal downstream. The safety property is load-
//! bearing only at the construction site (the JIT-codegen layer
//! that embeds untrusted payload values into synthesized source).

use rend::value::Value;
use rend::{Engine, Fuel};

#[test]
fn explicit_literal_primitive_int() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module el;
        fn main() -> u64 {
            return `42u64`;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(1_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(42));
}

#[test]
fn explicit_literal_array() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module el;
        fn main() -> u64 {
            let xs = `[10u64, 20u64, 30u64]`;
            return xs[0i64] + xs[1i64] + xs[2i64];
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(2_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(60));
}

#[test]
fn explicit_literal_struct_with_inert_fields() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module el;
        struct Point { x: u64, y: u64 }
        fn main() -> u64 {
            let p = `Point { x: 3u64, y: 4u64 }`;
            return p.x * p.y;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(2_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(12));
}

#[test]
fn explicit_literal_negative_numeric_shorthand() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module el;
        fn main() -> i64 {
            let n = `-5`;
            return n + 8i64;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(1_000), &kv).unwrap();
    assert_eq!(out.result, Value::int(3i64));
}

#[test]
fn explicit_literal_rejects_function_call() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module el;
        entry fn give() -> u64 { return 1u64; }
        fn main() -> u64 {
            let v = `give()`;
            return v;
        }
    ";
    let err = Engine::new()
        .execute(src, Fuel::new(1_000), &kv)
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("explicit literal"),
        "expected explicit-literal rejection, got: {msg}",
    );
}

#[test]
fn explicit_literal_rejects_identifier_reference() {
    // Names refer to bindings — evaluation-capable — so a backtick
    // body cannot mention them.
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module el;
        fn main() -> u64 {
            let x = 5u64;
            let v = `x`;
            return v;
        }
    ";
    let err = Engine::new()
        .execute(src, Fuel::new(1_000), &kv)
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("explicit literal"), "got: {msg}");
}

#[test]
fn explicit_literal_rejects_arithmetic_operator() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module el;
        fn main() -> u64 {
            let v = `1u64 + 2u64`;
            return v;
        }
    ";
    let err = Engine::new()
        .execute(src, Fuel::new(1_000), &kv)
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("explicit literal"), "got: {msg}");
}

#[test]
fn explicit_literal_rejects_nested_code_in_struct_field() {
    // A struct literal is allowed, but a field that itself
    // requires evaluation is not — the constraint propagates
    // through every sub-expression.
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module el;
        struct Point { x: u64, y: u64 }
        entry fn give() -> u64 { return 7u64; }
        fn main() -> u64 {
            let p = `Point { x: 3u64, y: give() }`;
            return p.x;
        }
    ";
    let err = Engine::new()
        .execute(src, Fuel::new(1_000), &kv)
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("explicit literal"), "got: {msg}");
}

#[test]
fn explicit_literal_rejects_nested_code_in_array_element() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module el;
        entry fn give() -> u64 { return 7u64; }
        fn main() -> u64 {
            let xs = `[1u64, give(), 3u64]`;
            return xs[0i64];
        }
    ";
    let err = Engine::new()
        .execute(src, Fuel::new(1_000), &kv)
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("explicit literal"), "got: {msg}");
}

#[test]
fn explicit_literal_rejects_non_neg_unary() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module el;
        fn main() -> u64 {
            let v = `!true`;
            if v { return 1u64; } else { return 0u64; }
        }
    ";
    let err = Engine::new()
        .execute(src, Fuel::new(1_000), &kv)
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("explicit literal"), "got: {msg}");
}

#[test]
fn explicit_literal_widens_into_plain_typed_parameter() {
    // `` `T` `` widens to T at the call site — a backtick-tagged
    // value can flow into any plain `T` parameter without
    // ceremony.
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module el;
        entry fn double(n: u64) -> u64 { return n * 2u64; }
        fn main() -> u64 {
            let v = `21u64`;
            return double(v);
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(2_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(42));
}

#[test]
fn sticky_param_accepts_backtick_value() {
    // A `` `string` `` parameter accepts a backtick-tagged value
    // — the sticky tag matches.
    let kv = rend::kv::InMemoryKv::new();
    let src = r#"
        module el;
        entry fn register(name: `string`) -> u64 { return 1u64; }
        fn main() -> u64 {
            return register(`"alice"`);
        }
    "#;
    let out = Engine::new().execute(src, Fuel::new(2_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(1));
}

#[test]
fn sticky_param_rejects_non_backtick_value() {
    // A `` `string` `` parameter rejects a plain (non-tagged)
    // value — sticky narrowing is forbidden.
    let kv = rend::kv::InMemoryKv::new();
    let src = r#"
        module el;
        entry fn register(name: `string`) -> u64 { return 1u64; }
        fn main() -> u64 {
            let s: string = "computed_at_runtime";
            return register(s);
        }
    "#;
    let err = Engine::new()
        .execute(src, Fuel::new(2_000), &kv)
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("expected `string`"),
        "expected sticky-narrowing rejection, got: {msg}",
    );
}

#[test]
fn sticky_propagates_through_let_to_parameter() {
    // A backtick-tagged value bound to a let preserves its tag,
    // so passing the binding to a `` `T` `` parameter works.
    let kv = rend::kv::InMemoryKv::new();
    let src = r#"
        module el;
        entry fn register(name: `string`) -> u64 { return 1u64; }
        fn main() -> u64 {
            let n: `string` = `"elliott"`;
            return register(n);
        }
    "#;
    let out = Engine::new().execute(src, Fuel::new(2_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(1));
}

#[test]
fn computing_operator_erases_the_explicit_literal_tag() {
    // Adding `` `5u64` `` and 1u64 produces a plain u64 — the
    // result is computed, not constructed inertly. Passing it to
    // a `` `u64` `` parameter must fail.
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module el;
        entry fn want(n: `u64`) -> u64 { return n; }
        fn main() -> u64 {
            let v = `5u64` + 1u64;
            return want(v);
        }
    ";
    let err = Engine::new()
        .execute(src, Fuel::new(2_000), &kv)
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("expected `u64`"),
        "expected post-operator tag-erasure rejection, got: {msg}",
    );
}

#[test]
fn field_access_on_explicit_literal_struct_preserves_tag() {
    // A `` `Point` `` value's fields are themselves inert
    // literals — field access keeps the tag, so the field value
    // can flow into a `` `u64` `` parameter.
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module el;
        struct Point { x: u64, y: u64 }
        entry fn want(n: `u64`) -> u64 { return n; }
        fn main() -> u64 {
            let p = `Point { x: 10u64, y: 20u64 }`;
            return want(p.x);
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(2_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(10));
}

#[test]
fn array_indexing_on_explicit_literal_array_preserves_tag() {
    // `xs: ` `` `[u64]` `` `: xs[i]` is `` `u64` ``.
    let kv = rend::kv::InMemoryKv::new();
    let src = r"
        module el;
        entry fn want(n: `u64`) -> u64 { return n; }
        fn main() -> u64 {
            let xs = `[7u64, 8u64, 9u64]`;
            return want(xs[1i64]);
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(2_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(8));
}

#[test]
fn struct_field_typed_as_sticky_requires_backtick_value() {
    // Declaring a struct field as `` `T` `` forces every value
    // populating that field to originate from a backtick.
    let kv = rend::kv::InMemoryKv::new();
    let src = r#"
        module el;
        struct Profile { name: `string`, age: u64 }
        fn main() -> u64 {
            // Note: the *whole* struct literal isn't backtick-tagged
            // — only `name` needs to be. The field-level constraint
            // propagates independently.
            let computed = "runtime_string";
            let p = Profile { name: computed, age: 30u64 };
            return p.age;
        }
    "#;
    let err = Engine::new()
        .execute(src, Fuel::new(2_000), &kv)
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("expected `string`"),
        "expected sticky-field rejection, got: {msg}",
    );
}

#[test]
fn explicit_literal_json_object_parses_and_stores() {
    // JSON literal inside a backtick context — every value in the
    // object must itself be inert. Confirms the JSON-shape branch
    // of the validator and that the parsed value flows through
    // typeck + compile + VM normally.
    let kv = rend::kv::InMemoryKv::new();
    let src = r#"
        module el;
        state config: json;
        fn main() -> u64 {
            config = `{"limit": 100, "active": true, "label": "alpha"}`;
            return 1u64;
        }
    "#;
    let out = Engine::new().execute(src, Fuel::new(2_000), &kv).unwrap();
    assert_eq!(out.result, Value::U64(1));
}
