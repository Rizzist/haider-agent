# v0.0.967 gate round 6: cross-platform failure report

## Outcome and attribution

Run `33332580594` attributes seven of the eight failures to Windows and one to
Linux. Android passed.

| Platform | Failing test | Direct failure |
|---|---|---|
| Windows | `manifest_creation_stays_full_while_heartbeat_uses_plain_sync` | `ERROR_SHARING_VIOLATION` while publishing the peer manifest |
| Windows | `attachment_receipt_replay_routes_through_fused_title_repair` | LF-only source boundary was absent in a CRLF checkout |
| Windows | `tool_calls_execute_and_continue_over_real_rpc` | a 1 TiB `set_len` fixture returned `ERROR_DISK_FULL` |
| Windows | `two_private_home_instances_are_self_contained_and_never_adopt_each_other` | assumed the named-pipe endpoint was a child of HOME |
| Windows | `built_status_json_completes_with_enabled_discovery` | compared named-pipe `Path::parent()` with the PID directory |
| Windows | `default_home_store_dir_is_preserved` | assumed the named-pipe endpoint was a child of the runtime directory |
| Windows | `private_home_contains_store_runtime_and_endpoint` | assumed the named-pipe endpoint was a child of HOME |
| Linux | `peer_maintenance_is_event_armed_with_heartbeat_and_audit_repair` | advanced paused time while the prior serialized reconciliation was still running |

## Root causes and repairs

1. Peer-manifest publication writes and syncs a staging file and atomically
   replaces the destination. The publisher retained its own staging `File`
   through replacement. Windows rejected that live handle. The staging handle
   is now explicitly dropped after its completed-file sync and before
   replacement (`peer/mod.rs:2257-2265`). The Unix atomicity guarantee is
   unchanged.
2. The four profile/home/status failures share one Unix-shaped test assumption,
   not four product defects. Windows deliberately derives a profile-digested
   named pipe independent of the disk runtime. Tests now compare the endpoint
   with `endpoint_path_for`, require distinct endpoints for distinct profiles,
   and check store/PID/runtime disk containment separately. The profile
   contract documentation now states the Unix/Windows split explicitly.
3. The tool-calling failure was a fixture portability defect. Unix `set_len`
   produced a sparse 1 TiB logical file; Windows tried to allocate it. The
   Windows fixture now marks the open file sparse through the platform crate
   before extending it. The ignored-tree test retains the same 1 TiB logical
   mutation-strengthening input.
4. The attachment test embedded LF in a source-boundary needle. The boundary is
   now a syntax token independent of LF/CRLF; all receipt/repair/validation
   ordering assertions remain unchanged.
5. The audit timer remains anchored and active. Its test counter records entry,
   however, so Linux could observe the event reconciliation and then advance
   paused time while the loop was still inside that serialized reconciliation.
   The test now fences on that work leaving the serial section before advancing
   to the unchanged thirty-second audit and still requires the repair count.
6. The Windows all-target Clippy leg also exposed existing cfg-narrow test
   helpers in `haider-tools`: one import and two no-op types were Unix-only in
   use but not cfg-scoped. They are now cfg-scoped. This is registry classes 5
   and 10 rather than a ninth runtime failure.

## Proven versus believed

Proven on this macOS host:

- the CI log's platform attribution and exact failure text;
- the manifest's staged handle is closed before the same replacement boundary;
- the endpoint assertions use the shared production derivation and retain
  store/runtime/PID and two-profile isolation invariants;
- the LF/CRLF-independent source scan retains its ordering assertions;
- the audit repair passes 20/20 together with the manifest test, and the full
  daemon library passes;
- the 1 TiB logical ignored-tree fixture and real RPC continuation still pass;
- all required macOS test families, host all-target check, host all-target
  deny-warnings Clippy, formatting, lock metadata, and unsafe guard pass.

Believed fixed pending the next xplat run, because this host cannot execute the
Linux or Windows kernels:

- Windows replacement no longer raises code 32 for the publisher's own staged
  handle;
- Windows sparse marking prevents code 112 for the 1 TiB logical fixture;
- all six remaining Windows test/Clippy failures compile and pass with Windows
  path, CRLF, and cfg semantics;
- the Linux scheduler can no longer advance the audit deadline behind the
  still-running event reconciliation.

The installed Rust toolchain has no Windows standard library despite stale
target-list metadata, so no cross-target compilation is claimed.

## Citation audit

