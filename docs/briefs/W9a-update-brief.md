# W9a — `haider update`: discovery, staged verification, transactional pair commit

AUTHORITY: docs/research/w9-updates-headless-research.md (read WHOLE,
first) — §Q1/§Q3/§Q4 W9a1+W9a2 bind.

## Scope (haider-cli/haider-client — NO haider-tui; no daemon changes
## beyond what §Q3 names)

1. **Discovery + gate (W9a1).** List published non-draft releases via
   the GitHub API (repo from CARGO_PKG_REPOSITORY), INCLUDING
   prereleases (never /releases/latest — every current release is a
   prerelease); parse v<semver>; pick the highest admissible with the
   exact `haider-vX.Y.Z-<target>.tar.xz` + `.sha256` pair; target from
   the RUNNING binary's compiled arch. Token source: `HAIDER_GITHUB_TOKEN`
   then `GITHUB_TOKEN` env (never logged, never persisted); absent →
   unauthenticated (the repo may go public). SemVer gate: newer →
   proceed; equal → successful no-op (zero network beyond listing, zero
   mutation); older/malformed/asset-mismatch → refuse before download.
2. **Immutable staging (W9a1).** Owner-only staging dir on the SAME
   filesystem as the install dir: bounded .part download → SHA-256
   verify (parse exactly one 64-hex digest; compare the referenced
   BASENAME — the workflow writes `dist/NAME`) → strict extraction
   (reject traversal/absolute/links/devices/dupes/extras/missing/
   oversized) → remove only `com.apple.quarantine` → ad-hoc
   `codesign --force --sign -` both staged binaries →
   `codesign --verify --strict` → staged `haider --version` smoke.
   NOTHING in this slice can touch canonical installed paths.
3. **Transactional commit (W9a2).** Update lock + durable phase marker
   in the install dir; validate current_exe() is the writable expected
   layout (refuse managed/read-only/symlinked); fsynced backups of BOTH
   installed binaries; rename staged haiderd then haider (same-fs
   rename(2), never unlink-first, never write a live executable); fsync
   dir; re-verify both canonical paths; any pre-restart failure →
   restore every touched path by rename. Crash recovery from the marker
   completes or restores — never a mixed pair.
4. **Drain + restart + health (W9a2).** Direct-connect WITHOUT
   auto-spawn; retain Welcome identity + kernel peer PID
   (tokio UnixStream::peer_cred, captured before into_split — small
   haider-client seam). No daemon running → never start one. After the
   pair commit only: exactly ONE SIGTERM to the authenticated PID
   (never the lock-file PID, never a second signal); observe
   ServerDraining + disconnect; prove finalization by acquiring and
   releasing the real OS profile lock (never reading its PID text);
   spawn the new sibling retaining the child; health = Ready + profile
   identity + features + `Welcome.daemon_version == target` exactly.
   Health failure → stop the child, restore both backups, restart the
   old sibling, report rollback. Print that active turns on this
   profile may be cancelled by drain; other-profile daemons are out of
   scope (say so).
5. **CLI arm**: `haider update [--check]` (--check = discovery+gate
   report only, zero mutation). Exit codes: 0 updated-or-current ·
   2 usage · 69 network/API unavailable · 74 I/O · 76 health/version
   mismatch after rollback · 70 internal.

## Laws

Standing lane laws (tests never inline; mutation docs with RUNTIME
failures; CARGO_INCREMENTAL=0; fmt + workspace clippy -D warnings; no
haider-tui; no Cargo.lock; no versions; leave uncommitted; no git).
The research's W9a1+W9a2 "Minimum laws" bind verbatim — including
fault-injection at EVERY commit boundary (backup1/backup2/rename1/
rename2/fsync/verify), the restart spy that must first read both live
paths as the new pair, and the never-mixed-pair crash fixtures. Network
tests: never assert error CODES on this box (TLS-intercepted network —
assert side effects); use local fixture servers.

Use up to 3 research subagents and 2 verify subagents. Print a final
summary of files changed and tests added.
