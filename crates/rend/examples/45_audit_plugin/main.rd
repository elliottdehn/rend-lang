// Driver — deposits some funds and runs a few transfers.
// The audit module's handler fires on each transfer; we end
// by asking audit how many entries it logged.

fn main() -> u64 {
    let alice = address("alice");
    let bob   = address("bob");
    let carol = address("carol");

    bank::deposit(alice, 1000u64);
    bank::deposit(bob,   500u64);

    bank::transfer(alice, bob,   100u64);
    bank::transfer(alice, carol, 250u64);
    bank::transfer(bob,   carol, 50u64);

    // Three transfers → three audit entries. The audit module
    // never appears in this driver except in this final read.
    return audit::entry_count();
}
