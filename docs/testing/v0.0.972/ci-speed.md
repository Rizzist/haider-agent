# 972 CI-speed follow-ups

## Platform-sensitive golden audit

`xtask check` now scans fixture, golden, and snapshot artifacts for
platform-shell wording, unambiguous Windows path syntax, and per-OS tool
inventories. A target-qualified filename or explicit content placeholder is
sufficient. A generic fallback with target-specific siblings passes only when
a source consumer references both names and contains an OS selection.

The existing corpus result is 431 scanned, three sensitive, three
parameterized, and zero violations:

| Artifact | Sensitive content | Parameterization |
| --- | --- | --- |
| `capability_profiles.json` | POSIX shell wording | target-selected Windows sibling family |
| `capability_profiles.windows.json` | PowerShell/System32 wording | `.windows` filename |
| `android_standalone_tool_inventory.json` | per-OS tool inventory | `android` filename |

The self-tests include the old failure shape: adding a sibling filename without
a consumer that selects it still fails.

## Race-window sweep and fixes

The repository policy is in `docs/TESTING.md`. The catalog contains 67 cases:
all 59 test functions with explicit race/concurrency/interleaving names, plus
eight known timing-window tests whose names do not advertise the risk. The gate
also rejects a catalog platform that contradicts a test's direct
`#[cfg(target_os = ...)]`; this prevents a zero-test Cargo result from becoming
false evidence.

The first sweep found two actionable issues:

1. `concurrent_staging_verifies_all_signatures_before_any_smoke` was cataloged
   for Linux despite being macOS-only. The loaded Linux command honestly
   reported zero selected tests. It is now a macOS case and passes 20/20.
2. `decision_deny_is_reject_once_and_malformed_output_falls_through` failed on
   Linux iteration 10. The hook emitted `Broken pipe` and lost an otherwise
   complete `deny` result. A hook is allowed to ignore its input, emit a result,
   and exit. If that exit wins the stdin write, EPIPE is now treated as
   unconsumed input while the exit status, stdout, and stderr remain
   authoritative. All other stdin errors remain failures; the assertion was
   not changed.

The corrected catalog and hook implementation finish with this loaded sweep:

| Host | Cases | Iterations per case | Passed | Failed |
| --- | ---: | ---: | ---: | ---: |
| Linux (Colima, `linux/amd64`, 3 CPUs, 5 GiB) | 54 | 20 | 1,080 | 0 |
| macOS | 13 | 20 | 260 | 0 |
| **Total** | **67** | **20** | **1,340** | **0** |

The initial diagnostic evidence is retained separately: it records 1,069
successful iterations out of 1,071 attempts, including the honest zero-match
catalog error and the Linux EPIPE failure. The final table counts the corrected
platform run and the fresh 20/20 Linux rerun of the affected hook test.

## Android native compile shutdown investigation

The failures are runner shutdown/cancellation events rather than workflow
timeouts. Both 2a21c9b0 workflows already limited Cargo to two jobs but had no
timeout on the native job. A later `android-daemon` retry had a 75-minute job
timeout and still received the shutdown after 31m48s of job runtime.

| Workflow/run | Head | Native step | Observation |
| --- | --- | ---: | --- |
| `android-daemon` 34748383851 | 2a21c9b0 | 23m29s | runner shutdown signal, exit 143 |
| `android-apk` 34749435030 | 2a21c9b0 | 30m23s | operation cancelled during two-ABI compile |
| `android-daemon` 34751667688 | 57cda337 | 24m40s | runner shutdown signal, exit 143 despite 75-minute timeout |
| `android-apk` 34820314534 | 1401cd94 | 27m11s | operation cancelled during the old unsplit compile |

The 0dc6c4a0 `android-apk` change is structurally appropriate: host tests and
each ABI now run in separate jobs, and each ABI saves a source-keyed verified
checkpoint before packaging. It does not change `android-daemon`, whose one job
still runs host tests followed by a two-ABI native compile. It also has no
GitHub execution evidence yet: 0dc6c4a0 is not an ancestor of the main-branch
SHA used by the listed scheduled runs. Therefore the split is a mitigation for
`android-apk`, not a demonstrated resolution of the standalone
`android-daemon` exit-143 noise.

## Thin-LTO revisit

The release binaries were built from clean worktree-local target directories.
Each profile was prepared outside the quiet window; file sizes and SHA-256
hashes were then recorded only while the orchestrator granted quiet and the
one-minute load was below 2. The comparison is:

| Binary | Fat LTO bytes | Thin LTO bytes | Thin/fat | 1.10x ceiling |
| --- | ---: | ---: | ---: | --- |
| `haider` | 24,053,952 | 29,450,528 | 1.2244 | fail |
| `haiderd` | 59,087,072 | 80,567,648 | 1.3635 | fail |
| `haider-tui` | 25,561,824 | 32,334,288 | 1.2649 | fail |

Thin LTO with 16 codegen units exceeds the size ceiling for every measured
binary (22.44% to 36.35% larger). Keep the existing fat-LTO/one-codegen-unit
release profile. Build wall time on the shared machine is intentionally not
used as performance evidence.
