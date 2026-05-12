// SLICE 8: optimistic concurrency with disjoint RW sets.
//
// The OCC driver (`rend::occ::commit_batch`) speculates each tx against a
// shared snapshot, then validates + commits in order. Disjoint RW sets
// compose; conflicting txs re-execute against the live KV.
//
// Voting illustrates this:
//   READS:   votes/<voter>, tally/<choice>
//   WRITES:  votes/<voter>, tally/<choice>
//
// Two voters voting for different choices → fully disjoint RW sets, both
// commit without re-execution. Two voters voting for the same choice
// conflict on `tally/<choice>` and the later one re-executes.

state votes: map<i64, i64>;   // voter id → 1 if voted else 0
state tally: map<i64, i64>;   // choice  → vote count

entry fn vote(voter: i64, choice: i64) -> bool {
    // Hoist the tally read above the votes write so both reads
    // (votes[voter], tally[choice]) cluster into one Kv::get_many.
    if votes[voter] != 0 { return false; }
    let prev_tally = tally[choice];
    votes[voter] = 1;
    tally[choice] = prev_tally + 1;
    return true;
}

fn main() -> bool {
    // Different drivers parameterize this — for the bare run we cast a
    // single ballot so the example file remains exercisable end-to-end.
    return vote(1, 1);
}
