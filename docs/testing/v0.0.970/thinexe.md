# thinexe — thin control executable and on-demand interactive payload

Branch `lane-970-thinexe`; workspace version `0.0.970`. Continuation 3 fixes the installer watchdog mechanism; the current evidence and verdict are in the final section below. Earlier failed runs remain historical evidence and are not relabeled as passes.
The orchestrator measured linger client peak RSS reductions of 21% maximum
and 26% median, TTL=0 parity and equal wall time. The earlier measured release
CLI shrank 34.9%, while its installed runtime total grew 15.8%. Historical
public-installation, auto-spawn-upgrade and watchdog-provenance limitations
remain recorded below. This continuation reruns the requested merged gates;
the orchestrator owns staging and committing the existing merge.
The common rules, brief, `turnperf/`, and `turnperf2/` inputs were read first and
are unchanged and excluded from the held delivery.

## Implementation

`haider-cli` now builds a thin `haider` binary and a shared control library.
`haider-tui-exe` builds the sibling `haider-tui`. The CLI's normal dependency
graph excludes TUI, STT, image decoding, core/store/tools/provider/accounts.
Store/provider remain test-only fixture dependencies. `haider-client` retains
its wire/RPC and file-attachment support; no image decoder is introduced there.
Existing PDF text extraction and SQLite-backed offline export remain in the
thin binary; the excluded store dependency is the `haider-store` crate.

One typed grammar in `haider-cli/src/routing.rs` serves both executables. The
thin process selects interactive commands before runtime or profile creation.
Unix uses `exec`, retaining PID, argv0, arguments, environment, signals and
standard streams. Windows spawns with inherited console/streams and returns
the full child exit code, then applies the existing Explorer failure hold.
The payload preserves the two-worker Tokio runtime and existing TUI functions.

| Commands | Execution boundary |
| --- | --- |
| Bare invocation, bare TUI flags, `tui`, `talk`, non-JSON `resume` | Payload |
| `ssh shell <host>` | Payload, selected through the existing SSH parser |
| SSH administration and `ssh shell <host> -- <command>` | Thin |
| `run`, status/sessions/daemon/provider/account/session verbs, agent/workflow, JSONL, other controls | Thin |
| `resume ... --json`, merged `session retract` | Thin |
| `--version` / `-V` / `version` | Local thin path, before runtime, lock or payload access |
| `self-test` | Thin validates payload identity as bounded file data, then executes daemon offline diagnostic |
| Internal `--install-bundle SOURCE DEST` | Shared verified bundle installation transaction |

The payload contains a contiguous versioned build marker. The verifier constructs
its search bytes at runtime so the thin CLI and daemon cannot accidentally pass
as payloads. Interactive lookup takes the updater's shared OS lock through exec;
all members are checked during installation. Missing/mismatched payloads fail
before profile creation. The real fake-provider self-test moved into `haiderd`
and does not start a profile daemon or execute TUI code.

## Projection completeness (registry #76)

No wire or JSONL schema was changed by the split. Existing subcommand parsers,
wire requests, serializers, error mappings and output functions stay authoritative.

| Moved seam | Authority and completeness protection |
| --- | --- |
| Dispatch | Typed exhaustive `Command` match; all headless variants audited; process-level interactive/headless routing tests |
| Agent/workflow validation | Protocol `spawn_subagent`; tools explicitly project every validated field with full-field/error tests |
| Notification/masking | Pure client functions, TUI reexports; same output helpers used by run/export/session-item |
| Fleet metrics/display | Pure client functions, TUI reexports and existing display tests |
| Trace ordinal | One protocol implementation, core reexport and CLI caller |
| Profile lock probe | Client OS-lock probe; read-only probe test preserves owner diagnostics |
| SSH terminal | Existing parser/connection flow; injected terminal callback supplies payload-only drawing |
| Automatic TUI update | Bridge moved to payload; neutral discovery/transaction/restart remain shared |
| Self-test report | Same real fake-provider exercise in daemon; `link:tui` backed by payload identity |

## Brief citation audit

| Cited construct | Audit against recovered source |
| --- | --- |
| R2-08 `haider-cli/Cargo.toml:13`, broad direct links | Correct at baseline 14750e31: dependencies at 13–29 included core/store/provider/tools/accounts/TUI. The candidate normal dependency list starts at 16 and excludes those layers. |
| R2-08 `haider-tui/Cargo.toml:9`, STT/image dependencies | Line drift: 9 is now the features table. Dependencies are at 17 onward, including STT at 22 and image rendering at 32–33. Mechanism remains correct. |
| R2-08 `main.rs:154`, dedicated headless branch | Line drift: baseline invokes `run_command` at 174. Candidate `main.rs` delegates to the library, with typed selection in `routing.rs` and headless execution in `lib.rs`. |
| C1/X1/X3 4 ms executable floor | Historical measured proxy only; baseline version dispatch constructed a full runtime, so it was not isolated loader cost. The candidate version path precedes runtime creation. |
| Brief 2.4/3.0 MB wire-only calibration | Wrong as accepted calibration: the underlying 969 report labels these rejected diagnostics. They are not used to establish a candidate win. |

## Packaging and upgrade migration

New installers use `haider-vVERSION-TARGET-split` archives. Release build,
signing, notarization, archive verification and SHA-256 sidecars include both
client executables and daemon; Linux retains its Wayland companion. Homebrew,
Scoop, npm and Chocolatey select split assets for new releases. Historical pinned
URLs/hashes remain historical until the release repinner writes actual new hashes.

Shell, PowerShell, npm and Chocolatey invoke the staged thin executable's
`--install-bundle` helper. The helper shares updater verification, exclusive lock,
immutable staging, digest checks, backup/publication, rollback and crash recovery.
The required bundle publishes daemon/payload before the control executable;
all-member verification precedes finalization. Markers record member digests,
with recovery support for the previous two-member marker. Fresh installs and
migration from an install without a payload use the same transaction.

An already installed <=969 macOS updater rejects a third archive member. New
release workflows therefore also retain canonical two-file assets. Their
`haider` is a compatibility entry point embedding the signed thin/payload bytes
as data. Local version and staging self-test do not mutate the installation;
self-test verifies the complete embedded bundle. The next normal invocation
installs that exact bundle through the shared transaction and forwards argv.
The compatibility executable is signed after embedding, and notarized together
with the split binaries. Version 970 makes the real historical 969 SemVer gate
exercise a genuine upgrade.

Valid macOS signatures now survive staging byte-for-byte. Unsigned local
binaries can receive an ad-hoc signature, while a corrupt existing signature
is refused rather than replaced. Fresh Unix executables use 0755; replacements
preserve incumbent modes and a newly introduced payload inherits the CLI mode.
Private stage, marker and lock permissions remain owner-only. Automatic shell
installation selects a prefix that meets the transaction's ownership/mode
checks; an explicit ineligible prefix receives an actionable refusal. Pinned
pre-970 shell/PowerShell installs retain the original archive and two-file path.

Native Windows/Linux behavior is **by inspection** on this macOS arm64 host.
`xplat.yml` includes native Rust routing/transaction tests, native installer
execution tests and dependency/artifact boundary checks. Those jobs must execute
in CI; wiring is not a claim that they have run here. Windows retains existing
platform directory-sync/ownership limitations; no new power-loss guarantee is
claimed beyond the platform primitives actually used.

## Merge forward

