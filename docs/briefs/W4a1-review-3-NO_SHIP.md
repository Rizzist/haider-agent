# W4a1.2 final confirm — NO_SHIP (a torn-read the metadata guard cannot catch; the ledger overclaims)

Reviewer: gpt-5.6 (codex), frozen 71e0216, final confirm of the coherent-snapshot + lock + ledger delta.

CONFIRMED closed: cooperating-writer serialization (two concurrent haider patches → exactly one applies, the other conflicts; patch↔fs_write pairing serializes; source.lock() revert kills the pin); the escape mutation-comment is accurate (RENAME_NOFOLLOW_ANY load-bearing, userspace recheck defense-in-depth). Diff executor-only, 883→886, no deletions.

REQUIRED FIXES (W4a1.3):
1. **P1 — MAP_SHARED torn-read false-pass; the metadata guard is insufficient.** A non-cooperating MAP_SHARED writer changes the already-read prefix then the not-yet-read suffix DURING each pread (filesystem.rs:1099), so the initial source read and the final rehash both return the SAME torn `a…a/c…c` mix that equals neither coherent state; the accept condition (filesystem.rs:1142) false-passes and the patch commits against content the file never had. Size/inode/nanosecond-mtime/nanosecond-ctime all unchanged (APFS host) — mmap writeback does not bump them. Coarse-timestamp filesystems (POSIX permits 1s; HFS+ stores seconds) reproduce it for ORDINARY writes.
   ENGINEERING FRAME: "no PREVENTABLE clobber; ledger the irreducible." APFS is the macOS DEFAULT and provides `clonefile()` — an atomic COW snapshot immune to concurrent writes to the original. Use it to obtain a genuinely coherent read basis for the content verify (and the source read if it shares the tear), closing the torn-read false-pass on APFS. On non-COW / coarse-timestamp filesystems the torn read is irreducible with portable APIs — fall back to best-effort and ledger THAT correctly. (Judged unavailable: RENAME_SWAP has no content/inode predicate; O_EXLOCK is advisory; F_BARRIERFSYNC is persistence ordering — none give mandatory exclusion or a conditional rename.)
2. **P1 — the ledger + code comment OVERCLAIM.** docs/OPTIMIZATIONS.md:148 says same-inode modification occurs AFTER a coherent verification and implies mtime/ctime detect overlapping in-place writers; the reproduced case occurs DURING verification and defeats mtime/ctime. Correct both: remove "coherent snapshot" and "mtime/ctime detect overlapping writers" wording; state the real, wider bound (a concurrent non-cooperating writer — especially MAP_SHARED, or any writer on a coarse-timestamp filesystem — can tear the verify read itself; the content verify is best-effort against cooperating/atomic-rename/inode-changing writers, a guarantee only where clonefile provides a coherent basis). A false safety claim is the blocker as much as the code gap.

CALIBRATION for the next confirm: rule on honest claims + close-where-the-default-platform (APFS/clonefile) allows. An honestly-bounded residual on non-APFS/coarse-timestamp filesystems is shippable; an overclaimed ledger or an APFS torn-read false-pass is not. This is the closing executor-safety round — do not invent blockers, but the claim must be TRUE.

Note: reviewer sandbox denied AF_UNIX (1/29 live reachable) — the FakeProvider CAS-patch real-UDS pass is the orchestrator's (green at commit).

W4a1.2 cannot be confirmed. Writer serialization is sound, but the coherent-snapshot claim and residual ledger are not.

### Torn-hash: not closed

- The committed ordinary-write attack passes, and removing the metadata comparison makes its pin fail as expected.
- A real 256 MiB `pread` overlapping an ordinary same-size write produced a torn buffer, but changed metadata caused rejection.
- However, a non-cooperating `MAP_SHARED` writer defeats [the verifier](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/filesystem.rs:1099) on this APFS host:
  - Initial state: `a…a/b…b`; completed final state: `d…d/c…c`.
  - During each `pread`, the mapping changed the already-read prefix, then the not-yet-read suffix.
  - Both snapshots contained the identical torn `a…a/c…c` mix.
  - Size, inode, nanosecond mtime, and nanosecond ctime remained unchanged before/after both reads.
  - Thus the initial source hash and final rehash can match while the current inode contains `d…d/c…c`; [the acceptance condition](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/filesystem.rs:1142) false-passes.
