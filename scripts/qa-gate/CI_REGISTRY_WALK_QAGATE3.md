# qagate3 CI error registry walk

Read against the final uncommitted tree before verification. “checked: none” means this turn-proof lane does not touch that error surface.

- #1 checked: none — no affected surface in this lane
- #2 checked: none — no affected surface in this lane
- #3 checked: none — no affected surface in this lane
- #4 checked: none — no affected surface in this lane
- #5 fixed — lazy POSIX harness load after need resolution (`gate/tui_probe.py`)
- #6 checked: none — no affected surface in this lane
- #7 fixed — the tracing-only provider dependency is declared in the manifest and lockfile
- #8 checked: none — no affected surface in this lane
- #9 checked: none — no affected surface in this lane
- #10 fixed — Python self-tests and `py_compile`; no dead Rust helpers added
- #11 checked: none — no affected surface in this lane
- #12 checked: none — no affected surface in this lane
- #13 checked: none — no affected surface in this lane
- #14 checked: none — no affected surface in this lane
- #15 checked: none — no affected surface in this lane
- #16 checked: none — no affected surface in this lane
- #17 checked: none — no affected surface in this lane
- #18 checked: none — no affected surface in this lane
- #19 fixed — `cargo fmt --check` and `git diff --check` cover the Rust/Python/docs surface
- #20 checked: none — no Rust test-count change
- #21 checked: none — no affected surface in this lane
- #22 fixed — `haider.turn` is opt-in and the installed subscriber accepts only audited numeric fields and phase enums
- #23 checked: none — no affected surface in this lane
- #24 checked: none — no affected surface in this lane
- #25 checked: none — no affected surface in this lane
- #26 checked: none — no affected surface in this lane
- #27 checked: none — no affected surface in this lane
- #28 checked: none — no affected surface in this lane
- #29 fixed — runner-owned status PID and no-orphan cleanup remain authoritative
- #30 fixed — named action/receipt actuals replace silent timeout-only evidence
- #31 checked: none — no affected surface in this lane
- #32 checked: none — no affected surface in this lane
- #33 fixed — runner metadata extension is additive and self-tested
- #34 checked: none — no affected surface in this lane
- #35 checked: none — no affected surface in this lane
- #36 checked: none — no affected surface in this lane
- #37 checked: none — no affected surface in this lane
- #38 checked: none — no Rust collection key seam
- #39 checked: none — no affected surface in this lane
- #40 checked: none — no affected surface in this lane
- #41 fixed — existing short hermetic root contract retained and exercised
- #42 fixed — installed-binary warmup remains runner-owned
- #43 checked: none — no affected surface in this lane
- #44 fixed — real UDS daemon/TUI probes executed in the allowed environment
- #45 checked: none — no affected surface in this lane
- #46 checked: none — no affected surface in this lane
- #47 checked: none — no affected surface in this lane
- #48 checked: none — no affected surface in this lane
- #49 checked: none — no affected surface in this lane
- #50 checked: none — no affected surface in this lane
- #51 checked: none — no affected surface in this lane
- #52 fixed — help and every required surface are pinned at 118x36 and 80x24
- #53 checked: none — no affected surface in this lane
- #54 checked: none — no affected surface in this lane
- #55 checked: none — no affected surface in this lane
- #56 checked: none — no affected surface in this lane
- #57 fixed — source help list, rendered help, and RPC catalog are reconciled together
- #58 checked: none — no affected surface in this lane
- #59 checked: none — no affected surface in this lane
- #60 checked: none — no affected surface in this lane
- #61 checked: none — no affected surface in this lane
- #62 checked: none — no affected surface in this lane
- #63 checked: none — no affected surface in this lane
- #64 checked — installed `haider`/`haiderd` inspected before final verdict
- #65 checked: none — no affected surface in this lane
- #66 checked: none — no affected surface in this lane
- #67 checked: none — no affected surface in this lane
- #68 checked: none — no affected surface in this lane
- #69 checked: none — no affected surface in this lane
- #70 checked: none — no affected surface in this lane
- #71 fixed — real installed artefact is exercised end-to-end, not inferred from tests
- #72 checked — fake-provider hermetic lane intentionally disables native discovery
- #73 checked: none — no fixed-byte source window
- #74 fixed — existing check context supplies throwaway HOME and profile
- #75 checked: none — no affected surface in this lane
- #76 checked: none — no affected surface in this lane
- #77 fixed — unsafe-count guard ran first and again at close (production=188/test=16); Python syntax/self-tests followed after harness edits
- #78 checked: none — no affected surface in this lane
- #79 checked: none — no affected surface in this lane
- #80 checked: none — no affected surface in this lane
- #81 checked: none — no affected surface in this lane
- #82 checked: none — no affected surface in this lane
- #83 checked: none — no affected surface in this lane
- #84 checked: none — no affected surface in this lane
- #85 checked: none — no affected surface in this lane
- #86 checked: none — no affected surface in this lane
- #87 checked: none — no affected surface in this lane
- #88 checked: none — no affected surface in this lane
- #89 checked: none — no affected surface in this lane
- #90 checked: none — no affected surface in this lane
- #91 checked: none — no affected surface in this lane
- #92 checked: none — no affected surface in this lane
- #93 checked: none — no affected surface in this lane
- #94 fixed — every nested wait is a named product budget; arithmetic is in each check
- #95 fixed — retained RPC links have a dedicated Ping/Pong reader and continuous request deadline
- #96 fixed — turn-wall timing is accepted only for one same-PID/generation warm settled daemon, exactly 25 valid samples per shape, exact physical provider counts 1/2, one monotonic local `process_exec` effect per tool case, and start/mid/end one-minute load strictly below 4; overload or an unsettled harness rejects timing without rewriting correctness. The CI artefact retains raw samples, median/MAD, wall/CPU/peak RSS, request ledgers, and binary/daemon/proxy/harness hashes; the separate exact-cardinality trace-on companion retains correlated stage timestamps.