Starting commit: `14750e312a0189caba3420a5ff845ae7aba50abf` (969).
The original worktree Git directory is read-only: fetch/merge metadata writes
were denied. A writable shared checkout at `target/thinexe-merge` fetched
`wave-970` to `b8635f88edc8c8a3f720b2f00adf026804666691`, merged forward without a
commit, and applied the lane patch with three-way conflict resolution. Incoming
source changes were copied back, preserving both sides. The incoming CLI retraction
route was projected into the shared grammar and help. Golden regeneration passed all four selected fixtures through the repository
tooling. At that merge the instruct-pipe pin remained 6,244 → 6,244 bytes
(6,166 invariant plus 78 POSIX bytes), and the source test count was 5,096.
`thinexe-evidence/isolated-{fetch,merge,apply}*` records the operations. The
orchestrator must record the merge in the original Git metadata.

The continuation fetched again and merged `229122b552e6c1583cf930b47923bb05e2ab0def`
in that writable checkout. Its three-file Chocolatey icon correction applied
cleanly to this working tree, preserving split-archive changes. The merged
packaging regression suite passed all nine tests; `merge-229122b5.log` and
`wave-229122b5.patch` retain the merge and copied delta. No Rust source changed
in this forward merge. During strict Clippy, the entire `target/` directory
disappeared: the compiler reported ENOENT for its temporary metadata directory,
and the debug siblings, release cache and first merge checkout were then absent.
The source and frozen `/private/tmp` artifacts survived. The cause is unknown;
no lane command requested this deletion. `target-disappearance.json` and
`workspace-clippy-1.log` preserve the observation. A replacement writable
checkout at `tmp/thinexe/merged-checkout` fetched and merged the same 229122b5
base, and a replacement gate uses `CARGO_TARGET_DIR=$PWD/tmp/thinexe/cargo-target`.

The last fetch advanced wave to `6c42fc02bccadce76aec55b388bfb55b90e7c5a7`
(Esc-to-retract TUI). Its source delta applied cleanly; the generated test-count
conflict was resolved through the repository tool at 5,109. The recovery
checkout now lives at `/private/tmp/thinexe-970-merged-checkout`: keeping a full
checkout inside `tmp/` caused the recursive test counter to include it twice,
so that transient 10,218 result was rejected and the checkout moved outside
the scan root before recounting. The merge is resolved in the writable checkout;
`wave-6c42fc02.patch` and `merge-6c42fc02.log` retain its source delta and merge.
The frozen 229122b5 artifact evidence is preserved with `pre-6c42fc02-` prefixes.
The final merged gate passes: 5,604 summed libtest passes, zero failures,
13 unchanged pre-existing ignores, strict workspace/tests Clippy clean, and
authoritative source count 5,109. All four regenerated goldens and the exact
6,244-byte instruct-pipe pin pass on this merged source.

## Measurement record

Baseline artifacts are frozen in `/private/tmp/thinexe-before`; source was the
starting 969 tree. Release build passed in 38m35s. Exact hashes are in
`thinexe-evidence/baseline.json`.

The following table compares the frozen baseline with the final **6c42fc02 +
thinexe** candidate in `/private/tmp/thinexe-after-final`. Its source manifest,
versions, file sizes and full SHA-256 hashes are in
`thinexe-evidence/release-final.json`. Earlier 229122b5 results remain in the
separate `pre-6c42fc02-release-final.json` record.

| Artifact | Before bytes | After bytes |
| --- | ---: | ---: |
| haider | 35,543,584 | 23,144,352 (−34.9%) |
| haiderd | 55,365,968 | 56,589,536 (+2.2%) |
| haider-tui | absent | 25,528,448 |
| Installed runtime total | 90,909,552 | 105,262,336 (+15.8%) |
| Legacy migration launcher | absent | 49,594,608 |

The before artifact predates the merged retraction lane; the final candidate
includes it. Comparisons must state both source identities rather than attributing
every change to this split alone. Historical 10.6 MB TUI footprint is distinct
from one-shot sampled client peak (11.3 MiB). The 2.425/3.015 MB values in the
969 report were rejected vmmap diagnostics, not accepted calibration samples.

`scripts/qa-gate/thinexe_abba.py` preserves ABBA × exec-floor/one-shot/warm/
conformance, existing sample counts and load <3. Exec floor uses direct-child
wait4 CPU/RSS. Existing harness process-tree CPU remains separate from additive
sampled own-client CPU, explicitly labeled a lower bound. Artifacts are hashed
before/after; failed/rejected peer reports stay failed/rejected.

The final runtime release build passed in 39m39s and the embedded compatibility
build passed in 14m06s. Rust sources and Cargo manifests remained unchanged
across both builds. The observed Rust/Cargo 1.95.0 arm64 macOS toolchain is
recorded in `thinexe-evidence/final-toolchain.json`. The CLI
shrinks, while the installed runtime total grows; the compatibility executable
is an additional legacy delivery mechanism, not part of the fresh three-member
installed total. These are local arm64 release executables with the repository's
normal release profile, not notarized/compressed release download sizes.

| Orchestrator paired measurement | Before | After | Result |
| --- | ---: | ---: | --- |
| Linger client peak RSS, maximum | 17.6 MiB | 13.9 MiB | −21% |
| Linger client peak RSS, median | 14.1 MiB | 10.5 MiB | −26% |
| TTL=0 client peak RSS | — | — | Parity |
| Wall time | — | — | Equal |
| CLI executable size (rounded, supplied units) | 34 MB | 22 MB | Smaller |

**MEASURED by the orchestrator:** release builds, ABBA, same machine, old
benchmark; source `tmp/thinexe-ab/`. These are the supplied Continuation 2
results, not a new local measurement. The raw directory is not present in this
worktree; sample counts, CPU values and separate `--version` RSS were not
supplied and are not inferred. The orchestrator accepted the RSS hold-out.

Earlier local attempts remain rejected evidence: the fixed ABBA command exited
75 at load 3.24755859375 above its unchanged 3.0 ceiling
(`thinexe-evidence/abba-final/abba.json`); the quiet retry hit EPERM during
process inventory (`abba-final-quiet-retry/abba.json`). The earlier load
4.52978515625 rejection is retained too. None is relabeled as an accepted pair.
The supplied orchestrator measurement supersedes the held, missing-RSS verdict.

```sh
export RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1
export HAIDER_TEST_DEVICE_NAME=test-mac CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
export CARGO_BUILD_JOBS=1
export CARGO_TARGET_DIR="$PWD/tmp/thinexe/cargo-target"
df -m /
cargo build --release --locked -p haider-cli -p haider-daemond -p haider-tui-exe
df -m /
HAIDER_THIN_EXE="$CARGO_TARGET_DIR/release/haider" HAIDER_TUI_EXE="$CARGO_TARGET_DIR/release/haider-tui" \
  cargo build --release --locked -p haider-compat --features embedded-bundle
python3 scripts/qa-gate/thinexe_abba.py boundary \
  --candidate /private/tmp/thinexe-after-final \
  --output docs/testing/v0.0.970/thinexe-evidence/boundary-final.json
python3 scripts/qa-gate/thinexe_abba.py measure \
  --baseline /private/tmp/thinexe-before --candidate /private/tmp/thinexe-after-final \
  --conformance-root /Users/rizzist/haider-run/bench-fix \
  --output-dir docs/testing/v0.0.970/thinexe-evidence/abba-final
# Same fixed command's quiet-host retry used --output-dir
# docs/testing/v0.0.970/thinexe-evidence/abba-final-quiet-retry.
```

Baseline build command and environment are retained verbatim in
`thinexe-evidence/baseline.json`. The fixed orchestrator owns warmup/sample
counts and ABBA order; the peer benchmark is unchanged.

## Verification

