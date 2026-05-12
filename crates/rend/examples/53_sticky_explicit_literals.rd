// Sticky explicit literals — `` `T` `` as a first-class type.
//
// The tag carries the "constructed inertly via a `<expr>` context"
// property and propagates through let bindings, struct field access,
// and array indexing. `` `T` `` widens to `T` (drop the tag), but
// `T` does NOT narrow to `` `T` `` — to re-tag a computed value you
// must wrap it in a fresh `<expr>`.
//
// The use case: API authors declare at the signature site that an
// input must be inert data, never computed. The compiler enforces
// it at every call site, no matter how many helper-function hops
// the value flowed through. This is the SQL-injection-class fix at
// the *API* level rather than the construction-site level — the
// safety property is documented in the type, not in convention.
//
// Per-field sticky is also expressible: only some fields of a
// struct need to be inert. The non-sticky fields take any value
// (computed or not); the sticky fields propagate the constraint
// to the struct literal site independently of the other fields.

module config;

// `name` must originate from a backtick context. `version` can be
// any `u64` — computed at runtime is fine.
struct Profile {
    name:    `string`,
    version: u64,
}

state profiles: pmap<u64, Profile>;
state next_id:  u64;

// `register` takes an ordinary `Profile`. The sticky constraint
// lives inside the struct — every call site that constructs a
// `Profile` must satisfy the `` `string` `` field constraint on
// `name`, but the function body sees an ordinary `Profile`.
entry fn register(p: Profile) -> u64 {
    let id = next_id + 1u64;
    next_id = id;
    profiles[id] = p;
    return id;
}

fn main() -> u64 {
    // (1) Whole-struct backtick. Every sub-value is inert; the
    //     resulting `` `Profile{...}` `` widens to `Profile` at
    //     the call site.
    register(`Profile { name: "alpha", version: 1u64 }`);

    // (2) Per-field sticky. Only `name` needs to be tagged; the
    //     `version` field accepts any computed `u64`. The struct
    //     literal itself isn't backtick-wrapped — the constraint
    //     is enforced at the field level.
    let computed_version = 2u64 * 3u64;  // 6 — runtime arithmetic
    register(Profile {
        name:    `"beta"`,
        version: computed_version,
    });

    // (3) Sticky propagation through a typed `let`. Once bound to
    //     a `` `string` ``-typed slot, the value carries the tag
    //     forward — passing `stored_name` into the struct's
    //     `name` field works because its tracked type already
    //     satisfies the field's `` `string` `` requirement.
    let stored_name: `string` = `"gamma"`;
    register(Profile {
        name:    stored_name,
        version: 30u64,
    });

    // For reference, all of these would fail at typeck — the
    // sticky constraint catches the misuse no matter the shape:
    //
    //   let runtime_name: string = "delta";
    //   register(Profile { name: runtime_name, ... });
    //     // → expected `string`, got string — sticky narrowing
    //
    //   register(Profile { name: lookup_name(user_id), ... });
    //     // → expected `string`, got string — call result is
    //     //   computed, no backtick origin
    //
    //   register(`Profile { name: user_input(), ... }`);
    //     // → parse error — `<expr>` body cannot contain calls,
    //     //   construction-site check fires before sticky typing
    //     //   even comes into play

    // Encoded result:
    //   next_id              * 1000  =  3 * 1000  =  3000
    //   profiles[1].version  *  100  =  1 *  100  =   100
    //   profiles[2].version  *   10  =  6 *   10  =    60
    //   profiles[3].version  *    1  = 30 *    1  =    30
    //                                                ----
    //                                                3_190
    return next_id * 1000u64
         + profiles[1u64].version * 100u64
         + profiles[2u64].version *  10u64
         + profiles[3u64].version;
}