## v0.0.970 codepagediet delta walk

- #1-#18 checked: none — measurement tooling and documentation only; no affected product surface
- #19 fixed — shell syntax, Python compilation, strict C compilation, focused region tests, and `git diff --check` cover the changed surface
- #20-#40 checked: none — no affected product surface or Rust test-count change
- #41 checked — M1 and turn-wall probes retain short hermetic temporary roots
- #42-#43 checked: none — no affected installed-warmup or product surface
- #44 checked — the unchanged warm and one-shot harnesses exercise the real UDS daemon path; `vmmap` denial does not weaken that probe
- #45-#63 checked: none — no affected product surface
- #64 checked — baseline `haiderd` is 52,341,120 bytes and the isolated PGO candidate is 45,373,872 bytes, both above 10 MiB
- #65-#70 checked: none — no affected product surface
- #71 checked — exact frozen client/daemon pairs are exercised end to end against the fake proxy and durable tool effect
- #72 checked — native discovery remains intentionally disabled in the hermetic measurement environment
- #73-#76 checked — throwaway HOME/profile/runtime ownership is retained; no fixed-byte product source window changed
- #77 checked — no Rust unsafe delta; the bounded Darwin libproc helper passes `clang -Wall -Wextra -Werror`
- #78-#93 checked: none — no affected product surface
- #94 checked — no product deadline changed; the diagnostic helper has a bounded five-second subprocess timeout and fails closed
- #95 checked: none — no new wait while a negotiated connection is open
- #96 fixed — warm/one-shot authority still requires start/mid/end load below 3; M1 now additionally records and gates both pre-run and post-run load below 3

## v0.0.970 daemonready delta walk

- #1-#18 checked: none — no affected packaging, dependency, or harness surface
- #19 fixed — every changed Rust file is rustfmt-clean and `git diff --check` is clean; the repository-wide formatter still names only three pre-existing unrelated files
- #20 fixed — four readiness regressions raise the reviewed Rust baseline from 4,437 to 4,441
- #21-#28 checked: none — no affected surface
- #29 fixed — status keeps the serving PID/path fields diagnostic and makes the positive readiness predicate authoritative; neither `lock.owner`, the daemon PID file, nor socket existence is treated as Ready
- #30-#32 checked: none — no affected surface
- #33 fixed — the additive `ready_since` and `providers_loaded` fields have old/new decoder coverage and exact JSON fixtures
- #34-#40 checked: none — no affected surface
- #41 fixed — in-process and subprocess tests retain short isolated store/runtime roots
- #42 fixed — subprocess coverage prebuilds the sibling `haider`/`haiderd` binaries before the daemon-heavy tests
- #43 checked: none — no affected surface
- #44 fixed — the slowed-init predicate and N-client race execute over the real UDS daemon path
- #45-#63 checked: none — no affected surface
- #64 checked — the final gate inspects the built `haiderd`; a binary at or below 10 MiB remains a stop condition
- #65-#70 checked: none — no affected surface
- #71 fixed — the exact `haider --ready`, spawn notification, `status --json`, and immediate-turn subprocess path is exercised end to end
- #72 fixed — the readiness delay environment seam is accepted only with the explicit fake-provider test seam; native discovery stays disabled
- #73 checked — no fixed-byte source window changed
- #74 fixed — every subprocess uses a throwaway HOME/profile and never reads host credentials
- #75-#76 checked: none — no affected surface
- #77 checked — no unsafe code was added; the standalone guard's existing four-test mismatch is confined to untouched `haider-tui` (`git diff --quiet -- crates/haider-tui` passes)
- #78-#93 checked: none — no affected surface
- #94 fixed — the test-only delay is capped at 10,000 ms and every process/poll wait retains an existing named deadline
- #95 checked — status remains a one-shot RPC and adds no wait while a negotiated connection is retained
- #96 checked — no TTL, warm-retention, or turn-performance policy changed

