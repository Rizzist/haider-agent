# v0.0.968 retain lane: verification and CI-prep record

## Outcome

The two measured daemon-lifetime retentions are closed.

- Session deletion now establishes an actor FIFO quiescence fence, asks the
  worker manager to close the supervisor, waits for an acknowledged lease
  unregister, observes the manager-owned `JoinSet` result, removes the slot,
  and only then stops and joins the actor. Live durably-quiescent supervisors
  use the same owned retirement path after a five-minute supervisor-local TTL.
  New work resets the whole TTL, and work racing a retirement is deferred and
  transparently recreated rather than receiving `Busy`.
- Historical per-session incarnation counters are gone. Each supervisor spawn
  receives a random 128-bit nonce that namespaces its event IDs without
  retaining a session-ID map.
- `ObserveDigestCache` admits at most eight concurrent `Building` entries and
  retains at most 256 `Ready` entries or 32 MiB of conservatively charged
  deep-owned heap. Eviction is Ready-only LRU. Concurrent misses share one
  rebuild, and a deleted in-flight build cannot reinstall itself.
- Deleted actor `JoinHandle`s are keyed, joined, and removed immediately. The
  permanent deletion tombstone remains unchanged.

## Citation audit

The brief's locations were taken from `d78bd15`; the implementation base here
is `8952219`. Every construct was found by name before use.

| Brief reference | Verdict | Audited base/final location |
|---|---|---|
| `worker.rs:2332`, supervisors/incarnations bounded by sessions served | correct claim, drifted line | Base manager maps were around 2334-2337. Final manager begins at `worker.rs:2436`; the incarnation map is removed. |
| `worker.rs:3996`, no normal idle exit | correct claim, drifted line | Base idle select ended only on shutdown/channel close around 3921-4027. Final local idle deadline is armed at `worker.rs:3798` and reports retirement through the owned lifecycle path. |
| `worker.rs:2596`, only daemon shutdown sends `Shutdown` | slightly overbroad | Base manager shutdown sent it around 2598-2601, but manager channel closure also used the shutdown fallback around 2618-2621. Neither was a normal retirement path. |
| `worker.rs:1807`, `:3920`, no retirement command | correct claim, drifted lines | Final `ManagerCommand::Retire`, `SupervisorCommand::Retire`, and typed `SupervisorOutcome::QuiescentRetired` are near 1867-1990. |
| `session_hub/mod.rs:5668`, `:5723`, deletion never notifies manager | correct claim, drifted lines | Final two-phase fence and `worker_manager.retire` are at `mod.rs:5624-5674`; cache cleanup is at 5817. |
| `mod.rs:5943`, `:6836`, completed actor handle retained | correct claim, drifted lines | Keyed insertion/removal/join are at `mod.rs:6036`, 5729, and shutdown fallback 6913-6940. |
| `mod.rs:873`, permanent deletion tombstone | correct, line drifted | The tombstone remains at `mod.rs:885` and is not timed or removed after successful deletion. |
| `worker.rs:2811`, incarnation required for collision safety | correct invariant, drifted line | The old retained counter is replaced by `worker_spawn_nonce` at `worker.rs:1999`; spawn use is at 3306. |
| `rpc.rs:1032`, every observed session retained | correct claim | The cache definition remains around 1032, now with bounded state and admission constants at 1039-1041. |
| `rpc.rs:1015`, `usage_report.rs:1215`, fold grows by run/agent/chunk/tool IDs | correct claim, drifted lines | Recursive projection charging is at `rpc.rs:1220-1404`; internal usage-fold charging is at `usage_report.rs:1258`. |
| `mod.rs:5739`, `rpc.rs:1385`, delete already removes observe state | correct claim, drifted lines | Final cleanup is `mod.rs:5817`; deletion-build token invalidation is in the cache removal/install seams. |
| `rpc.rs:1393`, exact journal rebuild | correct claim, drifted line | The deterministic replay oracle is `rpc.rs:2223`; single-flight callers use it after eviction. |

## Soak evidence

