# oauthcapture — multi-account OAuth capture and reset calendar

Date: 2026-09-04  
Base: `251b5f80` plus this uncommitted lane diff  
Verdict: **SHIP**

## Result

Haider now links every enrolled credential root without becoming a second
owner of its rotating refresh credential. The profile-scoped source registry
auto-enrolls the default Codex home when discovery is enabled and supports
additional roots through `haider account source add`. Each root has a stable
source ID, account alias, safe label, store mode, refresh owner, masked
identity, plan, scan/refresh/access-expiry timestamps, and health. The accounts
screen renders that access expiry instead of dropping it at the RPC-to-TUI
boundary. Removing a root leaves a visible `source_gone` account instead of
the ambiguous `credential unavailable` state.

File-backed Codex roots are read through at resolution time. Only the current
access token enters an ephemeral bundle; `refresh_token` is intentionally not
deserialized into the linked-source path. Codex remains refresh owner, and
Haider observes `auth.json` rotations through a coalescing 250 ms metadata
watcher plus a profile-jittered 15–20 second reconciliation fallback.
`last_refresh` is authoritative when present, token expiry is compared with
current wall time, and unchanged scans do not synthesize account generations
or management revisions. A restored/older `auth.json` generation is ignored by
reconciliation and rejected by read-through resolution. `file`, `keyring`,
`auto`, and `ephemeral` modes are
distinguished; non-file modes report `requires_origin_client`.

Strict prompt-free behavior is the production default. Device discovery and
the account actor never invoke the Claude Code native credential store, even
with a legacy import descriptor. A readable explicitly enrolled Claude file
survives a denied/unavailable native-store condition, but remains read-only and
policy-blocked because Claude subscription OAuth is not imported. Haider-owned
credentials remain in the profile file vault and retain their existing
single-flight refresh ownership.

The screenshot failure was repaired at both boundaries. OpenAI identities are
backfilled from ID-token claims and Codex account fields; legacy Anthropic
bundles receive a stable synthetic account coordinate. Missing or malformed
vault material now updates the descriptor to `source_gone` or
`source_unreadable`, and meter resolution maps typed expired, revoked,
policy-blocked, origin-client-required, and missing-source states instead of
collapsing them all to `credential_unavailable`.
Source reconciliation also preserves distinct `source_gone`, `unreadable`,
`symlink_escape`, `oversized`, `partial_write`, `missing_fields`, and
`invalid_json` health states; structurally valid Codex JSON with absent token
fields is classified as `missing_fields`.

The account management surface now includes source list/add/remove/scan RPCs,
CLI commands, source badges, and `haider account use`. Existing exact session
account pins continue to beat the mutable active/profile default; `--account`
is captured as that per-session pin. The accounts screen keeps its existing
scopes, and `s` now cycles through the reset calendar and back. `<` and `>`
continue to change the selected account.

The calendar uses the usage snapshot timestamp as “today” and renders provider
timestamps without inference: Anthropic `five_hour`/`seven_day` and OpenAI
primary/secondary window reset fields feed the five-hour and weekly markers.
Missing provider reset data renders `reset unknown`. Light and dark goldens are
pinned at 80×24, 118×36, and 160×50.

## Required behavior evidence

