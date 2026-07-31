# W6a — the subagent core: spawn tool, durable delegation, WAITING, auto-continue

AUTHORITY: docs/research/w6-subagent-research.md (read it FIRST, whole).
It maps every seam with file:line evidence and the build order. This
brief binds its Q1 architecture into scope; where the two disagree, the
research is right unless a law below says otherwise.

## Scope (W6a — nonrecursive; recursion + stall supervision are W6c)

1. **`spawn_subagent` tool** in the manifest (haider-tools): args
   `{ task: string, prompt: string }` (task = the short display label
   the TUI chip needs — persist it; the research flagged
   `AgentManifest` has no label). DEFERRED-tool shape per the research:
   the dispatcher call terminalizes its `AgentSpawn` effect once the
   child + link are DURABLE — never held open until child completion.
2. **Children are normal sessions.** Reuse the receipt-backed
   session-create machinery daemon-internally; each child gets the
   existing lazy worker supervisor and its own agentic turn loop, on the
   PARENT's provider/model/account.
3. **A daemon-owned delegation coordinator** with narrow cross-session
   authority: owns the parent↔child link table (opaque `AgentId`,
   ancestry, child session id, tool-call correlation, attempt/fence,
   report slot, collection markers) — all DURABLE.
4. **Parent parks `RunState::Waiting` (local-child reason)** after the
   spawn effects terminalize; every `ChildReport` lands durably; when
   ALL outstanding children of the turn have reported, the parent
   auto-continues IN THE SAME LOGICAL TURN: reports appended as the
   tool results, state → Thinking, next provider request. Never
   waiting → idle directly.
5. **`ChildWaitCheckpoint`** (or equivalent durable marker) so crash
   recovery RESUMES a waiting parent instead of terminalizing it — the
   research names the exact recovery seam that would otherwise kill it.
6. **Events**: emit the frozen protocol chain — `AgentSpawned(manifest)`
   (with parent + the new label), `AgentChipState`, child envelopes
   SCOPED with the child `agent_id`, `AgentReport`/ChildResult on the
   parent — exactly what the existing live TUI reducer already consumes
   (session.rs:220/269 per the research). No new wire states; WAITING
   uses the frozen state vocabulary.
7. **Child terminal states** (done, errored, cancelled) all produce a
   report (an errored child reports its public failure text — the
   parent model decides what to do with it).

OUT (W6c, do not build): recursion grants/depth caps, stall deadlines,
nudge/kill/respawn, chip steer/close RPCs (W6b/c), question-menu
bridging.

## Laws

- Tests NEVER inline; every law-bearing test documents its mutation +
  expected RUNTIME failure. `CARGO_INCREMENTAL=0` everywhere; finish
  with `cargo fmt --all`, workspace clippy `-D warnings` clean,
  `CARGO_INCREMENTAL=0 cargo test -p haider-core -p haider-daemon -p
  haider-tools -p haider-protocol` (sandbox socket failures expected;
  the host gate is authoritative), and `cargo run -p xtask --
  test-count --update`.
- Protocol additions must be ADDITIVE + tolerant (serde defaults); if
  `AgentManifest` gains the label field, old peers must still decode.
  Schema-affecting: regenerate any golden fixtures that carry manifests.
- Do NOT touch haider-tui rendering (W6b is a separate lane); the
  projection/reducer crates may only gain what the events require.
- Do NOT touch Cargo.lock or versions. Leave changes uncommitted; no
  git commands.

## Tests (minimum — the research's recovery/idempotency traps each get a pin)

- A turn that calls `spawn_subagent` terminalizes the spawn effect
  BEFORE the child completes (mutation: hold the effect open → the
  effect-journal pin fails).
- The parent parks Waiting after spawning and auto-continues with the
  report as the tool result when the last child reports; never
  waiting → idle (mutation: resume on FIRST of N reports → fails).
- A replayed spawn (same command/receipt) never creates a second child
  (mutation: drop the receipt check → duplicate child pin fails).
- Crash between child-done and parent-resume: recovery resumes the
  waiting parent from the checkpoint (mutation: drop the checkpoint →
  the recovery test terminalizes the parent and fails).
- An errored child still reports, and the parent resumes with the
  failure text as the tool result.

Use up to 3 research subagents and 2 verify subagents. Print a final
summary of files changed and tests added.
