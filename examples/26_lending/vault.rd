// SLICE 11+: module `vault` — collateral custody.
//
// Holds users' deposits of one asset (the collateral asset). `loans`
// pulls collateral via `withdraw` when opening a position and pushes it
// back via `deposit` when the position closes. The vault has no notion
// of debt; it just trusts whoever calls it. (In production you'd ACL
// callers via signed-message imports; for this example the invariant
// is enforced by `loans` being the only caller in normal flow.)

module vault;

state deposits: map<Address, u64>;

entry fn deposit(user: Address, amount: u64) {
    deposits[user] = deposits[user] + amount;
}

entry fn withdraw(user: Address, amount: u64) -> bool {
    let b = deposits[user];
    if b < amount { return false; }
    deposits[user] = b - amount;
    return true;
}

entry fn balance_of(user: Address) -> u64 {
    return deposits[user];
}
