# Native session close

Negotiate `session_close_v1` in `Welcome.features`. On a Control-capable
connection, attach the session in `control` mode, then send the existing method:

```json
{"method":"session.detach","attachment_id":"the-attachment","close_session":true}
```

A successful response includes both `attachment_id` and `closed_session_id`.
That field is the completion witness: the worker supervisor, attachment replay,
session actor, native sidecar writer, peer listener/subscriptions, and delayed
idle cleanup have finished. The native `.pipe` append descriptor, peer socket,
and its anchored runtime-directory descriptor are closed. The authenticated transport and
profile services (SQLite, HTTP pools, runtime workers) have independent lifetimes.
An older daemon can ignore the additive request field, so clients must require
both the feature and the response witness before claiming native close.
The replay barrier includes unfinished replays from earlier ordinary detaches;
an attach handler resumed after close cannot start a late replay.
An already submitted SQLite page read completes before its cancelled replay
returns, so joining the replay also waits for its blocking store operation.

Close preserves the journal, metadata, workspace, receipts, and session identity.
The session remains listed and readable; a subsequent attachment or turn reopens
the native actor. Omitting `close_session` preserves ordinary detach behavior.
No method name is added.

Close requires the caller's sole Control attachment. Another attachment,
descendant stream, nonterminal run, running background task, registered monitor,
live session hook execution/subscription, background-task recovery, or configured hook dispatch pending
produces a retryable refusal. Dormant outbox rows with no configured hooks
remain durable and do not prevent close. Settle active work and
detach observers before retrying. Close does not implicitly cancel running work.
Interactive connection-owned shells and profile hook services have independent
lifetimes. A late refusal after attachment cancellation requires reattaching
before retrying close.

Supervisor recovery acquires session ownership before its task is spawned and
registers a join handle. Direct recovery calls also transfer admission to a
tracked task; cancelling their caller cannot discard an admitted recovery sweep.
After every reaper outcome, a read-only exit observer retains
session ownership until the orphan group is gone. Journaling a cleanup failure
does not make native close eligible.
`AlreadyDead` can originate from a signal result and also requires this independent
absence witness. Deletion cancels and joins monitor runners before draining the
shared native task registry; the drain includes cleanup registered by retiring
producers. A reader cannot hold deletion ahead of its own cancellation.
The supervisor polls a blocked startup lease itself: a refused close resumes
recovery, while supervisor retirement drops the wait without spawning a late task.
Recovery admitted before close either finishes before the witness or causes a
Busy refusal; recovery cannot start behind the close fence and later recreate
an evicted actor without a new session admission.

An admitted close is owned by the hub even if its RPC caller disconnects.
At most 16 close operations are admitted concurrently; additional requests
receive `busy`. Close joins actual resource owners and has no success-by-timeout
path. Forced daemon shutdown may interrupt cleanup and cannot produce the close
completion witness. A transport failure makes the outcome unknown: reconnect,
attach the preserved session, and request close again.

For retention audits, distinguish true open file descriptors from macOS's
`pbi_nfiles`, which counts allocated descriptor-table slots. Capture descriptor
identities and named threads at warm baseline, close acknowledgement, and after
15 seconds (the existing 5-second derived-cache idle delay plus Tokio's
10-second blocking-worker keepalive). Shared runtime threads need not disappear
at a session acknowledgement. Report resource counts separately from latency
and CPU performance; external load invalidates timing comparisons.

The opt-in macOS regression audit uses a disposable profile and fake provider
through the real daemon RPC socket. It checks every close witness, exact journal
equality, and needle survival over four warmup turns plus 1,000 measured turns.
After the resource census, it re-reads every recorded journal envelope and
compares hashes to check that later turns and closes preserved earlier history.
Needle survival here measures journal fidelity, using unique synthetic user
messages and a fixed fake-provider reply.
The resource bound uses **zero descriptor and settled-thread slack** relative
to the warm baseline; in-flight shared blocking threads may grow during turns
but must retire within the 15-second window. Every sampled acknowledgement
must have no session pipe descriptor. It writes descriptor identities, named
threads, binary hash, receipts and an explicit verdict to a fresh output directory.

After the required machine gate and daemon build, run:

```sh
clang -Wall -Wextra -Werror scripts/qa-gate/session-close-resources.c -o /tmp/session-close-resources
python3 scripts/qa-gate/session-close-audit.py target/debug/haiderd \
  --native-close --sampler /tmp/session-close-resources \
  --output-dir /tmp/session-close-churn
```

Add `--reuse-session` and a fresh output directory to exercise 1,000 turns of
close/reopen on the same durable history. Omit `--native-close` to measure
ordinary detach on an older binary; this baseline mode records retention
without asserting native-close bounds. The audit removes only its own temporary
profile after recording results; `--retain-profile` keeps synthetic data for
diagnosis. This census is separate from machine-quiet CPU/latency qualification.

Monitor registration, removal, mutation, adoption, and source delivery participate
in the same session activity fence. Removing a monitor cancels its actual source
runner as well as timeout/enqueue work. Cancelled or replaced runners retain a
shared join handle until completion; native close also joins these retired owners.
Close can return Busy while cancellation is still draining, including a submitted
file snapshot. Retry after that work settles. The durable monitor journal is kept.

Process, poll, and CLI monitors own their full command process group. Normal
leader completion also terminates surviving descendants; monitor commands cannot
leave untracked background children. On Unix, exit is observed without reaping,
so the leader pins the original process-group identity during the descendant
sweep. Numeric group signaling ends before the leader is reaped. Windows retains
the exact Job Object authority through group exit.

Hooks use the same native child owner. Scope removal, actor cancellation, or a
bounded hook-result timeout transfers child cleanup to a retained task with the
session activity lease. Captured output readers likewise keep actual joins and
are cancelled when their result deadline expires. Native close cannot acknowledge
until these tasks finish. An OS cleanup error retains the owner and retries; it
cannot produce a successful close acknowledgement. Profile-scoped resident hook
services keep their independent lifetime.

This operation preserves the existing natural-completion policy of ordinary
foreground/background process tools, which can deliberately detach descendants.
It does not sweep those deliberately detached processes.

On macOS native hook/monitor cleanup queries group membership while the unreaped
leader pins the group identity. A two-PID buffer proves whether any descendant
remains; a full buffer or failed probe keeps cleanup pending. The acknowledgement
waits for descendants to leave the native process list, including their reap,
then reaps the leader. Signal success/ESRCH/EPERM is never an exit witness. This
stronger barrier may remain pending if an external parent does not reap a child;
it never converts a deadline into successful close. Native leader observation
uses an async non-reaping poll with the platform fallback's one-millisecond cap,
so no detached native observer thread or kqueue survives cancellation. The new
platform libproc call has one documented unsafe block (105 → 106 production);
the reviewed count baseline records that explicit addition.

Linux and Android use the kernel `/proc/<pid>/stat` live-member probe. The
unreaped leader in state Z/X does not keep the group live, so cleanup can reach
its final `wait`. Live descendants and unreadable or malformed visible process
records retain the conservative pending verdict. This changes Android's prior
signal-zero fallback in shared Rust code; compile checks and synthetic stat tests
do not establish Android device behavior.
