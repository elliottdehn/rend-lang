// Explicit literals — `` `<expr>` `` — solve the SQL-injection-class
// problem at the language level. The parser walks the inner
// expression and refuses to accept anything outside the
// inert-literal subset (primitive literals, struct literals with
// literal-only fields, array literals with literal-only elements,
// JSON literals, plus `-<numeric>` for negative numbers). Identifier
// references, function calls, computing operators, control flow —
// anything requiring runtime evaluation — is rejected at parse time
// with a clear span pointing at the offending node.
//
// The natural use site is the JIT-codegen boundary. When per-request
// `main` is synthesized from an untrusted request payload, the
// synthesizer emits the payload values inside backticks. The parser
// then guarantees that no code-shaped fragment could have slipped
// in — by construction, the embedded values are inert.
//
// Here we hardcode the "synthesized" body to illustrate the shape.
// In production the values inside backticks would be emitted by
// the codegen layer from JSON request bodies; the surrounding code
// stays the same.

module orders;

struct Order { price: u64, qty: u64, sku: string }

state book: pvec<Order>;

entry fn submit(o: Order) {
    pvec_push(book, o);
}

entry view fn book_value() -> u64 {
    let total = 0u64;
    for o in book {
        total = total + o.price * o.qty;
    }
    return total;
}

fn main() -> u64 {
    // Three orders coming from external input. Each Order struct
    // value is wrapped in `` `...` `` — the parser walked these
    // expressions and confirmed every sub-node is inert (literal
    // u64s and a literal string). No code, no operators, no
    // names; just data.
    submit(`Order { price: 100u64, qty:  5u64, sku: "widget" }`);
    submit(`Order { price: 200u64, qty:  3u64, sku: "gadget" }`);
    submit(`Order { price:  50u64, qty: 10u64, sku: "gizmo"  }`);

    // For reference, these would all fail at parse time inside a
    // backtick context — the validator refuses any node that
    // requires evaluation:
    //
    //   `book_value()`                          // function call
    //   `Order { price: 100u64 + 1u64, ... }`   // computing operator
    //   `Order { price: some_local_var,  ... }` // identifier ref
    //   `if true { 1u64 } else { 0u64 }`        // control flow
    //
    // The protection lives at the construction site. Once
    // validated, the inner value flows through typeck / compile /
    // VM as an ordinary Value — no type-level marker propagates,
    // so downstream code that consumes `o` doesn't need to know
    // it originated from a backtick context. The safety property
    // is load-bearing exactly where it needs to be: the point
    // where untrusted payload data crosses into rend source.

    // 100*5 + 200*3 + 50*10 = 500 + 600 + 500 = 1600.
    return book_value();
}
