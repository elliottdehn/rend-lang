// Driver tx that exercises the router across both
// implementations. Mints 1000 of each token to alice, routes
// payments through both, and returns alice's final balance in
// each — packed into one number for the smoke test.

module main;

fn main() -> u64 {
    let alice = address("alice");
    let bob   = address("bob");

    // Stand up balances on both sides.
    usd::mint(alice, 1000u64);
    eur::mint(alice, 1000u64);

    // Bind the router to USD, pay 100 via dynamic dispatch.
    router::use_token("usd");
    router::pay(alice, bob, 100u64);

    // Re-bind to EUR (same router code, different impl), pay 100.
    // EUR charges a 1u64 fee, so alice is down 101 not 100.
    router::use_token("eur");
    router::pay(alice, bob, 100u64);

    // Check both balances directly via each module's own API
    // (no dynamic dispatch here — concrete checks of state).
    let usd_left = usd::balance_of(alice);   // 900
    let eur_left = eur::balance_of(alice);   // 899

    // Pack: usd * 10000 + eur. Expected: 900 * 10000 + 899 = 9_000_899.
    return usd_left * 10000u64 + eur_left;
}
