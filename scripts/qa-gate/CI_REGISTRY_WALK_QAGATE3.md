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

## v0.0.970 customprov delta walk, merged through 7431f8e6

- #1-#8 checked — provider declares the already-locked workspace `unicode-width` dependency; no version, platform backend, packaging or global credential-store format changes.
- #9-#18 checked — shared ID-display validation is used at both discovery boundaries; no production lint suppressions, test ignores or assertion weakening are added. The new integration module follows the repository convention permitting descriptive `expect` calls in tests.
- #19/#20 checked — final merged source passes rustfmt and whitespace checks; `xtask test-count --update` and verification report 4944 → 4966 (+22 source test markers).
- #21/#41/#42/#44/#54/#64/#67/#71/#72/#74/#81/#92 checked — full ENV LAW, two build jobs, four test threads, disk check before each build-capable command and 700 MiB stop floor; fresh siblings precede `HAIDER_TEST_SIBLINGS_PREBUILT=1`. `haiderd` is 201235968 bytes. Fake loopback HTTP and UDS tests use hermetic profiles; no installed daemon or host credentials are reused.
- #22-#28 fixed/checked — API keys are staged through the existing same-user local vault path and masked in the card; probe commands/replies carry opaque references only. Secret reflection, control characters and zero-width model IDs cannot enter the picker/cache; errors use existing redaction. OAuth sources are untouched.
- #29/#30 checked — discovery is a correlated read-only accounts-worker job. Cancel, disconnect and a new attempt retire stale replies; probe changes no profile, account, cache or durable receipt.
- #31/#32 checked — the existing compatible catalog transport retains fixed-origin resolution/pinning, proxy-off, redirect refusal and bounded bodies. Saved-key probes enforce claimed-origin repoint guards before network access; explicitly confirmed manual inventory avoids redundant availability GETs.
- #33-#40 checked — additive `provider.models_probe` has a feature gate, exhaustive wire pair and Control/local credential checks. Final configure keeps existing CAS/receipt replay and persists the full confirmed inventory for `list_models`; no-op configure retains established cache behavior.
- #43/#45-#53 checked — create/edit field order, discovery/default selection, typed fallback and one-Enter confirmation are pinned. New picker/fallback goldens cover 80/118/160 columns. All twelve existing accounts/calendar goldens are byte-identical; per-file reasons are in `customprov.md`. Long fallback reasons retain a visible manual-entry instruction at 80 columns.
- #55-#63/#65/#66/#68-#70/#75/#76 checked — contract table and advertised feature count move together (132 → 133 methods; 115 → 116 features). No archive, STT, source discovery, platform error mapping, cleanup or publication boundary changes.
- #73/#89-#91 checked — real upstream merge completed in an isolated writable checkout because original Git metadata is read-only; both journalview and ceilingdecl are preserved. Incoming provider-request and JSONL goldens are regenerated through existing tooling, never hand-merged. Instruct-pipe pin stays 13552 → 13552. Tested workspace source and isolated lane source are compared byte-for-byte before commit delivery.
- #77-#80/#82-#84 checked — no unsafe, deadline exemption, release or performance claim; protected `oauth.rs` and `oauth_tests.rs` remain unchanged. Initial gate expectation failures are retained in evidence and repaired directly.
- #85-#88 checked — final gate runs `cargo test -q --workspace --no-fail-fast` and `cargo clippy --workspace --tests -- -D warnings` on merged source; exact exits and totals are recorded in `docs/testing/v0.0.970/customprov.md`.
- #93: no class #93 is defined in the supplied registry.
- #94/#95 checked — no new product deadline or blocking connection wait; the existing worker/driver reply loop continues servicing negotiated connection traffic.
- #96 checked — no measured latency claim. Tests execute on macOS arm64; Windows/Linux behavior is by inspection. All supplied turnperf and turnperf2 evidence was read and used to bound scope.

- Final customprov gates: workspace exit 0 (5372 top-level passes +12 nested probes, 0 failures, 13 existing ignores); strict Clippy with tests exit 0; test baseline 4966/4966. Gated-source hashes remain unchanged after both commands.
- Independent customprov verifier: findings=6 real=5 noise=1; all five real findings fixed, stale completion-wording report rejected as noise; code verdict SHIP. Original Git ref is read-only; the writable isolated lane checkout and bundle carry the commit.

## v0.0.970 agentcli / xplatfix merged delta walk

Both lane walks are retained verbatim below, ordered by first registry number.