- Coarse filesystems add the same problem for ordinary writes: POSIX permits timestamp resolution as coarse as one second, and HFS+ stores its relevant dates in seconds. [POSIX timestamp rules](https://pubs.opengroup.org/onlinepubs/9699919799/basedefs/V1_chap04.html), [Apple HFS+ format](https://developer.apple.com/library/archive/technotes/tn/tn1150.html).

### Cooperating-writer serialization: confirmed

- Two concurrent patches: patch A paused after verification; patch B remained blocked beyond 500 ms. After A renamed, B reopened the winner and returned a conflict. Exactly one applied.
- Removing [`source.lock()`](/Users/rizzist/haider-run/haider-agent/crates/haider-tools/src/filesystem.rs:1325) made B finish early and killed the serialization pin.
- Scratch patch→`fs_write` pairing also blocked correctly; after the patch renamed, the write detected its stale opened inode, reopened, and applied.
- Both overwrite paths retain the target-inode lock through rename. Apple documents this lock class as advisory, so non-cooperating writers remain outside its protection. [Apple `flock(2)`](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/flock.2.html).

### Residual honesty audit: failed

- The documented post-content-check same-inode window and post-identity-check replacement-inode window are real and irreducible with portable APIs.
- `RENAME_SWAP` supplies no destination content/inode predicate; post-swap verification exposes tentative content and rollback is another race. `O_EXLOCK` has advisory `flock` semantics. `F_BARRIERFSYNC` concerns persistence ordering, not mandatory exclusion or conditional rename. [Apple barrier guidance](https://developer.apple.com/documentation/xcode/reducing-disk-writes).
- But [the ledger row](/Users/rizzist/haider-run/haider-agent/docs/OPTIMIZATIONS.md:148) is not exhaustive: it says same-inode modification occurs after a coherent verification. The reproduced `MAP_SHARED` case occurs during verification, returns a torn false-pass, and expands the portable bound to include the final snapshot read itself. Coarse timestamp filesystems create the same underclaimed interval.
- Therefore the claimed two-window bound and the code comment that mtime/ctime detect overlapping in-place writers are overclaims.

### Mutation-comment accuracy: confirmed

- Removing `RENAME_NOFOLLOW_ANY` re-escaped and overwrote the moved outside target.
- Bypassing only userspace parent revalidation still returned the typed refusal because the atomic flag rejected the destination symlink.
- The layered comment is accurate.

### Scope, mutations, and gate

- Delta is executor-only: `filesystem.rs`, its W4a1.2 test module, `docs/OPTIMIZATIONS.md`, and `test-baseline.txt`. No approval, policy, CAS, bridge, daemon production, RPC, or TUI change.
- Baseline is 883→886; zero deleted test markers.
- Mutation audit: metadata-check removal killed; lock removal killed; atomic rename flag removal killed; userspace-only escape recheck removal survived as documented.
- Formatting, workspace clippy with `-D warnings`, repository checks, and 886/886 test count passed.
- Full workspace compiled with zero `could not compile`. Exactly 93 tests across eight UDS/process-backed targets failed because this sandbox rejects every AF_UNIX bind.
- Daemond live suite: 1/29 passed; 28 could not reach `Ready`. The required real-UDS FakeProvider CAS-patch acceptance was therefore not certified.
- `44a0e2b` is an ancestor; branch is 0 behind/6 ahead and fast-forwardable.
- Worktree remains clean and byte-identical at `71e02168fddc88e6070b0e97fb430d9ada9b9a75`.
- New findings: P0 none; P1 torn `MAP_SHARED` false-pass plus underbounded ledger; P2 none; P3 none.

VERDICT: NO_SHIP
