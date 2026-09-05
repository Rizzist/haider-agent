# v0.0.970 custom provider flow

Initial base: `38359fd3ba799c3e32a09c414f6f41abb90442bd` on `lane-970-customprov`. Final gate base: merged `origin/wave-970` at `7431f8e6` (journalview and ceilingdecl preserved).

## Claim audit (before editing)

| Claim | Verdict | Evidence |
| --- | --- | --- |
| `render.rs:1936–1942` renders Generic name/origin/model | Correct, incomplete | Those are the legacy/preset rows; a newer `discover_models` branch immediately above already rendered name/origin plus auth/family/key. |
| `app.rs:999–1000` pins Generic create/edit model-before-key order | Correct for legacy cards, incomplete | Legacy arrays are Name/Origin/Model and Origin/Model. The newer discovery create array at 986–997 was Name/Origin/Auth/ApiFamily/Key. Existing custom edits still selected the legacy array. |
| `app.rs:10203` assigns discovered models to this card | Wrong | This is `talk_setup_key_accepted`, the Deepgram transcription setup card. |
| `app.rs:10477` handles this card's empty discovered list | Wrong | This is `SetupStage::DeepgramModels`, also unrelated to provider setup. |
| `catalog.rs:149` defines `OpenAiCompatible { origin }` | Correct | The custom source already exists. |
| `catalog.rs:278` discovers `{origin}/models` under a credential-bearing fixed-origin guard | Correct endpoint/discovery, transport detail drifted | The custom branch at 296 uses shared `CompatibleOriginPolicy::TrustedLan` transport; fixed vendor sources use `FixedOriginGuard` at 329. The custom path retains resolve/validate/pin, proxy-off, redirect-refusal, bounded body and existing timeout discipline. |
| Discovery exists and only flow is wrong | Partially correct | Existing `provider.configure` discovers and persists inventory, but commits before the TUI can choose a model. There was no precreate picker or manual fallback. Registration also required another successful `/models` request. |

## Result

Generic create proceeds name → origin → masked API key → read-only discovery → model confirmation. Tab still exposes no-auth and API-family choices. A new non-durable `provider.models_probe` reuses the accounts worker and catalog transport; it changes no profile, account, cache or management receipt. Raw keys cross only `vault.stage`; the driver retains an opaque, connection-scoped reference through model selection and consumes it during account registration.

Successful discovery shows an arrow-key picker. Enter creates with the selected model and the entire discovered inventory. Selection prefers the existing configured default on edit, otherwise the server's advertised default, otherwise its first id. Supported default hints are top-level `default_model` or string `default`, and per-entry boolean `default` or `is_default`; a hint must name a returned id.

Unauthorized, unreachable, unavailable (including 404), non-compatible and empty-list responses fall back to a manually editable model id. The typed reason stays on the card; e.g. `server returned 404 for /models — type the model id`. Final configure preserves supplied inventory without requiring another successful model-list request. Custom account registration with an explicitly configured model does not use `/models` access as proof of inference authorization. An inference authentication error remains an inference error.

Existing custom edits reuse the saved key when key entry is blank, retain their identity and valid default, and use the same picker/fallback. Builtin editing and preset create defaults remain intact. Reopening connection fields or cancelling retires the old attempt; stale probe replies cannot modify a newer card. Disconnect drops the staged reference and returns to key entry. No raw key is rendered, Debug-printed, placed in a flash/event, or included in a durable configure command. Reflected keys in catalog ids/errors are rejected/redacted.

Read all supplied turnperf and turnperf2 evidence. Their conclusions constrain this lane: preserve durability and keepalive; make no unmeasured performance claim and do not change admission, CAS, startup or unrelated provider execution.

## Regression evidence