- #1-#18 checked — no dependency or platform backend change. Tests remain separate; strict lint findings are repaired rather than suppressed.
- #1–4/#6–8/#34–40/#45/#48/#50/#55/#58 checked: public Windows liveness API uses the existing typed platform implementation; no unsafe additions, protocol changes, or private-field exposure. Optional desktop clipboard dependency preserves the locked desktop graph and excludes arboard on Android. Source-sensitive worker pin receives LF through checkout attributes.
- #5/#10/#11/#18 fixed: Unix-only ACP helper/import cfgs align with their existing test; Windows-only test receives the established expect allowance; Linux clipboard Option fallback uses `?`. No test is ignored or disabled to make the default CI matrix pass.
- #9/#12–17/#19 checked: unchanged production lint policy; exact test targets are included in the gate. Source formatting and whitespace are checked after the real merge.
- #19/#20 checked — rustfmt, whitespace, and authoritative `xtask test-count --update` are recorded with the final gate in `docs/testing/v0.0.970/agentcli.md`.
- #20 checked: authoritative `xtask test-count --update` recounts after upstream customprov and the six added source tests; the final count and gate results are recorded in `docs/testing/v0.0.970/xplatfix.md`.
- #21/#41/#42/#44/#54/#64/#67/#71/#72/#74/#81/#92 checked — full ENV LAW, two build jobs, scoped per-crate tests, four test threads, disk checks and the 700 MiB floor; fresh sibling binaries precede final daemon/CLI tests. Real-daemon tests use isolated short profiles and retain exact process cleanup ownership. Only completed executables in this task's temporary target are reclaimed to maintain disk headroom.
- #21/#41/#42/#44/#54/#64/#67/#71/#72/#74/#81/#92 checked: full ENV LAW, two Cargo jobs, four local test threads, disk floor checked before builds, and fresh CLI/daemon siblings before SIBLINGS_PREBUILT. Cross C compiler/sysroot failures are environment-blocked checks, not passing platform evidence.
- #22-#28 checked — no credential transport, log expansion, account mutation, or process-global discovery policy change. Public prompts use the existing accepted-turn journal; machine output is a single versioned JSON document.
- #22–25/#27/#29/#30/#46/#47/#49/#51–53/#56/#57/#59–63/#65/#66/#68/#69/#73/#75/#76/#78–80/#84/#93 checked: none — no account/discovery, autospawn authorization, durable publication, process teardown policy, source-window pin, render, or release trigger change in this lane.
- #26 fixed: Windows alias profiles are asserted as exact typed paths instead of searching Display paths in Debug-escaped text. #28 fixed: Windows monitor fixture now registers/resumes the suspended child exactly like the product and releases its job after wait; command-success and cwd-rename assertions remain.
- #29/#30 fixed — launch returns actual durable parent/child coordinates; wait requires the original ChildResult plus child terminal. Follow-up message runs identify their report as child-journal evidence. Observation failure preserves known coordinates. Recovery fences broker completion, completes partial establishment without duplicate children, and cancels abandoned children even before a child turn exists. The real palette /sessions typo exposed delayed self-echo ownership loss: the existing watch acknowledgement now establishes daemon-minted caller identity before local publication, and its session/epoch context fences stale acknowledgements.
- #31/#32 checked — no new provider transport. Direct coordinator delegation invokes zero parent provider requests and retains canonical broker/deferred-child ordering. The adjacent explicit update-check correction opts into release-discovery cancellation on TUI close; its blocking worker retains ownership until the real curl child is killed/reaped and the watcher joins. Background and install paths remain unchanged.
- #31/#37/#77 fixed: Android backend capability has an explicit typed unavailable path, tested on a desktop host with the feature disabled; native Windows clipboard CI still runs with the default feature. Existing per-job unsafe guard is retained and checked.
- #33-#40 fixed/checked — optional headless pin omits cleanly for legacy requests, is negotiated through agent_cli_v1, and has round-trip/feature tests. Public headless interaction propagates independently of workflow admission authority. Parent finalization guards and human workflow gates remain enforced. Optional surface-watch caller_owner preserves older response bytes/reader compatibility; modern input ownership never guesses from matching foreign text/revisions, and legacy owner-less behavior is explicit.
- #43/#45-#53 checked — every public verb has a real-daemon black-box test; existing tests and goldens are not weakened or ignored. The new documentation example is typed and pinned to a normalized real-daemon fixture. Literal --help/--json prompts after -- are exercised through real child results. The monitor palette oracle requires real card anatomy at both sizes and rejects incomplete, footer-only, and stale cards. Attach readiness requires the exact composer in a fresh full frame; stale placeholder and replay/status-only evidence are rejected. OSC stripping now recognizes BEL/ST without erasing following paint. The mistaken raw-byte-absence diagnosis is explicitly corrected with byte counts/hashes. Earlier failures remain retained. Typing cadence and exact typed-frame guard are unchanged; input-mirror regressions preserve text/cursor/attachments and foreign-owner updates.
- #55-#63/#65/#66 checked — additive CLI exit/JSON contracts and help; no archive, STT, render, or platform error mapping change. Native workflow catalog/status, fleet, message and cancellation semantics remain authoritative.
- #68-#70/#75/#76 fixed/checked — the QA runner recognizes agent/workflow daemon spawn capability, respecting --no-spawn. Cleanup still requires status-owned PID identity, clean stop, and PID disappearance; no process-name census substitutes for proof.
- #73/#89-#91 checked — upstream fetch/merge was verified through a writable temporary clone because original Git metadata is read-only; both refs equal `9270f402`, with no merge content. Prompt/tool fixtures are regenerated with existing tooling, not hand-merged; exact byte/count results are in the lane report.
- #77-#80/#82-#84 checked — no unsafe, release, benchmark, test-suppression, or protected OAuth source changes. Platform claims are limited to macOS execution and Windows/Linux inspection.
- #82/#83/#90 checked: original macOS no-Idle fixture passed six direct runs under high load; controlled single-response backpressure reproduced the exact CI teardown panic. Corrected fixture reads both connections to EOF under the same two-second bound and additionally requires Graceful and one notice each. No sleeps or timeout inflation; two confounded probes were rejected.
- #85-#88 checked — full test coverage is executed with scoped -p commands, --no-fail-fast, and actual command exits. Strict Clippy includes test targets. Failed first attempts remain in the evidence ledger.
- #85–89/#91 checked: original Git metadata is read-only, so a real merge is performed in a writable temporary clone and all merged files are copied back. All 1,855 incoming paths exist; both runtime changes survive. Final full workspace tests and clippy run on these merged files, not only the affected crates. No golden is hand-merged and no prompt-byte pin is blindly bumped.
- #93 checked — no class #93 is defined in the supplied registry.
- #94 fixed — T0 BudgetSum is 288s: two command bounds each 30 startup + 60 request + 10 observation + 2 terminal, plus 60 cleanup status + 20 stop + 2 process grace + 2 PID disappearance. No bound hides a literal-only timeout; wait expiry never cancels accepted work. The explicit update checker uses the existing 2.5s TUI exit budget and a one-tenth observation interval; stalled-loopback tests require owned process reaping and watcher join within that same budget. Composer repaint phases are capped to the remaining original 25s TUI_BOOT deadline; neither phase starts a new allowance.
- #94/#95 checked: no new deadline. The complete isolated fixture's existing eight-second cap is below the ten-second keepalive interval; final reads use actual EOF, not a failed late Ping interpreted as EOF. Both the original two-second shutdown bound and all delegation assertions remain intact.
- #95 checked — long-lived RpcClient services Ping/Pong while observers page the journal and sleep; the attachment event receiver is continuously drained.
- #96 checked — this is correctness evidence; no turn-latency or performance estimate is claimed as a measured improvement.

