# 970 release pipeline: exact-SHA reuse and parallel pre-tag work

Owner directive, 2026-09-07. Implementation lane: `970-cifast`.
The supplied measurement is release run **34094765449**, v0.0.970:
about 5 hours from wave push to publish, 3 hours from tag to publish.
Those numbers are the historical baseline, not measurements of these changes.

## Pipeline and tag contract

```text
wave-* push ── ci (macOS lint/tests + Linux computer-use)
            ├─ xplat-check (Linux + 3 Windows test shards + Android)
            └─ ship-gate
                 ├─ evidence: wait for exact-SHA ci + xplat success
                 ├─ shared arm64 macOS release build ─┬─ client footprint
                 │                                   └─ daemon footprint
                 ├─ render benchmark (own release test binary)
                 └─ probe ladder (CI profile)
                    └─ verdict: all required results must succeed

same SHA → main: ci/xplat push omitted; ship-gate reuses completed wave evidence
same SHA → tag push: release looks up completed ship-gate success
                    ├─ found: reuse verdict + unsigned arm64 artifact if retained
                    └─ missing: call ship-gate, then require its success
                  → 5 build/sign/package targets → Additional installers → publish
```

The orchestrator may tag as soon as **ci, xplat-check and ship-gate's terminal
`verdict` are green on the exact candidate head**, with the ship-gate run itself
completed successfully. Fast-forward main to that same commit; do not wait for
main's duplicate measurement. Never manually dispatch `release.yml` (registry
#78). The tag push triggers release. Pre-tag ship-gate remains mandatory (#111);
release's fallback protects unusual invocation paths, not permission to skip
pre-tag validation.

`require-evidence.sh` retains its exact-SHA, completed-success policy and its
SHA-checked dispatch fallback for ci/xplat. Their `branches-ignore: [main]` push
filters avoid even creating duplicate workflows. PRs, other branch pushes and
`workflow_dispatch` remain enabled. If direct main work has no wave evidence,
ship-gate dispatches the missing workflows on main after verifying that main
still resolves to the candidate. A moving branch cannot validate an older SHA.

`find-evidence.py` is a read-only sibling: paginated lookup by workflow filename
and full SHA, any ref/event, only `status=completed` plus `conclusion=success`.
It excludes the current run (including reruns of that run). Wrong SHAs, queued,
failed, cancelled, skipped and neutral runs cannot satisfy it. A valid miss
outputs `satisfied=false`; malformed responses/API errors fail the job. No
successful lookup plus no successful fallback means no build. Installers,
publish and package follow-up explicitly require their direct dependencies to
succeed and reject cancellation, so the intentionally skipped reusable gate
cannot silently skip the rest of a successful release. A successful
older attempt/run remains usable, matching the existing require-evidence rule.

Ship-gate concurrency is keyed by **SHA**, with cancellation disabled. A main
push waits for an active wave run for that SHA and then performs a small reuse
lookup/verdict instead of measuring again. Different candidate SHAs can run
concurrently; obsolete candidates may be cancelled deliberately by the release
owner. The workflow does not cancel a measurement that a tag may be awaiting.
The verdict rejects failed, cancelled or unexpectedly skipped dependencies when
there is no completed-success reuse. No behaviour job waits for ci/xplat evidence;
only the two footprint jobs wait for their binary producer.

## Unsigned macOS artifact contract

`build-macos-release` uses `macos-15`, native `aarch64-apple-darwin`, Rust **1.95.0**:

```sh
source scripts/release/macos-release-env.sh
cargo build --release --locked -p haider-cli -p haider-tui-exe -p haider-daemond --target aarch64-apple-darwin
```

The shared env file is also sourced by the macOS release compiler and artifact
verifiers. `CARGO_INCREMENTAL=0` is required. Cargo.toml remains authoritative for
the release profile, except explicitly documented macOS experiment overrides.
No Rust sources, profile defaults for other targets, or budget thresholds change.

