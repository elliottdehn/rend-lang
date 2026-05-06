// SLICE 11 (working): module `governance` — proposal approvals + treasury payouts.
//
// Calls into the `ledger` module by qualifying with `::`. The cross-module
// call dispatches to ledger's entry function, which executes against
// ledger's state namespace. governance never sees ledger's internal state
// directly — only the values returned through entry functions.

module governance;

state proposals: map<i64, bool>;   // id → approved

entry fn approve(id: i64) {
    proposals[id] = true;
}

entry fn payout(id: i64, recipient: Address, amount: i64) -> bool {
    if !proposals[id] { return false; }
    return ledger::transfer(treasury(), recipient, amount);
}

fn treasury() -> Address {
    return address("0xtreasury");
}
