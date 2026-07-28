# W4a — the working-agent release: file edits + shell execution through the effect broker (→ v0.0.14)

This is the release the owner asked for by name: "which release will we have real turns for LLM agents actually doing work?" Today (v0.0.13) a turn is a real Anthropic-backed run but the tool surface is READ-ONLY (fs_read/fs_list/fs_search + request_input). W4a makes the agent able to CHANGE the workspace — write/patch files and run shell commands — with permission gating and durable, restart-safe effect discipline. On this landing, `haider` → type a task → the agent ships a change.

## Why this is mostly wiring, not new infrastructure

The hard parts were built and review-hardened FOR this, across the W3 wave:
- **The effect broker + journal** (haider-tools) with the held-effect→Unknown restart law: a daemon crash mid-effect can never silently re-run it. W3c1's scenario 11 pins exactly this.
- **The menu CAS** (§5.7, R13): first-committed-wins approval, answerable from ANY control attachment, generation-fenced. `request_input` already round-trips it end to end (W3c1 scenario 6).
- **cwd session binding + cancellation-truth**: a cancelled turn reaches one terminal state; a supervised process is killed on cancel (W3c1 scenario 8, process-group sweep).
So W4a is: implement the tools, define the approval policy, and surface diff/exec output in the TUI. It must NOT remodel the broker, the journal, the menu CAS, or the worker seal.

## Scope

1. **fs_write / fs_patch** (haider-tools + the worker's tool dispatcher):
   - fs_write(path, content) — create/overwrite; fs_patch(path, unified-diff or structured hunks) — apply against current content with a conflict result (never a silent clobber). Pick ONE patch representation and justify (unified diff is model-native and diffable; structured hunks are more robust — decide, cite).
   - Both go through the effect broker as DISPATCHED effects with durable intent → result, so a restart reconciles them (applied-once, or Unknown-then-reported, never double-applied). A patch that no longer applies (content changed under it) is a clean typed failure the model can react to, not a corruption.
   - Path safety: writes confined to the session's cwd subtree (the cwd binding already exists) — a path escaping it is a typed rejection, not a traversal. Symlink-escape considered.
2. **shell exec** (a supervised process effect):
   - exec(command, cwd?) — run in the session cwd (or a checked subdir), captured stdout/stderr streamed as events, exit code as the result; the process is in the turn's process group so cancel kills it (the W3c1 group-sweep). A time/output bound (ledger the limits). NOT a PTY/interactive shell in W4a — one-shot commands; interactive is a later lane.
3. **The approval policy** (the credential-correct part — this is where review rigor goes):
   - Every MUTATING effect (fs_write, fs_patch, exec) opens a permission menu via the CAS BEFORE it dispatches: approve-once / approve-for-session (per tool or per command-shape) / deny. Reads stay unprompted. The menu shows WHAT will happen (the target path + a diff preview for patches; the exact command for exec) — never a vague "allow tool?".
   - "approve-for-session" is durable session state (survives restart via the journal, like everything else) and scoped precisely (a blanket session-wide "allow all shell" is a footgun — scope to the tool, or the command's argv[0], and say which).
   - Deny is a typed tool result the model reads and can adapt to, not a turn abort.
   - The daemon is the authority: approval is checked at DISPATCH, server-side, so a compromised/confused client cannot bypass it. The TUI renders the menu; it does not decide.
4. **TUI surfaces** (Claude-owned UI, not codex): a diff view for pending/applied patches (reuse the transcript's existing rendering discipline), exec output streaming into the transcript, the approval menu as a first-class card (the TUI6 modal-card discipline applies — a mutation-approval card must be as modal and as un-spoofable as the login card was). NO new demo divergence — `--demo` scripts these as canned effects.

## Chunking (dependency order)

- **W4a1 — the two file effects + path safety + the broker wiring** (daemon/tools; the fake-provider gate scripts a patch/write turn end to end; restart reconciliation pinned).
- **W4a2 — shell exec** (supervised process effect, group-cancel, bounds).
- **W4a3 — the approval policy** (CAS-gated dispatch, approve-once/session/deny, server-side authority, the sentinel test: a mutation MUST NOT dispatch without a committed approval — mutation-check it).
- **W4a4 — the TUI surfaces** (diff view, exec stream, approval card; ladder + a live probe that drives an approved patch end to end).

## Discipline

The W3 laws are binding: the six invariants, the held-effect→Unknown restart law, the worker StoreHandle seal (tools reach the store only through the lease), persist-before-publish. Tests only up; MUTATION-CHECK law; the live gate is a REAL daemon + FakeProvider driving an approved fs_patch to a temp workspace over the wire (never a live-API gate). Approval-bypass is a P0 class — the sentinel (no mutation dispatches without a committed CAS approval) is the headline pin. Dual review of record on the approval chunk especially.

## The owner-facing promise

On v0.0.14 install: `haider`, `/login`, point it at a repo, "add a --json flag to the status command" — it reads, proposes a patch, you approve the card, it applies, it runs the test, you see the diff and the output. That is the release where it does work.
