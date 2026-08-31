# QA gate step 1 — citation and CI error registry audit

## Applicable verification

This lane adds only Python, shell, Markdown, and one generated JSON report. The
mission explicitly forbids Cargo builds on the shared machine, so Rust compile,
Clippy, rustfmt, test-ledger, and binary-link checks are not applicable to this
change. The applicable gate completed:

- `scripts/qa-gate/run.sh test`: 16/16 PASS, including foreign/missing-PID refusal,
  cleanup-exception containment, deadline arithmetic, and report mutations.
- installed `/usr/local/bin` v0.0.967 T0 run: 2/2 PASS, exit 0.
- deliberately mutated JSONL terminal expectation: 1/2 PASS, 1 FAIL, exit 1;
  diagnostic was `terminal_kind expected=failure actual=success`; restored.
- emitted report `run.sh validate`: `VALID`, schema `haider.qa-gate.v1`, 2 checks.
- report self-diff: `NO_CHANGES matched=2 mad=0.0ms`.
- `bash -n`, Python compile-all, and `git diff --check`: PASS.
- installed pair inspected as arm64 Mach-O; `haiderd` is 51,052,432 bytes and
  exceeds the 10 MiB truncation sentinel.

Timing was accepted on the final run because one-minute load 5.867 was below
10 logical CPUs; both correctness rows passed independently of timing.

## Citation audit

The mission's current-tree construct citations were audited before use:

| Citation | Verdict | Current evidence |
| --- | --- | --- |
| `scripts/tui-probes/probelib.py` throwaway/env/clean-exit laws | correct, but narrower than this gate needs | Refusal remains lines 73–90, env scrub 93–108, direct-child clean exit 154–178. It does not isolate runtime or HOME, which the new context adds. |
| `scripts/tui-probes/ladder.sh` | correct | Existing installed-binary argument pair and 14 demo + 2 live structure remain; this step does not modify or wire it. |
| `docs/jsonl-run-contract-v1.md` | correct | Acceptance/cursor lines 9–19 and terminal lines 78–102 match the check. |
| `crates/haider-cli/src/observe.rs` | correct construct; lens-B shape drift | Schema is line 22, fields 66–86, construction 582–600. `runtime_dir` is top-level, not inside `daemon`; the check uses the actual shape. |
| `crates/haider-cli/src/daemon.rs` | correct | Stop budget/schema lines 22–30, outcomes 38–68, exit mapping 133–139, report 611–628. A second `not_running` stop exits 69. |
| `crates/haider-cli/src/run.rs:26-34` | correct | Exit-code table is still exact. |
| `crates/haider-daemond/src/main.rs:225-255` | correct | Daemon parses and owns one injected fake; shared factory continues at 256–282. |
| `crates/haider-provider/src/lib.rs:2435-2688` | correct | Fake-step vocabulary and exact terminal segment cut remain in this range. |
| Lens-B sibling resolution `spawn.rs:154-156` | drifted/incomplete | The meaning is right, but executable sibling resolution is now `spawn.rs:554-577`. |

One unavoidable normalization is documented and tested: `haider --version`
prints `haider 0.0.967`, while status `.daemon.version` is bare `0.0.967`.

## §A–§D registry walk

`checked: none` means the class was inspected and this lane introduces no
instance. `fixed` names the new gate location exercising that registry law.

