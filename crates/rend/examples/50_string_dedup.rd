// High-performance string deduplication (interning). Maps each
// distinct string to a fresh u64 id; subsequent interns of the
// same string return the existing id at one-pmap-read cost.
//
// What makes it fluent + fast under rend:
//
//   - `forward: pmap<string, u64>` is HAMT-disjoint per key — two
//     concurrent interns of distinct strings hit different subtrees
//     and never fence each other at the storage layer.
//
//   - The only intentionally-contended cell is `next_id`. The hot
//     single-shot path bumps it inline; the batch path bumps it
//     once *outside* the `parallel { }` block, after which the
//     batch legs run uncontested over their pre-allocated slots.
//
//   - Each batch leg is leg-safe: interning an already-known string
//     is a one-read no-op. The pre-allocated slot is leaked (no
//     reverse entry, no harm) — the canonical id is the existing
//     one and subsequent reads still find it.
//
// id 0 is the "absent" sentinel: `pmap<string, u64>` defaults to 0
// for missing keys, so an empty `forward[s]` lookup is an
// unambiguous miss. First handed-out id is 1.

module strdedup;

state forward: pmap<string, u64>;
state reverse: pmap<u64, string>;
state next_id: u64;

entry view fn lookup(id: u64)  -> string { return reverse[id]; }
entry view fn highwater()      -> u64    { return next_id; }

// Single-string intern. One read, branch, two writes + counter
// bump on miss; one read on hit. The dedup branch returns before
// touching `next_id`, so the read of an already-interned string
// is fully concurrent with any other read.
entry fn intern(s: string) -> u64 {
    let existing = forward[s];
    if existing > 0u64 { return existing; }
    let id = next_id + 1u64;
    next_id = id;
    forward[s] = id;
    reverse[id] = s;
    return id;
}

// Batch primitive: register `s` at the caller-provided `slot` if
// `s` is absent, else return its existing id and leave `slot`
// orphaned. Designed for the parallel batch path — the caller
// pre-allocates the slot range serially (one `next_id` bump for
// the whole batch), then each `intern_at` leg runs in its own
// shadow Tx touching only its disjoint `forward[s]` /
// `reverse[slot]` cells.
entry fn intern_at(s: string, slot: u64) -> u64 {
    let existing = forward[s];
    if existing > 0u64 { return existing; }
    forward[s] = slot;
    reverse[slot] = s;
    return slot;
}

fn main() -> u64 {
    // ---- single-shot interns. Second alpha proves dedup. ----
    let id_alpha_1 = intern("alpha");                       // → 1
    let id_beta    = intern("beta");                        // → 2
    let id_alpha_2 = intern("alpha");                       // → 1 (hit)

    // ---- batch path: two fresh strings interned concurrently. ----
    // One serial next_id bump reserves slots 3 and 4 up front;
    // the two intern_at legs then run under shadow Txs that
    // touch fully disjoint forward / reverse cells. The legs'
    // returned ids escape the parallel block into the outer scope.
    let slot_a = next_id + 1u64;
    let slot_b = next_id + 2u64;
    next_id = slot_b;
    parallel {
        let id_gamma = intern_at("gamma", slot_a);          // → 3
        let id_delta = intern_at("delta", slot_b);          // → 4
    }

    // ---- round-trip: lookup the bytes back, re-intern, expect ----
    //      the same id. Confirms reverse map is populated AND that
    //      the bytes-driven dedup path also catches re-entry.
    let alpha_back = lookup(id_alpha_1);
    let id_alpha_3 = intern(alpha_back);                    // → 1

    let dedup_ok     = if id_alpha_1 == id_alpha_2 { 1u64 } else { 0u64 };
    let roundtrip_ok = if id_alpha_3 == id_alpha_1 { 1u64 } else { 0u64 };

    // Encoded result (positional — each slot verifies one property):
    //
    //   highwater()  =  4   (×1e8)   — four unique strings interned
    //   id_alpha_1   =  1   (×1e7)
    //   id_beta      =  2   (×1e6)
    //   id_gamma     =  3   (×1e5)
    //   id_delta     =  4   (×1e4)
    //   id_alpha_2   =  1   (×1e3)   — dedup hit returns same id
    //   id_alpha_3   =  1   (×1e2)   — round-trip via lookup
    //   dedup_ok     =  1   (×1e1)
    //   roundtrip_ok =  1   (×1e0)
    //
    //   = 412_341_111
    return highwater()  * 100000000u64
         + id_alpha_1   *  10000000u64
         + id_beta      *   1000000u64
         + id_gamma     *    100000u64
         + id_delta     *     10000u64
         + id_alpha_2   *      1000u64
         + id_alpha_3   *       100u64
         + dedup_ok     *        10u64
         + roundtrip_ok;
}