## v0.0.970 monitorcore delta walk

- #1-#18 checked: none — no affected dependency, packaging, or migration failure surface
- #19 fixed — affected Rust crates are rustfmt-clean, `git diff --check` is clean, and strict scoped clippy passes; repository-wide format drift remains confined to untouched files
- #20 fixed — twenty-three monitor/RPC regressions raise the reviewed Rust baseline from 4,464 to 4,487
- #21-#28 checked: none — no affected surface
- #29 fixed — monitor cancellation, shutdown, output-drain expiry, and ephemeral-daemon retirement retain background-work authority and sweep pipe-holding descendants
- #30 fixed — named source, mutation, delivery, Ask/Allow/Deny approval, restart, and RPC authorization tests assert concrete receipts and reports
- #31-#32 checked: none — no affected surface
- #33 fixed — monitor control/delivery frames are additive, nested future discriminants remain decodable, and mutation support has its own negotiation bit
- #34-#37 checked: none — no affected surface
- #38 fixed — the registry is session-bounded, the outbox is two-slot/coalescing, source queues are bounded, anchored file reads are capped at 64 KiB, and output is AHRB-bounded
- #39-#40 checked: none — no affected surface
- #41 fixed — all filesystem/process/integration regressions use short isolated temporary roots
- #42 fixed — current sibling `haider` and `haiderd` binaries are prebuilt before the daemon-heavy gate
- #43 checked: none — no affected surface
- #44 fixed — the real hub connection path proves rejection without Control/correct attachment; a black-box daemon connection proves successful attached pause/resume
- #45-#63 checked: none — no affected surface
- #64 checked — final `haiderd` is inspected and remains above the 10 MiB floor
- #65-#70 checked: none — no affected surface
- #71 checked — command-backed monitor execution uses the production command builder/process-group path; typed client control and lifecycle use the production daemon RPC path
- #72 checked — native discovery is disabled by the required hermetic test environment
- #73 fixed — every file poll uses no-follow component traversal and at most one 64 KiB tail read; command and report byte windows remain explicitly bounded
- #74 checked — tests use temporary profiles/workspaces and never read host credentials
- #75-#76 checked: none — no affected surface
- #77 checked — no unsafe code was added; strict affected-crate clippy passes
- #78-#93 checked: none — no affected surface
- #94 fixed — pipe drain grace is exactly two 500 ms process batches (1,000 ms), with the arithmetic adjacent to the constant; other monitor deadlines retain named budgets
- #95 checked — monitor runners wait outside negotiated connections, and control requests are one-shot; no retained RPC link gains an unserviced wait
- #96 checked — no warm-retention or turn-performance policy changed

## v0.0.970 turnid delta walk

