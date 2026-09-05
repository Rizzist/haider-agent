# ceilingdecl continuation 2 — merge resolution

The orchestrator started the real merge of `origin/wave-970` (`38359fd3`).
This continuation edits only working-tree files. No merge, checkout, reset,
staging, commit, or other Git metadata mutation was performed.

The initial marker scan found only `test-baseline.txt`; all incoming source,
contract, and fixture paths had merged automatically. The baseline tool
replaced both conflicting values, 4,904 and 4,910, with **4,929**: both lanes'
tests plus the new regression below. The index still reports the baseline as
unmerged until the orchestrator stages the resolved working-tree content.

## Semantic resolution and regression

Independent review found one real interaction: the new provider-rebind refresh
ran before the exhausted logical-request budget check. A route/registry/account
failure at that point replaced the required typed cap with `ProviderError`.
`crates/haider-core/src/actor.rs` now checks the existing logical budget before
refreshing the provider. Refresh remains before provider-specific history,
cache/CAS preparation and transport, and physical retries retain refresh.

`crates/haider-core/tests/support/request_budget_laws.rs` adds
`hard_request_bound_preserves_typed_terminal_before_provider_rebind_refresh`.
A resolver becomes unavailable after the two allowed requests. The new test
failed on the initial merge (`ProviderError` versus `RequestBudgetExceeded`)
and passes with the resolution. It checks two requests and refreshes, one typed
exit-78 terminal, unchanged before/after workspace receipts and retained partial
progress. All **12 request-budget laws pass**, including existing retry,
recovery, reused-call-ID and unavailable-receipt coverage. No test was weakened.

Preserved ceiling files include `actor_ceiling.rs`, `turn_workspace.rs`, protocol
`ceiling.rs`, client `headless.rs`, and CLI `run.rs`: typed cap results, hidden
chunked receipts, exact partial progress, durable replay and dedicated exit 78.
The worker retains both `ceiling_workspace` and the provider-rebind resolver.
Binary CAS frame/stream/hydration shapes, truncation footer/provenance and ordered
tool effects remain present and covered by their existing tests.

The owner-paste `[fidelity]` block in `ceilingdecl.md` was parsed with `tomllib`:
`declared_turn_ceiling = 64`, `internal_cap_exit_codes = [78]`, and
`workspace_path = "{workspace}"`. This remains an adapter-owner declaration;
no product adapter manifest is claimed installed.

## Pin verification

- `permissions_core_tests.rs`: **13,552 → 13,552 bytes**. The measured merged
  value passes `instruct_pipe_shrinks_the_advertised_wire_pack`, including the
  native-description accounting and unchanged 30% reduction floor.
- `connection_tests.rs`: incoming merge changes **114 → 115**. The exact required
  `cargo test -p haider-daemon --lib welcome_features_pin_served_management_families`
  passed at 115, including equality of the complete feature set. It emitted no
  `left:` diagnostic because the merged pin was already correct; no speculative
  re-pin or artificial failure was introduced.

## Golden regeneration and line review

All affected goldens were rewritten only by repository test tooling:

| Tool/test target | Update path | Passed |
|---|---|---:|
| CLI `turnhygiene_pin_tests` | `UPDATE_FIXTURES=1` | 10 |
| CLI `oneshot_boot_tests one_shot_jsonl_stream_matches_the_normalized_golden` | `HAIDER_ONESHOT_GOLDEN_UPDATE=1` | 1 |
| CLI `observe_cli_tests observe_json_schemas_are_goldened_and_secret_free` | `UPDATE_FIXTURES=1` | 1 |
| RPC `wire_golden_tests` | `UPDATE_FIXTURES=1` | 102 |
| Protocol `toolshape_tests` | `UPDATE_FIXTURES=1` | 10 |

A byte comparison against saved initial copies of all 160 JSON/JSONL fixtures
found **zero changes introduced by regeneration**. Against HEAD, precisely two
fixture files differ, both already carried by the incoming merge:

- `crates/haider-cli/tests/fixtures/observe_status.json`: its sole changed line
  adds only `daemon.caching` with the exact cache regime, per-provider regimes,
  TTL, prompt-cache/CAS flags and resident-session reuse values. Removing this
  added object produces the exact prior JSON value; no existing field changed.
- `crates/haider-rpc/tests/fixtures/provider_rebind_wire.json`: all 30 added lines
  were reviewed. They encode the correlated `session.provider.rebind` request
  with command/session/generation/provider/base URL/account, and its matching
  success receipt with selected sequence 17 and generation 3. No extra event,
  secret, or unrelated wire change is present.