- Original baseline release build: PASS.
- Candidate normal targets: PASS (`check-3.log`).
- Candidate all affected Rust targets before merge: PASS (`check-all-targets-2.log`).
- ABBA orchestrator/turnperf Python tests: 28 PASS.
- Continuation Python installer/packaging/ABBA/legacy/turnperf regressions: 60 PASS;
  the nine packaging tests also pass after the 229122b5 forward merge.
- Final merged native debug siblings: PASS; `haiderd` is 202,614,624 bytes,
  exceeding the required 10 MiB. Source test-count is updated to 5,109.
- Normal-edge dependency proof over all targets: 127 crates, zero forbidden
  dependencies. Final native `nm`, `otool`, `objdump` and raw-byte/source-marker
  inspection also pass (`boundary-final.json`).
- Unsafe-count gate: PASS, production=189/test=20. Formatting/diff checks pass.
- Final merged runtime release and embedded compatibility builds: PASS, with
  stable source manifests and all four versions equal to 0.0.970.
- Regenerated JSONL goldens: 4 PASS. Instruct-pipe pin: 6,244 → 6,244, PASS.
- Native macOS final release installer suite: 5 PASS, 62.672 seconds. Fresh install,
  missing/wrong payload refusal, npm publication, and retained recovery marker
  execute actual final frozen binaries (`native-installer-final.log`).
- Official `t1.store.previous_release_upgrade`: PASS (v966 → v970, schema
  equality, preserved sessions, new completed turn and exact-daemon cleanup).
  Final-candidate results are in `t1-install-final/checks.json`.
- Official `t1.install.paths`: FAIL, installer curl exit56 / HTTP404 for the
  unpublished v970 split asset. The raw result remains FAIL; the local native
  fixture is separate evidence. Both rows retain orphan-daemon cleanup PASS.
- Final historical updater fixture (`legacy-upgrade-merged.json`): persistent
  v969 daemon → compatibility entrypoint → thin/payload migration PASS, with
  exact signed bytes, old PID exit, preserved restarted PID, matching versions,
  transaction cleanup and explicit final daemon-stop proof. Historical strict
  three-member archive refusal PASS. Auto-spawn v969 daemon case still FAIL at
  post-commit drain; overall FAIL is retained. Frozen inputs are unchanged.
  The prior final-candidate fixture remains in `legacy-upgrade-final.json`.
  Original and intermediate reference-oracle failures remain in
  `legacy-upgrade.json` and `legacy-upgrade-rerun.json`; failed scratch paths,
  markers, canonical hashes and log tails are retained. A canonical-basename
  codesign diagnostic explains the corrected exact-byte reference; no hash
  assertion was removed or relaxed. Final failed scratch is retained at
  `/private/tmp/htlu-jrpff3xb`, including recovery markers and cleanup diagnostics.
- Final merged full workspace gate: PASS (`workspace-tests-retry-3.log`),
  5,604 summed libtest passes, zero failures, 13 unchanged pre-existing ignores;
  elapsed 1,853.69 seconds. Strict `cargo clippy --workspace --tests -- -D warnings`
  PASS; authoritative test-count update PASS at 5,109. Exact command/env/disk
  records are in `gates-resumed.json`, with totals in `final-test-totals.json`.
- Earlier failures remain separate evidence: omitted optional-member match
  arms were repaired and supplemented with three native portal tests; Clippy
  ENOENT followed the unexplained target-directory loss. Test retry 2 exposed
  the missing payload warmup (2.103224s versus the unchanged 950ms limit) and
  the shared serde type-name regression. Both are repaired and pass the final
  full gate. No failed attempt is relabeled as passing.
- Final artifact audit: PASS, current Rust/Cargo source matches the final build,
  both frozen artifact sets remain unchanged after all probes, and only the TUI
  among the three runtime siblings contains the contiguous payload marker
  (`final-artifact-audit.json`).
- Historical local ABBA attempts: ENVIRONMENT-BLOCKED before any sample.
  Continuation 2 orchestrator ABBA above passes the RSS hold-out; the rejected
  local attempts remain separate evidence.

Exact validation commands (run from this worktree):

```sh
export RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1
export HAIDER_TEST_DEVICE_NAME=test-mac CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
export CARGO_BUILD_JOBS=1 RUST_TEST_THREADS=4
export CARGO_TARGET_DIR="$PWD/tmp/thinexe/cargo-target"
df -m /
cargo build --locked -p haider-cli -p haider-daemond -p haider-tui-exe -p haider-tools --bins
export HAIDER_TEST_SIBLINGS_PREBUILT=1
df -m /
cargo test -q --workspace --no-fail-fast
df -m /
cargo clippy --workspace --tests -- -D warnings
df -m /
cargo run -q -p xtask -- test-count --update
df -m /
UPDATE_FIXTURES=1 cargo test -q -p haider-cli --test turnhygiene_pin_tests golden -- --test-threads=1
df -m /
cargo test -q -p haider-daemon --lib instruct_pipe_shrinks_the_advertised_wire_pack -- --nocapture
HAIDER_INSTALL_TEST_BIN_DIR=/private/tmp/thinexe-after-final \
  python3 -m unittest -v scripts/tests/test_install_bundle_native.py
PYTHONPATH=scripts/qa-gate python3 -m unittest \
  scripts/tests/test_release_packaging.py scripts/tests/test_install_bundle.py \
  scripts/qa-gate/tests/test_thinexe_abba.py scripts/qa-gate/tests/test_thinexe_legacy_upgrade.py \
  scripts/qa-gate/tests/test_turnperf.py
python3 tmp/thinexe/run-install-t1.py
python3 scripts/qa-gate/thinexe_legacy_upgrade.py \
  --baseline /private/tmp/thinexe-before --candidate /private/tmp/thinexe-after-final \
  --compat /private/tmp/thinexe-after-final/haider-compat --version 0.0.970 \
  --output docs/testing/v0.0.970/thinexe-evidence/legacy-upgrade-merged.json
```

The golden and pin commands were rerun successfully on the final merged source
after fresh sibling builds and before the final full workspace gate.
The T1 driver invokes only `t1.install.paths` and
`t1.store.previous_release_upgrade` through `runner.execute_check`, retaining
mandatory cleanup and `measurement_accepted=false`; it does not change either
check's result or deadline. Exact per-command environment, disk and exit records
are in `gates-resumed.json`. The first ENOSPC and later ENOENT attempts remain
visible and do not count as completed gates.

## Independent review corrections

Unique findings accepted (duplicate lock finding counted once):

1. Launcher lock default permissions prevented future updates: set 0600 and
   validate regular-file/owner/mode, with compatibility test.
2. Inherited old TUI launch version broke staged new-payload version checks:
   make local payload version query precede the handoff guard, with regression test.
3. Waiting Windows parent prevented payload's Explorer hold from firing:
   retain final hold in the thin parent after child/headless completion.
4. Compatibility smoke failed to validate embedded thin executable: verify the
   entire extracted bundle before reporting staging self-test success.
5. Shared contiguous identity marker also identified thin/daemon as payloads:
   payload-only marker plus runtime search construction and actual-binary tests.
6. Mixed thin/daemon/payload versions could report successful self-test: validate
   payload against thin version before daemon's independent version validation.
7. Directory installer dropped Linux Wayland companion: optional transaction
   member integration and coverage completed; the actual companion now supports
   the installer's standalone `--version` probe without desktop initialization.

Continuation review corrections (unique observations, counting the shared
prefix finding once across reviewers):

8. Windows routing fixture assumed Unix argv0: assert the native payload argv0
   on Windows while preserving every user argument and exit-code assertion.