Artifact **`release-build-aarch64-apple-darwin-<full-sha>`**, retained 14 days,
contains `release-build.tar.gz` and `manifest.json`. The archive includes `haider`,
`haider-tui`, `haiderd`, their packed `.dSYM` companions, and
`haider-symbol-archive` (needed by the unchanged native-ID symbol archive step),
plus its `.dSYM` if emitted. Cargo copies each bundle out of `deps/` but keeps
the hashed DWARF basename inside it, for example
`haider-tui.dSYM/Contents/Resources/DWARF/haider_tui-<hash>`. Preserve the entire
bundle, including relocation files; do not rename its internal members or run
`dsymutil` again on an already stripped executable. A tar preserves executable modes inside Actions'
artifact ZIP. Manifest entries record SHA-256, byte size and normalized mode for
every file. Identity includes the full candidate SHA, target, verbose rustc
version and a profile fingerprint over the release profile, release override
env/Rust flags, Cargo.lock and repository Cargo config.

Consumers validate identity, complete inventory, file sizes, modes and checksums
in private staging before copying to `target/aarch64-apple-darwin/release`.
Pack and restore also compare `xcrun dwarfdump --uuid` sets for each runtime
executable and its whole dSYM bundle, matching the mechanism in
`haider-symbol-archive`. Each runtime bundle requires `Info.plist` and a direct
DWARF file, without assuming the file's basename. Restore checks UUIDs in staging
before touching the destination.
Traversal, links, duplicate archive entries and missing symbols are rejected.
An existing but invalid artifact fails closed; it never silently falls back.
The release evidence job resolves an unexpired artifact's run ID from a successful
ship-gate run; download uses `run-id`, `github-token` and `actions: read`.
A reusable fallback uploads into its release caller run and returns that run ID.
A fallback that itself reuses wave evidence returns the original artifact run ID.
Expired/absent artifacts permit a normal local release compile after the gate.
API/download/verification failures remain failures.

Client and daemon measurements use these exact unsigned bytes. Only the small
`memdaemon_workload` debug driver is built by the daemon measurement job. The
render benchmark keeps its own release test binary and an explicit arm64 target;
it shares `macos-release-aarch64` with the producer (and release's dependency
cache). Release's cache step explicitly matches the producer's CARGO env before
key selection. Concurrent jobs cannot consume a cache that has not been saved yet, and
GitHub cache ref scope still applies; an artifact, not a cache hit, proves byte
identity. No test or timing measurement is satisfied by a cache.

Codesign, notarization, `haider-compat`, native-ID symbol archiving and packaging
steps are unchanged and consume the restored target paths. The small compat
build still runs where it did before. Raw binary sizes in the unsigned manifest
allow comparisons before signing changes bytes.

**Intel decision:** do not add a second shared producer. Intel macOS runner queue
latency is the slowest part of the fleet; making every wave gate wait for it would
move that delay before the tag. Intel builds once in release, with the same macOS
compiler env. Arm64 is the platform for the existing footprint budgets. Revisit
Intel prebuilding only with measured queue and critical-path evidence.

## Windows rehearsal and test shards

`windows-installer-check.yml` runs on `ci-winonly-*` pushes and dispatch. It copies
the Windows release build and the Additional installers steps, publishing only
Actions evidence artifacts. It has no GitHub Release, npm, package re-pin or
Chocolatey publish jobs. Tag checks use `refs/tags/`, including the inline smoke
version check, so a branch push can exercise the complete pipeline. Source
reference: `origin/ci-winonly-98e219c7`, including its later tag-condition fixes.

Optional dispatch input `reuse_run_id` skips compilation and downloads
`distribution-x86_64-pc-windows-msvc` from that prior run. The run's head must be
the **same full SHA** as this rehearsal and the artifact must be unexpired. A
changed installer commit needs its own build; a binary from an older SHA is not
candidate evidence. Empty input builds the current candidate. Existing payload
version/checksum/member validation, packaging tests and the Windows lifecycle
checks in `windows-build.ps1` remain intact. Additional installers steps match
release verbatim apart from download source and the preceding provenance check.

