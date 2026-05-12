// Arbitrary-precision `int` / `uint` and IEEE-754 `float`.
//
// `int` and `uint` graduated to BigInt — no fixed bit width, no
// silent wrap. Use the suffixless form `42` for `int` and `42u` for
// `uint`. Sized variants (`i32 / u32 / u64 / u128`) still exist for
// when you want overflow checking at a specific width.
//
// `float` is f64 with IEEE-754 semantics (`div` by zero is `inf`,
// not an error). Floats are useful for ratios where loss of
// precision is acceptable; never use them for money.
//
// This program models a sovereign-debt counter whose totals
// outgrow `u128` after a few decades of compound interest. Sized
// integers would overflow; `uint` just keeps going.

module treasury;

// Cumulative debt in the country's smallest currency unit. Easily
// past 2^128 once you compound for long enough — that's why this
// is `uint`, not `u128`.
state outstanding: uint;
// Number of bond auctions ever held. Granular `u64` is plenty
// for a counter — no reason to pay BigInt encoding for it.
state auction_count: u64;
// Inflation index. Floats are fine here: rounding noise at the
// 15th decimal place doesn't matter for an inflation reading.
state cpi: float;

// Issue `principal` of new debt. `principal` is `uint` so the
// caller can hand us a number that wouldn't fit anywhere else.
entry fn issue(principal: uint) {
    outstanding = outstanding + principal;
    auction_count = auction_count + 1u64;
}

// Compound the debt by `rate` percent (e.g. 5 means "5%"). The
// multiplication can overflow any fixed-width integer; `uint`
// scales as far as the call needs.
entry fn compound(rate: uint) {
    let scaled = outstanding * (100u + rate);
    outstanding = scaled / 100u;
}

// Update the CPI to `new_cpi`. Returns the delta as a float
// because the caller usually wants the change for display.
entry fn set_cpi(new_cpi: float) -> float {
    let delta = new_cpi - cpi;
    cpi = new_cpi;
    return delta;
}

fn main() -> uint {
    // Start with one unit. The point of the demo isn't the size of
    // the initial figure; it's that compounding past 2^128 just
    // works. A `u128`-typed state here would panic on overflow
    // around round 128.
    issue(1u);

    // 256 doublings (compound at 100% per round): the final number
    // is `2^256`, which is roughly `1.16e77` — well past
    // `u128::MAX` (~3.4e38). `uint` doesn't care.
    let i: u64 = 0u64;
    while i < 256u64 {
        compound(100u);
        i = i + 1u64;
    }

    // Inflation moved from 0.0 to 3.2 over the same period.
    let delta = set_cpi(3.2);
    // We don't return `delta` — it's just here to exercise floats.
    let _ = delta;

    return outstanding;
}