9. CI compiled compatibility tests but omitted their execution: include
   `haider-compat` in the per-crate test runner.
10. Historical pinned installer versions requested nonexistent split assets:
    restore pre-970 archive selection and two-member installation with checks.
11. Fresh public installations inherited private executable permissions:
    publish fresh binaries as 0755, preserve existing modes, and test migration.
12. Automatic prefix selection admitted directories refused by the transaction:
    align eligibility and cover fallback, retaining explicit-prefix trust checks.
13. Directory staging stripped valid native signatures: preserve verified bytes
    and refuse corrupt signed files, with a native macOS regression test.

14. CI self-test, TUI ladder/footprint and Android builds/packages omitted the
    required payload: build/stage the missing runtime siblings; Android retains
    its prior TUI compilation coverage. The native per-crate CI runner explicitly
    builds ordinary CLI/daemon/payload/tools executables before exporting
    prebuilt proof: `cargo test --no-run` alone can produce only a payload test
    harness. A failed sibling build clears inherited proof and is logged and
    aggregated. Native Windows/Linux/Android execution
    is still CI-only, not claimed as a local pass.
15. The live PTY probe copied only the old two-member bundle: require and copy
    the payload, and make ladder preflight reject incomplete bundles.
16. Native installer watchdog provenance was not derived: version probes now
    use shared `VERSION_QUERY`; the unchanged 120-second whole-fixture cap is
    explicitly identified as unproven under registry #94. Six version probes
    alone have a 180-second sum of existing allowances; no fictional arithmetic
    or cap relaxation is presented as a fix. This remains a separate hold-out
    issue even though the final five-test native suite passes in 62.672 seconds.

17. The legacy migration fixture conflated the 969 auto-spawn lifecycle with
    migration and used obsolete forced-signature expectations. It now keeps
    the historical failure, separately tests a persistent old daemon, always
    runs the strict-member negative case, uses correct signature expectations,
    and retains post-commit/recovery diagnostics. Frozen 969 source shows that
    launcher-idle demand followed by the updater SIGTERM escalates to forced
    shutdown; increasing TTL alone cannot repair that historical interaction.

18. The auto-spawn timing fixture warmed the CLI and daemon but omitted the
    newly executed payload inode. Warm `haider-tui --version` before its
    existing timer; retain the 950ms local and 10s CI limits and every runtime
    assertion. The observed 2.103224s failure stays in the original full-gate log.
19. The same fixture ignored the explicit prebuilt-sibling proof and launched
    nested Cargo builds on Linux or missing siblings. Require the existing
    `HAIDER_TEST_SIBLINGS_PREBUILT=1` convention and fail explicitly for missing
    daemon/payload files, matching the other subprocess fixtures; no fallback
    can replace a sibling during test setup.

20. Shared subagent serde validation exposed `SpawnSubagentArguments` in
    malformed-input errors despite the legacy rename attribute. Keep the actual
    derive authority named `SpawnSubagent` and re-export the unchanged public
    alias; this preserves scalar and sequence expectations (including the
    10-element wording), fields, validation and tool error class. The existing
    full-field/error-vocabulary regression is retained unchanged.

An earlier review rejected stopping at a migration-design report: the migration
is implementable engineering work. That changed the implementation/verdict path;
this report supersedes the earlier incomplete investigation draft. Counting that
earlier verdict-changing observation gives 21 unique accepted findings; the
other 20 code/test/verdict corrections are enumerated above. No independent
finding was rejected as noise. Duplicate observations are counted once.

## Held delivery

The original Git metadata is outside the writable sandbox. The requested
fallback is `tmp/thinexe/thinexe.bundle`, with a binary patch at
`tmp/thinexe/thinexe.patch` and exact base, held commit, changed paths and hashes
in `tmp/thinexe/bundle.json`. The writable recovery checkout at
`/private/tmp/thinexe-970-merged-checkout` records the resolved forward merge on
`lane-970-thinexe`; the bundle is verified against origin/wave-970 at 6c42fc02.
The commit has no trailer. Nothing was pushed, and this held branch is not a
performance acceptance or landing approval. Supplied lane/common and turnperf
inputs are excluded from both the commit and patch.

## CI registry walk

- #5/#7/#10/#19: platform imports and declared dependencies audited; formatting
  and strict workspace/tests Clippy pass. Native cross-platform execution remains
  CI-only; local macOS evidence does not establish a Windows/Linux pass.
- #20: recount through xtask after all added/moved/merged tests.
- #29/#41/#44/#64/#71/#72/#74: real prebuilt siblings, short throwaway profiles,
  hermetic HOME/discovery/device settings; no stub daemon or inferred binary gate.
- #52/#57: shared command grammar and moved TUI preserve the visible surface;
  `talk` is the requested new alias, and merged `session retract` stays headless.
- #76: projection ledger above; JSONL fixtures regenerated only with repo tooling.
- #77: no ignored tests, relaxed thresholds or platform gating to obtain green.
- #94: existing named command/cleanup budgets are reused where available, but
  the native installer fixture's 120-second watchdog lacks a valid derivation;
  this remains explicitly unresolved. #95: no additional negotiated RPC wait
  is introduced by pre-runtime handoff or install-directory helper.
- #96: no performance acceptance while compilation/load/proof restrictions fail.
- Remaining registry entries: final merged workspace gate and independent
  source review complete; no additional changed-surface issue found. Performance,
  public installation, historical auto-spawn upgrade and watchdog-provenance
  acceptance blockers remain explicitly recorded above.

## Merge with wave-970

Continuation 2 resolves the working files for held commit `59ce1bb8` merged
with `origin/wave-970` (`5468dec1`). Only file edits are made; the orchestrator
must stage the resolved files and commit the existing merge. Git's unmerged
index entry for `main.rs` is intentionally left for that staging step.

Source inspection found a brief/ref discrepancy: the actual
`git diff 6c42fc02 origin/wave-970 -- crates/haider-cli/src/main.rs` changes
only peer help wording; wave still contains its peer module and dispatcher.
The explicit continuation requirement of **no peer verbs** takes precedence.
The thin four-line `main.rs` is retained; peer module registration, command
enum/parser, dispatcher and usage advertisement are removed from the library.
The historical peer source files remain unreferenced; daemon/client peer RPC
behavior is outside this CLI removal and remains in the merged wave.

`agent`/`workflow` and `session_retract` were already ported in the held library.
Each keeps one parser/dispatcher path. Wave's module list, dispatch behavior,
constants and exit mappings were compared with the library and payload;
no other missing wave-side changes were found. The old main.rs line citations
in the turnperf lenses have drifted: current routing is in
`crates/haider-cli/src/routing.rs`, control runtime/dispatch is in
`crates/haider-cli/src/lib.rs`, and interactive implementation is in
`crates/haider-tui-exe/src/main.rs`. Their earlier image-size/timing estimates
are historical; the accepted table above is the performance evidence.

Added `removed_peer_verbs_are_unknown` and strengthened the observable-payload
integration loop: all five former peer verbs must return 2, report an unknown
command, omit peer help, and never execute the payload. Existing interactive
argv/exit-status and headless-family routing assertions remain intact.

Executed on this merged source under the complete requested ENV LAW, with
`CARGO_BUILD_JOBS=2` and
`CARGO_TARGET_DIR=/private/tmp/haider-thinexe-target`:

- Prebuilt CLI, daemon, payload and tools binaries: PASS. `haiderd` is
  203,268,512 bytes, above 10 MiB.
- `cargo test -q --workspace --no-fail-fast`: PASS, 5,617 summed libtest
  passes, zero failures, 13 unchanged pre-existing ignores.
