Reviewed committed state only: `bfeccfab9b0cbea4e75721b4c6612f2b4370f828`, branch `a3-hardening`, clean worktree.

### Release blockers

1. **Critical — whole-tree reaping regressed.**  
   `terminate_process_tree` takes one final snapshot, then `reap_survivors` only signals recorded PIDs ([supervise-process-lib.sh](/Users/rizzist/Documents/CODING/haider-agent/scripts/supervise-process-lib.sh:188), [line 203](/Users/rizzist/Documents/CODING/haider-agent/scripts/supervise-process-lib.sh:203)). Process-group TERM/KILL was removed. A recorded process can therefore fork—especially from a TERM handler—after the snapshot; once recorded processes exit, the harness reports success while the new child survives. This violates the original whole-process-tree requirement and can leave workspace-writing descendants after `done` or `retry`.

2. **High — round-2 discovery race remains.**  
   The first snapshot is earlier, but it is not atomic with spawning: the child can create a `setsid` descendant and exit before the parent shell resumes at the first collection ([codex-supervised.sh](/Users/rizzist/Documents/CODING/haider-agent/scripts/codex-supervised.sh:205)). The qualification case masks this exact race by keeping the leader alive for `sleep 0.2` ([supervise-qualify.sh](/Users/rizzist/Documents/CODING/haider-agent/scripts/supervise-qualify.sh:95)). Remove that delay and a detached, reparented child need not appear in any recorded ancestry.

3. **High — stale-lock recovery can steal a live replacement lock.**  
   After deciding the owner is stale, acquisition renames the lock directory without verifying that its owner is still the one inspected ([supervise-lib.sh](/Users/rizzist/Documents/CODING/haider-agent/scripts/supervise-lib.sh:147)). Between the check and `mv`, the old owner can release and another run can acquire the same path; the recovery then moves and deletes the new live owner’s lock. A live process delayed between `mkdir` and owner-file creation can likewise be stolen after the fixed four-spin grace. Release also deletes the current path’s owner without confirming ownership ([line 190](/Users/rizzist/Documents/CODING/haider-agent/scripts/supervise-lib.sh:190)), compounding the race and breaking journal serialization.

### Round-2 disposition

- PID-reuse identity stamping and pre-signal validation: fixed in code.
- Canonicalized, ASCII case-folded alias refusal: fixed as prescribed.
- Immediate/detached discovery: mechanism and test added, but underlying race remains.
- Single-`ps`, single-AWK descendant closure: fixed.
- Canonical-path owner-file stale-lock recovery: nominal recovery added, but concurrency safety is incomplete.

All five scripts parse under macOS GNU Bash 3.2, are executable, and remain below 300 lines. The qualification run stopped solely at sandbox-denied `mktemp`; that denial was not counted against the implementation. No files were modified.

VERDICT: NO_SHIP
