# v0.0.968 peakrss1 lane verification

## Scope and citation audit

This lane changes the hook outbox read-back, five daemon-owned HTTP consumers,
and the M1 sampler. The continuation changes only the two transport-constructor
hunks in `oauth.rs`; it does not move or modify refresh preparation, fence,
generation, or vault logic. `actor.rs`, the session-hub publication path, and
provider request/stream code remain untouched.

| Brief citation | Audit against this tree |
|---|---|
| `hooks.rs:87-88` page bounds | Drifted by three lines: the unchanged 256-row and 16 MiB constants are now `hooks.rs:90-91`. |
| `hooks.rs:1646-1668` engine entry | Correct construct, drifted lines: `run_engine` starts at `hooks.rs:1654`; startup and live replay now share the metadata drain. |
| `hooks.rs:1808-1821`, `1832-1900` eager page retention | Correct construct, drifted lines. `drain_hook_dispatch_page` now starts at `hooks.rs:1958`, its cursor update ends at `2178`, and exact-envelope hydration is at `2099`. |
| `event_store.rs:11660-11690` joined decode | Correct with minor drift: the legacy eager helper is `event_store.rs:11662-11718`; metadata and exact-coordinate reads are `11729-11848`. |
| `runtime.rs:965` unconditional hook service | Correct construct, drifted to `runtime.rs:957`. |
| `openai.rs:138-141` provider deadlines | Correct. The provider transport constants remain unchanged. |
| five daemon boot clients | Correct. Baseline builders were exactly plan, usage, web search, OAuth coordinator at `oauth.rs:2683`, and credential broker at `oauth.rs:5199`. The lifted-fence constructor plumbing is line-count neutral: the protected preparation-loss caller and helper remain at `oauth.rs:5835` and `5947`, exactly matching `HEAD`. |

## Outcome and evidence

- Lever 1: the hook engine first reads session id, sequence, and payload kind.
  Clean no-hook work is retained without decode when a future hook could match;
  statically impossible kinds are ACKed; uncertain or security/state kinds fail
  open to an exact one-envelope decode. Per-session sequence cursors avoid
  SQLite ROWID reuse, and a config content transition forces live process
  reconciliation. Static Lens-A accounting removes the modeled K1 text
  retention (estimated 1.1-4.3 MiB when it overlaps the high-water); M1 was not
  run because the load gate failed.
- Lever 5: usage, web search, plan polling, the OAuth coordinator, and the
  credential broker now share one lazy transport. No-proxy, no-redirect, and
  five-second-connect policy is unchanged. The shared 15-second default
  preserves both OAuth views; plan and usage explicitly retain 15 seconds and
  web search explicitly retains 45 seconds per request. Static A/B accounting
  proves the five scoped builders became one; the mutation control proves one
  shared build versus five independent builds.
- M1: `m1-rss-sampler.py` uses Darwin `proc_pidinfo(PROC_PIDTASKINFO)` at 1 ms,
  with a 136-byte `proc_bsdinfo` parent walk and fresh daemon PID claims. The
  self-test exercises both a synthetic root and descendant. At load 12.31 the
  driver reported `not measured, load too high`; self-test passed with 38
  samples and 24,494,080-byte maximum RSS. No before/after RSS is claimed.

Verification:

- Every Cargo build, test, Clippy, metadata, and xtask invocation was preceded
  by `df -m /`; available space stayed above 24,000 MiB, well above the 700 MiB
  environment-blocked floor.
- `cargo test --no-fail-fast -p haider-daemon --locked -- --test-threads=4`:
  861 passed, 3 ignored; integrations 103 + 1 + 1 passed. The dedicated OAuth
  module rerun passed 89/89.
- `cancelled_resolver_does_not_abandon_or_duplicate_refresh_flight`: 20/20
  independent processes passed, with a disk preflight before each invocation.
- Transport A/B: `HEAD` had six production builder sites (five scoped plus the
  DNS-pinned identity verifier); the worktree has two (one shared builder plus
  that verifier), proving the scoped result is 5 -> 1. The focused mutation
  test also passed.
- `cargo test --no-fail-fast -p haider-store -p haider-core -p haider-provider
  -p haider-accounts --locked -- --test-threads=4`: passed; only pre-existing
  manual/live/keychain ignores.
- `cargo clippy -p haider-store -p haider-core -p haider-daemon --all-targets
  --locked -- -D warnings`: passed.
- `cargo fmt --all -- --check`, `git diff --check`, shell syntax, Python bytecode
  compile, and two repeated sampler self-tests: passed.
- `cargo metadata --locked --no-deps --format-version 1`: passed; manifests and
  lockfile resolve without mutation.
- `cargo run -p xtask --locked -- test-count`: 4241, matching baseline 4241.
- Prebuilt `target/debug/haiderd`: Mach-O arm64, 181,100,080 bytes, above the
  registry #64 floor.

## CI error registry walk

No CI round ran in this lane, so no new registry class is appended. Each
existing class was checked against the touched surface:

