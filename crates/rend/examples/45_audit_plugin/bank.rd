// Bank module — owns the balances state and emits a `pub`
// struct on every transfer. It knows nothing about auditing;
// any module wanting to react to transfers wires up an
// `on bank::Transferred fn ...` handler.

module bank;

// `pub struct` makes the type referenceable from other modules
// via `bank::Transferred`. Bare `struct` would keep it private —
// foreign handlers wouldn't be able to bind to it.
pub struct Transferred { from: Address, to: Address, amount: u64 }

state balances: map<Address, u64>;

entry fn deposit(who: Address, amount: u64) {
    balances[who] = balances[who] + amount;
}

entry fn transfer(from: Address, to: Address, amount: u64) -> bool {
    let b = balances[from];
    let a = balances[to];
    balances[from] = b - amount;
    balances[to]   = a + amount;
    // Bank emits and moves on. The audit module's handler runs
    // in the same tx, atomically with the balance writes.
    emit Transferred { from: from, to: to, amount: amount };
    return true;
}

entry view fn balance_of(who: Address) -> u64 { return balances[who]; }
