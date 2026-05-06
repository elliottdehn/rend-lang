// Module `members` — voting-power registry for the DAO.
//
// One map per member → power. `0` means "not a member" (default for
// unwritten cells), so `power_of` doubles as a membership probe.
// Every state change emits an event for off-chain indexers.

module members;

state powers: map<Address, u64>;

event Added(member: Address, power: u64);
event PowerChanged(member: Address, from: u64, to: u64);
event Removed(member: Address);

entry fn add(member: Address, power: u64) -> bool {
    assert(power > 0u64, "power must be positive");
    let existing = powers[member];
    assert(existing == 0u64, "member already exists");
    powers[member] = power;
    emit Added(member, power);
    return true;
}

entry fn set_power(member: Address, power: u64) -> bool {
    let prev = powers[member];
    assert(prev > 0u64, "not a member");
    assert(power > 0u64, "use remove() to drop a member");
    powers[member] = power;
    emit PowerChanged(member, prev, power);
    return true;
}

entry fn remove(member: Address) -> bool {
    let prev = powers[member];
    assert(prev > 0u64, "not a member");
    powers[member] = 0u64;
    emit Removed(member);
    return true;
}

entry fn power_of(member: Address) -> u64 {
    return powers[member];
}

entry fn is_member(member: Address) -> bool {
    return powers[member] > 0u64;
}
