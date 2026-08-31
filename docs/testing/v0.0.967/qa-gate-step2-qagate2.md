# QA Gate Step 2 — qagate2 verification record

## Scope and runner changes

This lane adds only the seven requested untimed T0 headless checks and shared
stdlib helpers. It does not modify Rust product code, Cargo manifests, TUI/PTY
checks, or lifecycle/upgrade/kill-9 checks.

The runner changes are additive:

- optional semver-like `expected_fail_until` metadata is loader-validated,
  emitted on normal and ENV_BLOCKED rows, and report-validated without changing
  status;
- spawn tracking includes account/session/sessions/resume commands;
- client-only SIGINT is armed from drained JSONL stdout with named bounds;
- `run_isolated_haider` provides the narrow sequential budget-control
  exception: fresh short profile, copied one-segment script, mandatory cleanup,
  no nesting/overlap;
- stdlib helpers parse headless evidence, count completed provider-attempt
  ordinals, and serve a 125-line loopback OpenAI fixture.

## Verification

- `scripts/qa-gate/run.sh test`: 24/24 PASS.
- `git diff --check`: PASS.
- `python3 -m compileall -q scripts/qa-gate`: PASS.
- Installed pair: `/usr/local/bin/haider` 33 MiB and `haiderd` 49 MiB,
  arm64 Mach-O, both version 0.0.967.
- Installed T0 report: `qa-gate-t0-Syeds-MacBook-Air.local-20260831T163455Z.json`.
- `run.sh validate`: VALID, schema `haider.qa-gate.v1`, 9 checks.
- Summary: 4 PASS, 5 truthful product FAIL, 0 runner/cleanup failures.
- Deliberate mutation: changing the input check to expect exit 1 produced
  `installed_pin.exit expected=1 actual=0`; it was restored and the 24 tests
  plus installed tier were rerun.

## Citation audit

- **Correct:** brief `crates/haider-cli/src/run.rs:26-34` still exactly defines
  exits 2, 65, 69, 70, 74, 76, 77, 124, and 130.
- **Drifted/split:** the brief points generally to
  `crates/haider-cli/src/automation.rs`; parsing and machine documents remain
  there at current lines 137-317, but the readiness predicate is implemented in
  `crates/haider-client/src/observe.rs:1184-1205`.
- **Correct:** replay status/action is currently `run.rs:600-677`; replay
  projection/integrity and the zero-provider-request field are
  `run.rs:995-1119`.
- **Correct:** budget flags are currently `run.rs:168-190`; timeout parsing is
  `run.rs:216-223` and `run.rs:460-485`.
- **Correct:** current budget usage is folded from durable usage before the
  monitor declares exhaustion at `worker.rs:8607-8848`; the installed evidence
  confirms the 0.0.967 exchange-first defect.
- **Correct:** account selection is sent and persistence-checked by the client
  at `headless.rs:2634-2693`; current daemon selection rejects aliases absent
  from its descriptor view at `session_hub/rpc.rs:14080-14113`.
- **Wrong in the supplied QA research note:** this installed 0.0.967
  non-permission `request_input` path does not cancel. The existing product test
  at `crates/haider-cli/tests/cli_tests.rs:2450-2497` and installed gate both
  show typed `no_human_available`, provider continuation, and terminal Done.

## CI error registry walk

Each class supplied with the lane brief was read against this Python-only
change. `checked: none` means the class is not introduced or touched; `fixed`
names the gate file that directly addresses it.

