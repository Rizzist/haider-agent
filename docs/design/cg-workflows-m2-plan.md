# Convergence Graph / workflows → main feature: the M2 plan

Real-feature implementation plan (gpt-5.6 analysis; synthesized here since its
read-only sandbox blocked the direct write). Five additive milestones. Owner
decisions baked in: keep the whole template library, build per-subagent
workflows (sparse but real), fix evidence authority, add guardrails+telemetry
with click-to-see-stats, allow mid-run switching, support per-todo workflows.

## Sequencing principle
Five additive milestones. New event variants stay append-only; added fields use
defaults; the legacy `BUILD|VERIFY|SHIP` strings decode into the new node-name
type; v2 RPCs are feature-negotiated. This preserves the reducer's tolerance of
unknown facts and its "pinned spec, not current defaults" rule
(graph.rs:254, :272). **Evidence authority (M2a) comes first — it is the trust
boundary that gates everything.**

## M2a — Evidence authority (+ an immediate M1 hardening slice)
The load-bearing fix. Today `all-of-N` counts green tool calls with no
evidence-item key, so three duplicate attestations satisfy VERIFY.
- **Duplicate-attestation fix (ship immediately):** template-declared
  `EvidenceSlotSpec { id, authority, subject_selector }`; `graph_evidence`
  requires a `slot`; reduce the latest verdict per
  `(graph, node, epoch, subject_digest, slot)`. `all-of-N` becomes "all N
  declared slots green"; repeating a slot is idempotent, never another vote
  (replaces the unkeyed counter at graph.rs:164 / event_store.rs:1608).
- **Trusted signals:** new durable `ProcessSignalRecorded` (run/call/effect IDs,
  command-arg digest, daemon-observed exit code, BLAKE3 transcript digest,
  optional CAS artifact, post-exec workspace revision / subject digest). The
  model submits a *signal reference*, not a green verdict, for daemon-verified
  slots. `EvidenceRecorded` gains slot + subject + proof provenance (keeping the
  legacy model source). Child proofs reference durable delegation/report coords.
- **Authority split:** daemon-verified = process exit/output, completed
  child-root contracts, human-menu CAS. Qualitative review/intent gates stay
  explicitly `model_attested` — the UI must NEVER label those "verified." Wrong
  authority / duplicate slot / stale subject / non-zero-exit-claimed-green /
  mismatched child provenance → the existing E2–E4 typed rejection.
- **Laws:** three attestations to one slot can't satisfy three slots; altered
  output/revision can't replay; a red replaces only its slot frontier; retrying
  START creates a new epoch excluding old greens; same-fingerprint red across
  epochs still triggers no-progress; crash/replay produces one signal + one
  evidence fact.
- **TUI:** show `2/3 distinct`, slot names, `verified` vs `attested`, provenance
  IDs/digests (never raw unbounded output).

## M2b — General templates + atomic mid-run switching
- Replace `GraphNodeName`'s closed enum (graph.rs:21) with a bounded string
  newtype; add `template_version` + declared `start_node`; the digest covers the
  whole immutable instance. Ship all five catalog entries (ship-loop,
  super-ship-loop, staggered, sec-audit, docs-sweep).
- Replace the hardcoded BUILD/VERIFY/SHIP match (event_store.rs:1630) with
  **dependency-driven reduction**: after satisfaction, open every unsatisfied
  node whose deps are satisfied, in declaration order; complete when none
  remain. New `GraphNodeReadied` for non-linear graphs (retain legacy
  `GraphAdvanced`). Any bounded retry reopens declared START at epoch+1.
- **`graph.switch` (receipted):** in one session-actor transaction append
  `GraphSuperseded(old,new)`, close the old human menu, then `GraphPinned(new)`
  + START attempt 1. A NEW instance, not a re-epoch (the obligation schema
  changed). Old evidence stays inspectable; late old evidence gets a typed
  superseded rejection. Cache moves from latest-only trimming to
  `GraphId → reduction` + an active-root pointer.