- `cargo clippy --workspace --tests -- -D warnings`: PASS (4m11s).
- Explicit `payload_routing_tests`: 6 PASS; `routing::tests`: 4 PASS.
- Source test-count update: **5,134**. Conflict-marker scan empty;
  `git diff --check` clean.
- Python packaging, installer, ABBA, legacy-fixture and turnperf regressions:
  **60 PASS**. Native dependency/artifact boundary proof: PASS.
- Native installer suite against rebuilt debug siblings during test compilation:
  2 PASS, 3 errors (unchanged 120-second watchdog). The complete-archive cases
  timed out: direct fresh install, npm installation and npm recovery-marker
  refusal. Missing/wrong-payload cases passed. Direct CLI offline self-test
  separately passed. Serial retry without any Cargo process from this task:
  **3 PASS, 2 errors**, 421.148s. Fresh shell and npm installation again hit
  the unchanged 120-second watchdog; recovery-marker refusal now passed.
  Both runs are retained. No native release installer rerun on this merged
  source is claimed.

Exact script, logs, boundary proof and summary are in
`thinexe-evidence/merge-wave-970/`. The test-gate command script records the
requested environment and checks disk space before every build. The serial
native retry adds only timeout-output reporting, retaining the original test
methods, binaries, assertions and deadlines.

Independent verifier: source, exit mappings and routing/packaging coverage
inspected, zero findings. Windows/Linux execution remains by inspection in
this local macOS continuation. Prior public-release and historical-updater
fixture limitations above are retained as historical evidence; this merge
rerun does not claim those external fixtures were executed again.

The continuation's performance hold-out passes, and all required Rust gates
pass. Native packaging does not: two installation-success cases still time
out on the rebuilt debug siblings when run serially. Repeated hashing of the
larger debug bundle is plausible by inspection, but the timeout logs do not
prove the exact slow stage. No code defect is asserted from timing alone;
no deadline, assertion, ignore or platform gate was weakened. These persistent
native installer failures retain **NO_SHIP** for this continuation.

VERIFIER: findings=0 real=0 noise=0 — no findings
NO_SHIP


## Continuation 3 — watchdog mechanism and final merged gate

Read `LANE-COMMON.md`, `LANE-BRIEF-thinexe.md`, `turnperf/` and the round-2
lens tables first. Their historical `main.rs` citations still point to the
pre-split architecture; current routing/library/payload locations and the
citation audit above remain applicable. These supplied files are unchanged.

### Root cause and implementation

The 120-second value was a literal in
`scripts/tests/test_install_bundle_native.py`, not a shell/npm production
watchdog. The previous report's “serial retry” was a serial rerun of tests:
there was no member retry loop in the shared Rust transaction. Both wrappers
fetch one archive and one sidecar, rather than fetching each member separately.
Shell fetches had no wall limit, and npm requests had no wall limit either.

A baseline direct `--install-bundle` process completed successfully without
retries in **175.241192458 seconds**, progressing through private staging,
immutable files, canonical publication and transaction-marker removal.
Production-linked SHA-256 profiling measured **24.699 seconds** for one
three-member pass (daemon 13.601, payload 6.418, thin 4.680). This identifies
repeated debug hashing as the expensive stage. The seven-pass work estimate
is **172.893 seconds**, not a reconstruction of baseline wall time. Host work
and builds overlapped these diagnostic samples; the baseline directory was
not frozen atomically, and an earlier standalone SHA library calibration is
not substituted for the production-linked measurement. Exact diagnostic
provenance is in `continuation-3/watchdog-baseline.txt`.

Independent private member copies/hashes, quarantine/signature work, version
smokes, freeze operations and immutable checks now execute concurrently.
Every member finishes signature verification before any smoke can execute a
sibling. All workers join, including on error, before staging can be removed.
The ordered source manifest and serial durable publication sequence remain
unchanged. All existing SHA, mode/ownership, signature, exact version,
offline self-test, rollback and recovery checks remain authoritative.

`gate/install_budget.py` derives the harness `BudgetSum` from member count
and bytes. Let B be incoming bytes, O incumbent bytes and N member count:

- SHA allowance: `(7 * B + O) / (8 MiB/s)`. The seventh pass is finalization's
  canonical recheck; replacement also hashes the old members.
- Archive/read/extract/copy/durable-I/O allowance: `8 * B / (16 MiB/s)`.
- Staged/installed version probes: `2 * N * VERSION_QUERY`.
- Aggregate signature/quarantine allowance: `N * VERSION_QUERY`.
- One staged CLI offline self-test: `VERSION_QUERY`.

These are explicit conservative **harness resource allowances**, not claims
that each product phase has an enforced deadline or guaranteed disk service
rate. The 8 MiB/s SHA policy is below the measured production-linked 13–15
MiB/s range. Native fixtures account for their actual member sizes and any
old prefix. The static T1 declaration uses Rust's 256 MiB/member input capacity
and includes Linux's fourth Wayland companion. It does not assume a remote
release archive has the local debug executable sizes.

Shell archive and sidecar fetches now overlap; npm already overlapped them.
Each attempt gets `30s + ceil(resource_capacity / 1 MiB/s)`: archive
capacity 128 MiB gives 158s, checksum capacity 16 KiB gives 31s. Two attempts
yield a 316s maximum for concurrent archive/sidecar retrieval, separate from
member verification. Latest-release metadata has a 1 MiB capacity allowance;
pinned-version T1 does not fetch it. This byte derivation avoids imposing a
30s whole-download cap on legitimate large transfers. Redirects/body trickles
cannot reset an attempt deadline.
A failed shell attempt's partial bytes are discarded before retry. Wget gets
an outer wall timer because its timeout alone only bounds idle I/O. Npm owns
and destroys every redirect request/response on attempt completion or failure.
Both resources must succeed before checksum verification or extraction.
Native fixture expiry kills the exact owned wrapper/helper process group
(Windows tree cleanup by inspection), preventing a failed run's children
from contending with a subsequent serial retry.

### Repeated forward merge and packaging

The original worktree fetch was refused because its Git metadata is outside
the writable sandbox. A new successful `fetch origin wave-970` in the existing
writable temporary checkout still returned
`5468dec1fd4d7b09c2f704d5d408c3f9e19a4374`. A repeated `merge --no-commit`
there reconstructed the same merge; its three source conflicts were resolved
using the existing resolved working files. All other incoming files compare
byte-for-byte with the worktree. There is **no new incoming delta** and no
`packaging/installers/**` tree at that remote head. The existing shell,
PowerShell, npm, Homebrew/Scoop/Chocolatey member lists, Chocolatey uninstall,
and release post-pack verifier cover `haider-tui`. Historical Winget's pinned
0.0.934 two-member archive remains historical. `merge-forward.json`,
`merge-forward.log` and `merge-differences.json` retain this check.

Only file edits are left in the original worktree. Its existing unmerged
`main.rs` index entry requires the orchestrator's staging; no commit, checkout,
reset or stash was performed. No generated golden was hand-merged.

### Verification results

The final installer source is fixed and independently reviewed. Exact native
macOS debug-artifact timings (seconds, monotonic) are:

| Check | Result | Seconds |
| --- | --- | ---: |
| T1 `install.paths`, exact local candidate archive transport | PASS | 171.446399917 |
| T1 `previous_release_upgrade`, public pinned v966 → v970 | PASS | 35.947909291 |
| Native shell fresh prefix | PASS | 173.097302 |
| Native shell missing payload refuses/preserves incumbent | PASS | 6.167959 |
| Native shell wrong payload identity refuses before publication | PASS | 49.022647 |
| Native npm fresh vendor directory | PASS | 169.278014 |
| Native npm recovery marker refusal preserves all old bytes | PASS | 155.138027 |
| Complete final native suite, including archive setup/assertions | 5 PASS | 603.651 |