| Class | Result |
|---:|---|
| 1 | checked: both OAuth constructor seams and all production/test call sites retain their signatures and fields. |
| 2 | checked: new store methods and unchanged OAuth constructor APIs have all call sites compiled. |
| 3 | checked: none; ownership compiled and tests passed. |
| 4 | checked: none; no private test-field construction. |
| 5 | checked: none; imports are cfg-correct. |
| 6 | checked: none; no duplicate import or variant. |
| 7 | checked: none; manifests and lockfile are unchanged; `--locked` passed. |
| 8 | checked: none; no mechanical sweep. |
| 9 | checked: none; deny-warning Clippy passed. |
| 10 | checked: none; deny-warning Clippy passed. |
| 11 | checked: none; deny-warning Clippy passed. |
| 12 | fixed: the drain arguments are carried by `HookDrainContext`. |
| 13 | checked: none; deny-warning Clippy passed. |
| 14 | checked: none; derives are valid. |
| 15 | checked: none; no iterator-last change. |
| 16 | checked: none; no range change. |
| 17 | checked: none; no lock guard crosses an await. |
| 18 | checked: none; sibling test module uses the repository form. |
| 19 | checked: none; rustfmt passed. |
| 20 | fixed: test baseline advanced 4239 -> 4241. |
| 21 | checked: the required 8 MiB test stack was exported. |
| 22 | checked: none; no tracing/global install. |
| 23 | checked: none; no migration added. |
| 24 | checked: provider catalog authority and the DNS-pinned identity-verifier transport are unchanged. |
| 25 | checked: none; no render benchmark. |
| 26 | checked: Windows paths/process commands have cfg peers; by inspection. |
| 27 | checked: none; wire keepalive code untouched. |
| 28 | checked: none; process-tree runner unchanged. |
| 29 | checked: none; autospawn law unchanged. |
| 30 | checked: hook observers fail by derived deadlines and named state. |
| 31 | checked: none; Android untouched. |
| 32 | checked: none; publishing untouched. |
| 33 | checked: none; runner behavior unchanged. |
| 34 | checked: none; no dependency module or feature added. |
| 35 | checked: none; no ambiguous trait call remains. |
| 36 | checked: none; no temporary borrowed through `?`. |
| 37 | checked: none; no cfg-boundary identity type. |
| 38 | checked: none; collection key types compile. |
| 39 | fixed: new daemon test is a declared sibling `*_tests.rs` module. |
| 40 | checked: none; no dependency error conversion in Windows cfg. |
| 41 | checked: none; endpoint paths untouched. |
| 42 | checked: no cold-binary timing assertion. |
| 43 | checked: none; descriptor sweep untouched. |
| 44 | checked: daemon suite executed in this socket-capable environment. |
| 45 | checked: no new unsafe code. |
| 46 | checked: runtime-root behavior untouched. |
| 47 | checked: no filesystem walker. |
| 48 | fixed: sibling test declaration matches the ledger scanner. |
| 49 | checked: hook ACK coordinates remain idempotent and batched. |
| 50 | checked: no platform-dependent byte pin. |
| 51 | checked: profile lock untouched. |
| 52 | checked: TUI help untouched. |
| 53 | checked: runtime-root permissions untouched. |
| 54 | checked: test commands mirrored the required stack environment. |
| 55 | checked: no cfg-dependent unit binding. |
| 56 | checked: provider deadline mapping untouched. |
| 57 | checked: no UI layout pin. |
| 58 | checked: CAS inline threshold untouched. |
| 59 | checked: roster grammar untouched. |
| 60 | checked: Windows liveness code untouched. |
| 61 | checked: sampler claims are enforced by self-validation. |
| 62 | checked: no public return type changed. |
| 63 | checked: no platform archive tool. |
| 64 | checked: `haiderd` is valid Mach-O and 181,100,080 bytes. |
| 65 | checked: no raw errno outcome change. |
| 66 | checked: STT untouched. |
| 67 | checked: both sibling binaries were prebuilt; daemon tests exported the prebuilt flag. |
| 68 | checked: no swallowed-error hardening. |
| 69 | checked: no Windows path casing construction. |
| 70 | checked: workflow triggers untouched. |
| 71 | checked: no release or promotion action; real-artifact RSS run was load-gated. |
| 72 | checked: discovery-disabled env was deliberate for the exact conformance case and tests. |
| 73 | checked: no fixed-window source scanner. |
| 74 | checked: test subprocess home behavior untouched. |
| 75 | checked: session-hub shutdown ownership untouched. |
| 76 | checked: wire and CLI projections untouched. |
| 77 | checked: repository guards used here were run; no CI push was made. |
| 78 | checked: release workflow untouched. |
| 94 | fixed: hook observation deadlines sum spawn, hook wall, three one-second settlements, and poll. |
| 95 | checked: new waits observe local hook/store/process state and hold no negotiated client connection. |

## Verdict

Lever 1, the M1 instrument, and the full five-to-one shared transport are ready
for integration. The OAuth continuation is constructor-only, preserves every
effective timeout, and leaves the protected refresh-race coordinates unchanged.
Full-lane verdict: **SHIP**.
