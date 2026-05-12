// SLICE 29: capability tokens — unforgeable, non-Copy values that
// gate privileged operations. The pattern:
//
//   cap MintCap { uses_left: u64, max_per_call: u64 }
//
//   entry fn mint(auth: MintCap, amount: u64) -> MintCap { ... }
//
// To call `mint`, you must hand it a `MintCap`. The function consumes
// it (affine: non-Copy) and returns a fresh, slightly-reduced cap so
// the caller can mint again. There's no other way to obtain one — the
// literal `MintCap { ... }` is only legal inside this module.
//
// Compared with a permission table keyed by `msg_sender`:
//   * The static type of `mint` says exactly what authority is needed.
//   * Delegation is `derive_mint_cap` returning a narrower cap; no
//     central registry to update.
//   * Revocation is "burn the cap" (don't return it). No "revoked"
//     flag to forget to check.

module access_cap;

cap MintCap {
    // How many more mints this cap is good for. Zero means it's
    // exhausted — the next `mint` call will fail its assert.
    uses_left: u64,
    // Per-call ceiling. A "narrower" cap is one with a smaller
    // max_per_call.
    max_per_call: u64,
}

state balances: map<Address, u64>;
state total_supply: u64;

// The contract holds its own root cap in state. The module's
// `bootstrap` is the only path that can construct one from
// nothing — once it's been called, the cap exists only as a
// state value plus whatever copies the bootstrap caller kept.
state root_cap: MintCap;

entry fn bootstrap(uses: u64, ceiling: u64) -> u64 {
    // Construct the root cap. Privileged because the literal is
    // only legal inside this module.
    root_cap = MintCap { uses_left: uses, max_per_call: ceiling };
    return 1u64;
}

// Mint `amount` to `to`, consuming one charge of `cap`. Returns the
// remaining cap so the caller can chain mints.
entry fn mint(auth: MintCap, to: Address, amount: u64) -> MintCap {
    assert(auth.uses_left > 0u64);
    assert(amount <= auth.max_per_call);

    let prev_balance = balances[to];
    let prev_supply  = total_supply;
    balances[to] = prev_balance + amount;
    total_supply = prev_supply + amount;

    return MintCap {
        uses_left: auth.uses_left - 1u64,
        max_per_call: auth.max_per_call,
    };
}

// Delegation. Split the parent cap into (parent', child) where:
//   * the parent loses `give` charges,
//   * the child gets `give` charges,
//   * the child's per-call ceiling is `narrower_ceiling` (must be
//     <= the parent's, so we can't manufacture more authority).
//
// The tuple return is the affine-clean way to hand both halves back
// to the caller — the original `parent` cap is consumed exactly once.
entry fn derive_mint_cap(
    parent: MintCap,
    give: u64,
    narrower_ceiling: u64,
) -> (MintCap, MintCap) {
    assert(give <= parent.uses_left);
    assert(narrower_ceiling <= parent.max_per_call);
    return (
        MintCap {
            uses_left: parent.uses_left - give,
            max_per_call: parent.max_per_call,
        },
        MintCap {
            uses_left: give,
            max_per_call: narrower_ceiling,
        },
    );
}

// Convenience: pull the contract's root cap out of state, run a
// privileged op, put it back. This is the typical "the contract is
// its own admin" path — `msg_sender` checks would normally live
// here too in a production module.
entry fn admin_mint(to: Address, amount: u64) -> u64 {
    let auth = root_cap;
    let auth = mint(auth, to, amount);
    root_cap = auth;
    return balances[to];
}

entry fn balance_of(who: Address) -> u64 {
    return balances[who];
}

// ---- demo --------------------------------------------------------
//
// 1. Bootstrap the root cap with 5 charges, ceiling 1000.
// 2. Derive a child cap with 2 charges, ceiling 50 (deliberately
//    narrower) — Bob's "limited minter" cap.
// 3. Bob mints 30 to alice (under his ceiling, OK).
// 4. Put the parent cap back in state, mint another 100 to bob via
//    admin_mint (uses one root charge).
// 5. Return alice + bob balances summed for verification.

fn main() -> u64 {
    let alice = address("alice");
    let bob   = address("bob");

    bootstrap(5u64, 1000u64);

    // Pull root out of state, derive a child for bob, store the
    // (now-smaller) parent back.
    let parent = root_cap;
    let (parent, bob_cap) = derive_mint_cap(parent, 2u64, 50u64);
    root_cap = parent;

    // Bob mints 30 to alice. Bob_cap is consumed and reissued
    // (1 charge remaining on it after this) — but we drop it on
    // the floor since this is a one-off demo.
    let _bob_cap = mint(bob_cap, alice, 30u64);

    // Mint 100 to bob via the contract's own admin path. This
    // burns one of the root cap's remaining 3 charges.
    admin_mint(bob, 100u64);

    return balance_of(alice) + balance_of(bob);
}
