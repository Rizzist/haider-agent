# W6a — review of record #1 — SHIP

Reviewer: Fable 5. Branch `w6-a`, reviewed at 6d0e29c (frozen ref).
Implementer: codex lane (gpt-5.6 xhigh) per
docs/briefs/W6a-subagent-core-brief.md, mapped by
docs/research/w6-subagent-research.md.

## What shipped — the run's biggest capability milestone

`spawn_subagent` is a real tool. Children are NORMAL receipt-backed
sessions on the parent's provider identity, each with its own lazy
worker and agentic turn loop. A daemon-owned delegation coordinator
(`crates/haider-daemon/src/delegation.rs`) holds the narrow
cross-session authority; schema v9's `delegations` table persists the
full identity — agent id, ancestry (parent agent, root, depth), child
session/run, tool-call correlation under a
`(parent_session, parent_run, call_id)` idempotency key, task label,
manifest, the `spawned→running→reported→collected` state machine, and
the report slot. The spawn is a DEFERRED tool: its effect terminalizes
once the child+link are durable, never held open across the child's
life. The parent parks `Waiting`; the LAST sibling's report triggers
auto-continue in the same logical turn (reports as tool results,
waiting → thinking, never idle); `ChildWaitCheckpoint` makes the wait
crash-safe. `AgentManifest` gains the tolerant persisted `task` label
the chip UI was missing; golden fixtures regenerated. Errored children
still report — the parent model decides.

## Mutations (reviewer-chosen, EXECUTED post-commit at 6d0e29c)

| # | Mutation | Result |
|---|---|---|
| M1 | terminal children never derive a report | SURVIVED the haider-core pin (its dispatcher is a test double — boundary isolated, not vacuous), then KILLED by the daemon's production end-to-end chain in 10s |
| M2 | recovery forgets the child wait | KILLED (`child_done_parent_wait_crash_recovers_the_same_logical_turn`) |
| M3 | only the first sibling settles | KILLED (`waits_for_every_sibling_then_auto_continues_without_idle`) |

The M1 survival is journaled deliberately: core-crate pins prove the
ACTOR's laws against a mock; the daemon chain is the only pin that
proves the production dispatcher. Both layers are needed and both now
demonstrated their kill.

## Live acceptance (4/4)

The owner's original report, verbatim made real: "Spawn exactly one
subagent … report back" against the live ChatGPT subscription — spawn
tool invoked, parent parks WAITING, the child runs its own provider
turn, the parent auto-continues and answers with the child's word. No
ERRORED. (`probe_subagent_live.py` joins the release ritual.)

## Honest residuals (non-blocking → W6b/W6c)

- Chips show generic labels until W6b wires `AgentSpawned`/`task` into
  the existing recursive ChipModel (research Q2 checklist).
- No recursion grants, no stall supervision, no chip steer/close RPCs
  (W6c).
- Schema v9 is forward-only; v8 stores migrate on first open.

## Gate

Workspace clippy `-D warnings` clean; five affected crates green on the
host (329 passed, sockets included); full per-crate gate `gate24.out`;
ledger 1159 → 1169.

## Verdict

**SHIP** (merge to main, ships as v0.0.31).
