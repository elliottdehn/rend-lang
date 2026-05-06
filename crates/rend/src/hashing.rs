//! Deterministic 128-bit hashing.
//!
//! FNV-1a 128-bit. Stable across platforms and Rust versions; not
//! cryptographically strong, but the runtime never relies on collision
//! resistance for safety — only for storage-key namespacing.

const FNV_OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
const FNV_PRIME: u128 = 0x0000000001000000000000000000013b;

pub fn fnv128(bytes: &[u8]) -> u128 {
    let mut h = FNV_OFFSET;
    for &b in bytes {
        h ^= b as u128;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// Compose a sub-key beneath a parent root. Used at runtime to derive a
/// per-cell KV key from a map state's root + the serialized key value.
pub fn child(root: u128, payload: &[u8]) -> u128 {
    let mut buf = Vec::with_capacity(16 + payload.len());
    buf.extend_from_slice(&root.to_be_bytes());
    buf.extend_from_slice(payload);
    fnv128(&buf)
}

/// Compile-time root for a top-level state declaration. Modules will
/// eventually prefix this with a module id; for now `module_id` is a
/// caller-chosen string (use `"main"` if none).
pub fn state_root(module_id: &str, state_name: &str) -> u128 {
    let mut buf = Vec::with_capacity(module_id.len() + state_name.len() + 2);
    buf.extend_from_slice(module_id.as_bytes());
    buf.push(0);
    buf.extend_from_slice(state_name.as_bytes());
    buf.push(0);
    fnv128(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        assert_eq!(fnv128(b"hello"), fnv128(b"hello"));
        assert_ne!(fnv128(b"hello"), fnv128(b"hellp"));
    }

    #[test]
    fn known_offset() {
        // empty input → offset basis
        assert_eq!(fnv128(b""), FNV_OFFSET);
    }

    #[test]
    fn state_roots_namespace_by_module() {
        let a = state_root("mod_a", "count");
        let b = state_root("mod_b", "count");
        assert_ne!(a, b, "same state name in different modules must collide-free");
    }

    #[test]
    fn state_roots_distinguish_names() {
        assert_ne!(state_root("m", "x"), state_root("m", "y"));
    }

    #[test]
    fn child_is_stable() {
        let root = state_root("m", "balances");
        assert_eq!(child(root, &[1, 2, 3]), child(root, &[1, 2, 3]));
        assert_ne!(child(root, &[1, 2, 3]), child(root, &[1, 2, 4]));
    }
}
