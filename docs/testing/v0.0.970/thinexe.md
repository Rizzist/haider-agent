# thinexe — thin control executable and on-demand interactive payload

Branch `lane-970-thinexe`; workspace version `0.0.970`. Work remains uncommitted.
The common rules, brief, `turnperf/`, and `turnperf2/` inputs were read first and
are unchanged. This is a working verification record; pending checks are not
passing evidence.

## Implementation

`haider-cli` now builds a thin `haider` binary and a shared control library.
`haider-tui-exe` builds the sibling `haider-tui`. The CLI's normal dependency
graph excludes TUI, STT, image decoding, core/store/tools/provider/accounts.
Store/provider remain test-only fixture dependencies. `haider-client` retains
its wire/RPC and file-attachment support; no image decoder is introduced there.

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
tooling. The instruct-pipe pin remains 6,244 → 6,244 bytes (6,166 invariant
plus 78 POSIX bytes), and the source test count is 5,096.
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

## Measurement record

Baseline artifacts are frozen in `/private/tmp/thinexe-before`; source was the
starting 969 tree. Release build passed in 38m35s. Exact hashes are in
`thinexe-evidence/baseline.json`.

| Artifact | Before bytes | After bytes |
| --- | ---: | ---: |
| haider | 35,543,584 | 23,144,352 (−34.9%) |
| haiderd | 55,365,968 | 56,589,536 (+2.2%) |
| haider-tui | absent | 25,511,904 |
| Installed runtime total | 90,909,552 | 105,245,792 (+15.8%) |
| Legacy migration launcher | absent | 49,578,096 |

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

The final release and embedded compatibility builds passed, with unchanged Rust
source hashes across both builds. Frozen candidate paths, versions, sizes and
SHA-256 hashes are recorded in `thinexe-evidence/release-final.json`. The CLI
shrinks, while the installed runtime total grows; the compatibility executable
is an additional legacy delivery mechanism, not part of the fresh three-member
installed total. These are local arm64 release executables with the repository's
normal release profile, not notarized/compressed release download sizes.

| Required paired measurement | Before → after | Accepted pairs | Status |
| --- | --- | ---: | --- |
| `--version` peak RSS and CPU/wall floor | unmeasured → unmeasured | 0 | ENVIRONMENT-BLOCKED |
| One-shot sampled client peak RSS/CPU | unmeasured → unmeasured | 0 | ENVIRONMENT-BLOCKED |
| Warm sampled client peak RSS/CPU | unmeasured → unmeasured | 0 | ENVIRONMENT-BLOCKED |
| Conformance client rows | unmeasured → unmeasured | 0 | ENVIRONMENT-BLOCKED |

The actual fixed ABBA command exited 75 before any sample: one-minute load
4.52978515625 exceeded the unchanged 3.0 limit. See
`thinexe-evidence/abba/abba.json` and `abba-driver.log`. Earlier `/bin/ps` proof
was denied with EPERM; no alternative bypass was used. No null value is treated
as zero and no historical sample is substituted for this missing pair. The
HOLD-OUT verdict is **NO_SHIP** without accepted same-machine RSS evidence.

```sh
export RUST_MIN_STACK=8388608 HAIDER_DISCOVERY_DISABLED=1
export HAIDER_TEST_DEVICE_NAME=test-mac CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
export CARGO_BUILD_JOBS=1
df -m /
cargo build --release --locked -p haider-cli -p haider-daemond -p haider-tui-exe
df -m /
HAIDER_THIN_EXE="$PWD/target/release/haider" HAIDER_TUI_EXE="$PWD/target/release/haider-tui" \
  cargo build --release --locked -p haider-compat --features embedded-bundle
python3 scripts/qa-gate/thinexe_abba.py boundary \
  --candidate /private/tmp/thinexe-after \
  --output docs/testing/v0.0.970/thinexe-evidence/boundary-final.json
python3 scripts/qa-gate/thinexe_abba.py measure \
  --baseline /private/tmp/thinexe-before --candidate /private/tmp/thinexe-after \
  --conformance-root /Users/rizzist/haider-run/bench-fix \
  --output-dir docs/testing/v0.0.970/thinexe-evidence/abba
```

Baseline build command and environment are retained verbatim in
`thinexe-evidence/baseline.json`. The fixed orchestrator owns warmup/sample
counts and ABBA order; the peer benchmark is unchanged.

