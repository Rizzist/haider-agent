# Lane chocofix — the Chocolatey release job must never ship a stale URL/checksum again (v0.0.970, gpt-5.6 xhigh, RELEASE BLOCKER)
Worktree lane-970-chocofix (from origin/wave-970). A Chocolatey moderator rejected package haider 0.0.935: the nuspec said
`<version>0.0.935</version>` but tools/chocolateyinstall.ps1 still carried `$version = '0.0.934'`, the v0.0.934 download URL and the
v0.0.934 checksum, so installing 0.0.935 delivered the 0.0.934 binary. Moderator also asked for an `iconUrl` served from a CDN.
CLAIM-AUDIT FIRST (verify each before fixing; correct me where I am wrong):
- .github/workflows/release.yml (Chocolatey "Pack and push" step, ~lines 729-760) derives `$Version` from the tag, builds `$Url` from it,
  computes `$Sha` with Get-FileHash over the built artifact, then string-replaces into the copied packaging tree.
- The nuspec replace uses an UNANCHORED pattern (`'<version>[^<]+</version>'`) and it WORKED (moderator saw 0.0.935).
- The three chocolateyinstall.ps1 replaces and the VERIFICATION sha replace use `(?m)^…$` ANCHORED patterns and they did NOT work.
  PRIMARY HYPOTHESIS: on the Windows runner the checked-out file has CRLF line endings (git `core.autocrlf` is true by default on the
  hosted Windows image) while .gitattributes (repo root) forces LF only for *.json and a handful of named paths — packaging/** is NOT
  covered. In .NET regex, `(?m)$` matches immediately before `\n`, so a trailing `\r` defeats every `…''$` anchor while the unanchored
  nuspec pattern still matches. The VERIFICATION tag-URL replace is unanchored (`'releases/tag/v[^\s]+'`) which is why THAT line updated.
  VERIFY this hypothesis (e.g. reproduce the .NET/PowerShell regex behaviour against a CRLF fixture) before adopting it, and report if the
  real cause differs.
Deliver:
1. Make the substitution correct regardless of line endings AND fail loudly instead of silently skipping: every replace must assert it
   changed something (count matches; if a required substitution matched 0 times, fail the job with the file and pattern named). Prefer
   removing the fragility rather than patching regexes — e.g. generate the install script/VERIFICATION from a template with explicit
   placeholders, or parse-and-rewrite line by line — your call, but state why.
2. Add `packaging/** text eol=lf` (or the narrower correct set) to .gitattributes so the packaged files are LF everywhere, and confirm no
   existing golden/test depends on packaging CRLF.
3. Post-pack VERIFICATION GATE, the part that actually prevents recurrence: after `choco pack`, unpack the produced .nupkg and assert that
   the nuspec version, `$version`, the `$url64` tag segment and filename, the VERIFICATION tag URL and the VERIFICATION sha ALL equal the
   tag version and the artifact's real SHA-256; any mismatch fails the release job. Do the same for the sibling packagers that share this
   pattern (winget/scoop/homebrew/npm manifests in packaging/) — audit each for the identical stale-pin hazard and fix or explicitly state
   it is not affected.
4. `iconUrl`: add it to the nuspec pointing at a jsDelivr CDN URL pinned to the release tag, of the form
   `https://cdn.jsdelivr.net/gh/Rizzist/haider-agent@v<version>/packaging/chocolatey/icon.png`, with the tag substituted by the same
   templating as everything else (and covered by the gate in item 3). The icon FILE ITSELF is being produced separately by the owner
   (Arabic calligraphy "haider"); commit a placeholder path reference only if the file is absent, and say clearly in your report that the
   PNG must exist at that path in the tagged tree before the next release or the icon URL 404s.
Tests: a unit/integration test (or a CI-callable script test) that runs the substitution logic over BOTH LF and CRLF fixtures and proves
every field is rewritten; a test for the post-pack gate that it FAILS on a deliberately stale pin; existing packaging tests green. Run
`cargo test --workspace` and `cargo clippy --workspace --tests -- -D warnings` with the ENV LAW, update test-baseline.txt via the repo's
test-count tool. Do not push, do not tag, do not touch any credential. Write docs/testing/v0.0.970/chocofix.md (root cause, what changed,
what the gate catches, what remains manual). Commit on the lane branch, no co-author trailer. LAST line: SHIP or NO_SHIP.