- #1 checked: none — no Rust struct/enum changes.
- #2 checked: none — no Rust API rename/signature change.
- #3 checked: none — no Rust ownership changes.
- #4 checked: none — no Rust test visibility changes.
- #5 checked: none — no cfg-narrow imports.
- #6 checked: none — no import/variant tables changed.
- #7 checked: none — no Cargo files or lockfile changes.
- #8 checked: none — no mechanical Rust sweep.
- #9 checked: none — no Rust clippy surface.
- #10 checked: none — no Rust helpers added.
- #11 checked: none — no Rust Option/control-flow changes.
- #12 checked: none — no Rust argument-list changes.
- #13 checked: none — no Rust type aliases required.
- #14 checked: none — no Rust derives.
- #15 checked: none — no iterator rewrites.
- #16 checked: none — no Rust ranges/matches.
- #17 checked: none — no Rust lock/await changes.
- #18 checked: none — no Rust attributes.
- #19 checked: none — no `.rs` files changed; Python diff check passes.
- #20 checked: none — no Rust test-count baseline change.
- #21 checked: none — no Rust test runner change.
- #22 checked: none — no tracing subscriber change.
- #23 checked: none — no migrations.
- #24 checked: none — custom-provider authority unchanged.
- #25 checked: none — all new rows are untimed.
- #26 checked: none — no platform filesystem code.
- #27 checked: none — no wire/keepalive product changes.
- #28 checked: none — no Windows process-tree tests.
- #29 checked: none — no autospawn authorization change.
- #30 checked: none — waits fail with actual terminal/timeout state.
- #31 checked: none — no Android code.
- #32 checked: none — no release operation.
- #33 fixed: `scripts/qa-gate/gate/context.py:416` and self-tests at
  `scripts/qa-gate/tests/test_runner.py:419` scope spawn-tracking changes.
- #34 checked: none — no dependency module.
- #35 checked: none — no Rust trait calls.
- #36 checked: none — no Rust temporary borrows.
- #37 checked: none — no Windows cfg-boundary type.
- #38 checked: none — no collection key types.
- #39 checked: none — Python tests compile/load every added check.
- #40 checked: none — no dependency trait features.
- #41 checked: none — existing short-root guard remains active and every
  installed daemon started successfully.
- #42 checked: none — existing installed-binary warm-up remains unchanged.
- #43 checked: none — no descriptor-close sweep.
- #44 checked: none — real installed UDS runs completed in the enabled sandbox.
- #45 checked: none — no unsafe/cfg code.
- #46 checked: none — existing derived runtime root passed all runs.
- #47 checked: none — no filesystem walker.
- #48 checked: none — no Rust test module ledger.
- #49 checked: none — no queued batch acknowledgements.
- #50 checked: none — no byte-size pins.
- #51 checked: none — no profile-lock code.
- #52 checked: none — no UI viewport.
- #53 checked: none — existing owner-private roots passed all runs.
- #54 checked: none — no Rust async test future or test runner change.
- #55 checked: none — no Windows unit-valued binding.
- #56 checked: none — max-time evidence pins reason/dimension and exit 77.
- #57 checked: none — no UI layout pins.
- #58 checked: none — no CAS threshold.
- #59 checked: none — no roster UI grammar.
- #60 checked: none — no Windows connection/process liveness code.
- #61 checked: none — new Step 2 rows make no timing claims.
- #62 checked: none — no public Rust return-type change.
- #63 checked: none — no platform archive tool.
- #64 checked: none — installed binaries were verified as real 33/49 MiB Mach-O.
- #65 checked: none — no socket errno mapping.
- #66 checked: none — no STT surface.
- #67 checked: none — no Cargo suites; installed siblings explicitly supplied.
- #68 checked: none — no hardened product error path.
- #69 checked: none — no Windows path discovery.
- #70 checked: none — no CI trigger or dispatch.
- #71 checked: none — the actual installed artefacts ran end-to-end and emitted
  a validated report.
- #72 checked: none — hermetic discovery remains intentionally disabled; the
  account check uses owned loopback listeners, not native credentials.
- #73 checked: none — no fixed-window source scan.
- #74 checked: none — existing context pins temporary HOME/USERPROFILE/XDG.
- #75 checked: none — no actor shutdown change.
- #76 checked: none — account persistence now requires explicit
  `session.account_alias`, not a provider proxy.
- #77 checked: none — `git diff --check`, compileall, loader tests, report
  validation, and installed smoke all pass; no repository guard was changed.
- #78 checked: none — no workflow dispatch or tag operation.
- #94 fixed: every check declares a sum of named nested bounds; budget-control
  child + parent arithmetic is pinned at 252 seconds in
  `scripts/qa-gate/tests/test_runner.py:150`.
- #95 fixed: the SIGINT arm drains stdout while the client stays alive at
  `scripts/qa-gate/gate/context.py:264`; the SQLite terminal poll opens no
  negotiated product connection.
