# Patch brief A3 — codex supervision harness (build tooling)

Deliver in `scripts/` ONLY (no other dirs). Bash, macOS-compatible (bash 3.2, no GNU-only flags).

## scripts/codex-supervised.sh
Wrapper that runs `codex exec` under supervision:
- Usage: `codex-supervised.sh <brief-file> <output-file> [--max-stall-secs N] [--max-retries M]`
- Runs: `codex exec -s workspace-write -c model_reasoning_effort=xhigh "$(cat brief)" </dev/null >output 2>stderr-file`, in the CURRENT working directory.
- Monitors the output+stderr files: if neither grows for N seconds (default 600), the run is STALLED → kill the codex process tree, journal the event, and retry with an amended prompt prefix: "Previous run stalled and was killed. Inspect current git status/diff first, do not redo completed work, continue from where it stopped." Max M retries (default 2), then exit 1 with journal entry.
- On codex exit: journal outcome (exit code, duration, retries, bytes of output).
- Journal: append JSON lines to `scripts/run-journal.jsonl`: {ts, brief, event: start|stall_kill|retry|done|gave_up, exit_code?, duration_s?, retries?}. Use `date -u +%Y-%m-%dT%H:%M:%SZ`.
- Must reap the whole process tree on kill (codex spawns children): use process group (set -m / kill -- -PID or pkill -P fallback).

## scripts/supervise-qualify.sh
Qualification suite (the gate for this patch). Uses a FAKE_CODEX override:
`CODEX_BIN` env var (default `codex`) so tests substitute a fake binary. Tests:
1. happy path: fake codex writes output and exits 0 → journal has start+done, exit 0.
2. stall: fake codex writes once then sleeps forever → wrapper kills at --max-stall-secs 3, retries; second fake run (retry file flag) succeeds → journal start, stall_kill, retry, done.
3. give-up: fake codex always stalls → after max retries, exit 1, journal gave_up.
4. dirty-git safety: wrapper never touches git (assert `git status --porcelain` unchanged by wrapper itself when fake codex makes no changes).
Print PASS/FAIL per case and exit non-zero on any failure.

## Rules
- No test deletion elsewhere; touch only scripts/ and this brief's outputs.
- Files well-commented, small functions, no file >300 lines.
- Do NOT run the real codex binary in qualification (use fake via CODEX_BIN).
When done: run `bash scripts/supervise-qualify.sh` yourself and ensure all cases PASS.
