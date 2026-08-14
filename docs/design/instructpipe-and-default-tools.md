# InstructPipe + default-tools taxonomy + subagent-workflow templates

Design note (M2-track). Analysis by the gpt-5.6 lane; synthesized here after its
read-only sandbox blocked the direct write. Token figures are the lane's rough
tokenizer estimates over the current tool manifest, not exact counts.

## 1. Default-tools taxonomy — cut context cost by conditional surfacing

Today every model-facing tool's JSON schema is advertised in every provider
prompt. Measured footprint: **~17 tools ≈ 2,263 tokens per prompt**, paid on
every turn whether or not the tool is used.

**Tiering:**

- **ALWAYS-ON core (~5 tools, ~702 tokens):** `request_input`, `fs_read`,
  `fs_search`, `fs_edit`, `process_exec`. Needed on almost every coding turn.
- **CONDITIONAL — surfaced only when durable state or the turn warrants it:**
  | tool(s) | gating signal |
  |---|---|
  | `graph_evidence` | a graph is pinned for the session |
  | pdf/attachment tools | an attachment exists on the turn |
  | `task_output` / `task_kill` | a live background task exists |
  | `message_subagent` | the session has live children |
  | `web_fetch` / `web_search` | a URL or explicit search intent in the turn |
  | `spawn_subagent` | delegation-complexity past a threshold |
  | talk/voice tools | a talk session is engaged |
- **RARE / ON-DEMAND:** surfaced only when the model explicitly asks.

**Estimated saving:** dropping the conditional+rare sets by default saves
**~1.5k tokens per ordinary root request (~65–69%)**.

**Escape hatch (capability is never lost, only deferred):** a small ALWAYS-ON
`request_tool` affordance — the model names a tool and its schema is surfaced on
the next turn. A one-line catalog digest (names only, no schemas) can ride the
prompt so the model always knows what *exists* without paying for every schema.

Seams: the canonical registry is `ToolManifest` / `graph_evidence_manifest`
(crates/haider-tools) with the advertised==dispatchable law (W8); the prompt
projection is compiled in crates/haider-core (the actor's prompt reduce). The
gating layer belongs at that compile boundary, keyed off the same durable
reduction the GraphBrief already reads.

## 2. InstructPipe — a token-efficient tool-call encoding

JSON tool calls repeat braces, quotes, and field names every call. **InstructPipe**
is a compact, line-oriented alternative.

**Signature: `␞IP1<TAB>`** (`␞` = U+241E symbol for record-separator). Its
collision-proofness is **NOT** Unicode rarity (prose can contain any character) —
it comes from three structural rules:
1. The sequence is reserved **only at column zero** of assistant output.
2. The parser reads **only the assistant channel** — never user text or tool
   output.
3. **Reversible byte-stuffing:** a literal leading `␞` in escaped text gets one
   extra `␞` prepended. Call-frames and escaped-text are then disjoint
   productions — decodable without ambiguity.

**Grammar:** `␞IP1<TAB>` + 2-hex tool-id + TAB-delimited typed args
(`s:`string · `i:`int · `b:`bool 0/1 · `e:`enum), newline-terminated.

**Worked examples (JSON vs InstructPipe):**
```
fs_edit         ␞IP1<TAB>09<TAB>s:src/lib.rs<TAB>s:fn old() {}<TAB>s:fn new() {}<TAB>b:0
process_exec    ␞IP1<TAB>0A<TAB>s:cargo test -p haider-core<TAB>s:.<TAB>b:0
spawn_subagent  ␞IP1<TAB>0B<TAB>s:verify-core<TAB>s:Run haider-core tests and report failures.
graph_evidence  ␞IP1<TAB>02<TAB>e:VERIFY<TAB>e:green<TAB>s:haider-core tests passed
web_fetch       ␞IP1<TAB>0F<TAB>s:https://example.com/spec<TAB>i:16384
```
These five calls: **~137 JSON tokens → ~70 InstructPipe tokens (~49% saving)**.

**Provider coexistence:** every current adapter stays JSON-only initially —
textual InstructPipe requires explicit per-provider/model qualification (only
models that can be reliably prompted to emit it). A translation shim at the
provider boundary converts InstructPipe↔the provider's native tool form.

**Safety + streaming:** a malformed frame reuses Haider's one-repair typed
E2–E4 error path (and the existing normalized tool-call events). The column-zero
`␞IP1` signature is the framing anchor that detects a partial call mid-stream.

## 3. Dynamic subagent-workflow templates (caching)

The session model authors a workflow per subagent — a plain session, an
implement+verify loop, or something deeper. On each authoring a **template is
cached** so a repeated subagent kind reuses it (faster, cheaper). Abstracted
from the user; surfaced to the model via a DEFAULT tool call (built later, in
InstructPipe form, gated to only appear when `spawn_subagent` is in play).

- **Cache key:** task-shape fingerprint + effective tools/grants + the gate DAG +
  policy version + model profile. Two subagent tasks with the same key reuse the
  template; a policy or grant change invalidates it.
- **Substrate:** rides the Convergence Graph. M1 gives durable evidence-gated
  attempts; **M2** attaches a fresh child graph to the exact M1 attempt that
  spawned it (child graphs collapse to one evidence item on the parent). No
  cross-graph edges, ever.

## M2 backlog implied by this note
- A real `/workflow` command in the harness: select a template for a session,
  and AI-author a new one (the mockup at `/tui` is the design target).
- Conditional tool-surfacing at the prompt-compile boundary + `request_tool`.
- InstructPipe encoder/decoder + provider-boundary shim + capability gate.
- Subagent-template cache + the (later) default authoring tool.
