# W9a — review of record #1 — SHIP

Reviewer: Fable 5. Branch `w9-headless`, lane commit b65b745 (+ the
reviewer's transport-integrity law). Implementer: codex lane (gpt-5.6
xhigh) per docs/briefs/W9a-update-brief.md.

## What shipped

`haider update [--check]`: prerelease-aware GitHub release listing
(never /releases/latest — every current release is a prerelease),
compiled-arch asset selection, strict SemVer gate (newer proceeds,
equal no-ops with zero mutation, older/malformed/mismatched refuses
before download); token via env → curl stdin (never argv/env of the
child, never logged); immutable same-filesystem staging (bounded .part
download → checksum parse tolerant of the workflow's `dist/NAME`
spelling → strict extraction rejecting traversal/links/devices/dupes/
extras/oversize → quarantine-only xattr removal → ad-hoc re-sign →
`--verify --strict` → mode-0500 admitted binaries → version + offline
self-test smoke); durable two-binary transaction (lock + phase marker,
hard-link backups of both, daemon-then-CLI rename(2), fsync,
installed-pair re-verify, every pre-restart failure restores the exact
old inode/bytes/mode pair, crash fixtures for every marker phase — never
a mixed pair); authenticated restart (kernel `peer_cred` PID captured
before `into_split`, exactly ONE SIGTERM, matching ServerDraining, real
profile-lock acquire/release as the finalization proof, child-retaining
spawn, health = Ready + identity + features + EXACT
`Welcome.daemon_version`, health failure → stop child, restore pair,
restart old sibling); `haiderd --version` side-effect-free. No daemon
running → none started. 29 new tests + peer-cred/version coverage.

## Mutations (reviewer-chosen, EXECUTED post-commit)

| # | Mutation | Result |
|---|---|---|
| U1 | SemVer gate admits downgrades | KILLED (refuse-before-download law) |
| U2 | archive-content digest comparison dropped | SURVIVED — ISOLATED: the fixtures covered checksum PARSING failures only; a well-formed checksum with a wrong content hash (the tampered-transport case) was UNPINNED. Reviewer added `wrong_content_digest_with_valid_checksum_refuses_before_extraction`; mutation re-run: KILLED |
| U3 | rollback skipped on commit failure | KILLED (2 fault-injection laws) |
| U4 | double SIGTERM in signal_authenticated_peer | INVALID mutation — back-to-back signals coalesce in the kernel (the daemon sees one). Re-targeted at the drain-TIMEOUT path (U4b: signal again on timeout): KILLED — the escaped second signal terminated the test binary itself (the fixture's spied PID is the test process), the most direct runtime kill of the run |

## Doctrine notes

- U2 repeats the equivalence-pin lesson in a new costume: refusal
  fixtures at the PARSE layer say nothing about the COMPARE layer.
  Transport integrity needed its own law.
- U4 adds a new entry: a mutation must be DELIVERABLE — kernel signal
  coalescing made the naive double-kill unobservable; the honest seam
  was the temporally separated second signal.

## Residuals (flag to owner, non-blocking)

- The repo is private: unauthenticated discovery fails until it goes
  public; `HAIDER_GITHUB_TOKEN`/`GITHUB_TOKEN` env is the documented
  interim (research risk 2).
- Checksums authenticate transport, not the publisher (shared trust
  domain) — signed manifests are future hardening.
- Update restarts only the resolved profile's daemon; other-profile
  daemons keep running old code (stated in output).

## Gate

gate38: full per-crate gate GREEN (fail=0) — cli 63, client 38, daemond 90, all 13 crates clean; workspace clippy -D warnings clean. Verdict: SHIP (v0.0.37). · ledger 1294.