Windows xplat tests use three entries in the existing `rust` matrix, inside the
same **xplat-check workflow run**. `ci-test.sh` sorts its crate names with `LC_ALL=C`
and assigns `(sorted index modulo shard count)`, one-based shard inputs. Every
crate executes exactly once across three shards; each shard compiles only its
selected test crates and explicitly builds the runtime siblings needed by
subprocess fixtures. Unsharded macOS/Linux keep workspace test compilation and
the existing execution order. All retain the 8 MiB stacks, hermetic env,
15-minute per-crate cap, no-fail-fast behavior and full failure collection.

The streamed Windows process-tree test runs in haider-daemond's shard. The named
Windows clipboard gate, native bundle tests and thin executable boundary checks
run only in shard 1. The current deterministic partition also places haider-tui
in shard 1, so its ordinary clipboard test and the explicit gate remain on that
same shard. Failure artifact names include the shard number.

The five `test_ci_test_shards.ShardTests` cases are POSIX-only: they execute the
real Bash driver with executable shell shims and POSIX paths. Windows Python's
bare `bash` lookup selected the WSL launcher in the first installer rehearsal,
which has no installed distribution, so it never reached the driver or shim.
The class-level `skipUnless(os.name == 'posix')` reason names the covering
`xplat-check` Linux **check** leg's `pipeline regression tests (POSIX Bash driver)`
step; macOS `ci` also runs these cases. This skips only the Python shell-driver
harness on Windows; all three native Windows Rust test shards still execute
`ci-test.sh` through Actions' configured Bash shell.

The artifact test rejecting an unset POSIX execute bit is also POSIX-only,
because Windows cannot write that bit. Its reason names the same Linux check
leg. All other artifact tests and every evidence resolver test remain enabled
on Windows Python 3.12: they invoke `sys.executable` and replace only external
`rustc`/`xcrun`/`gh` transports inside the child process. The artifact fixture
supplies execute bits only for its generated macOS payload on Windows; it does
not alter production permission checks. The LF-newline driver case simulates
CRLF defaults on POSIX and does not claim Windows runtime coverage.

## First-execution repairs (970-cifast-fix)

Base: `cec5a62d86423f671d1031f0701f6a4265bd417e`. Ship-gate run **34127778785**
finished its thin-LTO build in 24m42s, then rejected the three hashed DWARF
members because the packer demanded un-hashed names. The bundles and plist
members were present. Windows installer rehearsal **34127927580** failed only
the five shard harness cases (two assertions and three missing-call-file errors);
the log's UTF-16 WSL error identifies the wrong Bash launcher. Neither failure
is evidence of a Rust source defect.

Local evidence is under
`/Users/rizzist/Developer/haiderharness/state/evidence/970-cifast-fix/1-impl/`:

- `cargo-symbol-layout.log` and `cargo-symbol-fixture/`: a zero-dependency Cargo
  fixture with the repository release profile and an unchanged copy of the
  actual symbol archiver, built with Rust 1.95.0, `--release --offline --target
  aarch64-apple-darwin`, `CARGO_BUILD_JOBS=2`, and thin/16 plus fat/1 overrides.
  Both produced hashed DWARF files and matching executable/bundle UUIDs. This
  tiny fixture observes the real compiler layout; it is not a Haider runtime build.
- `pre-fix-pack-python312.log`: the base packer rejects that actual thin build
  with exactly the CI missing-member list. The first attempt, retained in
  `pre-fix-pack.log`, instead failed because `env.sh` selected Python without
  `tomllib`; all subsequent gates put the supplied Python 3.12 venv first.
- `real-artifact-roundtrip.log`: fixed pack/restore passes for both actual
  profiles, all restored bytes match, the three fixture executables run, line
  tables remain present, and the restored original symbol archiver accepts all
  three runtime bundles using the release workflow's invocation pattern.
