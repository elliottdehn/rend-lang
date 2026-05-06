// SLICE 11 (working): the host submits this as the entry point.
//
// "Modules are defined as code, and execution is defined as submitting a
// main function involving that code." — each `.rd` file declares its
// own name with `module <name>;` at the top; the engine groups them
// without an out-of-band name table:
//
//   let sources = vec![
//       std::fs::read_to_string("examples/12_multi_module/main.rd")?,
//       std::fs::read_to_string("examples/12_multi_module/ledger.rd")?,
//       std::fs::read_to_string("examples/12_multi_module/governance.rd")?,
//   ];
//   let outcome = engine.execute_modules(&sources, "main", Fuel::new(100_000), &kv)?;
//
// All modules share one transaction; reads and writes accumulate across
// every module's state into a single (read_set, write_set) that the host
// commits via OCC.

module main;

fn main() -> bool {
    ledger::mint(address("0xtreasury"), 1000);
    governance::approve(42);
    return governance::payout(42, address("0xrecipient"), 100);
}
