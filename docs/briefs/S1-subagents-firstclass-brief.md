# S1 — subagents first class: visible prompt, steer messaging, handoff area

Owner directives (2026-08-04, with screenshots). NO haider-tui (S2/S3
render). Study W6 delegation.rs + the supervision STEER nudge machinery
+ B2a branch pinning before writing.

## Scope

1. **Spawn prompt visible in the child transcript** (bug): the child
   session's initial task/prompt message is not ui-visible — opening
   the child chip view shows only "thinking…". Root-cause where the
   delegation commits the child's first user turn (render targets /
   PromptRender) and make the spawn prompt render like any user
   message child-side. Parent side unchanged (chips already show task).
2. **message_subagent TOOL** (the model's steer): the parent model can
   message a child at ANY time. Delivery is STEER (force-injected into
   the child's CURRENT provider round, exactly the supervision-nudge
   mechanism — never queued behind the run) when the child is running;
   a child at rest gets it as an immediate queued turn that starts.
   Depth/ownership checks mirror spawn_subagent (only YOUR children).
   Tool result = delivery receipt (delivered_steer | delivered_queued)
   + child run state. Registry manifest additive.
3. **Parent-journal facts for the timeline** (S3 renders): additive
   ui-visible facts on the PARENT (correct branch pinned):
   AgentMessaged {agent, preview (bounded 200 chars), delivery} when a
   steer/message is sent (from the TOOL or the TUI chip composer wire)
   — the existing spawned/report/chip-state facts stay as-is (finished
   already surfaces via chip Done).
4. **TUI chip composer wire**: the existing chip "message <callsign>"
   composer path must ride the SAME steer machinery (find the current
   wire — if it's queued or dead, route it through the new delivery
   path; the TUI change itself is S3, but the WIRE must exist and be
   additive now).
5. **Ephemeral handoff area**: per parent-session scratch for specs the
   parent writes for children (the owner's md-spec scenario):
   `<workspace>/.haider/handoff/<session-short>/` — inside the broker
   root so fs tools just work for BOTH parent and children (children
   share the workspace); auto-created lazily at first spawn; path
   advertised in the spawn_subagent + message_subagent tool
   descriptions AND in each child's system prompt (one line naming the
   dir); best-effort recursive cleanup when the parent session is
   deleted (never on mere idle); .gitignore self-seeded inside the dir
   (a `.gitignore` containing `*`). Document that it is EPHEMERAL.

## Laws (minimum)

- child_transcript_renders_the_spawn_prompt (ui flag pinned).
- message_subagent_steers_a_running_child_mid_round (the injected text
  reaches the child's CURRENT round — nudge-precedent fixture; assert
  the child's next provider request contains it, not a queued turn).
- message_subagent_starts_an_idle_child (queued-immediate path).
- only_own_children_are_messageable_typed_error.
- parent_fact_journaled_with_branch_and_bounded_preview.
- handoff_dir_created_lazily_gitignored_and_cleaned_on_session_delete.
- handoff_path_reaches_child_system_prompt_and_tool_descriptions.
- depth_and_fencing_laws_unbroken (existing delegation suite green).

Standing lane laws: tests never inline; mutation-notes with RUNTIME
failures (literals, non-degenerate fixtures); CARGO_INCREMENTAL=0; fmt
+ workspace clippy -D warnings; additive protocol only (goldens);
ledger; NO haider-tui; no Cargo.lock; no version bumps; leave
uncommitted; no git. Up to 3 research + 2 verify subagents. Finish
with files/tests summary.
