// SLICE 11+: end-to-end NFT trade exercising three modules in one tx.
//
// Storyline:
//   - alice has 1000 coin. She mints two NFTs to herself and lists #1
//     at price 250 and #2 at price 400.
//   - bob, with 500 coin, buys NFT #1 — the buy is one transaction that
//     transfers coin and transfers the NFT atomically.
//   - alice cancels the listing for #2 because she changed her mind.
//   - main returns alice's final coin balance.
//
// Expected:
//   alice starts with 1000, gains 250 from the sale  → 1250
//   bob starts with 500, pays 250                    → 250
//   nft::owner_of(1) == bob, nft::owner_of(2) == alice (never sold)
//   market::is_listed(1) == false, market::is_listed(2) == false (cancelled)

module main;

fn main() -> u64 {
    let alice = address("0xa11ce");
    let bob   = address("0xb0b");

    coin::mint(alice, 1000u64);
    coin::mint(bob,    500u64);

    let id1 = nft::mint(alice);             // 1
    let id2 = nft::mint(alice);             // 2

    market::list(alice, id1, 250u64);
    market::list(alice, id2, 400u64);

    market::buy(bob, id1);                  // bob pays alice 250
    market::cancel(alice, id2);             // alice withdraws #2

    return coin::balance_of(alice);         // 1000 - 0 + 250 = 1250
}
