# B2a — the branch core: registry, fork RPC, branch-scoped turns, lineage

AUTHORITY: docs/research/b2-branches-research.md (read WHOLE, first).
Its Q1/Q2/Q4-B2a sections and all ten risks bind. W7a's exact-scope law
(`branch_agent_and_nonterminal_history_are_excluded_structurally`) must
be EXTENDED, never replaced.

## Scope (protocol/store/core/daemon/rpc — NO haider-tui)

1. **Durable branch registry + fact.** Branch table (id, display name,
   source branch, fork node id, fork seq, created seq/time, head node,
   head seq — head updated transactionally with branch-local node
   commits) + additive `BranchCreated` journal event. `None` stays the
   legacy/main branch (never mint a concrete main id).
2. **`branch.create` R2 RPC** exactly per research Q2: command_id,
   session, generation, source_branch (opt), fork_node_id + fork_seq
   (both validated against the committed envelope + declared lineage;
   fork only at stable terminal turn boundaries or idle compaction
   nodes), optional name; receipt-first lookup; stable secret-free
   response coordinates; `branch_create_v1` feature bit.
3. **Branch-scoped turns.** Optional `branch_id` on turn.submit → in
   the canonical digest, `TurnAcceptCommand`, receipt `AcceptedTurn`,
   queued work, worker startup, HarnessConfig, compiler calls,
   compactor, tool/effect/menu/cancel/terminal sinks, and recovery
   (research names every hard-coded None site — the mechanical audit
   list). Acceptance parents on the BRANCH head, stamps the batch,
   updates the head in the same transaction. An accepted run is PINNED
   to its receipt's branch forever.
4. **Lineage-aware compilation.** Named-ref fork: the new branch's
   ancestry = source fragments only through each fork coordinate (per-
   fragment owning scope + fork-seq ceiling), then its own scope;
   siblings/wrong agents/nonterminal stay structurally excluded. No
   journal cloning/re-iding.
5. **Compaction interplay** per research Q4-5/6 + risk 1: automatic and
   manual compaction use the accepted/selected branch; the global-head
   CAS stays as the safe minimum; fork-before-a-compaction renders
   original ancestry, fork-after inherits the summary; pending manual
   compaction resumes on ITS branch.
6. **Delegation pinning** (risk 5): SpawnCoordinates/DelegationRecord/
   parent-side emissions carry the parent branch.

## Laws

The research's "Minimum B2a law tests" bind verbatim (R2 fork receipt,
fork-coordinate validation, lineage compilation, branch propagation,
queued/recovery, compaction divergence with both crash boundaries,
delegation). Standing lane laws: tests never inline; mutation docs w/
RUNTIME failures; CARGO_INCREMENTAL=0; fmt + workspace clippy -D
warnings; ADDITIVE protocol (goldens); ledger; no haider-tui; no
Cargo.lock; no versions; leave uncommitted; no git. Aggregate
session-scope payloads route by TYPE before branch routing (risk 6).

Use up to 3 research subagents and 2 verify subagents. Print a final
summary of files changed and tests added.
