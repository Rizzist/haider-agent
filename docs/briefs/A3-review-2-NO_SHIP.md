NO_SHIP on frozen commit `48f7a69d5857e3321a7eb0627d8c918ed48f8518`. Branch is `w0-contracts`; worktree was clean.

### Release blockers

1. **Critical — survivor tracking can kill unrelated processes after PID reuse.**  
   The tree file retains bare PIDs for the entire attempt ([supervise-lib.sh:153](/Users/rizzist/Documents/CODING/haider-agent/scripts/supervise-lib.sh:153)). Reaping later signals every recorded live PID without verifying process identity or ancestry ([supervise-lib.sh:196](/Users/rizzist/Documents/CODING/haider-agent/scripts/supervise-lib.sh:196)). A short-lived descendant’s recycled PID can therefore target an unrelated process. Store and validate a stable start-time identity before signalling.

2. **High — alias refusal remains bypassable on case-insensitive macOS filesystems.**  
   `canonical_path` compares spelling, then uses `-ef` only when both paths already exist ([supervise-lib.sh:75](/Users/rizzist/Documents/CODING/haider-agent/scripts/supervise-lib.sh:75)). The wrapper performs this check before creating destinations ([codex-supervised.sh:67](/Users/rizzist/Documents/CODING/haider-agent/scripts/codex-supervised.sh:67)). On this case-insensitive volume, the frozen helper reports absent `scripts/RUN-JOURNAL.JSONL` and `scripts/run-journal.jsonl` as non-aliases; creating the output then makes it the default journal. Qualification tests only an existing symlink alias.

3. **High — recursive fallback still has a discovery race and is not exercised by qualification.**  
   The monitor sleeps before its first process snapshot ([codex-supervised.sh:203](/Users/rizzist/Documents/CODING/haider-agent/scripts/codex-supervised.sh:203)). A leader can create a child in another process group and exit before that snapshot; neither the original group check nor the remembered-PID sweep will find it. Both qualification trees explicitly remain in the supervised group ([supervise-qualify.sh:45](/Users/rizzist/Documents/CODING/haider-agent/scripts/supervise-qualify.sh:45)), so group killing alone passes the descendant assertions.

4. **Medium — process walking is prohibitively expensive.**  
   Every one-second sample scans the entire process table and starts an external `grep` for every row, potentially across several fixpoint passes ([supervise-lib.sh:163](/Users/rizzist/Documents/CODING/haider-agent/scripts/supervise-lib.sh:163)). Under many processes or a deep tree, this can create severe CPU/PID churn and delay supervision. Use one in-memory `awk` closure per snapshot.

5. **Medium — journal locks can become permanently stale.**  
   The mkdir lock has neither ownership metadata nor stale-lock recovery ([supervise-lib.sh:88](/Users/rizzist/Documents/CODING/haider-agent/scripts/supervise-lib.sh:88)). Graceful exits release it, but `SIGKILL` or a crash while held blocks every later run until manual removal. Locks are also derived from the uncanonicalized journal spelling, so aliases of one journal can use different locks.

### Round-one findings

| # | Status | Assessment |
|---|---|---|
| 1 | Fixed for tracked/grouped processes | Leader exit now checks the process group and known survivors, sweeps them, and records `orphans_reaped`. |
| 2 | Fixed | Qualification uses a private journal through `HAIDER_RUN_JOURNAL`; production journal backup/restore is gone. |
| 3 | Partial | Recursive snapshot and post-group PID signalling exist, but detached-before-sampling descendants escape and the gate does not test the fallback. |
| 4 | Core fix present | Appends are checked, same-spelling writes are locked, and every record has `run_id`; stale and alias-lock defects remain. |
| 5 | Fixed | Every wrapper case has a 60-second watchdog, and stall/orphan cases assert recorded PIDs are reaped. |
| 6 | Fixed | HUP/INT/TERM terminate the tracked tree and append `interrupted`. |
| 7 | Fixed | All representable bytes below `0x20` are escaped; the frozen helper passed a read-only 1–31 byte check. |
| 8 | Not fully fixed | Symlinks and existing hardlinks are refused, but absent case-folded aliases bypass the check. |
| 9 | Fixed | `NOW` is captured before output/stderr sampling. |

All four scripts parse under the installed macOS GNU Bash 3.2.57, remain under 300 lines, and are executable. I did not run the writable qualification suite, as instructed, and did not count sandbox restrictions against it. No files were modified.

VERDICT: NO_SHIP
