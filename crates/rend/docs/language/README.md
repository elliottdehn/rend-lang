# Language reference

The full language surface, organized by concept. Start with [lexical](lexical.md) and
[modules](modules.md) for the basics, then dip into specific features as needed.

- [lexical structure](lexical.md) — keywords, comments, literals
- [modules](modules.md) — module declarations, cross-module calls, imports
- [types](types.md) — primitives, structs, enums, tuples, arrays, sets, dicts, maps
- [state and persistence](state-and-persistence.md) — `state`, `pmap`, `pvec`, granular fields, constants
- [indexes](indexes.md) — auto-maintained derived `pmap` indexes; the SQL `CREATE INDEX` replacement
- [functions and effects](functions-and-effects.md) — `fn`, `entry`, `view`, `pure`, `nore`, the effects classifier
- [pattern matching](pattern-matching.md) — `match`, exhaustiveness
- [comprehensions](comprehensions.md) — list, set, and dict comprehensions
- [JSON](json.md) — dynamic `json` values, `->` path access, conversion builtins
- [capabilities](capabilities.md) — `cap`, affine ownership, privileged construction
- [interfaces](interfaces.md) — `interface`, `IFace::bind`, `$value::method(args)`
- [events](events.md) — `event` and `emit`
- [modifiers](modifiers.md) — Solidity-style function wrappers
- [operators](operators.md) — arithmetic, comparison, logical, bitwise, indexing
- [builtins](builtins.md) — host-provided functions

## Reading order

If you're new to the language, this order is roughly bottom-up:

1. **Lexical + modules + types + operators** — the syntactic substrate.
2. **State and persistence** — what makes rend useful as an on-chain language.
3. **Functions and effects** — `view`/`pure` is where the language earns its keep.
4. **Pattern matching, events** — small but ubiquitous.
5. **Capabilities, interfaces, modifiers** — the abstraction tools.

If you're porting from Solidity, jump to [interfaces](interfaces.md) and
[capabilities](capabilities.md) — those are where rend's design diverges most.