- `python-discovery.log`: Python **3.12.14**, full discovery with the supplied
  `970-winiss/1-impl/native-payload`, **72 tests, zero skips, PASS** on macOS.
  The tiny Python artifact fixture pins hashed names, hyphen-to-underscore
  conversion, relocation preservation, missing DWARF rejection, UUID mismatch,
  absent UUIDs and tool failures without compiling Rust.
- `local-gates.json`, `yaml-parse.log`: `actionlint -shellcheck=`, changed workflow
  YAML parsing, unsafe counts (**production=189, test=21**), release-evidence
  regressions and whitespace validation passed. Candidate identity and the
  separate Astra verdict are recorded alongside the lane evidence.

Required CI confirmation remains with the release owner after local gates and
integration: **`ship-gate` → `shared macOS release build (aarch64)`** on the next
wave head must pack/upload successfully, and its client/daemon footprint jobs
must restore that artifact. **`windows-installer-check` → `Additional installers
(x86_64-pc-windows-msvc)` → `Packaging regression tests`** on `ci-winonly-*` must
pass under native Windows Python 3.12, followed by the installer lifecycle steps.
The new POSIX harness coverage runs in **`xplat-check` → Linux check → `pipeline
regression tests (POSIX Bash driver)`**. macOS execution and source review do not
claim these new native CI results, signing/notarization, or thin-LTO performance
acceptance. This repair commits only on `lane-970-cifast-fix` and does not push.

## Registry #80 and proof before acceptance

No new performance threshold is introduced. Existing required daemon/render/
probe gates remain required. The client budget measurement remains advisory
(its comment requires three green main measurements before promotion), while
client setup, download, manifest validation and evidence upload are required.
The actual measurement outcome is recorded in the job summary. A job-wide
waiver must not mask artifact integrity or setup failures.
The shared producer, integrity checks and verdict are necessary build/evidence
plumbing; a broken build or corrupt artifact is never waived as advisory.
The new standalone Windows rehearsal is **advisory to release until its first
native CI green**, and the existing xplat installer compile check retains its
advisory status. Do not add either as a release dependency before first green.
Successful reuse-only main runs do not count as new client calibration runs;
promoting that existing policy requires actual measurement evidence.

Local shell/Python checks prove resolver behavior, tar/manifest integrity,
partition coverage and failure propagation. They cannot prove GitHub scheduling,
cross-run download permissions, native Windows runtime, signing/notarization or
performance. Required remote proof plan (release owner records actual run IDs):

| Deliverable | Exact proof workflow/ref and acceptance |
| --- | --- |
| 1. Reuse | `ship-gate.yml` on final `wave-970` head H completes success; tag at H triggers `release.yml`: lookup true, reusable call skipped, all five build legs start. An isolated pre-tag dispatch with no successful gate exercises lookup false and fallback; never manually dispatch release. CLI mutation tests cover API error and rejected evidence. |
| 2. Shared build | Same ship-gate run at H uploads manifest/artifact; both footprint logs restore it and contain no release compile. `release.yml` at tag H downloads that run's artifact, verifies identity and skips arm64 compile; existing symbols/sign/notary/package/smoke complete. Expired-artifact fallback is proven by a later authorized tag candidate lacking that artifact, or a separately reviewed nonpublishing harness; never delete candidate evidence to test it. |
| 3. Parallelism | `ship-gate.yml` at H: shared producer, probes and render begin while evidence waits; terminal verdict checks every dependency. Main at H performs reuse only, with no new measurements. Record start/end timestamps and both run IDs. |
| 4. Main duplication | Main fast-forward at H creates no ci/xplat push runs; `require-evidence.sh H` still finds successful wave runs. A fresh direct-main candidate, if one occurs, must dispatch exact-SHA missing runs. |
| 5. Installer rehearsal | Push final H to `ci-winonly-970-cifast` after local gates: one Windows build plus complete Additional installers/lifecycle green. After workflow is on default branch, dispatch that exact ref with its completed run ID: build skipped, same-SHA distribution verified, same installer checks green. Record both IDs. |
| 6. Shards | `xplat.yml` on final `wave-970` H: all three Windows test shards green, their logs partition all 19 crates, one explicit clipboard/native/thinexe leg. One successful xplat-check run satisfies require-evidence. Record each shard's wall time and total runner minutes. |
| 7. LTO experiment | Compare `ship-gate.yml` on `wave-970-cifast-fat` at the pre-experiment commit with final thin candidate H, whose crates tree is identical. Both client (every surface/sample), daemon budgets and render must actually pass, including advisory client step outcomes. Each unsigned runtime binary must grow **less than 10%** versus its fat baseline. Record manifests, raw measurements, runner/toolchain identity, run IDs and timings. Revert only the final experiment commit if any condition fails or proof is missing; rerun exact-head gates on the reverted candidate. |
| 8. Docs/tests | Final local evidence records changed workflow YAML parsing, structural comparison to lane base, actionlint including inherited findings, full Python discovery with native payload, unsafe counts and require-evidence regression exit codes. Separate Astra verification is bound to the final commit/tree. |