The final native fresh-install budget is **782.810596 seconds**, derived from
member sizes **69,006,688 / 95,949,344 / 203,235,680 bytes**. The incumbent
bytes contribute to refusal/replacement case budgets. Native and T1 installation
ran while workspace checks were active, so these are measured correctness-run
wall times, not an accepted isolated performance comparison. Final native
archive/member hashes are frozen in `native-final-artifact-hashes.json`.
Cargo's workspace feature-union build produced slightly different bytes from
the earlier prebuilt T1 archive; both are the same final Rust source.
`candidate-transport.json` separately identifies the exact T1 artifact.

The T1 candidate run invokes the official check unchanged, substituting only
an explicitly recorded transport for the exact candidate archive/sidecar URLs.
Checksum, shared helper verification, all three versions, daemon ready/status,
clean shutdown and PID disappearance assertions all execute normally. The
public v970 URL run remains **FAIL: HTTP 404**, **2.325045458 seconds**, with
`timed_out=false`: the split asset is unpublished. No public-download PASS is
claimed or fabricated. The historical prepublication limitation is separate
from the now-passing candidate installation mechanism. The previous-release
upgrade downloaded the pinned public v966 asset, verified its fixed SHA, kept
two sessions, completed a new turn, matched the upgraded schema to a fresh
profile and proved no orphan daemons.

Earlier measurements are retained separately: candidate T1 paths 142.986191333s;
native suite 5 PASS, 443.962s under the initial smaller 769.014416s allowance.
The final native suite reran all five cases after corrected budget accounting
and the byte-derived downloader policy, so the smaller-cap run is not presented
as the final-source gate.

Other completed checks: installer wrapper/budget regressions 17 PASS, 15.888s;
QA runner harness 36 PASS, 0.342s; independent packaging 9 PASS, 0.049s; regenerated
JSONL/request goldens 4 PASS; instruct-pipe 6,244 → 6,244 PASS; authoritative source
test-count 5,134 → 5,135; rebuilt daemon above 10 MiB. The new
`concurrent_staging_verifies_all_signatures_before_any_smoke` regression protects
the sibling signature barrier. Additional regressions cover stalled/trickling
redirect downloads, partial retry isolation, owned descendant timeout cleanup,
resource-budget scaling and Linux portal accounting. No test is newly ignored
or skipped on another platform.

The first full workspace attempt is retained: it ran alongside installer work
and failed the unchanged auto-spawn 950 ms assertion at 1.12148275s. No threshold
was relaxed and no CI environment escape hatch was enabled. The first attempt took **1,156.209567292 seconds**. The second (**540.327973708 seconds**), without
installer load, passed the auto-spawn suite but failed the pre-existing
`oauth::tests::device_flow_runner_continues_and_honors_slow_down_interval`:
its 2,000-iteration paused-clock real-I/O polling loop received no start
response. Both protected OAuth files remain unchanged; independent review
found no installer/staging dependency in that path. Its log is retained in
`quiet-final/`. The final full gate uses the same ENV LAW plus
`RUST_TEST_THREADS=2`, reducing competing independent libtest workers while
retaining every test, deadline, clock assertion and internal concurrency
scenario. This is resource control, not a threshold change. Final results on the identical source:

- `cargo test -q --workspace --no-fail-fast`: **PASS**, **694.906014792 seconds**;
  **5,618 summed libtest passes**, zero failures, **13 unchanged existing ignores**.
  This includes 5,606 unfiltered passes plus nested subprocess probes. The
  source counter is **5,135**, up one from 5,134. Earlier combined stdout can
  interleave nested result lines; `test-count-accounting.json` retains that
  audit rather than interpreting missing complete nested lines as removed tests.
- `cargo clippy --workspace --tests -- -D warnings`: **PASS**, **15.292936958 seconds**.
- Complete ENV LAW: `RUST_MIN_STACK=8388608`, `HAIDER_DISCOVERY_DISABLED=1`,
  `HAIDER_TEST_DEVICE_NAME=test-mac`, `CARGO_INCREMENTAL=0`,
  `CARGO_PROFILE_DEV_DEBUG=0`, `HAIDER_TEST_SIBLINGS_PREBUILT=1`,
  `CARGO_TARGET_DIR=/private/tmp/haider-thinexe-target`, `CARGO_BUILD_JOBS=2`,
  plus the explicit harness resource limit `RUST_TEST_THREADS=2`.
- Disk checked before every build/test/Clippy/count command; no check ran below
  the 700 MiB stop floor. Native daemon sizes in both recorded builds exceed
  10 MiB. Formatting and whitespace checks **PASS**.
- Final SHA-256 source manifest: **1,057 files**, **zero drift** across the final
  gate. Exact commands, environments, exit statuses, times and totals are in
  `limited-final/gates.json` and `summary.json`.
- A further pre-final-gate fetch still returned **5468dec1**;
  `fetch-before-quiet-final.log` retains it. No additional installer lane landed.
  Native Linux/Windows execution remains **by inspection** locally.

### Current verdict and independent verifier

**SHIP for the merged candidate implementation.** The prior native-watchdog
blocker is closed by the mechanism fix and final installer/T1 candidate gates;
the accepted earlier performance hold-out is unchanged. Public v970 asset
publication remains the orchestrator's release operation, not a fabricated
public-install pass. All files remain uncommitted, with the existing merge
index owned by the orchestrator.

Independent review accepted two findings and rejected none: the new npm
redirect chain initially retained an earlier response socket; it now tracks
and destroys every attempt resource, with a real loopback regression. The
initial budget omitted finalization's seventh hash pass and miscounted
self-tests; incoming/incumbent byte accounting and one staged self-test now
match the actual transaction. The reviewer also confirmed that limiting
independent libtest workers preserves all tests and internal concurrency.

Registry #94 is now addressed by the explicit resource sums and per-attempt
network bounds; #95 adds no negotiated RPC wait; #76/#77 retain every bundle
member and verification/recovery assertion; #20/#64 retain the source recount
and real sibling-binary floor. No protected OAuth file was edited.

VERIFIER: findings=2 real=2 noise=0 — fixed npm redirect socket cleanup with a regression; corrected hash-pass, incumbent-byte and self-test budget accounting
SHIP


## Round 4 — CI on ea0b9a6b

### Claim audit and fixes

Read the five requested GitHub job logs directly (`gh run view --job ID --log`):
ci 34027146069 / 101470031433; xplat-check 34027146045 /
101470060402, 101470060185, 101470060348, 101470060479. Local raw logs and
command/environment/exit records are retained under `/private/tmp/thinexe-round4/`.
The supplied lane rules and turnperf/turnperf2 evidence were consulted; this
round repairs the landed R2-08 packaging implementation, without new performance
claims or changes to durability boundaries. Those supplied files are excluded
from the commit.

