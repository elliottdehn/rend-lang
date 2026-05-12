// `reserve N from <state>` + `arr<T>[N]` + `parallel for ... to`
// — the three primitives that make rend's id-generating batch
// pattern read like the algorithm.
//
//   reserve N from next_id     →  [u64] of N fresh ids, one atomic
//                                 counter bump
//   arr<T>[N]                  →  [T] of length N filled with the
//                                 type's default value
//   parallel for id in src to out { body }
//                              →  body runs once per id under its
//                                 own shadow Tx; body's tail value
//                                 writes to out[idx]; `continue`
//                                 leaves the slot at default
//
// Demonstrates the workflow with a batch entity-registration use
// case: reserve 5 ids, process them in parallel (each leg records
// a state-pmap entry AND yields a timestamp into the output
// buffer), and demonstrate `continue` skipping one slot.

module registry;

state next_id:  u64;
state entities: pmap<u64, u64>;

fn main() -> u64 {
    // Reserve 5 fresh ids — one atomic bump on `next_id`, yielding
    // [u64] = [1, 2, 3, 4, 5].
    let ids = reserve 5u64 from next_id;

    // Pre-allocate the output buffer with the type's default (0).
    let timestamps = arr<u64>[5u64];

    // Parallel batch: each leg runs in its own shadow Tx with `id`
    // bound to ids[idx]. The body writes the entity record to a
    // disjoint pmap key (no conflict — HAMT-disjoint), then either
    // produces a "timestamp" tail value or `continue`s.
    //
    // `continue` leaves the output slot at its default (0). State
    // writes that happened *before* the `continue` still merge into
    // the parent Tx — the skip applies only to the output buffer
    // slot, not to the leg's state effects.
    parallel for id in ids to timestamps {
        entities[id] = id * 1000u64;
        if id == 3u64 {
            continue;
        }
        id * 1000u64
    }

    // Verify (encoded into one u64):
    //   next_id          = 5   (5 ids reserved, counter bumped)
    //   timestamps sum   = 12_000   (1000 + 2000 + 0 + 4000 + 5000;
    //                                slot for id=3 stays at default)
    //   entities[3]      = 3000     (state write happened before the
    //                                `continue`, so it persists even
    //                                though the output slot skipped)
    //
    //   = 5 * 100_000             (next_id)
    //   + 12_000 * 10             (sum × 10)
    //   + 1                       (entities[3] == 3000 sentinel)
    //   = 500_000 + 120_000 + 1 = 620_001
    let sum = timestamps[0i64] + timestamps[1i64] + timestamps[2i64]
            + timestamps[3i64] + timestamps[4i64];
    let entity_3_ok = if entities[3u64] == 3000u64 { 1u64 } else { 0u64 };
    return next_id * 100000u64
         + sum     *     10u64
         + entity_3_ok;
}
