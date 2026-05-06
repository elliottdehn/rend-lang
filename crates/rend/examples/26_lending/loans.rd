// SLICE 11+: module `loans` — collateralized debt positions.
//
// One transaction can:
//   1. Read an oracle price                 (oracle::price_of)
//   2. Pull collateral from a vault         (vault::withdraw)
//   3. Mint a new loan record               (own state)
// All under one (read_set, write_set) — the host commits or aborts the
// whole tx atomically. Two open-loans against disjoint assets touch
// disjoint price cells in `oracle`, disjoint deposit cells in `vault`,
// and disjoint id cells in `loans`, so they never conflict.
//
// Liquidation: if collateral_value < debt * 100 / liquidation_threshold,
// anyone may seize the collateral and close the position.

module loans;

struct Loan {
    borrower: Address,
    asset: Address,         // collateral asset (used to look up price)
    collateral: u64,        // units of `asset` locked
    debt: u64,              // amount owed (in cents, same unit as price)
    active: bool,
}

state book: map<i64, Loan>;
state next_id: i64;

// Health factor: returns true iff `collateral * price >= debt * threshold / 100`.
// Pulled out so `open` and `liquidate` agree on the rule.
fn healthy(asset: Address, collateral: u64, debt: u64, threshold_pct: u64) -> bool {
    let p = oracle::price_of(asset);
    return collateral * p * 100u64 >= debt * threshold_pct;
}

// Open a loan if the borrower can post enough collateral. Borrowers must
// be at least 150% over-collateralized at open time. Returns the new
// loan id, or -1 on rejection.
entry fn open(borrower: Address, asset: Address, collateral: u64, want: u64) -> i64 {
    if !healthy(asset, collateral, want, 150u64) { return -1; }
    if !vault::withdraw(borrower, collateral) { return -1; }
    let id = next_id + 1;
    next_id = id;
    book[id] = Loan {
        borrower: borrower,
        asset: asset,
        collateral: collateral,
        debt: want,
        active: true,
    };
    return id;
}

// Repay the full debt. Returns the collateral to the borrower and marks
// the loan inactive.
entry fn repay(id: i64, payment: u64) -> bool {
    let l = book[id];
    if !l.active { return false; }
    if payment < l.debt { return false; }
    vault::deposit(l.borrower, l.collateral);
    book[id].active = false;
    return true;
}

// Anyone may liquidate an underwater loan. Liquidation threshold is 110%:
// if collateral_value drops below 110% of debt, the position is unsafe
// and the keeper claims the collateral.
entry fn liquidate(id: i64, keeper: Address) -> bool {
    let l = book[id];
    if !l.active { return false; }
    if healthy(l.asset, l.collateral, l.debt, 110u64) { return false; }
    vault::deposit(keeper, l.collateral);
    book[id].active = false;
    return true;
}

entry fn debt_of(id: i64) -> u64 {
    return book[id].debt;
}

entry fn is_active(id: i64) -> bool {
    return book[id].active;
}