### agentcli gate evidence

- Final Rust/Python gates: 17 scoped crate suites total 5430 summed libtest passes, 0 failures, 13 pre-existing ignores, with all six crates touched by the final owner correction rerun in full. Strict Clippy includes tests across all nine affected crates. Test baseline is 4966 → 4995; instruct-pipe remains 13552 → 13552; feature count is 116 → 117. Python QA self-checks pass 76/76 and existing shared probe tests 2/2. Final full T0 and source/binary evidence are recorded in the lane report; earlier failed attempts remain retained.
- Final independent release audit: SHIP, findings=14 real=14 noise=0. Full T0 passes 15/15 with no failure, skip, or environment block; measurement eligibility is accepted. All 15 daemon cleanup proofs pass, and 105 unique TUI processes have natural exit 0, balanced alternate screens, and no panic. All 49 source hashes and both frozen binary hashes match the final tree and report. Changes remain uncommitted.

### xplatfix gate evidence — merge-forward through 9270f402

- Independent verifier value: findings=5 real=4 noise=1 — optional Windows clipboard integration target now declares its backend requirement; teardown consumes actual EOF; diagnostic backpressure probe is isolated after all behavior assertions; Windows liveness error mapping has deterministic coverage. The noise was a stale-log false alarm rejected by the later passing post-edit Windows-target Clippy artifact. Executed results and remaining CI acceptance limitations are in the lane report.
- Final merged macOS gate: workspace tests exit 0 (5,374 top-level passes plus 12 nested subprocess probes, 0 failures, 13 pre-existing ignores); strict workspace Clippy with tests exits 0; baseline 4,972/4,972; repository guards, formatting, and unsafe-count all exit 0. Production/test unsafe totals remain 189/20.


## v0.0.970 xplatfix — Round 2 Windows test job

