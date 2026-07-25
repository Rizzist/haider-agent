# Patch brief A3.1 — supervision harness fixes (NO_SHIP review round 1)

Read docs/briefs/A3-supervision.md (original spec) and docs/briefs/A3-review-1-NO_SHIP.md
(the full review — file:line refs). Apply ALL nine findings' fixes to
scripts/codex-supervised.sh and scripts/supervise-qualify.sh:

1. After leader exit, verify the process GROUP is empty before journaling done; sweep
   survivors (kill -- -PGID, poll, escalate to KILL) and journal `orphans_reaped` if any.
2. Qualification must use an ISOLATED journal (env override HAIDER_RUN_JOURNAL honored by
   the wrapper; suite sets it to a temp file) — never touch/restore the production journal.
3. Recursive descendant kill fallback: walk `ps -axo pid,ppid` to collect the full tree
   (loop until fixpoint), TERM then KILL; run the fallback even after a "successful" group
   kill if survivors remain.
4. Check every journal append's exit status (fail the wrapper with a stderr message if the
   journal can't be written); serialize appends via a lock dir (mkdir spinlock, macOS-safe);
   add a per-run `run_id` field (epoch+pid) to every record for correlation.
5. Qualification: wrap each case in a watchdog (background timer that FAILs the case and
   kills the wrapper if it exceeds 60s); after stall cases, assert no descendant of the
   fake-codex tree survives.
6. Trap HUP/INT/TERM: kill the tree, journal `interrupted` with run_id, then exit.
7. Escape ALL control bytes < 0x20 in the JSON string escaper (\u00XX loop is fine).
8. Refuse to run if output/stderr/journal/brief paths alias each other (realpath compare).
9. Capture NOW before sampling sizes in the stall loop.

Rules: bash 3.2 compatible, BSD tools only, each file < 300 lines (split a helper file
scripts/supervise-lib.sh if needed), do not touch anything outside scripts/ and this brief's
outputs. When done run `bash scripts/supervise-qualify.sh` — all cases must PASS.
