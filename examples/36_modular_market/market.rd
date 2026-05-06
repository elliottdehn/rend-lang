// Module: market. Lists items (identified by an integer id) for
// sale at a token price. Cross-module calls into `token` move
// the actual funds. Uses `pvec` for the listing log so reading
// "listing #N" is a constant-time indexed lookup.
//
// The interesting cross-module shape:
//   * `market::buy` calls `token::transfer` to move funds.
//   * `market::active_count` is a `view` query that reads
//     listings + asks `token::balance_of` for sanity.
//   * State writes happen in *both* modules during a buy —
//     `market.listings[id]` (sold flag) and `token.balances`.

module market;

struct Listing {
    seller: Address,
    price:  u64,
    sold:   bool,
}

state listings: pvec<Listing>;

// Listings are append-only. Returns the new listing's index.
entry fn list(price: u64) -> u64 {
    let entry_listing = Listing {
        seller: msg_sender(),
        price:  price,
        sold:   false,
    };
    return pvec_push(listings, entry_listing);
}

// Buy listing #id. Pays `price` from the buyer to the seller
// via the token module, then marks the listing sold.
entry fn buy(id: i64) -> u64 {
    let l = listings[id];
    assert(!l.sold);
    let buyer = msg_sender();
    // Cross-module call — funds move in token's state.
    token::transfer(buyer, l.seller, l.price);
    // Mark the listing sold in market's own state. (pvec
    // overwrites are bounds-checked.)
    listings[id] = Listing {
        seller: l.seller,
        price:  l.price,
        sold:   true,
    };
    return l.price;
}

entry view fn how_many() -> u64 {
    return pvec_len(listings);
}

// View entry that touches *two* modules' state — both reads,
// no writes. Counted as ReadOnly by the effects pass because
// `token::balance_of` is itself declared `view`.
entry view fn seller_balance(id: i64) -> u64 {
    let l = listings[id];
    return token::balance_of(l.seller);
}
