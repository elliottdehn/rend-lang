// SLICE 11+: module `market` — sealed-bid NFT marketplace.
//
// Composes two siblings: `nft` (ownership) and `coin` (payments). A buy
// is one atomic guest call that:
//   1. checks the listing is active (state read in `market`)
//   2. moves coin from buyer to seller (cross-module write to `coin`)
//   3. moves the NFT from seller to buyer (cross-module write to `nft`)
//   4. marks the listing inactive (granular field write in `market`)
//
// The host sees a single (read_set, write_set) covering all three modules.
// Two trades on disjoint listings + disjoint buyers/sellers commute under
// OCC and never conflict.

module market;

struct Listing {
    seller: Address,
    price: u64,
    active: bool,
}

state listings: map<i64, Listing>;

entry fn list(seller: Address, token_id: i64, price: u64) -> bool {
    // The seller must actually own the NFT they're listing.
    if nft::owner_of(token_id) != seller { return false; }
    listings[token_id] = Listing { seller: seller, price: price, active: true };
    return true;
}

entry fn cancel(seller: Address, token_id: i64) -> bool {
    let l = listings[token_id];
    if l.seller != seller { return false; }
    if !l.active { return false; }
    // Granular field-path write: read the Listing, flip `active`, write
    // the Listing back. (The cell is one Listing-shaped blob in the KV;
    // map-cell sharding by field is a future slice.)
    listings[token_id].active = false;
    return true;
}

entry fn buy(buyer: Address, token_id: i64) -> bool {
    let l = listings[token_id];
    if !l.active { return false; }
    // Payment first; if the buyer can't cover it, abort before touching
    // the NFT so we never have half-state.
    if !coin::transfer(buyer, l.seller, l.price) { return false; }
    if !nft::transfer(l.seller, buyer, token_id) { return false; }
    listings[token_id].active = false;
    return true;
}

entry fn ask(token_id: i64) -> u64 {
    return listings[token_id].price;
}

entry fn is_listed(token_id: i64) -> bool {
    return listings[token_id].active;
}