| Claim | Audit | Change |
| --- | --- | --- |
| Windows compatibility E0282/E0283 | Correct, `crates/haider-compat/src/lib.rs:18`; Unix alone previously pinned the closure error through `Err(command.exec())`. | Explicit `-> io::Result<i32>` covers the probe and both process-launch cfg branches. |
| Windows `items_after_test_module` | Correct, `crates/haider-cli/src/lib.rs:432`; also repeated through integration tests importing lib.rs. | Move the intact runtime test module after the Windows console item; no lint allow. |
| Linux signature-barrier regression | Correct failure; citation drifted from src to `crates/haider-cli/tests/update_tests.rs:1934`. `stage_release` calls macOS-only `compiled_target()` before verification. | Apply the neighboring packaged-staging macOS cfg, explicitly authorized for this round. macOS test body and production signature barrier are unchanged. |
| Native-pipe generation race | Failure is real, but generation hypothesis is wrong: header and both coverage records have generation 1. Actual assertion is two coverage lines versus one, at `crates/haider-daemon/tests/session_hub_tests.rs:9770`. | Deterministic atomic-batch integration fixture plus direct 255/256 renderer regression, retaining exact generation and coverage assertions. |

The native writer consumes head-only watch notifications in
`session_hub/actor.rs` and calls `maintain` with an empty envelope slice.
Unread heads therefore enter `reconcile_from`, which emits a watermark for
each observed suffix. Separate fixture commits can let it reconcile at 256
and then 257, producing the exact CI output; coalescing all notifications
instead produces only 257. Shutdown drains the writer but cannot remove an
already valid watermark. This is a test scheduling assumption, not evidence
of a production generation defect.

`native_pipe_atomic_non_row_batch_settles_to_one_coverage_line` now commits
seed plus 256 deltas atomically and checks exactly one coverage record at 257,
generation 1, after shutdown. The direct renderer regression retains the name
`native_pipe_coalesces_255_non_rows_and_covers_the_256th`: real delta envelopes
prove no bytes at 255, exact pending/covered cursors, and exactly one 257/gen-1
record at 256. No sleeps, ignores, broader assertions, or production pipe
changes were introduced. Threshold +/-1 mutations are rejected by these
assertions by inspection.

### Merge and verification

The worktree's shared Git metadata is read-only. The initial fetch failed on
FETCH_HEAD, so an owned writable Git copy was created under the evidence
folder and used with this worktree. `git fetch origin wave-970` followed by
`git merge --no-commit origin/wave-970` succeeded there and reported already
up to date at `ea0b9a6b996c1ca8af0a372aa38ad781b8c90833`, before the full gate.
There were no incoming conflicts, golden changes, or instruct-pipe byte drift.
The final commit is delivered as a bundle rather than modifying shared Git
metadata; no push is performed.

Both requested Windows commands used the Rust 1.95.0 rustup bin directory,
not Homebrew Cargo. `cargo check -p haider-compat --target
x86_64-pc-windows-msvc` was blocked in blake3 by missing `ml64.exe`.
`cargo clippy -p haider-cli --all-targets --target x86_64-pc-windows-msvc
-- -D warnings` was blocked in aws-lc-sys by missing Windows SDK headers
(`windows.h`). Neither is reported as passing; Windows compatibility and
console-order fixes and Linux cfg behavior remain **by inspection** until CI.

Full gate results and independent verifier verdict follow below.


The dependency-light Windows follow-up passed: `cargo check -p haider-verify
-p haider-pdf --all-targets --target x86_64-pc-windows-msvc`. Its 526.56-second
wall time includes waiting behind the native prebuild's Cargo lock.
Native prebuild of haider, haiderd and haider-tui passed in 530.65 seconds;
haiderd is 203,365,264 bytes (>10 MiB).

The first full workspace test attempt completed in 1,835.02 seconds including
fresh test-target compilation: 5,618 summed libtest passes, one failure, and
13 unchanged ignores. Sole failure: unchanged
`parent_exit_leaves_the_daemon_running` at autospawn_tests.rs:1064 took
1.104804958 seconds against its 950 ms bound, the same failure class already
recorded in Round 3. All other targets passed, including the two coverage
regressions and macOS signature-barrier test. The 200,000-row shape-test group
passed in 421.96 seconds. No deadline, assertion, or test body was changed to
retry this gate. `workspace-test.log` retains the failed first attempt.


`cargo clippy --workspace --tests -- -D warnings` passed in 259.74 seconds.
`cargo run -q -p xtask -- test-count --update` passed and updated 5,135 → 5,136,
reflecting the additional direct-renderer regression. Formatting and whitespace
checks passed. A second pre-final-test fetch/merge-forward check again reported
already up to date at ea0b9a6b. The instruct-pipe pin remains 6,244 → 6,244
(platform-invariant component 6,166); its existing workspace regression passed.
No goldens required regeneration because no incoming or local prompt/tool
surface changed.

Every build/test/count command used a preflight `df -m /` and remained above
700 MiB free. Environment: `RUST_MIN_STACK=8388608`,
`HAIDER_DISCOVERY_DISABLED=1`, `HAIDER_TEST_DEVICE_NAME=test-mac`,
`CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0`,
`HAIDER_TEST_SIBLINGS_PREBUILT=1`,
`CARGO_TARGET_DIR=/private/tmp/haider-thinexe-target`, `CARGO_BUILD_JOBS=2`,
`RUST_TEST_THREADS=2`, with the requested Rust 1.95.0 rustup bin directory first
on PATH. The libtest worker limit preserves all internal concurrency tests and
is unchanged between attempts. No new ignores or deadline relaxations were introduced.

Registry walk: #19/#20 formatting, test-target Clippy and authoritative recount;
#64 actual prebuilt sibling sizes; #76/#77 unchanged signature-before-smoke and
bundle integrity assertions; #94 no added deadline; #95 no new negotiated wait;
#103 scheduling-sensitive native-pipe fixture replaced by atomic journal input
plus deterministic threshold assertions. The one necessary cross-lane edit is
test-only native-pipe coverage in daemon tests. Protected oauth.rs/oauth_tests.rs
and production session-hub/pipe code remain unchanged.

Independent verifier reviewed the final code, cfg branches, original CI logs,
and writer call path: **zero new findings**, code-review SHIP. Original user
claims and the audit correction to the generation hypothesis are not counted
as new verifier findings. A 797-file source/build-input hash manifest confirms
no code drift between the native gate attempts.


### Final Round 4 verdict

The final unchanged-tree `cargo test -q --workspace --no-fail-fast` passed
(exit 0) in **584.23 seconds**: **5,619 summed libtest passes**, **zero failures**,
**13 unchanged ignores**, including **5,607 unfiltered passes** plus nested subprocess
probes. Auto-spawn passed with its original 950 ms bound. Source manifest:
797 inputs, zero drift. Final test log and exit/environment record are
`workspace-test-final.log` and `workspace-test-final.json` under the local
evidence folder. Clippy and the 5,136 baseline are green as recorded above.

**SHIP for landing this CI repair candidate. Acceptance is only proven by
ci + xplat-check green on wave-970 after landing.** This local verdict does
not claim Windows execution or post-landing CI acceptance. Deliver the
no-trailer lane commit via `tmp/thinexe/thinexe-round4.bundle`; no push.

VERIFIER: findings=0 real=0 noise=0 — no new findings
SHIP


## Round 5 — launch lock

### Root cause and deterministic repair

The landing failure citation is correct: the prior exclusive acquisition was
at `crates/haider-cli/src/payload.rs:139`. This is a test lifetime assumption,
not a second open in `install_read_lock`. The helper opens exactly one file;
`OpenOptions`, `symlink_metadata`, and `File::metadata` retain no duplicate.
The test passes `tempfile::tempdir()/haider-tui` explicitly, so its lock is in
that unique temporary directory, never `target/debug/deps`. The payload test
is compiled into the library harness and seven integration harnesses importing
`../src/lib.rs`, once per module in each harness; their invocations have
independent temporary paths.