- #1-#18 checked: none — no dependency, packaging, migration, or platform surface changed
- #19 fixed — all changed Rust files are rustfmt-clean, the workspace all-targets check passes, the Python QA self-tests pass, and `git diff --check` is clean
- #20 fixed — sixteen correlation regressions raise the reviewed Rust baseline from 4,748 to 4,764
- #21 checked: none — no ignored-test policy changed
- #22 fixed — exact session/run/turn/request trace strings pass a narrow visible-ASCII, delimiter, enum, and unsigned-decimal allow-list; arbitrary tracing fields remain excluded
- #23 checked — correlation contains opaque IDs and ordinals only; prompt, body, credential, path, and error text never enter the new journal or trace fields
- #24-#28 checked: none — no affected surface
- #29 fixed — request identity is committed before model, prewarm, cache-resource, or tool-support network I/O and every resumable recovery shape, including queued retries and manual compaction, seeds the next physical ordinal from the validated durable maximum
- #30 fixed — named adapter, body-golden, steering, delegation, Loom drafting, tool-support, restart, journal, trace, and prompt-sniff mutation tests assert concrete coordinates
- #31-#32 checked: none — no affected surface
- #33 fixed — `cache_request_attempt_v1.correlation` is optional/defaulted for legacy rows and `provider_request_attempt_v1` is an additive prompt-omitted extension
- #34-#37 checked: none — no affected surface
- #38 fixed — the trace registry keys by `(session_id, run_id)`, request ordinals share one atomic per turn, first-byte deduplication is an exact set rather than a 64-request bit window, and journal parsing rejects reuse
- #39-#40 checked: none — no affected surface
- #41 fixed — provider loopbacks, store tests, and warm-harness profiles retain short hermetic temporary roots
- #42 fixed — current `haider` and `haiderd` siblings are prebuilt before daemon subprocess tests and the performance harness
- #43 checked: none — no affected surface
- #44 fixed — the fake proxy exercises the real UDS daemon path, and every built-in adapter has a real loopback proxy-ledger pass recording locked headers independently of unchanged raw request bodies
- #45-#63 checked: none — no affected surface
- #64 checked — final measured release `haiderd` is 54,125,408 bytes, above the mandatory 10 MiB floor
- #65-#70 checked: none — no affected surface
- #71 fixed — built-in HTTP adapters, Gemini cache-resource operations, explicit prewarm, Loom drafting, and subscription web-search support use their production request builders; delegation and restart pins exercise production daemon ownership
- #72 checked — native discovery is intentionally disabled only in the required hermetic tests and measurement environment
- #73 checked — default strict-provider request bodies remain byte-identical; no fixed-byte source window was weakened
- #74 fixed — subprocess and benchmark coverage uses throwaway profiles and never reads host credentials
- #75-#76 checked: none — no affected surface
- #77 checked — no unsafe code was added; the standalone guard's existing four-test mismatch remains confined to untouched `haider-tui`
- #78-#93 checked: none — no affected surface
- #94 checked — no product deadline or retry wait changed; all existing harness bounds remain named and arithmetic-owned
- #95 checked — no retained negotiated connection gained a new wait
- #96 fixed — the warm harness now refuses prompt-derived attribution and validates exact header identity/kind/ordinal while retaining 5+25 ABBA, same-PID/generation, durable Idle, raw samples, median/MAD, and load-below-3 acceptance

## v0.0.970 chocofix delta walk

- #1-#18 checked: none — release packaging metadata/workflow only; no product protocol, migration, or dependency surface changed
- #19 fixed — Python compilation, six focused packaging regressions, workflow YAML parsing, full workspace tests/Clippy, and `git diff --check` cover the changed surface
- #20 checked — the Rust baseline was updated and verified unchanged at 4,748; the six new Python tests are separately CI-wired and no Rust test was removed or weakened
- #21-#40 checked: none — no affected product surface
- #41 fixed — every packaging regression uses a short `TemporaryDirectory` root and bounded synthetic archive
- #42-#63 checked: none — no affected runtime or product surface
- #64 checked — runtime binaries are untouched; the mandatory sibling prebuild succeeded
- #65-#70 checked: none — no affected product surface
- #71 checked — real npm packing plus synthetic Chocolatey nupkg inspection cover the exact publishable archive shapes; Windows CI performs the real `choco pack`
- #72-#73 checked — no native discovery or fixed-byte product read window changed
- #74 fixed — packaging tests use synthetic data and never read host credentials, HOME, or profiles
- #75-#76 checked: none — no affected product surface
- #77 checked — no unsafe code was added and workspace/tests Clippy passes with warnings denied; the standalone guard's existing four-test mismatch is confined to untouched `haider-tui` (`git diff --quiet -- crates/haider-tui` passes), so this lane does not rewrite that unrelated baseline
- #78-#93 checked: none — no affected product surface
- #94-#96 checked — no deadline, negotiated-connection wait, or turn-performance policy changed

## v0.0.970 actbias delta walk

- #1-#18 checked: none — no dependency, packaging, migration, or wire-version surface changed
- #19 fixed — changed Rust is rustfmt-clean, `git diff --check` is clean, focused prompt/schema/golden tests pass, and the workspace suite is green
- #20 fixed — two reviewed regression tests raise the Rust baseline from 4,748 to 4,750; `xtask test-count` confirms 4,750/4,750
- #21-#40 checked: none — no affected lifecycle, storage, or compatibility surface
- #41 checked — focused and golden tests use hermetic temporary roots and the fake provider
- #42 checked — sibling `haider`/`haiderd` binaries were prebuilt before daemon-driven goldens
- #43 checked: none — no affected installed-warmup surface
- #44 checked — provider-request and run goldens exercise the real CLI/daemon UDS path with a fake loopback provider
- #45-#63 checked: none — no affected surface
- #64 checked — prebuilt `haiderd` is 197,290,544 bytes, above 10 MiB
- #65-#70 checked: none — no affected surface
- #71 fixed — exact provider request, text/tool turn ledgers, and one-shot ledger were regenerated deliberately and then passed without update flags
- #72 checked — native discovery stayed disabled throughout the hermetic gate
- #73 fixed — prompt/tool byte changes are explicit: policy 359 -> 725, instruct pipe 12,122 -> 12,621, and combined stable prefix 12,481 -> 13,346
- #74 checked — tests use throwaway profiles and do not read host credentials
- #75-#76 checked: none — no affected surface
- #77 checked — no unsafe code was added; `cargo clippy --workspace -- -D warnings` passes
- #78-#93 checked: none — no affected surface
- #94-#96 checked — no deadline, negotiated-connection wait, or turn-performance policy changed