These are planned runs, not fabricated CI evidence. This lane makes no pushes,
no tags and no release dispatches. The owner must run applicable platform gates
on the integrated candidate before the first CI-triggering push.

## Expected wall times, not measured results

Without the final LTO experiment, wave-to-pre-tag is approximately the maximum of
ci/xplat and the existing ~80-minute arm64 footprint path, instead of their sum.
The tag path removes another ~80-minute gate and the ~66-minute arm64 compile;
Intel's ~70-minute build remains the likely pole. Budget **85–100 minutes from tag
to publish** and **165–190 minutes from wave push** before queue variance. These
ranges assume successful artifact reuse, unchanged workload and no reruns.

Three Windows test shards target a reduction from the supplied **53-minute**
Windows test job toward **25–35 minutes**, subject to cold sibling builds and
uneven crate execution time. Shards may increase total runner minutes; record
both wall time and total usage. Do not advertise a measured speedup until CI.

The macOS thin-LTO experiment's expected deltas and revert rule are recorded in
the final commit below; it must remain independently revertible.

### Final experiment: macOS thin LTO / 16 codegen units

The final commit exports `CARGO_PROFILE_RELEASE_LTO=thin` and
`CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16` in `macos-release-env.sh`. This affects
only the shared arm64 macOS producer and the macOS release compile steps
(arm64 fallback and Intel); manifest verification sources it to enforce identical
settings. Windows keeps its existing overrides. Linux, Android, Cargo.toml,
probe ladder and the render benchmark's release profile are unchanged. The
render benchmark must still pass independently; it remains a fat-LTO control,
not a measurement of the thin-LTO runtime binary. The footprint jobs measure the
actual thin-LTO payload. Codesign/notarization/compat/package steps are unchanged.

**Hypothesis, not a result:** thin LTO and 16 codegen units may reduce the supplied
61–66 minute arm64 and ~70 minute Intel compilation times by **30–60%**, with a
small binary-size increase. Plan for roughly **45–65 minutes** of pre-tag
ship-gate work and **45–70 minutes** from tag to publish if those compile savings
materialize. Render dependency reuse may be partial because its profile differs.
Runner queues, cold caches, sibling builds and ci/xplat can dominate these ranges.
Record the actual compile, measurement, signing and publish durations separately.

Keep this commit only after both footprint gates (including advisory client
samples) and render pass on CI, and **each** unsigned `haider`, `haider-tui` and
`haiderd` size is strictly less than 1.10 times its corresponding fat-LTO size.
Compare manifests from the fat parent and thin candidate with an identical
`crates/` tree and Rust version; compare raw unsigned binaries, not compressed
archives or signed files. Missing/failed measurements or growth at/above 10%
means revert **this single final commit**, then revalidate the resulting exact
candidate SHA. The optimization is an experiment awaiting that evidence, not an
advisory waiver for a failed existing budget.
