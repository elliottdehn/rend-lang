// SLICE 11+: module `nft` — non-fungible token ownership.
//
// State:
//   owners[id]   → current owner address, default 0x (unowned)
//   next_id      → monotonic id allocator
//
// Cross-module: `market` calls `nft::transfer` while atomically settling
// a sale; `nft` never sees market's state.

module nft;

state owners: map<i64, Address>;
state next_id: i64;

entry fn mint(to: Address) -> i64 {
    let id = next_id + 1;
    next_id = id;
    owners[id] = to;
    return id;
}

entry fn owner_of(id: i64) -> Address {
    return owners[id];
}

entry fn transfer(from: Address, to: Address, id: i64) -> bool {
    if owners[id] != from { return false; }
    owners[id] = to;
    return true;
}