The old combined signal is `(478.74 + 189.57) MiB / 10,000 = 70,077.38
B/session`. With `N=64`, an unfixed implementation would retain 4.277 MiB,
which is unambiguous against the asserted 16 KiB/session RSS ceiling while
remaining fast in CI. Eight unmeasured cycles warm allocator/runtime state.
The measured cycles create a session, run two fake-provider turns to durable
quiescence, observe it, and delete it. RSS is measured in an isolated
single-test subprocess so parallel tests cannot contaminate the process-wide
sample. Every logarithmic sample records elapsed uptime and host load.

Final local sample:

| N | elapsed ms | load averages | supervisors | observe entries | targeted cache bytes | RSS delta bytes |
|---:|---:|---|---:|---:|---:|---:|
| 1 | 22 | 14.05 / 12.64 / 21.01 | 0 | 0 | 0 | 32,768 |
| 2 | 57 | 14.05 / 12.64 / 21.01 | 0 | 0 | 0 | 180,224 |
| 4 | 113 | 14.05 / 12.64 / 21.01 | 0 | 0 | 0 | 360,448 |
| 8 | 184 | 14.05 / 12.64 / 21.01 | 0 | 0 | 0 | 475,136 |
| 16 | 329 | 14.05 / 12.64 / 21.01 | 0 | 0 | 0 | 557,056 |
| 32 | 612 | 13.33 / 12.51 / 20.92 | 0 | 0 | 0 | 737,280 |
| 64 | 1,169 | 13.33 / 12.51 / 20.92 | 0 | 0 | 0 | 917,504 |

Least-squares slopes: supervisors `0/session`, observe entries `0/session`,
targeted cache heap `0 B/session`, live RSS `11,945.09 B/session`. The
independent cache verifier repeated the earlier exact tree at RSS slopes
8,924.53, 5,330.10, and 6,856.69 B/session; every structural slope was zero.
Linux uses `/proc/self/status` by inspection. Windows has no host RSS sampler
in this unit test and is explicitly not claimed as locally executed.

## Behavioral and mutation proof

- `quiescent_supervisor_retirement_joins_before_slot_recreation` proves the
  manager acknowledgement is downstream of an actual `JoinSet` yield and a
  racing submit recreates without `Busy`.
- `durably_quiescent_supervisor_retires_at_the_conservative_idle_ttl` proves a
  stale pre-activity deadline cannot retire a supervisor and that activity
  earns a complete new five-minute TTL.
- `supervisor_retirement_requires_every_durable_run_to_be_terminal` excludes
  queued, active, cancelling, input-menu, and permission-menu work.
- `delete_during_an_active_turn_waits_for_the_actor_fence` proves the deletion
  fence stays behind a pre-fence active acceptance.
- The cache tests pin eight-building admission, the 256-entry LRU bound, the
  actual 32 MiB path, sparse B-tree node slack, Ready-only eviction,
  single-flight rebuilding, build/delete token invalidation, and byte-identical
  digest rebuilding from the journal.
- Mutation: acknowledging retirement before `JoinSet` yield failed
  `quiescent_supervisor_retirement_joins_before_slot_recreation` with joined
  count `0` versus expected `1`.
- Mutation: replacing the random nonce with the old fixed/recreated counter
  namespace failed `supervisor_recreation_has_a_unique_event_id_namespace`
  because both first event IDs were identical.

## Verification

All Cargo commands used:

`RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1
HAIDER_TEST_DEVICE_NAME=test-mac CARGO_INCREMENTAL=0
CARGO_PROFILE_DEV_DEBUG=0`

Daemon tests additionally used `HAIDER_TEST_SIBLINGS_PREBUILT=1` after
prebuilding `haiderd` and `haider`.

