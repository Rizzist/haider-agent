# Durable follow-up obligations

A monitor report, completed task, or answered `request_input` question records an
observation. Delivering it to a run does not establish that its follow-up action
ran. The session journal now projects a separate obligation for that work.

The stable key is session-scoped `monitor:<report_id>`, `task:<task_id>`, or
`menu:<menu_id>`. Source facts create the obligation atomically with the
observation; no second write is needed to make it pending. One-shot monitor
observation can stop after delivery while the obligation survives. A terminal
report already in the outbox suppresses source restart even if its delivery
receipt was interrupted. Removing a
watch explicitly dismisses its pending obligations; a timeout or one-shot
completion does not. Copied fork history cannot arm an obligation in the child.

| State | Meaning |
| --- | --- |
| `pending` | Observed, awaiting a consuming attempt. |
| `admitting` | Attempt is journaled; admission/handoff is incomplete. |
| `active` | Bound to the admitted consuming run. |
| `awaiting_receipt` | Run finished successfully but handling is unacknowledged. |
| `parked` | Failed, interrupted, or exhausted; remains visible. |

`completion_attempt`, `completion_admitted`, and `completion_delivered` distinguish
attempt creation, durable turn admission, and worker handoff. The admission
command includes the attempt identity. Replaying an interrupted handoff uses the
same command; a new attempt uses a new command tied to the same obligation. Busy
wakes bind to the actual consuming run returned by admission, including subturns.
Each coalesced report remains independently acknowledgeable. The source outbox
owns first delivery and recovery of its ordered/coalesced pending slots; the
completion reconciler only rearms an existing attempt. Legacy delivered reports
without attempt receipts stay visible for explicit claim rather than automatically
replaying historical actions. A native `run.retry`
receipt rebinds the failed run's obligations to the fresh consuming run and
counts a new attempt. Task steers retain the actual consuming branch rather
than assuming it is the task's source branch.

Failures remain pending. Nonretryable provider errors park for request repair;
retryable provider/quota failures park for provider recovery; budget failures
park without resetting the budget. An explicit resume/claim is required for these
failures, so an unchanged HTTP 400 never loops. Interrupted monitor attempts and
successful but unacknowledged monitor turns from an earlier daemon generation
are rearmed. Automatic attempts are bounded at three, including the first; the
ordinary turn/provider budget gates still apply. Tasks never start provider work
by themselves. A task or menu follow-up is claimed in an explicitly started run;
neither the source process nor the menu answer is replayed.

`session.observe.pending_follow_ups` exposes the projection additively. The
`haider session <id>` depth view preserves it in JSON and names pending work in
terminal output. The
existing `monitor` tool's `list` operation returns `pending_follow_ups` and recent
successful `action_evidence` IDs for the current run. Provider context refreshes
include up to 16 obligations on the current branch/agent. The tool's additive
`follow_up` operation takes `obligation_id`, the expected `attempt`, and
`completion_action`:

- `claim`: bind pending/parked work to the current run. An active owner cannot be
  replaced by another run. Tasks and menus use this explicit opt-in.
- `resume`: rearm a parked/terminal monitor obligation after repair. It does not
  replay the source or erase the attempt ceiling.
- `handled`: include `evidence_event_ids` from successful action or reconciliation
  tool results after the attempt's admission. Report-only monitors may instead
  cite a committed, completed owner message. The daemon checks run, branch,
  agent, attempt and committed result identity. Listing tools, answering the
  question, or operating this receipt interface is not action evidence.
- `dismiss`: explicitly cancel the obligation.

Only `completion_handled`, `completion_dismissed`, or explicit watch removal
settles an obligation. Duplicate receipts are idempotent and stale attempt
receipts cannot settle a newer attempt. A successful model turn alone never
settles it. A handled receipt is an explicit assertion about the named action
with journal evidence, not an automatic proof of arbitrary task semantics.

Delivery is at least once. After a crash between an external side effect and its
receipt, inspect/reconcile the existing outcome using the stable obligation ID
before repeating it. Arbitrary shell or remote effects are not exactly once.
No fsync, signing, Android tool ceiling, or RPC method-count policy changes here.
All existing wire fields retain their types; empty follow-up lists are omitted.

Final integrated verification also requires the request-shape goldens from the
monitor-subturn lane, the monitor tool-grant fix, and the monitor redaction fix.
The recording HTTP regression deliberately returns a synthetic 400, restarts an
isolated production daemon, claims the obligation in a fresh explicit turn,
writes one marker through `fs_write`, then crashes the owned daemon after the
action and successful turn but before the receipt. Restart rearms the same
obligation; `fs_read` reconciles the existing marker and the third attempt
commits the handled receipt over real IPC. A final crash/restart verifies that
the handled obligation stays settled. When `HAIDER_COMPLETION_CLI` names a built
CLI, the fixture also checks its real session JSON before and after restart.
Its optional evidence directory contains synthetic requests, journals,
session snapshots and the marker digest; no build artifacts are copied there.
