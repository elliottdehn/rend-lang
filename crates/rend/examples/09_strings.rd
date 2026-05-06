// SLICE 9 (working): UTF-8 strings + Address.
//
// A simple name registry mapping addresses to display names. Demonstrates:
//   - string state values (variable-length, UTF-8 safe)
//   - Address-keyed maps
//   - the `address(s: string)` builtin that constructs a typed Address
//     out of an underlying UTF-8 byte sequence

state names: map<Address, string>;

entry fn register(who: Address, label: string) {
    names[who] = label;
}

entry fn lookup(who: Address) -> string {
    return names[who];
}

fn main() -> string {
    let alice = address("0xa11ce");
    register(alice, "Alice in Wonderland");
    return lookup(alice);
}