`provider_request_no_budget.json`, the text/tool JSONL fixtures, one-shot JSONL,
existing wire fixtures and toolshape fixtures regenerate byte-identically.
The hidden ceiling receipt event pairs and subsequent sequence ordinals from
the prior ceilingdecl implementation remain intact. No golden was hand merged,
and no normalization rule was changed. Full gates run with all update flags
explicitly removed.

## Environment and preliminary gates

Every build/test/count invocation sets `RUST_MIN_STACK=8388608`,
`HAIDER_DISCOVERY_DISABLED=1`, `HAIDER_TEST_DEVICE_NAME=test-mac`,
`CARGO_INCREMENTAL=0`, and `CARGO_PROFILE_DEV_DEBUG=0`. A recorded `df -m /`
preflight enforces the 700 MiB floor before each command. Fresh sibling builds
preceded daemon tests and fixture regeneration; ordinary tests set
`HAIDER_TEST_SIBLINGS_PREBUILT=1`.

Final rebuilt binaries: `haider` **111,122,416 bytes**, `haiderd`
**200,738,352 bytes**, satisfying the daemon's 10 MiB floor.
`cargo fmt --all -- --check` passes. The unsafe guard passes at production
**189**, tests **20**. `xtask check` passes at **4,929/4,929**, retaining nine
existing soft LOC warnings. Product/source/contract whitespace checks pass.
Incoming raw evidence logs contain their original whitespace, which is retained
as evidence rather than edited to alter their recorded output.

## Full gate

`cargo test -q --workspace --no-fail-fast` **passed**, exit **0**, in
**1,212.53 seconds**. Its **336 emitted test-result records** total **5,341
passed, 0 failed, 13 existing ignored** tests (including nested subprocess
records). This executed count is distinct from the static source baseline of
4,929. No ignored test, platform gate or assertion was changed. The existing
large-history TUI test passed in 213.68 seconds. Fixture-update flags were unset.

All **1,045 tracked crate/manifest/lock/baseline input hashes** remained identical
before and after the complete workspace invocation.

`cargo clippy --workspace --tests -- -D warnings` **passed**, exit **0**, in
**173.56 seconds**, without diagnostics. All 1,045 input hashes remain unchanged
after Clippy as well. The complete working-tree marker scan finds **zero
conflict markers**; `rg` exit 1 records no matches, not a scan error.

Raw commands, environment/disk preflights, results and logs live in
`/tmp/ceilingdecl-continuation2/`. Primary records: `workspace-tests.log`,
`workspace-tests.result.json`, `workspace-test-totals.json`, `clippy-tests.log`,
`gate-inputs.json`, `post-test-input-check.json`, `post-clippy-input-check.json`,
`marker-scan.json`, `source-diff-check.json`, `golden-review.json`,
`goldens.diff`, `sibling-sizes.json`, and `test-count.log`.

## Citation audit and registry delta walk

Read the supplied common/brief, turnperf and turnperf2 evidence. Historical
receipt references in D7-5/X1-7: entry 304 and nonrepository check 345 are correct;
spawn-blocking 901 → 906 and post-scan 1005 → 1009 are drifted. The repository
walk is currently 614. No historical performance estimate is claimed measured
by this continuation. Windows/Linux behavior is by inspection; runtime gates
here execute on macOS arm64.

- #1–18: retained both typed API/metadata additions; the only production edit is
  budget-versus-provider-refresh ordering, with no new dependency or unsafe code.
- #19–20: formatter/whitespace checks and actual test-count tool; no test removal.
- #21, #42, #54, #64, #67, #81, #92: full ENV LAW, disk preflights, rebuilt sibling
  receipts and the mandatory daemon binary-size floor.
- #22–41, #43–53, #55–63, #65–66, #68–76, #78–80, #82–84: no new affected surface;
  prior credential, receipt, lifecycle, platform and integration tests retained.
- #30, #33, #50, #73, #88: V1 regression proves the cap wins before unavailable
  provider refresh; typed terminal/workspace/progress and both exact pins kept.
- #77, #85–87: unchanged unsafe counts; exact full workspace test and Clippy with
  test targets required, with actual exits and emitted test totals recorded.
- #89–91: resolve the real working tree; regenerate every affected golden through
  tooling, retain both merge sides, leave Git metadata and committing to owner.
- #93: no defined additional class in the supplied walk.
- #94–96: no new deadline, negotiated-connection wait or durability boundary;
  the existing cap now precedes unnecessary provider work. No performance claim.

## Per-file incoming resolution inventory

All rows below are automatic working-tree merges retained by this continuation,
except the explicitly described V1 adjustment in `actor.rs`. The new regression
and regenerated `test-baseline.txt` resolution are described above.