## Verification

- Original baseline release build: PASS.
- Candidate normal targets: PASS (`check-3.log`).
- Candidate all affected Rust targets before merge: PASS (`check-all-targets-2.log`).
- ABBA orchestrator/turnperf Python tests: 28 PASS.
- Continuation Python installer/packaging/ABBA/legacy/turnperf regressions: 59 PASS;
  the nine packaging tests also pass after the 229122b5 forward merge.
- Continuation native debug siblings: PASS; `haiderd` is 202,614,752 bytes,
  exceeding the required 10 MiB. Source test-count is updated to 5,096.
- Normal-edge dependency proof over all targets: 127 crates, zero forbidden
  dependencies. Final native `nm`, `otool`, `objdump` and raw-byte/source-marker
  inspection also pass (`boundary-final.json`).
- Unsafe-count gate: PASS, production=189/test=20. Formatting/diff checks pass.
- Final merged release build and embedded compatibility build: PASS.
- Regenerated JSONL goldens: 4 PASS. Instruct-pipe pin: 6,244 → 6,244, PASS.
- Native macOS release installer suite: 5 PASS, 34.650 seconds. Fresh install,
  missing/wrong payload refusal, npm publication, and retained recovery marker
  execute actual frozen binaries (`native-installer-release.log`).
- Official `t1.store.previous_release_upgrade`: PASS (v966 → v970, schema
  equality, preserved sessions, new completed turn and exact-daemon cleanup).
- Official `t1.install.paths`: FAIL, installer curl exit56 / HTTP404 for the
  unpublished v970 split asset. The raw result remains FAIL; the local native
  fixture is separate evidence. Both rows retain orphan-daemon cleanup PASS.
- Expanded historical updater fixture: retained auto-spawn case FAIL at
  post-commit drain; strict three-member archive negative PASS. Persistent old
  daemon update exits 0, but its exact signed-reference assertion fails; this
  is not yet an accepted migration result. `legacy-upgrade-rerun.json` retains
  all three results and failed scratch diagnostics.
- First workspace retry: FAIL at compilation on two omitted optional-member
  match arms; repaired and supplemented with three native portal tests. First
  Clippy attempt: FAIL ENOENT after `target/` disappeared. The source recount
  passed at 5,096. Clean sibling/test/clippy rebuild is in progress under the
  replacement target directory; these attempts are not green evidence.
- Fixed ABBA attempt: ENVIRONMENT-BLOCKED before any sample; NO_SHIP.

## Independent review corrections

Unique findings accepted so far (duplicate lock finding counted once):

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
    its prior TUI compilation coverage. Native Windows/Linux/Android execution
    is still CI-only, not claimed as a local pass.
15. The live PTY probe copied only the old two-member bundle: require and copy
    the payload, and make ladder preflight reject incomplete bundles.
16. Native installer watchdog provenance was not derived: version probes now
    use shared `VERSION_QUERY`; the unchanged 120-second whole-fixture cap is
    explicitly identified as unproven under registry #94. Six version probes
    alone have a 180-second sum of existing allowances; no fictional arithmetic
    or cap relaxation is presented as a fix. This remains a separate hold-out
    issue even though the actual five-test native suite passes in 34.650 seconds.

17. The legacy migration fixture conflated the 969 auto-spawn lifecycle with
    migration and used obsolete forced-signature expectations. It now keeps
    the historical failure, separately tests a persistent old daemon, always
    runs the strict-member negative case, uses correct signature expectations,
    and retains post-commit/recovery diagnostics. Frozen 969 source shows that
    launcher-idle demand followed by the updater SIGTERM escalates to forced
    shutdown; increasing TTL alone cannot repair that historical interaction.

An earlier review rejected stopping at a migration-design report: the migration
is implementable engineering work. That changed the implementation/verdict path;
this report supersedes the earlier incomplete investigation draft. Counting that
earlier verdict-changing observation gives 18 unique accepted findings; the
other 17 code/test/verdict corrections are enumerated above. No independent
finding was rejected as noise. Duplicate observations are counted once.

## CI registry walk

- #5/#7/#10/#19: platform imports and declared dependencies audited; final
  formatting, all-targets checking and scoped clippy required.
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
- Remaining registry entries: final affected-crate gate and review pending;
  no blanket clean claim made before that evidence exists.

NO_SHIP
