// SLICE 13a+ (working once 13a lands): a constant-product AMM.
//
// One pool per (token_in, token_out) ordered pair. The reserve cells live
// under the same `reserves` map, keyed by the token Address. Trades follow
// the textbook x*y=k invariant:
//
//   amount_out = (r_out * amount_in) / (r_in + amount_in)
//
// u128 is used for reserves so the pool can hold balances larger than 2^63
// without surprise.
//
// Demonstrates: u128 + Address + map<Address, u128> + structs + state
// transition that yields a clean RW set under OCC.

state reserves: map<Address, u128>;

struct Quote { amount_out: u128, fee_paid: u128 }

entry fn deposit(token: Address, amount: u128) {
    reserves[token] = reserves[token] + amount;
}

entry fn swap(token_in: Address, token_out: Address, amount_in: u128) -> Quote {
    let r_in = reserves[token_in];
    let r_out = reserves[token_out];

    // 0.3% LP fee, applied to amount_in before the curve
    let fee = amount_in * 3u128 / 1000u128;
    let in_after_fee = amount_in - fee;

    let amount_out = (r_out * in_after_fee) / (r_in + in_after_fee);
    reserves[token_in]  = r_in + amount_in;
    reserves[token_out] = r_out - amount_out;

    return Quote { amount_out: amount_out, fee_paid: fee };
}

fn main() -> u128 {
    let usdc = address("0xusdc");
    let weth = address("0xweth");

    deposit(usdc, 1000000u128);
    deposit(weth, 500u128);

    let q = swap(usdc, weth, 1000u128);
    return q.amount_out;
}
