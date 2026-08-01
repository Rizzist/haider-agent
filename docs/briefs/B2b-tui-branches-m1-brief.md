# B2b m1+m2 — TUI branches: state, routing, capture, /branch commands

AUTHORITY: docs/research/b2-branches-research.md §"B2b — TUI/live
client" (read it WHOLE first, plus §Q3 and all ten Risks). This lane
is milestones 1+2 (state/routing/capture/RPC + commands + indicator).
The tree-screen fork/jump UI (m3: typed rows, f-fork, drill, jump
geometry) is a LATER lane — do NOT start it.

## Scope

1. **State unfold (research item 1, Option-incremental)**: the
   existing checked-out projection stays the MAIN-branch view AND the
   sole session cursor authority (projection.admit is untouched law).
   Add warm per-branch views (transcript + run/footprint/menus/todos
   display state) materialized from BranchCreated + branch-stamped
   envelopes, for the active session AND background slots. Branch
   registry from EventPayload::BranchCreated descriptors (name, fork
   coordinates). Derive branch count; never store a scalar copy.
2. **Routing (item 2, risk 3/6)**: admit ONCE through the session
   cursor, then route: aggregate session-scope payloads (SessionState
   etc.) by TYPE to session-global surfaces FIRST; then branch (outer)
   then agent. Inactive-branch events still materialize (switching is
   immediate; no reattach/rewind on switch). Sibling footprint/
   compaction never leaks into another branch's view.
3. **Capture at issuance (item 3, risk 4)**: active_branch_id is
   per-session DISPLAY state. Every mutation captures the branch at
   issuance: AppRequest::SubmitText/Compact/Cancel/menu answers gain
   the captured branch; LiveCommand::{Submit,Compact,Cancel} carry it;
   link.rs encodes RequestBody::TurnSubmitWithBranch /
   SessionCompactOnBranch when Some, and the LEGACY TurnSubmit /
   SessionCompact forms when None (main bytes stay historical).
   A submit queued before a later switch still lands on its captured
   branch (law).
4. **AppRequest::BranchCreate + LiveCommand::BranchCreate + link
   mapping** to RequestBody::BranchCreate with exact
   {session, source_branch, fork_node_id, fork_seq, name}; receipt
   correlation installs NOTHING locally — the daemon's BranchCreated
   journal event is the only branch materializer (law: no live branch
   before daemon truth; typed failure leaves topology unchanged).
   Fork coordinates come from a NARROW session-global command-
   coordinate tracker: record the last NodeCommitted (node_id, seq)
   per (branch) from admitted envelopes EVEN when render.ui is false —
   documented as command coordinates, NOT display state (the
   render.ui==false never-mutates-display law keeps holding for
   display surfaces; add the doc note + law test).
5. **Commands + indicator (owner menu/esc laws bind)**:
   `/branch` opens a numbered picker menu (arrow highlight, enter
   activates, esc cancels the menu — session-scoped esc law) listing
   main + named branches with the active one marked ●; `/branch new
   [name]` forks at the active branch's last committed node (only when
   the session is settled/idle — reuse busy(); otherwise honest
   notice); `/branch <name>` switches directly. Switch atomically
   swaps transcript/header/tokens/todos/chips surfaces (risk 8: one
   atomic authority; survives session A→B→A checkout). Status bar
   gains the active branch name (only when non-main). Feature-gate on
   the welcome's advertised branch feature: without it, /branch shows
   an honest "daemon does not support branches" notice and NOTHING is
   fabricated. Demo/sim sessions may mirror the simulator's fork
   vocabulary only insofar as it already exists; durable truth is
   daemon-owned.

## Laws (minimum — from research §Minimum B2b law tests, m1/m2 subset)

- Fork issuance emits exact session/source-branch/node/seq; no live
  branch before daemon truth; replayed success installs one branch and
  activates only the originating session; typed failure leaves
  topology unchanged.
- Branch switch swaps transcript/header/tokens/todos/chips and
  survives session A → session B → session A; a submit queued before a
  later switch still carries the original branch.
- Interleaved session seqs for two branches advance ONE cursor and
  materialize both views; switching neither reattaches nor rewinds;
  detach/reattach duplicates nothing.
- Inactive-branch child-chip updates stay there; launcher aggregate
  counts all branches; sibling compaction/footprint never leaks.
- Aggregate SessionState payloads route session-global from a
  branch-stamped stream (risk-6 type-first law).
- render.ui==false still never mutates DISPLAY state (fork-coordinate
  tracker is exempt as command state — pin both halves).
- Feature-ungated daemon → /branch honest notice, no fabrication.

## Lane laws

Tests NEVER inline in src (existing haider-tui test layout: tests/
files + src *_tests.rs modules follow the crate's convention — match
it). cargo fmt --all; cargo clippy -p haider-tui --all-targets -- -D
warnings; cargo test -p haider-tui green; ledger via `cargo run -p
xtask -- test-count --update`. Commit on branch b2b-tui when green
(you may commit milestone-wise). No version bumps, no Cargo.lock-only
commits, no protocol/rpc/daemon changes (if a wire gap blocks you,
STOP and report it instead of changing other crates). Write
docs/briefs/B2b-m1-mutation-notes.md (production mutation → runtime
observer table) before finishing.