- **Laws:** unique bounded names, one START, acyclicity, reachability, known
  deps, deterministic ready ordering, legacy ship-loop equivalence, switch-vs-
  evidence/menu races. Reject malformed DAGs before pinning; enforce node/edge/
  slot/attempt ceilings.

## M2c — Adoption guardrail + telemetry + inspection (click-to-see-stats)
- At provider `EndTurn`, after items settle but before `RunState::Done`, consult
  the graph in `HarnessActor::run_turn` (streamed text can't be honestly
  erased).
  First unmet finalization state → `GraphFinalizationDeferred` + one managed
  continuation reminder. A changed state digest proves graph progress and may
  continue again; recurrence of the same `(run, digest)` fails closed (or opens
  the interactive durable `GraphAbandonConfirm`). Never silently drop.
- Journal facts stay authoritative; transactionally maintain rebuildable
  `graph_runs`, `graph_node_attempts`, `graph_template_rollups`. Derive
  completion/abandon rates, mis-gates/overrides, attempts/node, node duration
  from committed timestamps. Supersession ≠ abandonment.
- **TUI:** register the currently-missing strip hit target (render.rs:3672); add
  paged `graph.inspect` (status, run/template stats, live evidence provenance);
  **clicking the strip opens a scrollable inspection screen** — this is the
  owner's "click the active workflow → stats."
- **Laws:** never commit Done with unmet obligations absent explicit abandon;
  exactly one reminder per graph/run across restart; changed workflow states
  may continue within the run's request/deadline/cost budgets; metrics rebuild
  byte-for-byte from events; parallel durations aren't summed.

## M2d — Per-todo workflow groups (K runs per turn)
- **Each todo is a separate CHILD GRAPH, not a re-epoched replay** (epochs mean
  retries of one subject; using them for K todos would couple stale-green/no-
  progress and corrupt per-item metrics). Scope by
  `(session_id, Plan ItemId, TodoItem.id)` — IDs are stable only inside the
  whole-list-replace Plan lifecycle (history.rs:110, actor.rs:3524).
- New `GraphRunSetOpened` + deterministic `TodoGraphAttached`. The session/root
  graph is the aggregate owner; N children instantiate the selected template;
  its gate consumes one terminal contract per required child. Preserve G1's
  root-only `todo_write` + 50-item bound.
- **Laws:** reorder/replay can't retarget children; one child retry can't
  invalidate siblings; deps unlock by ID; aggregate completion needs K terminal
  contracts. **TUI:** todo rows show child stage; header aggregates
  `completed/K`, stage counts, attempts, critical-path elapsed.

## M2e — Sparse, dynamic subagent workflows (fully functional, rarely used)
- Extend `spawn_subagent` with optional `plain | implement_verify | deeper |
  workflow_ref`. New `ChildGraphAttached` using the exact parent run/call/tool
  coords (delegation.rs:260). Pin the child graph before its first turn;
  terminal child contract collapses to exactly ONE parent-slot evidence item;
  no cross-graph edges.
- **A daemon decision gate defaults to BARE ATTEMPT.** Grant a graph for
  mutation + independent verification; grant `deeper` only for dependent phases,
  fan-out, distinct review, or crash recovery (the collapse ladder —
  brainstorm-verify-fanout.out:7896). Workflow-enabled children get their own
  GraphBrief + scoped `graph_evidence` (both deliberately withheld today).
- Conditionally surface `workflow_author`. **Cache by
  `task-shape + effective tools/grants + gate-structure` — NOT the full DAG**;
  promote only after ≥3 successful equivalent observations in distinct parent
  attempts; revalidate policy/bounds on reuse.
- **Laws:** parent-attempt attachment, one collapse, child revision provenance,
  no authority growth, bare-default behavior, cache non-promotion on first
  sight, poisoned/colliding-bucket rejection.

## Revised readiness verdict
**Yes — after M2a through M2e, this is a defensible MAIN feature.** M2a is the
non-negotiable trust boundary: until distinct slots and daemon-bound signals
land, "green" is model testimony and main-feature positioning creates false
assurance. With evidence authority, explicit finalization exits, append-only
switching, scoped child runs, and auditable telemetry, the feature's claims
become honest and operationally supportable.
