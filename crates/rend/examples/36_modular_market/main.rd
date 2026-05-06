// Module: main. The transaction's entry point. Demonstrates
// composing the token + market modules:
//   1. Mint to two addresses (alice and bob).
//   2. Alice lists three items.
//   3. Bob buys two of them.
//   4. Return a packed result the smoke test can verify.
//
// In a deployed setup these `module ...` files would each
// compile to their own artifact; the host would deploy
// `token` first (running its constructor), then `market`,
// then run a tx artifact whose `main` looks like this.

module main;

fn main() -> u64 {
    // The tx sender plays both seller and buyer in this demo
    // (msg_sender() is constant across the tx). In a real flow,
    // each user would invoke a separate tx with their own
    // TxContext. Here we just need the sender to have enough
    // balance for the buy, so we mint to themselves first.
    let me = msg_sender();
    token::mint(me, 1000u64);

    // List three items. msg_sender() = `me` is recorded as the
    // seller; `me` will pay the price into `me`'s own balance
    // on each buy, which is fine — we're exercising the cross-
    // module call shape, not modeling a real market.
    let i0 = market::list(50u64);
    let _i1 = market::list(75u64);
    let i2 = market::list(20u64);

    market::buy(i64(i0));
    market::buy(i64(i2));

    // Pack three facts into one number for the smoke test:
    //   how_many() * 1_000_000 (= 3) plus
    //   sold-listing prices (50 + 20 = 70).
    return market::how_many() * 1000000u64 + 70u64;
}
