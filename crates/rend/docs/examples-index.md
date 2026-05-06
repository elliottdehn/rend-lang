# Examples index

Working programs for every major feature, in [`examples/`](../examples).

## Single-file examples

| File | Demonstrates |
|---|---|
| [01_arithmetic.rd](../examples/01_arithmetic.rd) | arithmetic, control flow, recursion |
| [02_typed.rd](../examples/02_typed.rd) | explicit type annotations |
| [03_affine.rd](../examples/03_affine.rd) | affine ownership |
| [05_host_imports.rd](../examples/05_host_imports.rd) | host function binding |
| [06_persistent_counter.rd](../examples/06_persistent_counter.rd) | persistent single-slot state |
| [07_token.rd](../examples/07_token.rd) | `map<K,V>` ledger (ERC20 balances) |
| [08_concurrent_voting.rd](../examples/08_concurrent_voting.rd) | OCC with disjoint reads/writes |
| [09_strings.rd](../examples/09_strings.rd) | UTF-8 strings + Address |
| [10_arrays.rd](../examples/10_arrays.rd) | array literals, indexing, `len()` |
| [11_structs.rd](../examples/11_structs.rd) | struct decl + literal + field access |
| [15_mutable_fields.rd](../examples/15_mutable_fields.rd) | mutable struct field paths |
| [16_auction.rd](../examples/16_auction.rd) | sealed-bid auction (struct + map) |
| [17_integer_types.rd](../examples/17_integer_types.rd) | i32/u32/u64/u128 + conversions |
| [18_loops.rd](../examples/18_loops.rd) | `while` and `for-in` loops |
| [19_list_comprehensions.rd](../examples/19_list_comprehensions.rd) | list comprehensions |
| [20_sets.rd](../examples/20_sets.rd) | in-memory `set<T>` |
| [21_dicts.rd](../examples/21_dicts.rd) | in-memory `dict<K,V>` |
| [22_amm.rd](../examples/22_amm.rd) | constant-product AMM |
| [23_pipe.rd](../examples/23_pipe.rd) | pipe `\|>` and `$$` placeholder |
| [24_advanced.rd](../examples/24_advanced.rd) | ranges + bitwise + ergonomics |
| [27_token.rd](../examples/27_token.rd) | ERC20 with events + assertions |
| [29_vault.rd](../examples/29_vault.rd) | time-locked vault |
| [30_sum_types.rd](../examples/30_sum_types.rd) | enums + match |
| [31_qol_pack.rd](../examples/31_qol_pack.rd) | range loops + bitwise |
| [32_capabilities.rd](../examples/32_capabilities.rd) | unforgeable capability tokens |
| [33_pmap.rd](../examples/33_pmap.rd) | persistent maps (HAMT) |
| [34_pvec.rd](../examples/34_pvec.rd) | persistent vectors (trie) |
| [35_queryable_token.rd](../examples/35_queryable_token.rd) | `view`/`pure` end-to-end |
| [39_indexed_queries.rd](../examples/39_indexed_queries.rd) | walks + comprehensions + aggregations + indexes (the SQL-replacement story) |
| [40_event_log.rd](../examples/40_event_log.rd) | opaque JSON ingest + `->` paths + delete + sorted/multi indexes (full event-log shape) |

## Multi-file examples

| Directory | Demonstrates |
|---|---|
| [12_multi_module/](../examples/12_multi_module) | minimal multi-module split |
| [25_nft_marketplace/](../examples/25_nft_marketplace) | NFT marketplace |
| [26_lending/](../examples/26_lending) | lending protocol |
| [28_dao/](../examples/28_dao) | DAO with voting and governance |
| [36_modular_market/](../examples/36_modular_market) | modular market design |
| [37_interface_dispatch/](../examples/37_interface_dispatch) | interface dispatch with two impls |
| [38_swappable_oracle/](../examples/38_swappable_oracle) | swappable oracle via interface binding |

## Tests as examples

The [`tests/`](../tests) directory often shows shorter, more focused versions of the
same patterns, with assertions on expected output. Read `tests/<feature>.rs` for the
fastest path to "what does this feature actually do?"