| Brief citation or claim | Verdict | Audited location/evidence |
|---|---|---|
| peer manifest failure at `peer_tests.rs:112` | correct on `8e9ed15` | run log and base source agree |
| Linux audit assertion at `peer_tests.rs:201` | correct on `8e9ed15` | the completion fence moves the final assertion to line 207 |
| attachment boundary at private test line 94 | correct on `8e9ed15` | the revised boundary ends at line 96 after its explanatory comment |
| `HAIDER_DAEMON_TRACE=1` at telemetry line 13 | correct | `telemetry.rs:13`; allow-list is lines 14-15 |
| subscriber installed at `main.rs:120` | drifted | line 120 is blank; containing function starts 121 and the call is 122 |
| `CountingAllocator` at provider lines 354-371 | correct as a containing range | test module begins 355, type is 368, allocator static is 370-371 |
| daemon library is 828 passed / 3 ignored | drifted | this integrated tree is 850 passed / 3 ignored |
| RPC goldens are 180 | drifted | the fixture is 182; the current package run totals 155 passing tests |
| core-loop E2E is 10/10 | correct | required full daemond run is 10/10 |
| unsafe guard is production 188 / test 15 | correct | guard exits 0 at exactly 188/15 |
| journal commits issue `F_FULLFSYNC` | wrong, as corrected | WAL uses configured `synchronous=NORMAL`; `fullfsync` is not issued |
| `queue_wait_micros=269` is mutex contention | wrong, as corrected | it is blocking-pool scheduling delay |
| armed process exit still polls at 1 kHz | wrong, as corrected | supported armed paths use kernel notification |
| fresh boot overstates settled footprint by about 2.4x | not re-proven | retained measurements use a ten-second settle and make no fresh-boot claim |

## Verification evidence

All Cargo commands used the required environment and the test commands also
used `HAIDER_TEST_SIBLINGS_PREBUILT=1` after prebuilding both binaries.

| Check | Result |
|---|---|
| disk preflight | 28,758 MiB free at final CI-prep |
| `check-unsafe-counts.sh` | PASS, production 188 / test 15 |
| locked metadata | PASS; lockfile unchanged |
| `haiderd` / `haider` prebuild | Mach-O arm64, 179,343,664 / 100,896,656 bytes |
| daemon library | 850 passed / 3 ignored |
| daemond package | 136 passed; core-loop E2E 10/10 |
| RPC package | 155 passed; wire fixture 182 frames |
| status discovery smoke | 1/1 |
| manifest + audit repetitions | 20/20 each |
| workspace all-target check | PASS (one run) |
| workspace all-target Clippy `-D warnings` | PASS (one run) |
| rustfmt 2024 / diff check / conflict and unmerged scans | PASS |
| `cargo tree -d` | reviewed; no new dependency version, only features on the existing `windows-sys` dependency |

The registry's `$T/ci-prep.sh` is not present and `$T` is unset. Its applicable
steps were therefore run directly. A workspace-wide test command was not
substituted because the gate explicitly forbids `cargo test --workspace` and
provides the required family list.

## Measurement

Gate six makes cross-platform correctness changes and claims no new performance
improvement. The release's retained inside-process before/after measurement is
therefore reported without reinterpretation: every daemon settled for ten
seconds; `ls` process-exec latency was 18.761208 ms before and 16.774792 ms
after (-1.986416 ms), RSS delta was +1,409,024 B before and +1,441,792 B after
(+32,768 B), and physical-footprint delta was +311,296 B before and +294,936 B
after (-16,360 B). This is an honest null memory result, not a memory-win claim.

## Verify-until-SHIP

1. Independent verifier iteration 1: `SHIP` after inspecting the exact final
   diff, run-log attribution, root-cause mapping, and completed verification
   evidence. No verifier-requested repair iteration was needed.

## §A-§D registry audit

`checked` means the class was read against this exact tree; `fixed` identifies
the gate-six repair.

