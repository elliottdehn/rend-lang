// SLICE 11+: a complete lending lifecycle in one transaction.
//
// Setup:
//   - oracle says ETH = 2000 cents (think $20.00 per unit)
//   - alice deposits 100 ETH in the vault (→ 200000 cents of value)
//   - alice opens a loan against her ETH for 100000 cents of debt
//     (50% LTV, well under the 150% over-collateralization rule)
//   - alice repays the full 100000 — vault credits her 100 ETH back.
//
// Final balance: 100 ETH (the original deposit, returned on repay).

module main;

fn main() -> u64 {
    let alice = address("0xa11ce");
    let eth   = address("0xeth");

    oracle::set_price(eth, 2000u64);
    vault::deposit(alice, 100u64);

    let loan_id = loans::open(alice, eth, 100u64, 100000u64);
    if loan_id < 0 { return 9999u64; }   // shouldn't hit this branch

    loans::repay(loan_id, 100000u64);

    return vault::balance_of(alice);     // 100
}
