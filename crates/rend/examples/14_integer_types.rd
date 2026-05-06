// SLICE 13+ (sketch): integer types beyond i64.
//
// Today the only integer type is `i64`, with checked overflow on every op.
// Future slices add:
//
//   i32, i64       — signed
//   u32, u64, u128 — unsigned (u128 for crypto-adjacent code)
//
// Conversions are explicit and bounds-checked at runtime:
//
//   let x: u64 = u64(some_i64);     // panics if some_i64 < 0
//   let y: i64 = i64(some_u128);    // panics if some_u128 > i64::MAX
//
// Arithmetic semantics:
//   - all ops checked by default (no silent wrap)
//   - explicit `wrapping_add`, `wrapping_mul` etc. for the rare wrap cases
//   - mixed-type ops require explicit conversion; no implicit promotion
//
// Storage encoding: each variant gets a serialization tag (0x10..0x18) so
// that storing a `u128` vs an `i64` in the KV is unambiguous.

fn main() -> i64 {
    // placeholder — recompiles unchanged once the new types land
    return 0;
}
