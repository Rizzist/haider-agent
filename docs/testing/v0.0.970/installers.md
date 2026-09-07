# v0.0.970 installer lane

## Release run 4 repair — lane 970-winiss

This update supersedes the historical Windows inspection-only verdict below.
Candidate `71c4f45e130e13a197b1020b9b35c71542bdbd51` (fourth `v0.0.970`
tag) reached Windows installer compilation in release run `34094765449`.
The `Build sign and inspect Windows installer` step failed at `windows.iss:35`:
`Unknown type 'HKEY'`. The preceding absent-signing-input SKIP was expected;
ISCC aborted before the native lifecycle gate. Nothing was published in that run.
Earlier Python source scans never compiled the Pascal Script.

The repair uses `Integer` for the external registry root, matching Inno Setup 6.
The live [support reference](https://jrsoftware.org/ishelp/topic_scriptfunctions.htm)
now contains newer signatures; the versioned
[Inno 6.7.3 reference source](https://github.com/jrsoftware/issrc/blob/is-6_7_3/ISHelp/isxfunc.xml)
and [DLL examples](https://github.com/jrsoftware/issrc/blob/is-6_7_3/Examples/CodeDll.iss)
were used to check compatibility. `Cardinal` DWORDs, `var` outputs, Unicode
strings, `stdcall`, and the null data pointer remain appropriate for Inno 6's
32-bit Setup/Uninstall. `RegGetValueW` remains necessary: the built-in string
query accepts both string types, the DWORD query reads only DWORD data, and
value existence does not distinguish `REG_SZ` from `REG_EXPAND_SZ`.

The type query now checks its status and rejects unexpected types before PATH
mutation; a missing PATH still defaults to `REG_EXPAND_SZ`. The original type
is read once for saving/writing, and a dead assignment is removed. See the
[DLL reference](https://jrsoftware.org/ishelp/topic_scriptdll.htm) and
[RegGetValueW contract](https://learn.microsoft.com/en-us/windows/win32/api/winreg/nf-winreg-reggetvaluew).
An additional audited edge case is fixed: restoring a saved `REG_SZ` when
the current value is `REG_EXPAND_SZ` first removes that value, because
[RegWriteStringValue preserves an existing expandable type](https://jrsoftware.org/ishelp/topic_isxfunc_regwritestringvalue.htm).
The owner key, ARP configuration, PATH token cleanup order, and silent versus
interactive state-removal behavior retain their contracts.

`xplat-check` now runs `Windows installer compile check (advisory)` immediately
after checkout in `check (x86_64-pc-windows-msvc)` only. It installs Inno Setup
6 if absent with the release workflow's Chocolatey snippet and invokes
`packaging/installers/windows-compile-check.ps1`. Per registry #80 the step
has `continue-on-error: true` until its first green CI run, after which it must
be made required. The release owner must inspect this individual step's actual
result on the new candidate before retagging; a green advisory job alone is
not compiler evidence.

The compile script creates disposable `haider.exe`, `haiderd.exe`, and
`haider-tui.exe` stubs in a checksummed ZIP, calls `installer_payload.prepare`
to produce the release-format manifest, and uses the same hash-validating
`scripts/windows_installer_inputs.py` renderer as `windows-build.ps1`.
It runs [ISCC `/Qp`](https://jrsoftware.org/ishelp/topic_compilercmdline.htm)
on the real template, throws on preparation/compilation errors, and removes
all temporary output. It neither signs nor executes the stubs/installer,
runs no lifecycle gate, and produces no retained release assets.
Python tests exercise this real renderer, exact generated members/definitions,
and rejection of altered bytes, bad manifest hashes, coordinates and members.

Local repair validation passed all **53 Python tests** using Python 3.12.14
and `HAIDER_INSTALL_TEST_BIN_DIR` pointing to the checksum-verified macOS ARM64
split payload from run `34094765449`, artifact `10013247348`, at the exact
`71c4f45e` candidate. The initial Python 3.9 run failed an existing use of
`Path.write_text(newline=...)` and lacked the required native sibling directory;
no test was skipped or changed to bypass those requirements. YAML parsing
passed with PyYAML 6.0.3 in a temporary venv. The new fixture/render CLI was
also exercised directly, including a nonzero hash-mismatch refusal before
any compiler inputs were written. Full command/exit-code logs are in the
lane's external evidence directory, `state/evidence/970-winiss/1-impl`.

The native Windows install → upgrade → uninstall gate still runs **only in the
release installers job**, through `windows-build.ps1`, before signature checks,
sidecars and asset upload. It is unchanged and required. This Mac has no
Windows, Wine, ISCC or PowerShell; local renderer/test evidence is not native
compiler or lifecycle evidence. This repair is **pending Windows CI and
independent verification, not SHIP**. No commit, push or tag is made by this lane.

## Scope and source audit

Base and live fetched wave: `5468dec1fd4d7b09c2f704d5d408c3f9e19a4374`.
The workspace still declares 0.0.969. Installer names and metadata take the
actual workspace/tag version; this lane does not bump the release version.

Read the supplied common/installer briefs and turnperf/turnperf2 evidence as
historical context; they are excluded from delivery. The brief's `install.sh`
reference resolves to `scripts/install.sh`. `docs/install.md` did not exist and
is added. `thinexe.md` is absent in this worktree; read the matching document
from sibling `lane-970-thinexe` as held, unmerged context. That implementation
names its new member `haider-tui` and retains compatibility archives beside
`-split` archives. The installer selector therefore prefers a unique complete
split bundle if present, otherwise the current canonical bundle. It rejects
ambiguity instead of guessing.

The two-binary claim is correct for current Windows/macOS. Linux already ships
`haider-wayland-portal` as a third member. `installer_payload.py` verifies the
archive sidecar and target, rejects unsafe/duplicate/link members, requires
`haider` and `haiderd`, and discovers every `haider-*` sibling (Windows `.exe`).
An optional `--members` exact list supports an explicit release-member contract.
Each builder consumes the frozen name→SHA256 manifest; none recompiles or
re-signs payload binaries. Workflow `haider --version` checks actual source
binary version before packaging.

The release calls reusable `ship-gate`; relgate's exact-SHA ci/xplat evidence
job lives in `ship-gate.yml`, not directly in `release.yml`. It is unchanged.
The new installer matrix needs the existing archive build; publish needs both.
All native post-pack and lifecycle failures block additional-asset upload and
publication. Existing package-manager source files and install.sh are untouched.
The chocofix pattern is applied to actual packed output, not just templates.

## Assets, uninstall proof and signing

Every final EXE/PKG/DMG/DEB/RPM and standalone uninstaller has a SHA256 sidecar.
Checksums are written after native verification and any signing/stapling.

| Platform | Asset and path | Post-pack and uninstall proof | Signing status / execution |
| --- | --- | --- | --- |
| Windows x64 | `haider-vV-x86_64-pc-windows-msvc-setup.exe`; `%LOCALAPPDATA%\Programs\Haider` | Inno installs explicit manifest members; gate executes a synthetic older-metadata installer, upgrades to final EXE, compares all binary hashes/member names, embedded coordinates and ARP version, then uninstalls. It asserts directory/ARP/owner-key removal, exact raw user PATH value/type restoration and unchanged `.haider` sentinel. PATH cleanup precedes Inno's final environment-change broadcast. Interactive removal asks about state; silent retains it. | PFX-gated setup and embedded-uninstaller signing; clear SKIP when absent. **By inspection only locally; Windows CI is authoritative and has not been run by this lane.** |
| macOS ARM64/Intel | `haider-vV-TARGET.pkg` inside matching `.dmg` (PKG also separate); `/usr/local/bin`, manifest `/usr/local/share/haider/installer-manifest.json` | Native pkg expansion checks identifier/version/install location, exact members, each hash, all executable bits, no scripts. Final DMG mounts read-only and its PKG must equal the verified PKG. Local opt-in lifecycle requires older and new PKGs, checks receipt versions and final hashes, runs shipped uninstaller, verifies all paths/receipt disappear and state remains. Uninstaller removes supported Haider launch-agent files, manifest and receipt, and asks before state deletion. | Real unsigned synthetic PKG built/extracted/verified locally. No signing credentials used; SKIP paths executed. **DMG creation blocked by host (`Device not configured`); signing, notarization, mounted-DMG check and privileged lifecycle remain unexecuted.** |
| Linux x64/ARM64 | `haider-vV-TARGET.deb` and `.rpm`; `/usr/bin`, `/usr/share/haider/manifest.json` | dpkg-deb/rpm metadata query and native extraction check exact members, hashes, modes, version/arch and no state hooks. dpkg-shlibdeps derives Debian dependencies; RPM derives ELF requirements and disables stripping. Native Ubuntu/Fedora containers test exact read-only final packages: synthetic older metadata → real version, runtime `haider --version`, remove and direct Debian purge, no package files/metadata, unchanged root and ordinary-user state sentinels. | No package signing requested. **Native Linux packing/lifecycle inspected, not executed here: Docker daemon socket unavailable.** |
| POSIX tarballs | Additional `haider-vV-TARGET-uninstall-haider.sh` | Generated exact member/hash list; changed or symlinked binaries fail before any removal. `--prefix` selects the installation bin directory. Unrelated files stay; noninteractive input or `--keep-state` retains user state. | Executed in temporary macOS prefixes using synthetic bytes. Release tarballs themselves are unchanged. |

Clean lifecycle proof covers stopped applications. Before upgrade/uninstall,
stop each active profile with `haider daemon stop` and close clients. Package
managers deliberately have no hooks that enumerate or mutate user profiles;
this lane does not claim that removing a running Unix executable stops its
already-running process. No real user's state or installed Haider was touched.
Synthetic older-version fixtures use current release bytes with lower package
metadata; they prove native version transitions, not historical runtime/schema
migration. Existing T1 store-upgrade tests cover the separate store contract.

## Owner signing inputs

- Windows: `WINDOWS_SIGNING_PFX_BASE64`, `WINDOWS_SIGNING_PFX_PASSWORD`;
  code-signing identity/private key in PFX. Inno and Windows SDK SignTool sign
  installer/uninstaller with SHA256 and RFC3161 timestamp. Payload bytes stay
  identical to the ZIP. Absent credentials skip, configured errors fail.
- macOS Application binaries retain existing `MACOS_SIGNING_CERT_P12_BASE64`
  and `MACOS_SIGNING_CERT_PASSWORD`. Missing inputs now explicitly skip instead
  of aborting the archive build, enabling the requested unsigned fallback.
- PKG needs **Developer ID Installer**, distinct from Application. Set
  `MACOS_INSTALLER_CERT_P12_BASE64` / `MACOS_INSTALLER_CERT_PASSWORD`, or include
  Installer identity in the existing bundle. Application-only fallback emits
  a SKIP, not a false signing claim. See [Apple's distribution reference](https://help.apple.com/xcode/mac/current/en.lproj/deve51ce7c3d.html).
- Existing `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID` notarize/staple the signed
  PKG, then DMG. Missing credentials/signature skip clearly; rejected submissions
  fail. Sidecars cover final stapled bytes. Successful use of owner credentials
  remains unproved locally.

Windows PATH ordering follows Inno's [installation/uninstallation order](https://jrsoftware.org/ishelp/topic_installorder.htm)
and its `ChangesEnvironment` notification. Linux files use `/usr/bin`, consistent
with [Debian's restriction on package-owned `/usr/local` files](https://www.debian.org/doc/debian-policy/ch-opersys.html).

## Reproduction

```sh
python3 scripts/installer_payload.py --archives /path/to/downloaded-archives \
  --output /tmp/haider-payload --version 0.0.970 --target aarch64-apple-darwin
bash packaging/installers/macos-build.sh /tmp/haider-payload /tmp/haider-assets \
  0.0.970 aarch64-apple-darwin
# Disposable macOS account/runner only: refuses existing files/receipt/state.
HAIDER_INSTALLER_QA_ALLOW_SYSTEM=1 bash scripts/qa-gate/installers-macos.sh \
  /path/older.pkg /path/new.pkg /tmp/haider-payload

# Native Linux runner with dpkg-dev, rpm, cpio, ALSA/GLib runtime libraries:
python3 packaging/installers/linux-build.py --payload /tmp/linux-payload \
  --output /tmp/linux-assets --version 0.0.970 --target x86_64-unknown-linux-gnu
bash scripts/qa-gate/installers-linux.sh /tmp/linux-payload /tmp/linux-assets \
  0.0.970 x86_64-unknown-linux-gnu
python3 -m unittest discover -s scripts/tests -p 'test_*.py'
```

Windows CI calls `windows-build.ps1 -Payload payload -Manifest
payload/manifest.json -Version V -Target x86_64-pc-windows-msvc -Output installers`.
The builder invokes `scripts/qa-gate/installers-windows.ps1` before writing its
sidecar. Inno Setup is installed when absent. No workflow was dispatched and no
release was published from this lane.

## Verification and delivery

- Packaging: **25 tests pass**. Archive SHA/name/target/member/unsafe-path gates,
  future split selection, Linux/macOS packed hash/version/membership/mode mutations,
  Windows environment-notification ordering, and POSIX removal/state retention.
  POSIX permissions are explicitly modeled for synthetic fixtures on all hosts,
  so Windows executes those tests without pretending Windows chmod sets POSIX
  execute bits. The two shell execution cases run on POSIX; native Windows
  removal is exercised by its PowerShell CI gate.
- Shell syntax, Python compilation, release/ship-gate YAML parsing and
  `git diff --check`: pass. `actionlint` unavailable.
- Existing Homebrew/Scoop/Chocolatey/npm/install.sh byte-unchanged check: pass.
- Unsafe count guard: pass, production 189 / tests 20.
- Fresh siblings under ENV LAW: pass; haiderd 202,987,424 bytes (>10 MiB).
- `cargo test -q --workspace --no-fail-fast`: **PASS (exit 0)** under all ENV
  LAW variables plus `HAIDER_TEST_SIBLINGS_PREBUILT=1`: 5,546 summed passes,
  zero failures, 13 unchanged existing ignores across 345 libtest results.
  The existing large-row rendering benchmark completed without modification.
- `cargo clippy --workspace --tests -- -D warnings`: **PASS (exit 0)**,
  5m02s, with ENV LAW and fresh prebuilt siblings.
- `cargo run -q -p xtask -- test-count --update` and subsequent `test-count`:
  **PASS**, baseline **5,093 → 5,093**. Python tests are counted separately.
- Named provider-request golden regenerated through `UPDATE_FIXTURES=1` using
  the just-built test executable: one pass, file byte-unchanged. The named
  instruct-pipe test passed at **6,244 → 6,244 bytes**, full catalog 20,770.
  Both had also passed in the full workspace run; no golden was hand-merged.

Local logs and the allowed delivery bundle live under `tmp/installers/`.
`native-macos.log` is explicitly a reconstructed command/result record, not a
verbatim retained console log; the synthetic unsigned fixture PKG is retained
beside it and is **not** a release asset. `native-macos-final.log` is the
verbatim final synthetic PKG build/extraction check using the final hash-protected
uninstaller and an additional `haider-tui` fixture member; it passes. The separate
DMG device limitation was not retried.

The shared worktree Git metadata refuses FETCH_HEAD/ORIG_HEAD/index writes.
A writable shared clone at `/private/tmp/haider-installers-merge` fetched the
actual origin, confirmed identical base/wave SHA, and ran `git merge --no-commit
origin/wave-970` (“Already up to date”). No source merge or golden conflict exists.
The lane-only commit/bundle excludes supplied briefs/turnperf data and all
build binaries, with no trailer and no push. A second live fetch before delivery
again returned the same SHA; merge was already up to date.

## CI error-registry walk

Read registry through #106 (including duplicate numbers). #19/#20/#85–88:
packaging tests, full workspace tests, Clippy with `--tests`, test-count and
merged byte/golden checks; no Rust tests weakened. #21/#41/#64/#74/#81: ENV LAW,
short temporary fixture roots, fresh siblings and daemon size recorded.
#31/#32/#70/#80: no tag, push or duplicate dispatch; new installer gates are
strict because the owner explicitly requires post-pack gating before upload,
while their unexecuted native status remains visible. #44/#89/#91/#99/#100:
sandbox metadata limitation handled by writable checkout and explicit bundle;
base equality verified, no saved files overwrite a later merge. #77: unsafe
counts unchanged and pass. #92/#94/#95/#96/#98/#105/#106: disk checked before
Cargo, no shared-cache deletion or broad process manipulation, failures retained.
#94 deadline/#95 keepalive: no application transport wait or new nested runtime
deadline introduced. #97: explicit source-only delivery list, no binary/evidence
sweep. #101: delivery verdict follows command exit codes. #102: no benchmark or
real profile daemon spawned by installer checks. #103/#104: unrelated runtime
race classes remain governed by full-gate evidence, not package-source edits.
All other registry classes were inspected against this packaging-only scope;
no product/protocol/OAuth/Android/UI/tool-schema code changes were introduced.

## Independent verifier value

Four actionable verifier observations were corrected: CI discovery omitted
six macOS package tests; POSIX chmod-based fixtures would fail on Windows (now
explicitly modeled on every host, plus POSIX path normalization); Windows PATH
cleanup ran after Inno's environment notification (moved before it, with a
regression pin); and the source archive target needed an explicit filename check
(now validated and mutation-tested). Follow-up reviews confirm install-time
notification order and signed-uninstaller `.msg` handling.

One additional live-daemon teardown concern is classified as noise for this
cold lifecycle gate: install/upgrade/uninstall requires stopped applications,
which is now explicit in docs. The package-manager no-user-state-hooks rule is
preserved; no claim is made that Unix unlink terminates a live process. This
changed documentation only, not code, tests or the verdict.

## Historical verdict (superseded by release run 4 repair status above)

SHIP for the lane implementation and source bundle: all required local gates
pass and independent code reviews return SHIP by inspection. Native Windows/Linux
CI and macOS DMG/signing/privileged lifecycle evidence remain explicitly
unexecuted locally as recorded above; publication is blocked by the configured native package CI
gates until those execute successfully. The privileged macOS lifecycle is an
opt-in local check, not a release-job dependency. Missing signing secrets alone do not fail
the release. Nothing was pushed or published.

VERIFIER: findings=5 real=4 noise=1 — included omitted macOS tests; modeled POSIX fixture modes portably; fixed uninstall PATH notification ordering; validated source archive target
SHIP