## v0.0.970 providerrebind delta walk

- #1-#18 checked — no new dependency, platform backend, global credential mutation or secret-bearing output; typed CLI/RPC validation and registry guards are covered by focused tests.
- #19 checked — Rust formatting and whitespace checks apply to the merged tree.
- #20 checked — test baseline is regenerated with `xtask test-count --update` after the added regressions.
- #21-#28 checked — additive serde fields preserve absent legacy bytes; status golden and event-schema changelog pins exercise the contract.
- #29-#30 checked — real daemon/proxy tests retain exact session/run identity, receipt replay and owned cleanup.
- #31-#40 checked — journal/event/receipt commit shape remains atomic; no dependency or collection ownership policy changes.
- #41 checked — hermetic profiles and short temporary IPC roots are retained.
- #42-#43 checked — benchmark siblings are frozen by build; no installed user daemon is reused.
- #44 checked — native UDS and real loopback HTTP tests exercise the serving path on macOS; Windows/Linux are by inspection.
- #45-#56 checked — no TUI layout/backend change; the exhaustive session event projection recognizes the new additive kind.
- #57 checked — CLI usage, client RPC/feature tables, automation contract and advertised feature pin are updated together.
- #58-#63 checked — no packaging, credential-store format, OAuth or asset changes.
- #64 checked — prebuilt debug `haiderd` is 199,140,464 bytes, exceeding 10 MiB; benchmark artifacts are reported in the lane evidence.
- #65-#70 checked — no changed external agent or device workflow.
- #71 checked — production account factory, registry validation, wire RPC and HTTP adapters are exercised together.
- #72 checked — discovery disabled and deterministic test-device identity used for builds/tests.
- #73-#93 checked — sandbox roots/permission ceilings retained; provider-view CAS and durable request markers were not moved or weakened.
- #94 checked — new integration wait bound is configured provider-open budget plus the shared RPC/journal observation budget; no production deadline was added.
- #95 checked — negotiated clients service Ping/Pong while waiting; request snapshots retain in-flight adapter ownership.
- #96 checked — warm ABBA uses the existing harness, exact provider ledger/cardinality gates, 5+25 samples and its load gate. Results and any environmental rejection are recorded in `docs/testing/v0.0.970/providerrebind.md`; no outlier deletion or relaxed correctness gates.

## v0.0.970 providerrebind clippy continuation delta walk

Scope: the uncommitted `session_provider.rs` initializer correction and two
`provider_rebind_tests.rs` lint fixes on committed merge HEAD `45f3d5c5`. Gate evidence is in
`docs/testing/v0.0.970/providerrebind-clippy-continuation/README.md`.

