// SLICE 11 (working): module `ledger` — a token balance ledger.
//
// The `module ledger;` declaration at the top names this file's module so
// other modules can reach it via `ledger::<entry_fn>`. Cross-module callers
// see only `entry` functions; non-entry functions and state are
// module-private and unreachable from outside.
//
// State keys are namespaced by module: `balances` here lives under
//   state_root("ledger", "balances")
// so the same state name in another module can't collide.

module ledger;

state balances: map<Address, i64>;

entry fn balance_of(who: Address) -> i64 {
    return balances[who];
}

entry fn mint(to: Address, amount: i64) {
    balances[to] = balances[to] + amount;
}

entry fn transfer(from: Address, to: Address, amount: i64) -> bool {
    let b = balances[from];
    let a = balances[to];
    if b < amount { return false; }
    balances[from] = b - amount;
    balances[to]   = a + amount;
    return true;
}

// Module-private helper — not callable from other modules even if the name
// were guessed.
fn zero() -> i64 {
    return 0;
}
