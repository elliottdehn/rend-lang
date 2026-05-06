// SLICE 13a (working): integer types beyond i64.
//
// Types: i32, i64, u32, u64, u128. All arithmetic is checked — overflow
// panics with a runtime Error, never wraps silently.
//
// Literals carry their type via a suffix:
//   42         i64 (default)
//   42i32      i32
//   42u32      u32
//   42u64      u64
//   42u128     u128
//
// Conversions are explicit and bounds-checked at runtime:
//   u128(some_i64)   panics if some_i64 < 0
//   i64(some_u128)   panics if some_u128 > i64::MAX
//
// Mixed-type ops are a compile-time error — there is no implicit promotion.
// Use the conversion builtins to cross types.

state total_supply: u128;
state nonce: u64;

entry fn mint(amount: u128) {
    total_supply = total_supply + amount;
    nonce = nonce + 1u64;
}

entry fn small_count(n: u32) -> u32 {
    return n + 1u32;
}

fn main() -> u128 {
    mint(100u128);
    mint(250u128);
    let _ = small_count(7u32);
    return total_supply;
}