Focused TUI checks pass: 11 custom unit tests and 92 tests across ten affected integration targets (zero failures/ignored). All six new goldens pass without update mode. New named tests cover create/edit ordering, staging/probe/link mapping, full inventory and chosen default, manual fallback, stale attempts, masking, server-default variants, fake HTTP success/401/404/non-compatible/empty/unreachable/reflected-secret responses, and probe no-write behavior.

New picker and 404 fallback goldens cover 80×24, 118×36 and 160×50. The six existing `antigravity_accounts_*` and six `oauth_calendar_*` goldens do not render the Generic card and require no changed rows. Their pre-edit sorted SHA-256 manifest digest is `ffb3a548967e276f52ec15eee1f399154db8803406707106f3296a05f40add13`; final byte-identity check is recorded below. Existing accounts hierarchy and preset/card tests remain required.

| Existing golden | Rows changed | Reason |
| --- | --- | --- |
| `antigravity_accounts_dark.80x24.golden` | None; byte-identical | This fixture does not open the Generic provider card. |
| `antigravity_accounts_dark.118x36.golden` | None; byte-identical | This fixture does not open the Generic provider card. |
| `antigravity_accounts_dark.160x50.golden` | None; byte-identical | This fixture does not open the Generic provider card. |
| `antigravity_accounts_light.80x24.golden` | None; byte-identical | This fixture does not open the Generic provider card. |
| `antigravity_accounts_light.118x36.golden` | None; byte-identical | This fixture does not open the Generic provider card. |
| `antigravity_accounts_light.160x50.golden` | None; byte-identical | This fixture does not open the Generic provider card. |
| `oauth_calendar_dark.80x24.golden` | None; byte-identical | This fixture does not open the Generic provider card. |
| `oauth_calendar_dark.118x36.golden` | None; byte-identical | This fixture does not open the Generic provider card. |
| `oauth_calendar_dark.160x50.golden` | None; byte-identical | This fixture does not open the Generic provider card. |
| `oauth_calendar_light.80x24.golden` | None; byte-identical | This fixture does not open the Generic provider card. |
| `oauth_calendar_light.118x36.golden` | None; byte-identical | This fixture does not open the Generic provider card. |
| `oauth_calendar_light.160x50.golden` | None; byte-identical | This fixture does not open the Generic provider card. |

## Merge, gate and commit

The original worktree stores Git metadata outside the writable sandbox. Fetch was rejected opening external `FETCH_HEAD`, and merge was rejected locking external `ORIG_HEAD`. To complete a reviewable merge and lane commit without changing those permissions, an isolated checkout with writable Git metadata was created at `/private/tmp/customprov-git` on the same `lane-970-customprov` branch. Its origin is the same repository. Fetch and `git merge --no-commit origin/wave-970` succeeded there, first through journalview `368f093c`, then through ceilingdecl `7431f8e6`; lane changes reapplied without conflicts. No golden was hand-merged. The merged tracked source plus the eight new lane files was copied back to the original workspace and byte equality was verified before the final sibling build and gates. The original worktree's Git ref remains unchanged; the isolated lane commit and `customprov.bundle` in the original worktree are the delivery mechanism. The bundle carries the lane commit and merged upstream history since the initial `38359fd3…` base. No push is authorized or performed.

The first workspace run, before copying the merged source, completed with 5,341 aggregate passes, three failures and 13 existing ignores. It exposed three outdated expectations: welcome feature count 115 → 116, the inventory test still expecting discarded discovery inventory after an explicit model selection, and the exhaustive wire matrix missing the new probe method. All were corrected without weakening the underlying assertions. The method matrix is now 133 methods (68 supplemental). The redundant sibling-build invocation using package `haider-daemon` was corrected to the binary's actual package `haider-daemond`; no source fix was involved. All four required provider/CLI fixtures were regenerated via the existing test update modes and remain byte-identical to merged upstream: `provider_request_no_budget.json`, `run_jsonl_text_turn.jsonl`, `run_jsonl_tool_turn.jsonl`, and `oneshot_run_golden.jsonl`. The measured instruct-pipe test passes at 13,552 → 13,552 bytes (full prefix 19,736; registered tools 29; advertised tools 26; native descriptions 690). The handshake test passes at 115 → 116 features. Both selectors executed exactly one test.

