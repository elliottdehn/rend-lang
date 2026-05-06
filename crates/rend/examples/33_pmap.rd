// SLICE 30: persistent maps — `pmap<K, V>` is a HAMT-backed state
// type with the same `state[key]` syntax as `map<K, V>`, plus one
// builtin that `map<K, V>` cannot support correctly:
//
//   * `pmap_contains(s, k)` — distinguishes "explicitly set to V's
//     default" from "never set". `map<K, V>` collapses both into
//     "the default value", which can hide bugs.
//
// Notably absent: `pmap_len`. Maintaining a count would force every
// insert/remove to write a new root carrying the updated total,
// turning otherwise-disjoint subtree updates into root-cell write
// conflicts. Callers that need a count track it in their own state
// cell.
//
// Storage is tree-spread: each HAMT node lives in its own
// content-addressed KV cell, so two transactions inserting into
// disjoint subtrees touch disjoint cell sets.

module registry;

// A minimal user-registry / referral-credit ledger:
//   * `members` — who has registered. Membership is by *presence*,
//     not by value, so `pmap_contains` is what tells us "are they
//     in?". The stored u64 just records when they joined.
//   * `referrals` — who-referred-whom. A pmap of address→address.
//   * `credit` — running referral credit per address.

state members:   pmap<Address, u64>;
state referrals: pmap<Address, Address>;
state credit:    pmap<Address, u64>;

const REFERRAL_BONUS: u64 = 50u64;

// Register a new user. Returns `false` if they were already
// registered (so the caller can skip the bonus path), `true`
// otherwise.
entry fn register(who: Address) -> bool {
    if pmap_contains(members, who) {
        return false;
    }
    members[who] = 1u64;
    return true;
}

// Register `who` and credit their referrer. Demonstrates the
// composition: contains-then-set with a presence-distinct check.
entry fn register_with_referral(who: Address, referrer: Address) -> bool {
    if pmap_contains(members, who) {
        return false;
    }
    if !pmap_contains(members, referrer) {
        // Referrers must themselves be registered. Without
        // `pmap_contains` we couldn't tell "registered with bonus
        // 0" apart from "not registered".
        return false;
    }
    members[who] = 1u64;
    referrals[who] = referrer;
    credit[referrer] = credit[referrer] + REFERRAL_BONUS;
    return true;
}

entry fn credit_of(who: Address) -> u64 {
    return credit[who];
}

entry fn is_member(who: Address) -> bool {
    return pmap_contains(members, who);
}

// ---- demo --------------------------------------------------------
//
// Build a tiny network:
//   * alice registers solo.
//   * bob registers via alice's referral → alice's credit jumps.
//   * carol tries to register via dave (not registered) → fails.
//   * carol registers via bob → bob's credit jumps.

fn main() -> u64 {
    let alice = address("alice");
    let bob   = address("bob");
    let carol = address("carol");
    let dave  = address("dave");

    register(alice);
    register_with_referral(bob, alice);
    register_with_referral(carol, dave);   // fails — dave isn't a member
    register_with_referral(carol, bob);    // succeeds — bob is

    // Sanity-check membership and credits in the return value.
    // Layout: alice_credit * 1000 + bob_credit + (1 if carol present).
    let total = credit_of(alice) * 1000u64 + credit_of(bob);
    if is_member(carol) {
        return total + 1u64;
    }
    return total;
}
