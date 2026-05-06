# JSON

The `json` type carries dynamic, schema-less data: opaque payloads from a
host, event blobs, configuration, anything where rend's static type system
isn't a fit. Internally it's **jsonb**-shaped: parsed at the trust boundary
into a structured value, then accessed in parsed form (no re-parsing per
field). State storage uses a tagged binary serialization, not the raw text.

**Buyer beware:** there is no declared schema. Missing keys yield
`json_null`. Type-mismatched conversions (calling `json_to_string` on a
number, etc.) abort the transaction with a runtime error.

## Construction

```rd
let j = parse_json("{\"name\": \"alice\", \"age\": 30}");
```

Or receive a `json` value from a host function (see [host](../runtime/host.md)).

The empty/default `json` value is `null`. Reading an unset state slot of
type `json` yields `null`.

## Path access (`->`)

```rd
let j = parse_json("{\"user\": {\"emails\": [\"a@x\", \"b@x\"]}}");

let user_obj      = j -> user;                 // → json (object)
let first_email   = j -> user -> emails -> [0]; // → json (string)
let with_quoted   = j -> "user" -> "emails";   // same as above
```

Three path-step forms:

- `j -> ident` — sugar for `j -> "ident"`. Convenient when the key is a
  valid identifier.
- `j -> "literal"` — explicit string-keyed access. Use this when the key
  has hyphens, dots, or non-ident characters.
- `j -> [expr]` — array index by integer expression.

Every step returns another `json`. Missing keys / out-of-range indices
return `null` (no error). Path access through `null` stays `null`:

```rd
let v = parse_json("{\"a\": null}");
v -> a -> nested -> deeper      // → null, no error
```

For computed string keys, use `json_get_field(j, key_expr)` directly —
the `->` syntax accepts only literal idents, quoted strings, or `[expr]`.

## Conversion to typed values

Once a path lands on a primitive, convert to a typed value:

| Function | Aborts if |
|---|---|
| `json_to_string(j) -> string` | not a JSON string |
| `json_to_i64(j) -> i64` | not an integer (or out of i64 range) |
| `json_to_u64(j) -> u64` | not a non-negative integer |
| `json_to_bool(j) -> bool` | not a JSON bool |
| `json_is_null(j) -> bool` | never (returns `true` for null, `false` otherwise) |
| `json_stringify(j) -> string` | never (canonical text dump) |

`json_to_*` functions abort on shape mismatch — use `json_is_null` first
when in doubt.

## Numeric handling

JSON has one numeric kind. rend splits it:

- Integers in `i64` range parse as `Json::Int`.
- Larger non-negative integers parse as `Json::U64`.
- **Fractional or exponent numbers (`1.5`, `1e10`) reject at parse time** —
  rend has no float type. If your JSON has floats, the host should
  pre-process them (round, scale, encode as strings) before sending in.

## Storage

`json` is storable in any composite: `state x: json;`, struct fields,
`pmap<K, json>`, `pvec<json>`, `pbtree<K, json>`. The serialized form is
the parsed binary structure — typically smaller than the original text and
faster to load. Round-trips lose original whitespace and may reorder keys
in objects (insertion order is preserved across a single round-trip but
isn't guaranteed across formats).

```rd
struct Event { id: u64, payload: json }

state events: pvec<Event>;

entry fn record(id: u64, raw: string) -> u64 {
    pvec_push(events, Event {
        id,
        payload: parse_json(raw),    // parse once, store parsed
    });
    return id;
}

entry view fn user_for(idx: u64) -> string {
    return json_to_string(events[idx].payload -> user -> id);
}
```

## Patterns

**Aggregating a typed field across opaque events:**

```rd
state events: pvec<json>;

entry view fn total_amount() -> i64 {
    let sum = 0;
    for e in events {
        sum = sum + json_to_i64(e -> amount);
    }
    return sum;
}
```

**Defensive lookup with default:**

```rd
let role = j -> user -> role;
let role_str = if json_is_null(role) { "guest" } else { json_to_string(role) };
```

**Stringifying for round-trip / hashing:**

```rd
let canonical = json_stringify(j);
// `canonical` is deterministic given `j` — same input → same bytes.
```

## What's missing

- **Object iteration** — no `for (k, v) in j` over JSON object pairs yet.
  Workaround: extract specific keys.
- **Computed array slicing / filtering** — only `[n]` direct indexing.
- **Float numbers** — fractional JSON numbers fail to parse.
- **Surrogate-pair Unicode escapes** — `\uXXXX\uYYYY` for code points outside
  the BMP isn't supported. Single BMP code points work.
- **Indexes that project through `->`** — out of scope for slice 1; for now,
  extract typed fields into struct columns and index those.

## See also

- [`tests/json.rs`](../../tests/json.rs) — full behavior surface
- [`src/json.rs`](../../src/json.rs) — parser, serializer, path helpers
- [host functions](../runtime/host.md) — passing `json` values across the boundary