| Class | Result | Evidence |
| ---: | --- | --- |
| 1 | checked: none | No Rust struct/enum changed. |
| 2 | checked: none | No Rust API changed. |
| 3 | checked: none | No Rust ownership code. |
| 4 | checked: none | No Rust visibility seam. |
| 5 | checked: none | No cfg-narrow Rust import. |
| 6 | checked: none | No enum/import table edit. |
| 7 | checked: none | No manifest or lockfile edit. |
| 8 | checked: none | No mechanical source sweep. |
| 9 | checked: none | No Rust Clippy surface. |
| 10 | checked: none | Python helpers are exercised by self-tests or runtime checks. |
| 11 | checked: none | No Rust combinator lint surface. |
| 12 | checked: none | No Rust function signature. |
| 13 | checked: none | No Rust type alias surface. |
| 14 | checked: none | No Rust derives. |
| 15 | checked: none | No iterator-last rewrite. |
| 16 | checked: none | No Rust range expression. |
| 17 | checked: none | No Rust async lock. |
| 18 | checked: none | No Rust lint attribute or production unwrap. |
| 19 | checked: none | No `.rs` changed; Python compile-all and diff check pass. |
| 20 | checked: none | No Rust tests added; test ledger unchanged. |
| 21 | checked: none | No Rust test process. |
| 22 | checked: none | No tracing subscriber. |
| 23 | checked: none | No migration/schema edit. |
| 24 | checked: none | Provider catalogs untouched; only the injected fake is used. |
| 25 | checked: none | No render benchmark. |
| 26 | fixed: `scripts/qa-gate/gate/context.py:43` | Canonical absolute path comparison is platform-aware; Windows remains by inspection. |
| 27 | checked: none | Runner never speaks the wire protocol. |
| 28 | checked: none | No existing process-test runner altered. |
| 29 | fixed: `scripts/qa-gate/gate/context.py:246` | Cleanup status is explicitly `--no-spawn`; only check entry points may spawn. |
| 30 | fixed: `scripts/qa-gate/checks/t0/t0.run.jsonl_contract.py:23` | Finite single terminal segment and actual-value terminal diagnostic. |
| 31 | checked: none | Android untouched. |
| 32 | checked: none | No release action. |
| 33 | fixed: `scripts/qa-gate/run.sh:8` | Short-root environment change is scoped to the new runner process. |
| 34 | checked: none | No dependency or feature added. |
| 35 | checked: none | No Rust trait call. |
| 36 | checked: none | No Rust temporary borrow. |
| 37 | checked: none | No cfg-boundary type. |
| 38 | checked: none | No collection key seam. |
| 39 | checked: none | New tests import only public Python gate modules. |
| 40 | checked: none | No dependency error conversion. |
| 41 | fixed: `scripts/qa-gate/gate/context.py:140` | Per-check root is short and status must remain under its runtime child. |
| 42 | fixed: `scripts/qa-gate/runner.py:160` | Both version paths and an isolated ready/start/stop warm-up precede timed checks. |
| 43 | checked: none | No descriptor sweep change. |
| 44 | fixed: generated v0.0.967 report | Real installed-binary UDS smoke passed in this managed lane. |
| 45 | checked: none | No unsafe code. |
| 46 | fixed: `scripts/qa-gate/gate/context.py:143` | Owned 0700 child under sticky `/tmp`; product owner-root derivation unchanged. |
| 47 | checked: none | No filesystem walker. |
| 48 | checked: none | Tests are stdlib discovery modules, not Rust ledger inputs. |
| 49 | checked: none | No queued acknowledgement code. |
| 50 | checked: none | No exact platform-dependent byte pin. |
| 51 | checked: none | Profile-lock implementation untouched. |
| 52 | checked: none | No TUI viewport. |
| 53 | fixed: `scripts/qa-gate/gate/context.py:143` | Harness-created root and descendants are owner-controlled mode 0700. |
| 54 | checked: none | No Rust stack-bound suite; installed processes completed. |
| 55 | checked: none | No cfg-Windows binding. |
| 56 | checked: none | Product exit mapping untouched; JSONL asserts terminal reason and exit. |
| 57 | checked: none | No UI layout pin. |
| 58 | checked: none | No CAS threshold. |
| 59 | checked: none | No roster rendering. |
| 60 | checked: none | No connection-liveness implementation. |
| 61 | fixed: `scripts/qa-gate/gate/report.py:114` | README guarantees are enforced by the same executable report validator. |
| 62 | checked: none | No public Rust return type changed. |
| 63 | checked: none | No external archive/platform tool. |
| 64 | checked: none | Installed pair are valid Mach-O; daemon is 51,052,432 bytes. |
| 65 | checked: none | Harness asserts semantic JSON outcomes, not errno. |
| 66 | checked: none | STT untouched. |
| 67 | fixed: `scripts/qa-gate/gate/context.py:136` | Exact installed sibling pair is required; no build fallback. |
| 68 | checked: none | No swallowed product error changed. |
| 69 | fixed: `scripts/qa-gate/gate/context.py:136` | Commands use canonical real installed paths, not synthesized casing or PATH. |
| 70 | checked: none | No workflow trigger/dispatch edit. |
| 71 | fixed: `scripts/qa-gate/checks/t0/t0.run.jsonl_contract.py:58` | Real installed artefact boots a daemon and completes a fake-provider turn. |
| 72 | fixed: `scripts/qa-gate/gate/context.py:174` | Discovery and update checks are disabled only after the fake path is explicitly armed. |
| 73 | checked: none | No fixed-window source scan. |
| 74 | fixed: `scripts/qa-gate/gate/context.py:179` | Every check gets scratch HOME, USERPROFILE, XDG state, and workspace. |
| 75 | checked: none | Hub shutdown implementation untouched; stop is bounded. |
| 76 | checked: none | No wire projection field. |
| 77 | checked: none | No repository guard or workflow changed; applicable syntax/diff/schema guards pass. |
| 78 | checked: none | No tag or release dispatch. |
| 79 | checked: none | Process completion ownership untouched. |
| 80 | checked: none | No session-idle observer; terminality comes from JSONL. |
| 81 | checked: none | Subprocess output is fully drained by `communicate`. |
| 82 | checked: none | No foreground/background ownership seam. |
| 83 | checked: none | No detach fallback. |
| 84 | checked: none | No paused-time process test. |
| 85 | checked: none | Product process terminal classification untouched. |
| 86 | fixed: `scripts/qa-gate/gate/context.py:58` | PID liveness is observed independently after typed stop completion. |
| 87 | checked: none | No thread-count fence. |
| 88 | checked: none | No staging publication. |
| 89 | checked: none | Windows named-pipe handling is by inspection; no false filesystem-parent assertion is made there. |
| 90 | checked: none | No sparse-file fixture. |
| 91 | fixed: `scripts/qa-gate/gate/contract.py:151` | Human evidence is explicitly non-empty and CR/LF-free. |
| 92 | checked: none | No maintenance-loop timer. |
| 93 | checked: none | No throughput sampler. |
| 94 | fixed: `scripts/qa-gate/gate/contract.py:57` | All deadlines are named sums; literal-only budget and under-segment self-tests pass. |
| 95 | fixed: `scripts/qa-gate/gate/context.py:196` | Python uses finite CLI one-shots and never holds a negotiated connection during external waits. |

No new CI error class was discovered by this lane.
