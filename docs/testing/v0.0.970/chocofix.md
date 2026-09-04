# chocofix — fail-closed release package pins

## Disposition

The Chocolatey release step now renders explicit placeholders, hashes the
download artifact itself, packs the rendered tree, and inspects the resulting
`.nupkg` before `choco push` can run. A missing/duplicate placeholder, stale
version, stale tag, stale filename, stale checksum, malformed package, or stale
icon URL exits nonzero before publication.

## Claim audit and root cause

The brief's behavioral claims were correct; only its line numbers drifted.

- The old `Pack and push` step derived the version from `GITHUB_REF_NAME`, built
  the Windows artifact URL from that version, and used `Get-FileHash` on the
  downloaded artifact before editing the copied package tree. Those inputs were
  correct; the unchecked edits were not.
- The nuspec edit used the unanchored pattern
  `'<version>[^<]+</version>'`. The moderator-observed `0.0.935` nuspec is
  consistent with that replacement succeeding.
- `$version`, `$url64`, and `$checksum64` in `chocolateyinstall.ps1`, plus the
  checksum line in `VERIFICATION.txt`, used multiline patterns ending in `$`.
  None checked its match count. The VERIFICATION release-tag edit and nuspec
  edit were unanchored.
- Before this change, `.gitattributes` forced LF for `*.json` and a few named
  source/fixture paths, but not Chocolatey's nuspec, PowerShell, or text files.
  The [GitHub Windows image installs Git for Windows silently without a
  `CRLFOption` override](https://github.com/actions/runner-images/blob/9b5b6780d9becdb8ab6a810fbd88cb8136185f1e/images/windows/scripts/build/Install-Git.ps1#L25-L38),
  while the [Git for Windows installer defaults that choice to
  `CRLFAlways`](https://github.com/git-for-windows/build-extra/blob/21d6ed86c38d28e89c950c9e1e349b6edefd1afb/installer/install.iss#L2368-L2389)
  (`core.autocrlf=true`). Thus these package files were eligible for CRLF
  checkout on `windows-latest`.

The regex behavior was reproduced with PowerShell 7.6.5/.NET against otherwise
identical LF and CRLF strings. The reported columns are the old anchored install
pattern, anchored VERIFICATION SHA pattern, unanchored tag pattern, and
unanchored nuspec pattern:

```text
LF:   install,sha,tag,nuspec=1,1,1,1
CRLF: install,sha,tag,nuspec=0,0,1,1
```

In .NET multiline mode, `$` matched before `\n` but not before the preceding
`\r`, exactly reproducing the split outcome reported by moderation. The rejected
job's checkout bytes/log are not retained in this worktree, so that particular
run cannot be forensically reread; however, the runner configuration, regex
reproduction, source patterns, and observed two-success/four-failure split all
agree. No differing cause was found.

## Implementation

`scripts/release_packaging.py` is the shared rendering and verification
authority.

Chocolatey source files now contain one explicit token for each independently
required field. Rendering copies the template, performs byte-for-byte token
replacement, requires each token to occur exactly once, names the file/field/
token and observed count on failure, and rejects any unresolved Haider token.
Because it never splits or regex-matches lines, it behaves identically for LF
and CRLF and preserves the source line endings.

Both render and final-gate commands validate the expected artifact filename and
calculate SHA-256 directly from
`haider-v<version>-x86_64-pc-windows-msvc.zip`. The post-pack gate opens the
exact `haider.<version>.nupkg` and verifies:

- the nuspec `<version>`;
- the tag-pinned jsDelivr `iconUrl`;
- install-script `$version`;
- install-script `$url64`, including both its `v<version>` tag segment and exact
  archive filename;
- install-script `$checksum64`;
- the VERIFICATION release-tag URL; and
- the VERIFICATION SHA-256 line.

`choco push` is sequenced after this gate. Even though the Chocolatey publishing
job remains best-effort for absent/expired credentials, no failed gate can reach
the push command.

`.gitattributes` now applies `text eol=lf` to `packaging/**`. A later rule keeps
`packaging/**/*.png` binary with no EOL attribute, protecting the separately
produced icon. Before the change, `git grep` found no existing test or golden
that referenced packaging byte shapes or required CRLF. After the change,
`git ls-files --eol packaging` reports LF in the index/worktree and `eol=lf` for
every tracked packaging text file.

## Sibling packager audit

| Packager | Stale-pin exposure | Result |
|---|---|---|
| Homebrew | The prior release job rewrote fixed versions, URLs, and SHA-256 values. It counted regex matches, but its URL/SHA pair regex embedded `\n` and was line-ending-sensitive. | Replaced by line-by-line, EOL-neutral, unique-match rewrites. A post-write gate checks the version and all four exact artifact URLs/checksums before either repository can be updated. |
| Scoop | The prior job structurally assigned the version, URL, checksum, and extraction directory in parsed JSON, so it did not have the Chocolatey regex failure. It lacked a final consistency gate. | Required keys are now asserted before assignment and every final value is checked against the tag and real Windows artifact SHA before commit. |
| npm | It has no fixed artifact checksum: `install.js` derives every URL/filename from the package's own version and verifies the downloaded release `.sha256` sidecar. The release still published a directory immediately after `npm version`. | The job now creates a tarball first, checks the packed `package.json` version, rejects static release URL/filename pins in packed `install.js`, and publishes that exact verified tarball. |
| winget | The checked-in `0.0.934` manifests are manual first-submission material. `release.yml` never reads, rewrites, packs, commits, or submits them; later updates are explicitly performed with `wingetcreate update`. | Not affected by the automated stale-shipment path. It remains a manual operator responsibility and was deliberately not made part of this release job. |

## Verification

- `python3 -m unittest scripts/tests/test_release_packaging.py` — 6/6 pass.
  The suite renders every Chocolatey field from both LF and CRLF fixtures,
  proves a missing token fails with its file/field/count, verifies a valid
  archive, proves a deliberately stale install URL fails the post-pack gate,
  exercises Homebrew/Scoop generation plus a stale Scoop mutation, and checks
  npm's packed metadata/dynamic URLs.
- An actual `npm pack --ignore-scripts` archive from `packaging/npm` passed the
  new npm archive gate.
- `cargo test --workspace --locked` — pass for all unit, integration, and
  doc-test targets under the environment law, with prebuilt sibling binaries.
- `cargo clippy --workspace --tests --locked -- -D warnings` — pass under the
  environment law.
- `cargo run -p xtask --locked -- test-count --update`, followed by
  `test-count` — baseline updated/verified at 4,748. The value is unchanged
  because the added tests are Python rather than Rust; no Rust test was removed
  or weakened.
- Python compilation, workflow YAML parsing, `git diff --check`, and the
  packaging LF/attribute audits pass.
- `bash scripts/check-unsafe-counts.sh` reports the inherited unrelated
  `haider-tui category=test` mismatch (baseline 0, actual 4).
  `git diff --quiet -- crates/haider-tui` passes and this lane adds no unsafe
  code, so the unrelated reviewed baseline was not rewritten.

Chocolatey CLI is Windows-only and unavailable on this macOS worker, so a real
local `choco pack` was not possible. The CI-callable tests construct nupkg ZIPs
with Chocolatey's relevant layout and exercise the same archive reader; the
release's `windows-latest` step remains the authoritative real pack execution,
with the verifier immediately after it. The regression suite is wired into the
macOS repository-guard job and the Windows cross-platform check job.

## What remains manual

`packaging/chocolatey/icon.png` is intentionally absent because the owner is
producing the Arabic-calligraphy artwork separately. It **must exist at that
exact path in the tagged tree before the next release** or the tag-pinned
jsDelivr `iconUrl` will return 404. The renderer and final package gate validate
the URL string, not CDN availability.

Chocolatey moderation and credential maintenance remain operational/manual.
Winget submission remains manual as documented above. No credential was read,
printed, or changed, and no tag or push was performed.

## CI error-registry delta

- #19: Python compilation, six focused regressions, workflow YAML parsing,
  workspace tests/Clippy, and `git diff --check` cover the changed surface.
- #20: the Rust ledger remains honestly verified at 4,748; six Python tests are
  separately named and CI-wired.
- #41/#74: every test uses short `TemporaryDirectory` roots and synthetic data;
  it reads no home/profile or credentials.
- #64/#71/#72/#73: runtime binaries, product execution paths, discovery, and
  fixed-byte product windows are unchanged.
- #77: this lane adds no unsafe code and workspace/tests Clippy passes. The
  standalone unsafe-count guard has the inherited `haider-tui` test-category
  0-to-4 mismatch; `haider-tui` is byte-untouched here, so its baseline was not
  changed by this lane.
- #94/#95/#96: no deadline, negotiated-connection wait, or turn-performance law
  changed.

## Verdict

The implementation gates support SHIP, but this managed workspace permits only
read access to the linked worktree's shared `.git` directory. The required
scoped `git add` failed while creating `index.lock`, and the native Terminal
bridge was unavailable. The working-tree changes therefore remain uncommitted;
the lane is not complete until they are committed without the supplied
LANE/turnperf evidence.

NO_SHIP