Parallel `update_restart_tests` call `CapturedDaemon::spawn` (line 454), then
`spawn_daemon_with_piped_stderr`; `configure_daemon` installs `pre_exec` at
`crates/haider-platform/src/spawn.rs:937`. Fork temporarily duplicates the
parent's open descriptors, including this unrelated shared lock, until the
child descriptor sweep or exec. CLOEXEC does not close descriptors at fork.
Consequently parent `drop(File)` is synchronous close, but is not necessarily
the last close and does not imply synchronous flock release. This mechanism
is reproduced locally with pipe-synchronized fork, without sleeps or retries:
parent close leaves exclusive acquisition blocked; child close releases it.
The historical failing process was not traced, so identification of its exact
fork interval is inferred from this call path and reproduced OS behavior.
See Apple's [flock documentation](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/flock.2.html)
and Rust's [File lock/unlock contract](https://doc.rust-lang.org/stable/std/fs/struct.File.html).

The same owner-only contract test now explicitly unlocks through the acquiring
descriptor before the exclusive attempt. It retains the regular-file and mode
assertions, adds a current-owner assertion, and proves an independently opened
updater descriptor receives `WouldBlock` while the launch lock is held. A
`try_clone()` stays alive across unlock, original descriptor drop, and successful
exclusive acquisition, deterministically modeling the inherited reference.
The updater lock is explicitly released too. Removing the shared-lock unlock
would leave that deliberate duplicate holding the lock on flock platforms.
No retries, sleeps, ignored tests, thread-count reductions, or production
launch/exec changes are introduced. Linux/Windows are by inspection locally.

Evidence is under `/private/tmp/thinexe-round5/`, including
`flock-fork-reproduction.py` and `.log`. The supplied lane rules and round-2
lens evidence were read and remain excluded from the commit. The earlier
R2-08 citation audit and performance claims are unchanged.

### Merge, gates, and delivery

The shared Git directory rejects FETCH_HEAD writes under the sandbox. A
writable Git copy uses this worktree and the lane branch; its successful
`fetch origin wave-970` and `merge --no-commit origin/wave-970` reported already
up to date at `ea0b9a6b996c1ca8af0a372aa38ad781b8c90833`, before the gates.
No incoming goldens or instruct-pipe inputs changed, so no regeneration or
byte repin was needed. Commit delivery uses the writable lane branch and a
bundle; the shared original ref cannot be advanced here. No push or trailer.

The sibling prebuild passed in **241.62 seconds**; binaries are haider
69,078,864 bytes, haiderd **203,365,264 bytes** (>10 MiB), and haider-tui
96,040,352 bytes. An initial command named the daemon library package rather
than `haider-daemond` and failed target selection before compilation; the
corrected prebuild is recorded separately.

The requested serial loop of `cargo test -q -p haider-cli --test
update_restart_tests` passed **30/30**, **83 tests each**, **2,490 passes**, zero
failures/ignores. Total command wall time was **2,379.88 seconds**, including
initial compilation and waiting for the sibling prebuild. Each invocation
overlapped successful `cargo build -p haider-cli --bin haider` work in the
separate `/private/tmp/haider-thinexe-round5-load-target` directory; that
separation prevents Cargo's target lock from serializing the intended load
away. The initial build was followed by a driver rebuilding the CLI package
until all iterations completed. After initial high host load, the load driver
used one Cargo job and one codegen unit; the tested target remained at two
build jobs and four test threads. All **393 background builds** passed, and
both background drivers exited. `stress-evidence.json` records each test
result and overlapping build intervals. The raw loop record's `load_running`
field refers only to the first background process at iteration end; it does
not include the continuing rebuild driver. No overlap claim relies on it.

All tested commands use Rust 1.95.0 with `RUST_MIN_STACK=8388608`,
`HAIDER_DISCOVERY_DISABLED=1`, `HAIDER_TEST_DEVICE_NAME=test-mac`,
`CARGO_INCREMENTAL=0`, `CARGO_PROFILE_DEV_DEBUG=0`,
`HAIDER_TEST_SIBLINGS_PREBUILT=1`,
`CARGO_TARGET_DIR=/private/tmp/haider-thinexe-target`, `CARGO_BUILD_JOBS=2`,
and **`RUST_TEST_THREADS=4`**. Every build/test/count command checks `df -m /`
and stops below 700 MiB. No test or production deadline was changed.

The first full workspace test gate completed in **1965.26 seconds**,
including fresh workspace test compilation: **5615 summed passes**,
**3 failures**, **13 unchanged ignores**. The failures were:

- `parent_exit_leaves_the_daemon_running`: 2.8298145 seconds versus its
  unchanged 950 ms deadline, the failure class already recorded in Round 4.
- `racing_launcher_never_owns_the_other_launchers_winner` and
  `closed_handshake_is_retried_only_after_a_spawnable_failure_authorizes_a_candidate`:
  their two-second candidate-marker observation deadlines expired. Both
  recorded their script's expected exit 75 by the outer five-second timeout.

The latter two are in the separate haider-client integration binary, which
does not compile the edited payload test. They use unique temp directories
and explicit shell-script candidates, not the real CLI/payload. Their marker
creation is the first shell operation, so the logs fit delayed startup under
contention; exact timing is inferred because fixture teardown removed their
temporary logs. Host load was around 32 near initial failures. No production
code, startup deadlines, protected OAuth files, or client tests were modified.
The first gate's failures remain recorded; it is not relabeled as a pass.

Final verification on unchanged source:

- `cargo clippy --workspace --tests -- -D warnings`: **PASS**, **267.61 seconds**.
- `cargo run -q -p xtask -- test-count --update`: **PASS**, **1.74 seconds**;
  authoritative baseline **5,136 → 5,136** (existing test strengthened, no new test).
- Complete `haider-client --test client_tests` recheck: **20 PASS**, plus one
  nested probe; **37.17 seconds** including compilation, **1.22 seconds** in libtest.
- Final `cargo test -q --workspace --no-fail-fast`: **PASS**, **611.94 seconds**,
  **5,618 summed libtest passes**, **5,607 unfiltered passes**,
  **zero failures**, **13 unchanged ignores**. All three startup failures
  from the first gate passed with the same environment and original assertions.
- **1,110 source/build inputs**, **zero drift** across the stress loop and gates.
  Formatting and whitespace checks passed. No platform gate or ignore was added.
- A refreshed successful fetch/merge immediately before the final run again
  reported already up to date at **ea0b9a6b**; `fetch-final.log` and
  `merge-final.log` retain it. Instruct-pipe remains **6,244 → 6,244** and its
  existing regression passed; no prompt/tool golden required regeneration.

Registry walk: #19/#20 retain formatting, test-target Clippy and authoritative
recount; #64 real prebuilt siblings exceed the daemon floor; #76/#77 preserve
owner-only lock and bundle verification contracts; #94 adds no deadline or
retry; #95 adds no negotiated wait; #103 replaces the fork-sensitive last-close
assumption with explicit OS unlock and a deliberately live duplicated descriptor.
Production payload execution and every protected/parallel-owned source file
remain unchanged. Independent code and final evidence review returned **zero
findings**. The no-trailer commit is on the writable copy's `lane-970-thinexe`
branch and delivered in `/private/tmp/thinexe-round5/thinexe-round5.bundle`;
the original shared lane ref remains at b078c582 because its metadata is
read-only here. No push was performed.

**SHIP for the launch-lock repair candidate**, with the first gate's unrelated
startup failures retained above and the final unchanged-tree gate green.
Native Linux/Windows execution remains by inspection until CI.

VERIFIER: findings=0 real=0 noise=0 — no findings
SHIP
