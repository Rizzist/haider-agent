# C1 filesystem-tools mutation notes

Every mutation below was executed on 2026-08-03: production code was changed,
the named observer was run to a RUNTIME assertion failure, and the mutation was
then reverted. No compile-only failure is claimed as evidence. Observers live
in `crates/haider-tools/tests/c1_filesystem_tools_tests.rs` unless another file
is named.

| # | Production mutation (applied → reverted) | Runtime observer | Observed RUNTIME failure |
|---|---|---|---|
| M1 | Raised `SEARCH_PREVIEW_MATCHES` from the literal 200 to 201. | `search_caps_preview_at_200_and_cas_preserves_every_match` | `assertion failed: result.truncated` for the 201-match fixture. |
| M2 | Raised `GLOB_ENTRY_LIMIT` from the literal 500 to 501. | `search_and_glob_are_root_confined_sorted_and_bounded` | `assertion failed: glob.truncated` for 501 matching files. |
| M3 | Let a singular `fs_edit` proceed whenever its anchor count was nonzero instead of exactly one. | `edit_requires_exactly_one_anchor_or_nonempty_replace_all` | `expect_err("ambiguous anchor")` received an applied result reporting two matches. |
| M4 | Collapsed the missing-freshness `fs_edit` branch into `InvalidArgument` instead of typed `UnreadFile`. | `unread_existing_edit_and_write_are_typed_refusals` | The literal `matches!(edit, ToolError::UnreadFile { .. })` assertion failed. |
| M5 | Inverted the locked digest comparison for `fs_edit`. | `stale_mutation_is_typed_and_requires_a_reread` | The observer reached `expected stale_read` after a literal external rewrite instead of the typed stale verdict. Expectations use literal file contents and independently compare the two digests; no production constant can satisfy its own test. |
| M6 | Removed the success-path `FileFreshness` returned by the mutation worker. | `self_edit_and_write_chains_never_retrip_freshness` | The second write failed at runtime with `UnreadFile` instead of completing the create→write→edit→edit chain. |
| M7 | Made durable freshness reduction first-write-wins (`or_insert`) instead of last-outcome-wins. | `crates/haider-daemon/src/permissions_core_tests.rs::durable_tool_state_reduces_latest_freshness_per_session` | Exact comparison failed with left `blake3:old-literal`, right `blake3:new-literal`. |
| M8 | Renamed only the canonical `fs_edit` manifest to `fs_edit_mutant`. | `crates/haider-daemon/src/permissions_core_tests.rs::advertised_equals_dispatchable_for_all_three_c1_tools` | Runtime assertion: `fs_edit must be advertised`. The observer separately names search, glob, and edit literals and compares every typed route. |
| M9 | Appended `!` to every successful legacy `fs_read` result. | `existing_read_and_create_write_results_remain_byte_exact` | Exact output comparison failed with left `"exact\n!"`, right `"exact\n"`. |
| M10 | Advanced broker freshness before awaiting the terminal journal append. | `failed_terminal_append_never_advances_freshness` | After the injected append failure, `expect_err("failed read outcome cannot make file fresh")` received a successful edit. |
| M11 | Dropped freshness from a landed write whose change-ledger append failed. | `landed_write_with_ledger_failure_still_updates_freshness` | The next write failed `StaleRead` with distinct recorded/current literal digests instead of succeeding. |
| M12 | Renamed the additive outcome field on the wire to `freshness_mutant`. | `crates/haider-protocol/tests/golden_tests.rs::golden_effect_phases` | Golden drift showed `freshness_mutant` where the frozen fixture requires `freshness`. Existing outcome goldens remained unchanged when the optional field was absent. |
| M13 | Inverted the same locked digest comparator while exercising two sessions. | `child_edit_trips_parent_stale_without_sharing_session_state` | The child’s fresh edit failed spuriously with `StaleRead` even though its recorded/current digests were identical, proving the session-specific state and comparator are live in the child/parent law path. |
| M14 | Added a plausible recursive-search symlink arm using plain following `openat` instead of the descriptor-relative no-follow discipline. | `search_and_glob_are_root_confined_sorted_and_bounded` | Exact output gained the escaped line `src/escape/secret.rs:1:NEEDLE outside`; the literal confined result assertion failed at runtime. |
| M15 | Replaced the search collector's 8 KiB preview budget with `usize::MAX`. | `search_preview_is_eight_kib_utf8_safe_with_full_cas_overflow` | Runtime assertion `result.preview.len() <= 8 * 1024` failed for one multibyte line; the same observer decodes the preview as UTF-8 and compares the complete CAS bytes. |

Degenerate-fixture audit: match caps, tool names, error kinds, remediation,
anchor counts, file contents, and old/new digest strings are asserted as
literals in the tests. Search/glob limits are not imported from production.
The stale and child fixtures modify real files between broker/session states;
they do not manufacture a `StaleRead` value. The legacy-result observer checks
the complete `BoundedResult` projection rather than restating a helper.