- #1-#8 checked: none — the added `ClientConfig` import names the existing public type; no API, field, dependency, ownership, or platform boundary changed.
- #9-#18 fixed: `crates/haider-cli/src/session_provider.rs:70` — initialize `EnsureOptions` and its nested `ClientConfig` with struct literals and `..Default::default()`, removing `field_reassign_with_default` without a lint suppression.
- #11/#15 fixed: `crates/haider-daemon/src/provider_rebind_tests.rs:500` and `:572` — replace `.last()` with `.next_back()` on the double-ended slice/filter iterator, and `.err().expect(...)` with `.expect_err(...)`. Terminal selection and recovery-refusal assertions retain their meaning. Both diagnostics came from the first exact clippy `--tests` run (exit 101); keep that log alongside the final gate.
- #19 checked — workspace rustfmt and whitespace checks are recorded with the gate evidence.
- #20 checked — recount `test-baseline.txt` with `xtask test-count --update`; source-marker counts are reported separately from executed libtest totals.
- #21 checked — all build/test/count commands use `RUST_MIN_STACK=8388608` and the full lane ENV LAW.
- #22-#53 checked: none — no global state, schema, receipt, platform behavior, timing bound, fixture, test registration, or UI layout changed.
- #54 checked — preserve the corrected 8 MiB stack law; the exact workspace gate uses `--no-fail-fast` and its real process exit.
- #55-#63 checked: none — no cfg seam, exit mapping, rendering, CAS threshold, public return type, or archive handling changed.
- #64 checked — freshly built `haiderd` is 199,587,392 bytes, above 10 MiB; both siblings are Mach-O arm64 executables. Disk checks precede build-capable commands and enforce the 700 MiB floor.
- #65-#66 checked: none — no platform error mapping or STT changes.
- #67 checked — build both `haider` and `haiderd` before enabling `HAIDER_TEST_SIBLINGS_PREBUILT=1` for the full workspace gate.
- #68-#76 checked: none — no cleanup, process discovery, CI dispatch, credential discovery, source pins, global profile state, shutdown, or wire projection changed.
- #77 checked: none — no unsafe block, dependency, generated fixture, workflow, or repository-guard policy changed; this continuation's exact landing-gate evidence does not claim a new release or cross-platform gate.
- #78-#80 checked: none — no release dispatch, benchmark bound, or CI job changed.
- #81 checked — fresh sibling build completed on the already merged tree before trusting prebuilt binaries.
- #82-#84 checked: none — OAuth, hook, and session-hub tests are unmodified; no failure is suppressed or relabeled by this correction.
- #85-#86 checked — run `cargo test -q --workspace --no-fail-fast` across every workspace crate, with the full result totals and exit recorded.
- #87 fixed — run `cargo clippy --workspace --tests -- -D warnings` verbatim; `cli_tests.rs:25` includes `../src/main.rs`, so the production initializer must also satisfy test-target linting.
- #88-#91 checked: none — the real merge and `SessionMetadataV1` completion are already committed; no merge recreation, prompt/tool edit, fixture hand-merge, or byte-pin change is needed. Recount the merged test baseline.
- #92 checked — build-capable commands record disk headroom; the gate retains failures and missing-executable diagnostics in its raw log.
- #93 checked: none — no class #93 is defined in the supplied registry.
- #94-#96 checked: none — no deadline, negotiated-connection wait, provider durability boundary, or performance policy changed. Windows/Linux behavior is by inspection; gates here run on macOS arm64.

## journalview continuation, merged through 38359fd3

- #1-#18 checked: no dependency, platform seam, lint exemption, or harness weakening introduced. New tests exercise durable ownership and actual item lifecycles.
- #19/#20 checked: formatting, whitespace, exact Clippy test targets, and authoritative `xtask test-count --update`; merged upstream 4910 -> lane 4925.
- #21/#41/#42/#44/#54/#64/#67/#71/#72/#74/#81/#92 checked: full ENV LAW, two build jobs, disk checks before each build with the 700 MiB floor, fresh siblings, `HAIDER_TEST_SIBLINGS_PREBUILT=1`, and haiderd 200566368 bytes. Four test threads avoid the observed shared-host scheduling failure without changing any test deadline or assertion.
- #22/#23 checked: request/Finish metadata is content-free; narrative remains the already captured provider output and is not added to diagnostic logs.
- #24-#28 checked: no credential flow, process-global configuration, release workflow, or public error policy changed.
- #29/#30 fixed: correlation is stamped before durable append and publication; recovery uses the exact source item, not a later Side request. Named journal/live/JSON/replay regressions retain raw byte equality. Rebind-error cleanup closes recovered items before the terminal under the original request.
- #31/#32 checked: no new provider transport or dispatch path; failed admission cannot claim an unsent request.
- #33 fixed: additive schema ledger, legacy compatibility, typed atomic compaction announcement and matching-overlay validation. Replay normalizes zero fields inside raw envelopes.
- #34-#40 checked: shared arena assembles normalized rounds without duplicate completion snapshots; JSONL retains no duplicate summary. Actual Finish reasons are preserved; incomplete summary text remains incomplete.
- #43/#45-#53 checked: no new timing threshold or platform gate. Existing golden sequences are regenerated, not hand-merged; primary and empty-summary terminal markers retain the frozen Started/Completed lifecycle.
- #55-#63/#65/#66 checked: no archive, STT, rendering, or platform error-mapping change. Incoming casstream/providerrebind changes remain preserved.
- #68-#70/#75/#76 checked: no new external wait or discovery/cleanup policy. Private summarizer output remains prompt-omitted and excluded from final response selection.
- #73 checked: all 70 changed/new JSONL golden lines reviewed; `provider_request_no_budget.json` is byte-identical to merged upstream and the measured instruct-pipe pin stays 13552 -> 13552 bytes.
- #77-#80 checked: no unsafe added and no release, dependency, or benchmark claim. Existing unsafe and QA self-checks retained.
- #82-#84 checked: both protected OAuth files remain unchanged; the initial timing failure and unchanged isolation/daemon reruns are retained as evidence.
- #85-#91 checked: full workspace test with `--no-fail-fast` and Clippy `--workspace --tests -- -D warnings`; both content merges preserve the incoming sides, and the final ref guard rejects a stale-tree verdict. Git merge recording remains for the orchestrator because the Git directory is read-only.
- #93: no class #93 is defined in the supplied registry.
- #94/#95 checked: no new product deadline or negotiated-connection wait; the regression adds no timeout.
- #96 checked: no latency or benchmark-score claim. macOS execution only; Linux/Windows behavior is by inspection. AHRB scoped credit remains undeclared until its owner maps the checker units; the supplied TOML declares announced-only support.