All build/test invocations use `RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1 HAIDER_TEST_DEVICE_NAME=test-mac CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0`; disk is checked before every build-capable command with a 700 MiB stop floor. Sibling binaries are prebuilt before enabling `HAIDER_TEST_SIBLINGS_PREBUILT=1`; final gate uses two build jobs and four test threads. Merged `haiderd` is 201,235,968 bytes, above 10 MiB. `xtask test-count --update` and verification report 4,944 → 4,966 (+22 source test markers). Host execution is macOS arm64; Windows/Linux behavior is by inspection.

## Final gate results

- `cargo fmt --all -- --check`: exit 0.
- `cargo run --locked -q -p xtask -- test-count --update`, then verification: exit 0, 4,966 / 4,966.
- Provider-request/text/tool golden regeneration: 3 tests passed; one-shot regeneration: 1 passed. All four regenerated files remain byte-identical to merged upstream, and the full workspace run verifies them with update flags off.
- Instruct-pipe and handshake selectors: one test each, exit 0; measured values 13,552 bytes and 116 features.
- `cargo test -q --workspace --no-fail-fast`: exit 0, 5,372 top-level tests passed, 12 nested subprocess probes passed, 0 failed, 13 pre-existing ignored. Full command took 969.79 seconds including compilation. No ignores, timeouts or assertions were weakened. The existing debug render benchmark is functional test evidence only; this lane makes no latency claim.
- `cargo clippy --workspace --tests -- -D warnings`: exit 0, 161.14 seconds; no diagnostics.

All twelve existing accounts/calendar goldens were compared directly to initial commit `38359fd3…` and are byte-identical. Protected OAuth sources are unchanged. Source comparison covers all 1,854 tracked/new files in the merged delivery checkout; the 1,057-file gated-source hash manifest has SHA-256 `e6fac32e61848a48d5dc81403291dd507473afc9962f84adc7a9f6799bb57407`. After both gates, all 1,057 gated-source hashes were rechecked in both checkouts and remain unchanged.

Raw gate logs and machine-readable command exits are retained at `/private/tmp/customprov-final-*.log` and `/private/tmp/customprov-final-results.json`; the initial failed workspace run is `/private/tmp/customprov-workspace-test.log`. These totals distinguish source-marker baseline, top-level libtest counts, and reexecuted child probes.

## Independent verifier

- V1: before an edit probe can use an existing key, enforce the same claimed-origin repoint guard as final configure. Accepted; production guard and zero-discovery-call regression added.
- V2: final configure's endpoint validator issued another `/models` availability GET, defeating unreachable/manual fallback. Accepted; retain origin safety while removing the redundant availability dependency for explicitly chosen inventory, with a regression.

- V3: a keyless→keyed edit (or a provider with its account removed) could skip key collection. Accepted; blank-key reuse now requires a selected saved API-key account, and both missing-key transitions have tests.
- V4: control-separated key text in a model id could pass raw substring checks and be reconstructed by terminal rendering. Accepted; reject control-bearing and zero-width custom ids through shared terminal-width validation before picker/cache, with ESC, newline, U+200B and U+FEFF HTTP/daemon fixtures and a rendered no-echo test.
- V5: the rendered no-echo test exposed a long typed fallback reason clipping the manual-entry instruction at 80 columns. Accepted; wrap the reason by terminal width and keep the action intact on a separate row.

- V6 rejected as noise: a later review cited the old “live models discovered” completion text. Current code already used truthful “configured · model ready” and “registering its account” wording; rereading confirmed no change was needed.

Current verifier count: findings=6, real=5, noise=1. Both full gates passed. The independent verifier returned SHIP with no unresolved code findings.
