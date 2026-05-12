// Polymorphic `json` cells, source-level JSON literals, and the
// `->` arrow operator for path access.
//
// Anywhere a `json` typed value is expected, you can write a JSON
// literal directly in source:
//
//     {"key": value, ...}     // object
//     [a, b, c]               // array
//     null                    // explicit null leaf
//     42 / "hi" / true / 3.14 // primitive (materializes as the
//                             // native rend type — no wrapper)
//
// Object/array literals can be arbitrarily nested. Primitive
// leaves keep their native rend type — there is no `Json::Int`
// vs `int` distinction.
//
// At runtime, `json` cells are *polymorphic*: the same cell can
// hold an int one block and an object the next; the deserializer
// dispatches on the on-disk tag. Use the `j -> key -> key` path
// operator (sugar for `json_get_field`) to drill into nested
// shapes, and the `json_to_*` builtins to extract a concrete
// type at a leaf.
//
// Example workload: a feature-flag store. Each flag's value is
// "whatever shape the team wanted" — a bool for on/off, a number
// for thresholds, an object for richer policy, a list for
// per-tier rollout percentages.

module flags;

state value: pmap<string, json>;

// Read-only getters typed at the leaf — convenient to call from
// product code that knows what shape it expects.

entry view fn flag_bool(name: string) -> bool {
    return json_to_bool(value[name]);
}

entry view fn flag_int(name: string) -> int {
    return json_to_i64(value[name]);
}

// For object-shaped flags, drill into a nested key and return the
// int leaf. The `->` operator only takes a *literal* key — when
// the key is a runtime value (a parameter), call
// `json_get_field` directly.
entry view fn flag_object_int(name: string, key: string) -> int {
    return json_to_i64(json_get_field(value[name], key));
}

fn main() -> int {
    // Set each flag with a different JSON shape:
    //   * primitive bool   — exposed as a literal `true`
    //   * primitive int    — bare integer
    //   * nested object    — { "type": "...", "max": ... }
    //   * object holding an array — tier rollout percentages
    //   * explicit null    — "not yet configured"
    value["dark_mode"] = true;
    value["max_login_attempts"] = 5;
    value["rate_limit"] = {"type": "per_user", "max": 100, "window_sec": 60};
    value["rollout"] = {"tiers": [10, 25, 50, 100]};
    // `null` is only a JSON literal inside `{...}`/`[...]`. At an
    // assignment RHS, fall back to `parse_json("null")`.
    value["experimental_routing"] = parse_json("null");

    // Pull a few values back out — exercising both the leaf
    // extractors and the path arrow:
    let attempts = flag_int("max_login_attempts");
    let max_rate = flag_object_int("rate_limit", "max");

    // Array indexing through json_get_index: drill into the
    // nested "tiers" array and read tier index 2 (50%).
    let tier2 = json_to_i64(json_get_index(value["rollout"] -> tiers, 2));

    // is_null check on the explicit-null flag.
    let is_unset = json_is_null(value["experimental_routing"]);

    // Encode the result so a single int return covers everything:
    //   attempts*1_000_000 + max_rate*100 + tier2*10 + (1 if unset).
    let out = attempts * 1000000 + max_rate * 100 + tier2 * 10;
    if is_unset {
        return out + 1;
    }
    return out;
}