| Check | Result |
|---|---|
| Disk preflight | never below 28,592 MiB free; 700 MiB stop threshold never approached |
| `cargo test -p haider-daemon --locked` | library 871 passed / 3 ignored; `session_hub_tests` 103/103; remaining integration binaries 1/1 and 1/1 |
| Focused retention cache suite | 9/9, including isolated soak |
| Supervisor manager-law suite | 11/11 |
| `cargo clippy -p haider-daemon --all-targets --locked -- -D warnings` | PASS |
| Locked metadata | PASS after `cargo update --workspace --offline`; only the existing `libc` crate became a macOS test dependency |
| Unsafe-count guard | PASS, production 188 / test 16; the one reviewed test block is the macOS kernel RSS query and has a local SAFETY proof |
| Test-count guard | 4,247 / baseline 4,247 |
| Rustfmt / diff / conflict / unmerged checks | PASS |
| `cargo tree -d -p haider-daemon --locked` | reviewed; no new package version, only an existing `libc` edge for the macOS test probe |
| Built siblings | arm64 Mach-O; `haiderd` 181,223,568 bytes, `haider` 102,348,848 bytes; daemon exceeds 10 MiB |

One unrelated `native_pipe_mixed_row_and_coverage_tail_resumes_from_coverage`
coordinate assertion failed once after passing in the preceding full run. It
then passed five exact repeats and the final complete package run. No
native-pipe production or test code was changed by this lane.

`$T/ci-prep.sh` was unavailable because `$T` is unset and no script exists in
the worktree. Its applicable scoped steps were executed directly. The lane
brief forbids workspace-wide Clippy, so the exact affected-package all-target
command above was used.

Independent verification iterated to closure:

1. Lifecycle verifier: `NO_SHIP` for a missing explicit join witness and stale
   manager-owned TTL event. Both were repaired; re-review returned `SHIP`.
2. Cache verifier: `NO_SHIP` for non-isolated RSS sampling, shallow byte
   charging, and a synthetic cap test. Isolation/warmup, recursive conservative
   charging, and a real 32 MiB test were added.
3. Cache verifier: `NO_SHIP` for sparse B-tree node slack. Full-node charging
   and a sparse-root mutation pin were added; re-review returned `SHIP`.

Linux and Windows runtime behavior not executable on this macOS host is by
inspection only. The changes use Tokio channels/timers and platform-neutral
ownership for production behavior; only the optional test RSS sampler differs
by host.

## CI error registry walk

`checked` means the class was read against this exact lane tree. `fixed` names
the lane repair. No new error class was discovered.