- #7/#19/#20 checked: Windows hook fixture uses an explicit base64 dev dependency; format/diff checks and the source test-count update cover the changed tests.
- #29/#83/#90 checked: no sleep-as-fix. Sidecar failure injection joins the writer while the obstruction exists, asserts dirty on the same writer, then verifies complete generation-10 repair. Guard failures expose journal sequence/stage and typed tool/run reasons; failed observation queues a best-effort drain wake without claiming an awaited shutdown.
- #82/#83/#90 checked: the Windows log has no process timing, so cold PowerShell latency is inferred, not measured. Its ten-second outer observation is provably shorter than the nested sixty-second foreground process budget. No product deadline or assertion is weakened.
- #94 checked: process-aware observer BudgetSum is 10s graph/journal + 30s existing PowerShell startup fixture policy + 60s process wall + 2s termination + 2s pipe drain + 2 × 500ms receipts = 105s for one process; zero-process graph fixtures retain 10s. Named ProcessBounds values supply the product components.
- #95 checked: the changed guard observes a direct hub/store fixture, not a negotiated connection. CLI hooks retain the client's existing connection service; no new external-state wait is added.
- Windows path checks: retained ancestor handles deny rename through spawn; DOS/UNC process spelling must retain file identity. Monitor verification compares identity and relative sentinel contents, including short/long name equivalence. Native pipe error classification distinguishes an obstructed parent from missing state even for Windows NotFound.
- Golden checks: only the validated native process-command manual is normalized; all other provider request bytes and same-platform budget parity remain exact. Instruct-pipe expectation derives this one serialized field's platform contribution.
- Executed-vs-inspected results, claim audit, mutation evidence, merge and final gate are recorded in docs/testing/v0.0.970/xplatfix.md. Windows runtime acceptance remains contingent on green xplat-check after landing on wave-970.
## v0.0.970 docsync delta walk

- #1–18 checked — only capability prose/Rustdoc and regression assertions change; no production type, signature, ownership, async-lock, dependency, lint-suppression, or serde surface changes. Tests use existing public request types and private manual helpers in their existing modules.
- #19 checked — rustfmt and whitespace checks cover the changed Rust/docs; final gate outcomes are recorded in `docs/testing/v0.0.970/docsync.md`.
- #20 fixed — `xtask test-count --update` recounts 4,966 → 4,968 for the manual-capability and account-contract regressions. Source markers are distinct from executed libtest totals.
- #21 checked — every build/test uses `RUST_MIN_STACK=8388608`; no recursion or test-stack behavior changed.
- #22–49 checked — no process-global state, store schema, provider authority, platform, transport, shutdown, lock, unsafe, filesystem walker, or runtime deadline implementation changed. The provider-request golden was regenerated with the fresh sibling daemon through the real loopback harness. Non-host behavior is by inspection.
- #50 fixed — exact instruct-pipe 13,552 → 13,856 (+304), full-prefix macOS 19,736 → 19,962 (+226); existing Linux/Windows/other offsets and the 30% guard are retained. Seven native descriptions remain 690 bytes. Fixture JSON 17,006 → 17,310 differs only in the two system-manual lines.
- #51–53 checked — profile locks, UI layout, and runtime-root security are untouched. Runtime-resolution prose now describes the existing typed CLI provenance.
- #54 checked — full gates use the required stack environment; no tests are ignored, gated, or weakened to pass.
- #55–63 checked — no Windows binding, exit-code, rendering, CAS, release, or privilege behavior change; the manual states the existing effect broker policy.
- #64 checked — prebuilt `haiderd` is 201,235,968 bytes, above 10 MiB; siblings were refreshed after source edits and `HAIDER_TEST_SIBLINGS_PREBUILT=1` is used.
- #65–71 checked — no registry/account selection/recovery semantics changed; the client contract is tied to actual typed account request serialization and the provider fixture uses the built daemon.
- #72 checked — `HAIDER_DISCOVERY_DISABLED=1 HAIDER_TEST_DEVICE_NAME=test-mac` is set throughout hermetic gates.
- #73–93 checked — no live workload, release deployment, unsafe boundary, process ownership, or performance policy is modified. The 175-match source audit distinguishes historical evidence from current capability facts.
- #94–96 checked — no new wait/deadline, negotiated-link loop, durability boundary, or performance claim; original timing/load/keepalive rules remain unchanged.
- Integration constraint — `git fetch origin wave-970` and `git merge --no-commit origin/wave-970` were attempted before the gate; the sandbox denied external gitdir writes. Read-only live upstream matched HEAD `9270f402`. No recorded merge or commit is claimed; delivery verdict and exact gate results are in the lane report.
- Final validation — corrected full `cargo test -q --workspace --no-fail-fast` exits 0; exact `cargo clippy --workspace --tests -- -D warnings` exits 0; formatting/whitespace are clean; final source count is 4,968/4,968. The initial workspace run had one stale exact spawn-line pin, now updated with rationale and retained as an exact assertion. Verifier: one substantive finding, the corrected monitor-update ID requirement.
