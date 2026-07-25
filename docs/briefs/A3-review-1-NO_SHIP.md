## Findings

1. **Critical — orphaned descendants after unusual Codex exit.**  
   The monitor stops supervising as soon as the group leader disappears, then journals `done` without checking whether the process group still exists ([codex-supervised.sh:217](/Users/rizzist/Documents/CODING/haider-agent/scripts/codex-supervised.sh:217), [codex-supervised.sh:246](/Users/rizzist/Documents/CODING/haider-agent/scripts/codex-supervised.sh:246)). On macOS Bash 3.2, I confirmed that a leader can exit while `jobs -pr` is empty and its descendant process group remains alive. Such children can continue modifying the workspace after the wrapper reports completion.

2. **Critical — qualification can destroy concurrent journal entries.**  
   The suite copies the shared journal, truncates it, then overwrites it from the snapshot during cleanup ([supervise-qualify.sh:16](/Users/rizzist/Documents/CODING/haider-agent/scripts/supervise-qualify.sh:16), [supervise-qualify.sh:21](/Users/rizzist/Documents/CODING/haider-agent/scripts/supervise-qualify.sh:21), [supervise-qualify.sh:35](/Users/rizzist/Documents/CODING/haider-agent/scripts/supervise-qualify.sh:35)). Any real runs or second qualification process appending meanwhile lose their records. Tests need an isolated journal, not backup/restore of the production journal.

3. **High — process-tree fallback is not recursive.**  
   If process-group signalling fails, `pkill -P "$pid"` only signals immediate children before killing the parent ([codex-supervised.sh:75](/Users/rizzist/Documents/CODING/haider-agent/scripts/codex-supervised.sh:75)). Grandchildren can be reparented and escape. Descendants that create another process group also escape because a successful group kill never invokes the fallback. This does not satisfy the whole-tree requirement in the brief.

4. **High — journal persistence is neither checked nor safely serialized.**  
   Failed appends are ignored ([codex-supervised.sh:43](/Users/rizzist/Documents/CODING/haider-agent/scripts/codex-supervised.sh:43)), so disk-full, permission, or I/O failures can produce a successful exit with no audit record. Concurrent correctness also depends on Bash emitting each arbitrarily long `printf` as one write; no locking guarantees intact JSONL records. Concurrent runs of the same brief are additionally impossible to correlate.

5. **High — the qualification gate can hang indefinitely.**  
   Stall cases invoke the wrapper without a test-level watchdog ([supervise-qualify.sh:161](/Users/rizzist/Documents/CODING/haider-agent/scripts/supervise-qualify.sh:161), [supervise-qualify.sh:182](/Users/rizzist/Documents/CODING/haider-agent/scripts/supervise-qualify.sh:182)). A broken monitor or failed kill causes the gate itself to hang rather than print `FAIL`. It also never asserts that descendants were actually reaped.

6. **Medium — signals leave an incomplete journal.**  
   HUP/INT/TERM kills the active tree and exits without recording an outcome ([codex-supervised.sh:105](/Users/rizzist/Documents/CODING/haider-agent/scripts/codex-supervised.sh:105)). That leaves a permanent `start` record with no terminal event, contrary to journaling the Codex exit outcome.

7. **Medium — JSON escaping is incomplete.**  
   Only a subset of control characters is escaped ([codex-supervised.sh:30](/Users/rizzist/Documents/CODING/haider-agent/scripts/codex-supervised.sh:30)). Valid filenames containing other bytes below U+0020, such as ESC or vertical tab, create invalid JSONL.

8. **Medium — unsafe destination aliasing.**  
   There is no check preventing the output file from being the journal, brief, stderr file, or an alias of them. Retry truncation ([codex-supervised.sh:186](/Users/rizzist/Documents/CODING/haider-agent/scripts/codex-supervised.sh:186)) can therefore destroy the journal or input brief.

9. **Low — boundary race in stall sampling.**  
   Sizes are sampled before `NOW` is captured ([codex-supervised.sh:206](/Users/rizzist/Documents/CODING/haider-agent/scripts/codex-supervised.sh:206)). A write between sampling and timestamp capture can still be killed as stalled when the threshold is crossed.

Portability otherwise looks good: both scripts parse under the installed macOS GNU Bash 3.2.57, use BSD-compatible tools, remain below 300 lines, invoke the specified Codex arguments, and avoid GNU-only options.

I attempted `bash scripts/supervise-qualify.sh`, but this review environment is read-only; it exited before any case with `mktemp: ... Operation not permitted`. Therefore I could not truthfully confirm 4/4 here. No repository files were modified.

VERDICT: NO_SHIP
