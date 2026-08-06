# W-C research — foreign harness session formats (local inspection, 2026-08-06)

Source: read-only Explore of the REAL local stores (codex-cli 0.145.0,
Claude Code 2.1.217+, opencode 1.17.20). Redacted; verified on-disk.

## Codex (~/.codex)

- Path: `sessions/<YYYY>/<MM>/<DD>/rollout-<YYYY-MM-DD>T<HH-MM-SS>-<uuid-v7>.jsonl`;
  filename uuid MUST equal session_meta payload id. Sibling
  `history.jsonl`: `{"session_id","ts","text"}` per user prompt.
- Every line: `{"timestamp":"<ISO8601 ms Z>","type","payload"}`.
- Line 1 `session_meta`: payload {session_id, id (same), timestamp, cwd
  (resume picker filters by cwd), originator, cli_version, source,
  model_provider, base_instructions{text}, history_mode:"legacy",
  git{commit_hash,branch,repository_url}}.
- `turn_context` per turn: {turn_id, cwd, approval_policy,
  sandbox_policy{type}, model, effort, summary}.
- `response_item` payload.type: message {role user|assistant|developer,
  content:[{type input_text|output_text, text}], id msg_* (assistant)};
  reasoning {id rs_*, summary[], encrypted_content OPAQUE — omit, never
  fabricate}; function_call {id fc_*, name, arguments (json string),
  call_id call_*}; function_call_output {call_id, output};
  custom_tool_call {name:"exec", input}.
- `event_msg` payload.type: task_started, user_message {message},
  agent_message, token_count, task_complete {duration_ms}.
- MINIMUM to list+resume: correct filename+dir, session_meta line 1 with
  matching ids + cwd + cli_version + originator, response_item message
  records (user/assistant); event_msg user_message feeds picker preview.
- Prompts: ~/.codex/prompts/ ABSENT on this machine; binary supports
  $ARGUMENTS, $ARGUMENTS[N], $N; codex 0.145 migrates commands into
  ~/.codex/skills/ (SKILL.md dirs, mostly symlinks to ~/.claude/skills).
  AGENTS.md global at ~/.codex/AGENTS.md.

## Claude Code (~/.claude)

- Path: `projects/<cwd-slug>/<sessionId-uuid4>.jsonl`; slug = cwd with
  "/" and "." → "-". Subagents under <sessionId>/subagents/; oversized
  tool outputs under <sessionId>/tool-results/. Todos: ~/.claude/tasks/
  (NOT todos/). history.jsonl: {display, timestamp ms, project,
  sessionId}.
- Conversation envelope: parentUuid (null root), isSidechain:false,
  type, uuid, timestamp ISO, sessionId (= filename), cwd, version,
  gitBranch, userType:"external", entrypoint:"cli", slug (readable
  name), session_id dup.
- `user`: message {role:"user", content: "<str>" | [{type:"tool_result",
  tool_use_id, content, is_error}]}; tool results also top-level
  toolUseResult {stdout,stderr,interrupted,isImage}.
- `assistant`: message = FULL Anthropic API message {model, id msg_*,
  type:"message", role, content:[text|thinking(+signature)|tool_use{id,
  name,input}], stop_reason, usage} + top-level requestId, effort. (We
  natively speak this shape — highest-fidelity export target.)
- Session-state records (no uuid chain): mode, permission-mode,
  ai-title {aiTitle} (the picker title — no summary records in this
  version), last-prompt {lastPrompt, leafUuid}, file-history-snapshot.
- MINIMUM to resume: correctly-slugged dir + <sessionId>.jsonl, uuid/
  parentUuid chain of user+assistant records with sessionId/cwd/version/
  timestamp, isSidechain:false; add ai-title + last-prompt for a good
  picker row.
- Commands format (real plugin examples): YAML frontmatter
  {description, allowed-tools, argument-hint, model}; body = prompt;
  $ARGUMENTS + $1..$N; !`cmd` inline execution exists (Haider will NOT
  support inline exec v1). Agents format: frontmatter {name,
  description, model, effort, color, tools: comma list}; body = system
  prompt. User dirs ~/.claude/commands|agents/*.md (absent here; format
  confirmed from installed plugins).

## OpenCode (1.17.20)

- SQLite at ~/.local/share/opencode/opencode.db (WAL). Legacy file
  storage mostly retired (only storage/session_diff/ remains).
- Tables: session(id ses_*, project_id (git-root hash or "global"),
  slug, directory, title, version, time_created/updated unix-ms, agent,
  model, cost, tokens_*); message(id msg_*, session_id, time_*, data
  JSON {role, time{created}, agent, model{providerID,modelID} |
  assistant: + cost, tokens, modelID, providerID, finish}); part(id
  prt_*, message_id, session_id, data JSON type text|reasoning|tool{
  tool, callID, state}|step-start|step-finish); todo table.
- MINIMUM: INSERT session + message + part(text) rows via sqlite (WAL —
  write with sqlite3/rusqlite, never file-copy; refuse when db locked),
  ids live in COLUMNS not JSON.
- Command/agent md frontmatter keys (from binary strings; dirs
  unconfirmed locally): description, agent, model, subtask, mode,
  tools, permission, temperature; $ARGUMENTS/$N.

## W-C implications (lock into the brief)

- Export writers: codex = rollout JSONL (uuid-v7 filename==meta id,
  omit reasoning); claude-code = uuid-chained JSONL (native Anthropic
  message shape — highest fidelity), + ai-title/last-prompt; opencode =
  guarded sqlite INSERT behind an explicit flag.
- Haider commands/agents adopt CC-COMPATIBLE frontmatter so users'
  existing .claude files drop in unchanged.