| File | Resolution |
|---|---|
| `crates/haider-cli/src/main.rs` | Retain incoming provider-rebind/caching implementation alongside existing ceilingdecl, CAS, and toolshape behavior. |
| `crates/haider-cli/src/observe.rs` | Retain incoming provider-rebind/caching implementation alongside existing ceilingdecl, CAS, and toolshape behavior. |
| `crates/haider-cli/src/session_provider.rs` | Retain incoming provider-rebind/caching implementation alongside existing ceilingdecl, CAS, and toolshape behavior. |
| `crates/haider-cli/tests/autospawn_tests.rs` | Retain automatically merged additive provider-rebind/caching tests and optional metadata defaults; covered by full workspace gate. |
| `crates/haider-cli/tests/fixtures/observe_status.json` | Regenerate via observe CLI golden helper; only daemon.caching is additive against HEAD. |
| `crates/haider-cli/tests/observe_cli_tests.rs` | Retain automatically merged additive provider-rebind/caching tests and optional metadata defaults; covered by full workspace gate. |
| `crates/haider-client/src/observe.rs` | Retain incoming provider-rebind/caching implementation alongside existing ceilingdecl, CAS, and toolshape behavior. |
| `crates/haider-client/tests/headless_run_tests.rs` | Retain automatically merged additive provider-rebind/caching tests and optional metadata defaults; covered by full workspace gate. |
| `crates/haider-client/tests/observe_tests.rs` | Retain automatically merged additive provider-rebind/caching tests and optional metadata defaults; covered by full workspace gate. |
| `crates/haider-core/src/actor.rs` | Retain ceiling capture/typed terminal and rebind/cache epoch; check exhausted logical budget before refreshing provider (V1). |
| `crates/haider-core/src/lib.rs` | Retain ceiling/receipt exports and add provider-rebind exports. |
| `crates/haider-core/src/sqlite_store.rs` | Retain CAS hydration and add provider-rebind store delegation. |
| `crates/haider-core/tests/runtime_tests.rs` | Retain ceiling law module and add rebind/rotation tests. |
| `crates/haider-daemon/src/accounts.rs` | Retain incoming provider-rebind/caching implementation alongside existing ceilingdecl, CAS, and toolshape behavior. |
| `crates/haider-daemon/src/accounts_provider_rebind_tests.rs` | Retain automatically merged additive provider-rebind/caching tests and optional metadata defaults; covered by full workspace gate. |
| `crates/haider-daemon/src/accounts_tests.rs` | Retain automatically merged additive provider-rebind/caching tests and optional metadata defaults; covered by full workspace gate. |
| `crates/haider-daemon/src/cache_policy_tests.rs` | Retain automatically merged additive provider-rebind/caching tests and optional metadata defaults; covered by full workspace gate. |
| `crates/haider-daemon/src/connection.rs` | Retain binary artifact negotiation and advertise provider rebind. |
| `crates/haider-daemon/src/connection_tests.rs` | Retain exact feature set including binary artifact and rebind; incoming numeric pin 114 → 115 passes. |
| `crates/haider-daemon/src/loom_authoring_tests.rs` | Retain automatically merged additive provider-rebind/caching tests and optional metadata defaults; covered by full workspace gate. |
| `crates/haider-daemon/src/mobile_runtime_tests.rs` | Retain automatically merged additive provider-rebind/caching tests and optional metadata defaults; covered by full workspace gate. |
| `crates/haider-daemon/src/permissions_core_tests.rs` | Keep permission tests with additive metadata defaults; real instruct pipe remains 13,552 bytes. |
| `crates/haider-daemon/src/project_instructions_tests.rs` | Retain automatically merged additive provider-rebind/caching tests and optional metadata defaults; covered by full workspace gate. |
| `crates/haider-daemon/src/provider_rebind.rs` | Retain incoming provider-rebind/caching implementation alongside existing ceilingdecl, CAS, and toolshape behavior. |
| `crates/haider-daemon/src/provider_rebind_tests.rs` | Retain automatically merged additive provider-rebind/caching tests and optional metadata defaults; covered by full workspace gate. |
| `crates/haider-daemon/src/session_hub/actor.rs` | Retain incoming provider-rebind/caching implementation alongside existing ceilingdecl, CAS, and toolshape behavior. |
| `crates/haider-daemon/src/session_hub/mod.rs` | Retain incoming provider-rebind/caching implementation alongside existing ceilingdecl, CAS, and toolshape behavior. |
| `crates/haider-daemon/src/session_hub/provider_rebind.rs` | Retain incoming provider-rebind/caching implementation alongside existing ceilingdecl, CAS, and toolshape behavior. |
| `crates/haider-daemon/src/session_hub/provider_rebind_tests.rs` | Retain automatically merged additive provider-rebind/caching tests and optional metadata defaults; covered by full workspace gate. |
| `crates/haider-daemon/src/session_hub/rpc.rs` | Retain incoming provider-rebind/caching implementation alongside existing ceilingdecl, CAS, and toolshape behavior. |
| `crates/haider-daemon/src/session_hub_private_tests.rs` | Retain automatically merged additive provider-rebind/caching tests and optional metadata defaults; covered by full workspace gate. |
| `crates/haider-daemon/src/subagent_core_tests.rs` | Retain automatically merged additive provider-rebind/caching tests and optional metadata defaults; covered by full workspace gate. |
| `crates/haider-daemon/src/tasks_runtime_tests.rs` | Retain automatically merged additive provider-rebind/caching tests and optional metadata defaults; covered by full workspace gate. |
| `crates/haider-daemon/src/worker.rs` | Retain ceiling_workspace and toolshape effects; install rebound metadata/resolver/cache epoch. |
| `crates/haider-daemond/tests/provider_rebind_rpc_tests.rs` | Retain automatically merged additive provider-rebind/caching tests and optional metadata defaults; covered by full workspace gate. |
| `crates/haider-protocol/src/session.rs` | Add optional provider metadata/rebound event; retain existing session fields and backward-compatible omissions. |
| `crates/haider-protocol/tests/golden_tests.rs` | Retain automatically merged additive provider-rebind/caching tests and optional metadata defaults; covered by full workspace gate. |
| `crates/haider-protocol/tests/schema_changelog_tests.rs` | Retain automatically merged additive provider-rebind/caching tests and optional metadata defaults; covered by full workspace gate. |
| `crates/haider-rpc/src/frame.rs` | Retain incoming provider-rebind/caching implementation alongside existing ceilingdecl, CAS, and toolshape behavior. |
| `crates/haider-rpc/src/lib.rs` | Retain incoming provider-rebind/caching implementation alongside existing ceilingdecl, CAS, and toolshape behavior. |
| `crates/haider-rpc/tests/automation_contract_doc_tests.rs` | Retain automatically merged additive provider-rebind/caching tests and optional metadata defaults; covered by full workspace gate. |
| `crates/haider-rpc/tests/common/mod.rs` | Retain automatically merged additive provider-rebind/caching tests and optional metadata defaults; covered by full workspace gate. |
| `crates/haider-rpc/tests/fixtures/provider_rebind_wire.json` | Regenerate via RPC golden helper; review every one of the 30 new request/receipt lines. |
| `crates/haider-rpc/tests/wire_golden_tests.rs` | Retain automatically merged additive provider-rebind/caching tests and optional metadata defaults; covered by full workspace gate. |
| `crates/haider-store/src/event_store.rs` | Keep receipt and CAS hydration boundaries; add atomic provider-rebind metadata/event/receipt operation. |
| `crates/haider-store/src/lib.rs` | Retain incoming provider-rebind/caching implementation alongside existing ceilingdecl, CAS, and toolshape behavior. |
| `crates/haider-store/tests/session_provider_rebind_tests.rs` | Retain automatically merged additive provider-rebind/caching tests and optional metadata defaults; covered by full workspace gate. |
| `crates/haider-tui/src/live.rs` | Retain incoming provider-rebind/caching implementation alongside existing ceilingdecl, CAS, and toolshape behavior. |
| `crates/haider-tui/tests/session_browser_tests.rs` | Retain automatically merged additive provider-rebind/caching tests and optional metadata defaults; covered by full workspace gate. |
| `docs/automation-contract-v1.md` | Retain existing artifact/tool behavior and add provider-rebind RPC/feature/caching docs. |
| `docs/client-contract-v1.md` | Retain typed cap/result/replay contract and add provider-rebind and caching contract. |
| `docs/event-schema-changelog.md` | Retain ceiling terminal and add provider-rebound event entry. |

## Final independent review and handoff

The research audit and final independent verifier agree on one finding in this
continuation: provider-rebind refresh could erase an already-exhausted typed
cap. It is fixed by the ordering change and the regression that failed before
and passed after the fix. No additional findings or rejected noise were found.
The verifier independently reviewed every changed golden line, all pin/count
receipts, and recomputed the 1,045 input hashes. Prior implementation findings
in the historical `ceilingdecl.md` report are not counted again here.

The merge remains uncommitted. All working-tree conflict markers are resolved;
`test-baseline.txt` remains `UU` in Git's unchanged index until the orchestrator
stages it. The supplied lane common/brief and turnperf/turnperf2 evidence were
not edited by this continuation, and no staging or commit was attempted.

VERIFIER: findings=1 real=1 noise=0 — provider refresh could erase an exhausted typed cap; budget precedence restored and regression added
SHIP