| Class | Result | Evidence |
|---:|---|---|
| 1 | checked | No existing public struct/enum shape changed. |
| 2 | checked | One additive Windows fs helper; its definition and sole call were grep-audited across the cfg seam. |
| 3 | checked | Workspace check found no ownership error. |
| 4 | checked | Tests use public constructors/accessors. |
| 5 | fixed | Unix-only async trait and `Stdio` imports are cfg-scoped. |
| 6 | checked | No duplicate import or variant. |
| 7 | fixed | Required `windows-sys` modules are enabled; locked metadata passes without lock drift. |
| 8 | checked | Changes were applied once and re-read. |
| 9 | checked | Deny-warnings Clippy passes. |
| 10 | fixed | Unix-only no-op test helpers are cfg-scoped. |
| 11 | checked | Deny-warnings Clippy passes. |
| 12 | checked | No new long argument list. |
| 13 | checked | No type-complexity diagnostic. |
| 14 | checked | No new equality derive. |
| 15 | checked | No iterator-last change. |
| 16 | checked | No range diagnostic. |
| 17 | checked | No lock is held across a new await; the test-only fence acquires and releases directly. |
| 18 | checked | No duplicate lint allowance; unsafe remains isolated in the existing platform boundary. |
| 19 | fixed | Rustfmt 2024 check passes on every touched Rust file. |
| 20 | checked | No test-count baseline change. |
| 21 | checked | Every test used the required 8 MiB stack. |
| 22 | checked | No tracing install change. |
| 23 | checked | No migration/schema change. |
| 24 | checked | No provider-catalog change. |
| 25 | checked | Retained measurement is reported; no new benchmark claim. |
| 26 | fixed | Windows named-pipe and sparse-file behavior are explicit; no directory-fsync assertion. |
| 27 | checked | No Windows wire change. |
| 28 | checked | No Windows process-tree runner change. |
| 29 | checked | No autospawn policy change. |
| 30 | checked | Existing terminal observers unchanged. |
| 31 | checked | Android was green; no Kotlin/APK change. |
| 32 | checked | No release action. |
| 33 | checked | No test runner behavior changed. |
| 34 | fixed | `Win32_System_IO` and `Win32_System_Ioctl` enabled in `haider-platform`. |
| 35 | checked | No ambiguous trait call. |
| 36 | checked | No temporary borrowed through `?`. |
| 37 | checked | Windows helper arms share typed `io::Result<u64>`. |
| 38 | checked | No map/set key change. |
| 39 | fixed | Every changed test source was compiled locally and Windows-only paths were read. |
| 40 | checked | No dependency error conversion through `?` into a trait object. |
| 41 | checked | Unix budget fallback remains; Windows named-pipe derivation is documented separately. |
| 42 | checked | No launch-timing assertion. |
| 43 | checked | No descriptor sweep change. |
| 44 | checked | macOS proof is local; Linux/Windows are explicitly classified as believed. |
| 45 | fixed | No new unsafe block/count; sparse control reuses the reviewed Windows platform boundary. |
| 46 | checked | Sticky-root derivation unchanged. |
| 47 | fixed | Large ignored-tree fixture remains logically 1 TiB on Windows via sparse marking. |
| 48 | checked | No daemon source test module was added. |
| 49 | checked | No acknowledgement replay change. |
| 50 | checked | No platform-dependent byte pin. |
| 51 | checked | No profile-lock change. |
| 52 | checked | No TUI viewport change. |
| 53 | checked | Filesystem runtime containment remains owner-private. |
| 54 | checked | Correct runner stack used; all later binaries were reached. |
| 55 | checked | No cfg-windows unit-valued binding. |
| 56 | checked | No deadline exit mapping. |
| 57 | checked | No UI layout pin. |
| 58 | checked | No inline/CAS threshold change. |
| 59 | checked | No roster suffix change. |
| 60 | checked | No IPC connection-liveness change. |
| 61 | checked | Every guarantee described here retains an assertion. |
| 62 | checked | Additive fs API does not replace an existing return type. |
| 63 | fixed | Uses the Win32 API directly, not a divergent shell utility. |
| 64 | checked | Both prebuilt binaries are valid Mach-O; `haiderd` exceeds 10 MiB. |
| 65 | checked | Failure attribution uses typed semantic/platform codes, not a portable raw-errno assertion. |
| 66 | checked | No STT surface. |
| 67 | checked | Sibling binaries were prebuilt and the required flag was exported. |
| 68 | checked | No swallowed error hardened. |
| 69 | checked | No Windows executable discovery/casing change. |
| 70 | checked | No workflow trigger/dispatch. |
| 71 | checked | Real status and RPC artefact tests pass locally. |
| 72 | checked | Status test deliberately enables discovery within its bounded real-daemon fixture. |
| 73 | fixed | Source pin no longer embeds line-ending bytes. |
| 74 | checked | Real-daemon fixtures retain temporary machine-user homes. |
| 75 | checked | No actor drain ownership change. |
| 76 | checked | No wire projection field change. |
| 77 | checked | Unsafe guard ran first in final CI-prep and passed. |
| 78 | checked | No tag or release dispatch. |
| 79 | checked | No natural process-completion change. |
| 80 | checked | Core loop stays 10/10. |
| 81 | checked | No output-reader readiness change. |
| 82 | checked | No foreground/background ownership change. |
| 83 | checked | No completion-detach change. |
| 84 | fixed | Serialized reconciliation completion is fenced before paused-time advance. |
| 85 | checked | No late-cancellation classification change. |
| 86 | checked | No exit-observer error change. |
| 87 | checked | No thread-count lifecycle fence. |
| 88 | fixed | Staging handle closes before atomic manifest replacement. |
| 89 | fixed | Windows endpoint scope is asserted by derivation, not disk parentage. |
| 90 | fixed | Windows fixture marks the file sparse before `set_len`. |
| 91 | fixed | Source boundary is LF/CRLF independent. |
| 92 | fixed | Entry observation is followed by a serialized-work completion fence. |