## ceilingdecl continuation 3, prestarted merge through 368f093c

- #1-#18/#22-#28 checked: no dependency, platform seam, credential flow, lint exemption, or test weakening. The actor resolution retains both module declarations; production control flow is otherwise unchanged.
- #19/#20 checked: formatting passes after formatting the new content-free handshake diagnostic; `cargo run -q -p xtask -- test-count --update` resolves the 4929/4925 baseline conflict to 4944. Saved source backups live outside the workspace so the counter sees no duplicate Rust tests.
- #21/#41/#42/#44/#54/#64/#67/#71/#72/#74/#81/#92 checked: full ENV LAW, two build jobs, four test threads, disk checks before every build-capable command with the 700 MiB floor, fresh `haider`/`haiderd` siblings, and `HAIDER_TEST_SIBLINGS_PREBUILT=1`. The built `haiderd` is 200942448 bytes, above 10 MiB.
- #29-#40 checked: narrative capture/recovery, actual provider Finish metadata, normalized provider rounds, scoped compaction announcements, durable request ownership, cap receipts, and typed end reasons from both merge sides remain present. `hard_request_bound_preserves_typed_terminal_before_provider_rebind_refresh` executes and passes with two provider opens, two refreshes, and one typed cap terminal with workspace receipts/progress.
- #43/#45-#63/#65/#66/#68-#70/#75-#80/#82-#84 checked: no timeout, platform gate, transport, publication boundary, archive, STT, rendering, cleanup policy, unsafe, release, or protected OAuth source changes. No failure or ignore is suppressed.
- #73/#89-#91 checked: all three conflicted JSONL fixtures are regenerated through their existing test update modes, never hand-merged. Every line is compared to both saved merge sides: 76 changed/new lines versus ceilingdecl HEAD (17 one-shot, 17 text, 42 tool), only narrative correlation/Finish fields, four atomic terminal pairs, and derived sequence/item IDs; against incoming journalview, only three workspace receipt pairs and derived sequence/item/workspace-revision references. Provider-request fixture is also regenerated and remains byte-identical. Measured instruct-pipe pin stays 13552 -> 13552 bytes (full prefix 19736, registered 29, advertised 26, native descriptions 690); handshake stays 115 -> 115 features. The initial exact handshake selector matched zero tests; the corrected selector executes one passing test and prints 115.
- #85-#88 checked: the orchestrator already started the real merge; only working-tree files are edited, with no git metadata command or commit. Exact full-gate commands and their final exits/totals are recorded in `docs/testing/v0.0.970/ceilingdecl-continuation3/`; the orchestrator owns merge recording.
- #93: no class #93 is defined in the supplied registry.
- #94/#95 checked: no product deadline or negotiated-connection wait is added or changed.
- #96 checked: no performance claim. Execution is macOS only; Linux/Windows behavior is by inspection. Historical lens citations are treated as drifted: for example, the old actor budget-projection location near 3549 is now near 3988; cap-before-refresh and narrative ownership are audited by construct in the merged source.
- Final gates: `cargo test -q --workspace --no-fail-fast` exits 0 (5350 top-level passes, 0 failures, 13 pre-existing ignores; 12 additional nested subprocess probes pass), and `cargo clippy --workspace --tests -- -D warnings` exits 0. Final `xtask test-count` confirms 4944/4944. Independent verifier: findings=1 real=1 noise=0, the corrected empty handshake selector; final code/golden/gate verdict SHIP.

## v0.0.970 economydiet, merged through 7431f8e6