| Requirement | Test evidence |
| --- | --- |
| Multiple Codex roots, stable selection, source deletion, file/keyring/auto/ephemeral modes, source-declared freshness, no copied refresh token | `source_reconciliation_links_every_codex_root_without_owning_refresh_rotation` |
| File rotation wakes reconciliation; periodic fallback is stable and jitter-bounded | `credential_source_watcher_observes_atomic_origin_rotation` |
| Default strict discovery never touches a fake native store; readable file fallback remains visible | `strict_discovery_never_touches_native_store_when_file_is_absent`, `strict_discovery_links_readable_claude_file_as_policy_blocked_without_native_touch`, `native_denial_does_not_suppress_an_explicitly_readable_claude_file` |
| External imports never spend their refresh token | daemon OAuth/import refresh ownership tests plus the linked-bundle assertion above |
| Missing credential and Anthropic legacy identity migration | `startup_backfill_marks_missing_oauth_secret_as_source_gone`, `startup_backfill_synthesizes_stable_anthropic_meter_identity` |
| Typed revoked/expired/source meter failures and account-ID routing | `expired_revoked_and_linked_source_failures_have_distinct_meter_reasons`, usage-report fixture suite |
| Exact access expiry projection and rendering | `accounts_screen_renders_linked_and_unlinked_source_truth`, `account_list_link_carries_sources_without_dropping_or_rederiving_them` |
| Seven distinct source-failure health states and semantic missing-field parsing | `linked_source_failure_health_states_remain_distinct`, `linked_codex_missing_required_fields_are_reported_as_missing_fields` |
| Exact session pin beats active/profile selection | `selected_session_account_bypasses_mutable_active_account`; CLI account parsing/command tests |
| Calendar reset timestamps, scope cycling, account switching, both themes and three widths | `oauth_calendar_tests` (8 passed) and six `oauth_calendar_*.golden` fixtures |
| Additive RPC compatibility | `wire_golden_tests` (99 passed), including all source method pairs |

## Verification

All Cargo gates used `RUST_MIN_STACK=8388608`,
`HAIDER_DISCOVERY_DISABLED=1`, `HAIDER_TEST_DEVICE_NAME=test-mac`,
`CARGO_INCREMENTAL=0`, and `CARGO_PROFILE_DEV_DEBUG=0`. Daemon tests also used
`HAIDER_TEST_SIBLINGS_PREBUILT=1`. Free space was checked before every build
and stayed above the 700 MiB stop threshold.

- `cargo test -p haider-daemon --lib`: **934 passed, 3 pre-existing live tests ignored**.
- `cargo test -p haider-tui`: **passed**, including all 8 calendar tests.
- `cargo test -p haider-rpc --test wire_golden_tests`: **99 passed**.
- `cargo test -p haider-accounts -p haider-protocol -p haider-rpc -p haider-cli`: **passed**.
- Scoped all-target Clippy for `haider-accounts`, `haider-protocol`,
  `haider-rpc`, `haider-daemon`, `haider-cli`, and `haider-tui` with
  `-D warnings`: **passed**.
- `bash run.sh test` from `scripts/qa-gate`: **64 passed**.
- `cargo fmt --all -- --check` and `git diff --check`: **passed**.
- Fresh `target/debug/haiderd` measured 191,269,392 bytes during the initial
  gate and 220,079,272 bytes after the final prebuild, both above 10 MiB.

macOS behavior and the strict no-Keychain path were executed. Linux/Windows
Claude-file discovery and file-watch behavior are platform-neutral Rust and
are **by inspection** in this lane.

## Citation and scope audit

The researched design was supplied as `oauthcapture-analysis.md`; the requested
`oauth970/PLAN.md` path is absent. Its cited paths were from an older worktree,
so line numbers drifted while the described constructs remained correct:
single-root discovery, interactive native-store adoption, denied-store
fallback, OpenAI account-ID preflight, Anthropic identity absence, imported
refresh fallback, and session exact-pin behavior were all grep-located before
editing.

`LANE-COMMON.md` says not to touch `oauth.rs`/`oauth_tests.rs`, but the later
owner requirements explicitly identify those exact files and behaviors and
require removing interactive adoption, broadening fallback beyond `Missing`,
and preventing imported refresh rotation. The owner-specific instruction was
treated as controlling; changes in those files are limited to those requested
security/ownership corrections and their tests.

The owner-supplied untracked `LANE-COMMON.md`,
`LANE-BRIEF-oauthcapture.md`, `turnperf/`, and `turnperf2/` artifacts were not
modified. The work remains uncommitted.

SHIP
