// SLICE 32: a token contract that demonstrates the
// `view` / `pure` annotation system end-to-end:
//
//   * `view fn` declarations on every entry the host can invoke
//     via `Engine::query` — read-only paths that are guaranteed
//     not to mutate state, validated by the compiler.
//   * `pure fn` for derived calculations whose output depends
//     only on inputs (no state, no time, no randomness). The
//     compiler refuses to let one read state.
//   * Capability-gated mint: only the holder of a `MintCap`
//     (issued at construction time) can mint, so the visible
//     entry surface stays decoupled from "who's in charge".
//   * `pmap<Address, u64>` for balances — disjoint transfers
//     commit in parallel via the OCC root-pointer merge.
//   * Main runs once at deploy and seeds initial state.

module token;

cap MintCap {
    // How many more mints this cap is good for. A "narrower"
    // version of the root cap can be derived for delegated
    // mintage; once exhausted, the cap is dead weight.
    uses_left: u64,
    // Per-call ceiling, also enforced on derive.
    max_per_mint: u64,
}

state balances:     pmap<Address, u64>;
state total_supply: u64;
// Where the contract's own minting authority lives. The
// constructor seeds it; the `mint_for_self` entry rotates it
// after each use so the cap is never duplicated outside the
// state cell.
state root_cap:     MintCap;

// ---- read-only entries (queryable via Engine::query) ----------

entry view fn balance_of(who: Address) -> u64 {
    return balances[who];
}

entry view fn supply() -> u64 {
    return total_supply;
}

// `pmap_contains` distinguishes "explicit zero balance" from
// "address never touched the contract" — useful for any
// holder-list rendering. Counted as ReadOnly by the effects
// pass, so it's safe inside `view`.
entry view fn is_holder(who: Address) -> bool {
    return pmap_contains(balances, who);
}

// ---- pure helpers (no state, no host) -------------------------

// Bps = basis points = parts per ten thousand. Pure because the
// answer depends only on `amount` and `fee_bps`.
pure fn apply_fee(amount: u64, fee_bps: u64) -> u64 {
    return amount * fee_bps / 10000u64;
}

pure fn amount_after_fee(amount: u64, fee_bps: u64) -> u64 {
    return amount - apply_fee(amount, fee_bps);
}

// ---- mutating entries -----------------------------------------

entry fn transfer(to: Address, amount: u64) -> u64 {
    let from = msg_sender();
    assert(balances[from] >= amount);
    balances[from] = balances[from] - amount;
    balances[to]   = balances[to]   + amount;
    return balances[from];
}

// Mint by burning a charge of the contract's own root cap.
// Returns the new total supply.
entry fn mint_for_self(to: Address, amount: u64) -> u64 {
    let auth = root_cap;
    assert(auth.uses_left > 0u64);
    assert(amount <= auth.max_per_mint);
    balances[to]   = balances[to] + amount;
    total_supply   = total_supply + amount;
    root_cap = MintCap {
        uses_left:    auth.uses_left - 1u64,
        max_per_mint: auth.max_per_mint,
    };
    return total_supply;
}

// ---- constructor ----------------------------------------------
//
// `main` runs once when the host calls `Engine::deploy(...)`.
// Seeds initial supply to the deployer and installs the root
// cap with a generous per-mint ceiling and a finite charge.

fn main() -> u64 {
    let deployer = msg_sender();
    balances[deployer] = 1000000u64;
    total_supply       = 1000000u64;
    root_cap = MintCap {
        uses_left:    100u64,
        max_per_mint: 10000u64,
    };
    return total_supply;
}
