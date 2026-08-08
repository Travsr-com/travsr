# get_execution_path benchmark — `travsr`

_2026-08-03T13:40:54.122Z_ · 36 ground-truth pairs · p50 7 ms

## Summary

| metric | value |
|---|---|
| recall (sink returned) | **100%** |
| source returned | 100% |
| mean nodes returned | 27.3 |
| mean padding beyond shortest path | 23.3 |
| mean path share (upper bound) | 0.221 |

`path_share` = (hops + 1) / nodes returned. It bounds how much of the
response can be route; the remainder is λ-corridor padding. It does **not**
say whether that padding is useful — that needs a judgement pass.

## By hop distance

| hops | pairs | recall | mean returned | path share |
|---|---|---|---|---|
| 2 | 12 | 100% | 22.9 | 0.287 |
| 3 | 12 | 100% | 27.9 | 0.178 |
| 4 | 12 | 100% | 31.1 | 0.199 |

## Per pair

| id | hops | source → sink | returned | sink? | ms |
|---|---|---|---|---|---|
| H2-01 | 2 | `fn:find_references_dotted_unique_bare_member_resolves` → `fn:SqliteStore.seed_synonyms_if_empty` | 45 | yes | 8 |
| H2-02 | 2 | `fn:SandboxedSpawn.spawn` → `fn:Session.filter` | 23 | yes | 26 |
| H2-03 | 2 | `fn:registerParityCommands` → `fn:stripLangTokens` | 15 | yes | 7 |
| H2-04 | 2 | `fn:noise_detects_root_node_modules` → `fn:is_scip_anonymous_local` | 3 | yes | 3 |
| H2-05 | 2 | `fn:chain_a_b_c_gives_shell_one` → `fn:ParseCache.insert` | 17 | yes | 10 |
| H2-06 | 2 | `fn:generic_go_map_function_emitted` → `fn:fixtures_dir` | 3 | yes | 3 |
| H2-07 | 2 | `fn:install_backend_with_progress` → `fn:travsr_dir` | 42 | yes | 2 |
| H2-08 | 2 | `fn:exact_match_wins_over_fts_result` → `fn:SqliteStore.open_in_memory` | 41 | yes | 7 |
| H2-09 | 2 | `fn:table_get` → `fn:spec` | 12 | yes | 16 |
| H2-10 | 2 | `fn:ingest_g2` → `fn:make_relative` | 16 | yes | 3 |
| H2-11 | 2 | `fn:commit_advance_misses` → `fn:status_for` | 14 | yes | 17 |
| H2-12 | 2 | `fn:find_references_path_hint_respects_slash_boundary` → `fn:sqlite_migration_runner` | 44 | yes | 8 |
| H3-01 | 3 | `fn:find_references_dotted_unique_bare_member_resolves` → `fn:get` | 46 | yes | 16 |
| H3-02 | 3 | `fn:SandboxedSpawn.spawn` → `fn:Resolver.build` | 23 | yes | 6 |
| H3-03 | 3 | `fn:chain_a_b_c_gives_shell_one` → `fn:i64_to_node_id` | 17 | yes | 7 |
| H3-04 | 3 | `fn:install_backend_with_progress` → `fn:AppContainerSpawn.set_stderr` | 42 | yes | 6 |
| H3-05 | 3 | `fn:exact_match_wins_over_fts_result` → `fn:SqliteStore.bootstrap_meta` | 41 | yes | 7 |
| H3-06 | 3 | `fn:table_get` → `fn:load_table` | 12 | yes | 14 |
| H3-07 | 3 | `fn:ingest_g2` → `fn:status_for_tables` | 16 | yes | 14 |
| H3-08 | 3 | `fn:commit_advance_misses` → `fn:global_path` | 14 | yes | 14 |
| H3-09 | 3 | `fn:find_references_path_hint_respects_slash_boundary` → `fn:SqliteStore.vocab_increment` | 44 | yes | 7 |
| H3-10 | 3 | `fn:l2a_fts_vocab_refcount_decremented_on_delete_prefix` → `fn:SqliteStore.backfill_fts_if_needed` | 28 | yes | 6 |
| H3-11 | 3 | `fn:ppr_returns_all_nodes_when_k_is_zero` → `fn:SqliteStore.backfill_vocab_if_needed` | 34 | yes | 7 |
| H3-12 | 3 | `fn:register_creates_registry_on_first_call` → `fn:home_dir` | 18 | yes | 3 |
| H4-01 | 4 | `fn:find_references_dotted_unique_bare_member_resolves` → `fn:unknown_key_msg` | 46 | yes | 16 |
| H4-02 | 4 | `fn:SandboxedSpawn.spawn` → `fn:Resolver.build_from_markers` | 23 | yes | 5 |
| H4-03 | 4 | `fn:chain_a_b_c_gives_shell_one` → `fn:repo_path` | 17 | yes | 14 |
| H4-04 | 4 | `fn:install_backend_with_progress` → `fn:marker_language` | 42 | yes | 3 |
| H4-05 | 4 | `fn:exact_match_wins_over_fts_result` → `fn:SqliteStore.put_node_fts` | 41 | yes | 7 |
| H4-06 | 4 | `fn:table_get` → `fn:resolve_status` | 12 | yes | 10 |
| H4-07 | 4 | `fn:ingest_g2` → `fn:table_get` | 16 | yes | 10 |
| H4-08 | 4 | `fn:find_references_path_hint_respects_slash_boundary` → `fn:SqliteStore.vocab_decrement` | 44 | yes | 6 |
| H4-09 | 4 | `fn:put_and_iter_edges` → `fn:node_id_to_i64` | 23 | yes | 6 |
| H4-10 | 4 | `fn:make_store_with_root` → `fn:SqliteStore.node_fts_tokens` | 23 | yes | 6 |
| H4-11 | 4 | `fn:ppr_returns_all_nodes_when_k_is_zero` → `fn:parse_env_f32` | 34 | yes | 2 |
| H4-12 | 4 | `fn:spawn_background_reindex_phase1` → `fn:one_minute_load` | 52 | yes | 4 |