- #1-#8 checked — additive discovery configuration and typed receipt; existing authorization catalogs remain the ceiling. No dependency or platform interface changes.
- #9-#18 checked — no new unsafe or lint suppression in production. Provider projection recognizes only tool-owned envelopes and preserves opaque output.
- #19/#20 checked — rustfmt, exact Clippy test targets and the authoritative test-count tool are recorded in `docs/testing/v0.0.970/economydiet-evidence/merged-gate-steps.json`; source markers are 4,944 upstream -> 4,969 lane.
- #21/#41/#42/#54/#64/#67/#71/#72/#81/#92 checked — full ENV LAW, two build jobs, disk checks with the 700 MiB floor, fresh CLI/daemon siblings and pinned binary hashes. Every frozen daemon exceeds 10 MiB. Baseline and candidate use the same development profile; the final comparison rebuilds both against merged upstream.
- #22-#28 checked — model-facing receipt removal does not remove journal diagnostics. No OAuth, credential, release or profile-discovery policy changes; original fixtures and protected OAuth files remain intact.
- #29-#40 checked — discovery promotion follows durable tool settlement; replay requires a correlated successful actor result. Workspace changes revoke consent while retaining presentation choices; forks reset session scope. Process/mutation receipts, `/effects[n]`, source omission facts and typed truncation provenance stay durable. Graph selectors resolve this run's journal facts; the store retains authority/freshness/exit validation.
- #43/#44 checked — runtime validation is macOS arm64; Windows/Linux are by inspection. No new platform gate, ignored test or timing threshold.
- #45-#56 checked — no TUI production change. The optional T0 monitor oracle now recognizes the exact rendered controls at both sizes; negative pins still reject footer-only and unchanged screens. Its captured surface is retained, including the verifier's spacing correction. The shared probe ANSI parser now accepts BEL and ST OSC terminators, with regression pins: three retained failed-attach captures preserve the composer, and a fresh isolated attach succeeds with unchanged assertions/deadline and clean daemon shutdown.
- #57 checked — `list_tools` exposes authorized names, describes/promotes filtered tools, and preserves native constraints. The model catalog is separate from the full user/RPC inventory; strict inventory pins include the new primitive.
- #58-#63/#65/#66 checked — no packaging, asset, STT, archive or platform error-mapping change. Native action descriptions, actbias contract and worked example remain exact pins.
- #68-#70/#74-#76 checked — timing and economy use frozen task-owned binaries and isolated profiles. New tool-result state reduction is bounded to typed discovery facts; no worker retirement or process cleanup ownership change.
- #73 checked — JSONL goldens regenerate through `UPDATE_FIXTURES`/`HAIDER_ONESHOT_GOLDEN_UPDATE`, preserving all 116 merged records and workspace receipts. Relative to merged upstream, durable golden changes are only system-version v4 -> v5. The provider golden changes to the measured eight-tool pack and 606-byte policy; native schemas retain exact byte/digest fixtures.
- #77-#80 checked — no benchmark thresholds, dependencies, release dispatch or security guard weakened. AHRB remains read-only; its official completion limitation is reported separately from the independent effect join.
- #82-#84 checked — scripted fixtures explicitly select the noncore tools they exercise; permission, cancellation, budget and recovery assertions/deadlines are preserved. Initial failures and interrupted stale-tree runs remain evidence, not green gates.
- #85-#91 checked — merge-forward preserves upstream cap-before-provider-refresh and its request-budget tests. Full workspace test, Clippy `--workspace --tests -- -D warnings`, fixture verification and test-count use the merged tree. The real worktree Git metadata is read-only; local commits and merge are transported in a bundle from a writable Git directory, without pushing.
- #93: no class #93 is defined in the supplied registry.
- #94/#95 checked — no production deadline or negotiated-connection wait added. Evidence selection reads bounded journal pages; existing provider and client keepalive behavior is unchanged.
- #96 checked — all eight ABBA suites pass existing warm/one-shot proof pins, sample counts, load rejection and median/MAD criterion. Warm single/tool improve; one-shot is neutral within MAD. Final loads are 1.77–2.80 (<3); all 108 known owned daemon PIDs are gone. Rejected load and long-temp-path attempts are retained and excluded, with no accepted-regression retry. Final metrics, timing status and any limitations are in `docs/testing/v0.0.970/economydiet.md`.
- Final gates: merged workspace 5,375 top-level passes plus 12 nested probes, zero failures and 13 unchanged ignores; Clippy test targets, fmt, xtask checks and 4,969/4,969 source test-count pass. Python QA 66 and PTY probe 4 pass. Final unchanged T0 retry is 14/14 and validates, with all 16 owned-daemon cleanup rows passing. The prior 13/14 `/update` clean-exit failure and both passing isolated A/B activations remain documented; no assertion, deadline or production update code was changed to obtain the retry result.