| Class | Result | Evidence |
|---:|---|---|
| 1 | fixed | Every changed manager command/outcome/actor command constructor and match was reconciled; all-target check through Clippy and full tests compile. |
| 2 | fixed | `UnregisterHarness` gained an acknowledgement and all call sites were updated. |
| 3 | checked | No moved-value error remains; all-target Clippy passes. |
| 4 | checked | Tests use crate-visible helpers/accessors, not private external fields. |
| 5 | fixed | macOS RSS code and dependency are target-scoped; Linux and fallback arms compile by inspection. |
| 6 | checked | No duplicate imports or variants. |
| 7 | fixed | `cargo update --workspace --offline` normalized the lock; locked metadata passes. |
| 8 | checked | Every mutation was reverted and the final diff reread. |
| 9 | checked | Final deny-warnings Clippy passes. |
| 10 | checked | No dead/unused helper remains. |
| 11 | checked | Final deny-warnings Clippy passes. |
| 12 | fixed | The expanded supervisor factory uses `SupervisorSpawnState`; no `too_many_arguments` suppression remains. |
| 13 | checked | No type-complexity diagnostic. |
| 14 | checked | No new problematic equality derive. |
| 15 | checked | No iterator-last sweep. |
| 16 | checked | No manual-range diagnostic. |
| 17 | fixed | Manager/actor mutex guards are dropped before retirement and task joins; Clippy reports no held-lock await. |
| 18 | checked | One smallest-scope reviewed macOS test unsafe block; no duplicate lint allowance. |
| 19 | checked | `cargo fmt --all -- --check` passes. |
| 20 | fixed | Test baseline updated to and verified at 4,247. |
| 21 | checked | Every test command used the required 8 MiB stack. |
| 22 | checked | No tracing subscriber change. |
| 23 | checked | No migration/schema change. |
| 24 | checked | No provider-catalog authority change. |
| 25 | checked | Soak reports fitted slopes, not a stopwatch benchmark claim. |
| 26 | checked | No production platform filesystem change. |
| 27 | checked | No Windows wire change. |
| 28 | checked | No Windows process-tree runner change. |
| 29 | checked | No autospawn change. |
| 30 | checked | Soak child failure includes complete stdout/stderr and exit status. |
| 31 | checked | No Android change. |
| 32 | checked | No release action. |
| 33 | fixed | RSS measurement is isolated to its own single-thread test process; no broad runner change. |
| 34 | checked | No new dependency module feature; `libc` is the already-locked macOS API crate. |
| 35 | checked | No ambiguous trait call. |
| 36 | checked | No temporary reference borrowed through `?`. |
| 37 | checked | Platform RSS arms share `Option<u64>`. |
| 38 | checked | Cache maps are probed with their exact key types. |
| 39 | fixed | Full library and every package test binary compile and pass on the final tree. |
| 40 | checked | No Windows dependency-error conversion. |
| 41 | checked | No UDS path change. |
| 42 | checked | No launch-timing assertion. |
| 43 | checked | No descriptor sweep change. |
| 44 | checked | macOS proof is local; Linux/Windows are explicitly by inspection. |
| 45 | fixed | The single macOS test unsafe block is allowed on the smallest function and documented with SAFETY; count baseline is reviewed. |
| 46 | checked | No runtime-root derivation change. |
| 47 | checked | No walker behavior change. |
| 48 | checked | Tests remain in declared inline/sibling modules; ledger test and count pass. |
| 49 | checked | Observe build completion publishes once; pending commits are merged once by exact sequence. |
| 50 | checked | Digest parity is byte-compared; no platform-dependent prose pin. |
| 51 | checked | No profile-lock change. |
| 52 | checked | No TUI viewport change. |
| 53 | checked | No runtime-root permission change. |
| 54 | checked | Correct runner stack used; all later test binaries were reached. |
| 55 | checked | No cfg-Windows unit binding. |
| 56 | checked | No deadline exit-code mapping. |
| 57 | checked | No UI layout pins. |
| 58 | checked | No tool-result CAS threshold change. |
| 59 | checked | No roster rendering change. |
| 60 | checked | No IPC connection-liveness change. |
| 61 | fixed | Every retirement/cache/soak guarantee in this record has a named assertion. |
| 62 | checked | `unregister_worker` retains its `Result<(), _>` surface; actor acknowledgement is internal. |
| 63 | checked | No archive shell utility. |
| 64 | checked | Both built binaries are valid Mach-O; `haiderd` is 181,223,568 bytes. |
| 65 | checked | Tests assert typed outcomes rather than raw errnos. |
| 66 | checked | No STT surface. |
| 67 | fixed | Siblings were prebuilt and daemon tests used `HAIDER_TEST_SIBLINGS_PREBUILT=1`. |
| 68 | checked | Lease-unregister failure remains typed; benign superseded lease is still a no-op acknowledged by the actor. |
| 69 | checked | No executable discovery/casing change. |
| 70 | checked | No workflow trigger or dispatch. |
| 71 | checked | This lane changes daemon internals and makes no release-binary smoke claim beyond valid binary inspection. |
| 72 | checked | No credential discovery path change. |
| 73 | fixed | The shell-arm structural test now uses a semantic source boundary, not a fixed byte window or formatting-adjacent needle. |
| 74 | checked | Soak uses a temporary profile store and no machine-global state. |
| 75 | fixed | Keyed actor joins and explicit lease acknowledgement avoid hub-owned sender/join deadlock. |
| 76 | checked | No wire projection field change. |
| 77 | fixed | Unsafe-count guard ran and passed in final CI-prep. |
| 78 | checked | No tag/release dispatch. |
| 94 | checked | The only new TTL test uses paused Tokio time and no outer deadline shorter than a production budget. |
| 95 | checked | No negotiated connection remains open while the soak waits on external state. |

