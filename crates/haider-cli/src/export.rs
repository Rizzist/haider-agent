//! W-C M3 — `haider export`: a pure rendering pass over the durable journal.
//!
//! The journal IS the transcript; export never captures anything new. It reads
//! the committed conversation-tree facts (`NodeCommitted` user/assistant/tool
//! nodes) for a session and renders them to a chosen format:
//!
//! - `markdown` (default) — a readable, shareable transcript;
//! - `json` — the fact list projected to a stable public schema;
//! - `codex` — a codex-cli rollout JSONL (filename uuid == `session_meta` id);
//! - `claude-code` — a uuid/parentUuid-chained JSONL in the native Anthropic
//!   message shape (highest fidelity — we already speak it);
//! - `opencode` — a guarded SQLite INSERT (behind `--confirm`, refused if the
//!   db is missing or locked).
//!
//! `--masked` runs the P1 masking pass (identities hidden) — our streamer-safe
//! differentiator. The uuid-v7 / uuid-v4 identities and every timestamp are
//! DERIVED deterministically from the session's own id + `created_at` fact, so
//! a given session always exports to the same bytes (which is also what makes
//! the "never overwrite a foreign session file" collision refusal meaningful).

use std::path::{Path, PathBuf};

use haider_protocol::EventPayload;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::error::ErrorPresentation;
use haider_protocol::history::NodeKind;
use haider_protocol::item::{ItemEvent, TurnItem};
use haider_tui::notify::mask_text;
use serde_json::{Value, json};

/// The session-level facts an export needs, sourced from the durable
/// `SessionMetadataV1` (cwd/provider/model/created_at) and the daemon title.
#[derive(Debug, Clone)]
pub struct ExportMeta {
    pub session_id: String,
    pub title: Option<String>,
    pub cwd: String,
    pub provider: String,
    pub model: String,
    pub created_at_ms: u64,
    /// The exporting harness version, stamped into foreign `*_meta` records.
    pub cli_version: String,
}

/// A projected transcript turn — the reduced, format-agnostic unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Turn {
    User {
        text: String,
        at_ms: u64,
        /// The journal seq of the producing envelope — the row identity a
        /// live subscriber keys by, so cold-rebuilt rows match live rows.
        seq: u64,
    },
    Assistant {
        text: String,
        at_ms: u64,
        seq: u64,
    },
    AssistantIncomplete {
        text: String,
        interruption: ErrorPresentation,
        at_ms: u64,
        seq: u64,
    },
    Error {
        presentation: ErrorPresentation,
        at_ms: u64,
        seq: u64,
    },
    Tool {
        name: String,
        summary: String,
        at_ms: u64,
        seq: u64,
    },
}

impl Turn {
    /// The journal seq of the producing envelope.
    #[must_use]
    pub fn seq(&self) -> u64 {
        match self {
            Self::User { seq, .. }
            | Self::Assistant { seq, .. }
            | Self::AssistantIncomplete { seq, .. }
            | Self::Error { seq, .. }
            | Self::Tool { seq, .. } => *seq,
        }
    }

    fn at_ms(&self) -> u64 {
        match self {
            Self::User { at_ms, .. }
            | Self::Assistant { at_ms, .. }
            | Self::AssistantIncomplete { at_ms, .. }
            | Self::Error { at_ms, .. }
            | Self::Tool { at_ms, .. } => *at_ms,
        }
    }
}

/// The projected session — the single source every renderer reads.
#[derive(Debug, Clone)]
pub struct SessionExport {
    pub meta: ExportMeta,
    pub turns: Vec<Turn>,
    /// The highest journal seq SEEN in the replay (not just of rendered
    /// turns) — the exact catch-up cursor: subscribe `after_seq=head_seq`
    /// or re-export `--since head_seq` and nothing is missed or repeated.
    pub head_seq: u64,
}

/// The export formats. `markdown` is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Markdown,
    Json,
    Codex,
    ClaudeCode,
    OpenCode,
    /// The instruct-pipe line format: one typed line per event, escaped so
    /// the line law holds. Deterministic and append-only — exporting a
    /// longer session yields the earlier export plus suffix lines, the same
    /// prefix-stability the journal itself has.
    Pipe,
}

impl Format {
    /// Parse a `--format` value.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "markdown" | "md" => Ok(Self::Markdown),
            "json" => Ok(Self::Json),
            "codex" => Ok(Self::Codex),
            "claude-code" | "claude" => Ok(Self::ClaudeCode),
            "opencode" => Ok(Self::OpenCode),
            "pipe" | "instructpipe" => Ok(Self::Pipe),
            other => Err(format!(
                "unknown format `{other}` (markdown|json|codex|claude-code|opencode|pipe)"
            )),
        }
    }
}

/// A hard failure surfaced to the CLI.
#[derive(Debug)]
pub enum ExportError {
    /// A foreign session file already exists — never overwrite it.
    Collision(String),
    /// The opencode db is absent (we never create a harness's store).
    OpenCodeMissing(String),
    /// The opencode db is locked (a live opencode) — never risk corruption.
    OpenCodeLocked(String),
    /// The opencode writer needs an explicit confirmation flag.
    OpenCodeUnconfirmed,
    /// A filesystem/sqlite IO failure.
    Io(String),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Collision(path) => write!(
                formatter,
                "refusing to overwrite an existing session file: {path}"
            ),
            Self::OpenCodeMissing(path) => {
                write!(formatter, "opencode database not found: {path}")
            }
            Self::OpenCodeLocked(path) => write!(
                formatter,
                "opencode database is locked (close opencode and retry): {path}"
            ),
            Self::OpenCodeUnconfirmed => write!(
                formatter,
                "exporting into opencode's live database mutates a foreign app's store — pass --confirm to proceed"
            ),
            Self::Io(message) => write!(formatter, "{message}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Projection
// ---------------------------------------------------------------------------

impl SessionExport {
    /// Project the durable envelopes into a transcript. Only committed
    /// conversation-tree nodes (`UserTurn` / `AssistantCommit` / `ToolExchange`)
    /// become turns; every other fact is skipped (an export is a transcript,
    /// not a raw journal dump). Envelopes are read in `seq` order.
    #[must_use]
    pub fn project(meta: ExportMeta, events: &[RawEnvelope]) -> Self {
        let mut ordered: Vec<&RawEnvelope> = events.iter().collect();
        ordered.sort_by_key(|envelope| envelope.seq);
        let head_seq = ordered.last().map_or(0, |envelope| envelope.seq);
        let mut turns = Vec::new();
        for envelope in ordered {
            let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.clone())
            else {
                continue;
            };
            let at_ms = envelope.committed_at_ms;
            let seq = envelope.seq;
            match payload {
                EventPayload::NodeCommitted(node) => match node.kind {
                    NodeKind::UserTurn { text, .. } => {
                        turns.push(Turn::User { text, at_ms, seq });
                    }
                    NodeKind::AssistantCommit { text, .. } => {
                        turns.push(Turn::Assistant { text, at_ms, seq });
                    }
                    NodeKind::ToolExchange { tool, summary, .. } => {
                        turns.push(Turn::Tool {
                            name: tool,
                            summary,
                            at_ms,
                            seq,
                        });
                    }
                    _ => {}
                },
                EventPayload::Item(ItemEvent::Completed {
                    item: TurnItem::IncompleteAgentMessage { text, interruption },
                    ..
                }) => turns.push(Turn::AssistantIncomplete {
                    text,
                    interruption,
                    at_ms,
                    seq,
                }),
                EventPayload::RunFailed {
                    presentation: Some(presentation),
                    ..
                } => turns.push(Turn::Error {
                    presentation,
                    at_ms,
                    seq,
                }),
                _ => {}
            }
        }
        Self {
            meta,
            turns,
            head_seq,
        }
    }

    fn title(&self, masked: bool) -> Option<String> {
        self.meta
            .title
            .as_deref()
            .map(|title| apply_mask(title, masked))
    }

    fn text(&self, raw: &str, masked: bool) -> String {
        apply_mask(raw, masked)
    }

    fn foreign_assistant_text(&self, turn: &Turn, masked: bool) -> Option<String> {
        match turn {
            Turn::Assistant { text, .. } => Some(self.text(text, masked)),
            Turn::AssistantIncomplete {
                text, interruption, ..
            } => Some(format!(
                "{}\n\n[Incomplete: stream interrupted ({})]",
                self.text(text, masked),
                interruption.subcode.as_str()
            )),
            Turn::Error { presentation, .. } => Some(format!(
                "[Error: {} — {} ({})]",
                self.text(&presentation.title, masked),
                self.text(&presentation.detail, masked),
                presentation.subcode.as_str()
            )),
            Turn::User { .. } | Turn::Tool { .. } => None,
        }
    }

    /// The last user prompt (for foreign picker rows), masked as requested.
    fn last_user_prompt(&self, masked: bool) -> Option<String> {
        self.turns.iter().rev().find_map(|turn| match turn {
            Turn::User { text, .. } => Some(apply_mask(text, masked)),
            _ => None,
        })
    }

    // -----------------------------------------------------------------------
    // Native renderers
    // -----------------------------------------------------------------------

    /// The instruct-pipe rendering. Line vocabulary:
    ///
    /// ```text
    /// pipe-export/v1 session=<id> provider=<p> model=<m> created_ms=<n> cwd=|…| title=|…|
    /// U  <at_ms> |user text|
    /// A  <at_ms> |assistant text|
    /// A! <at_ms> |partial text| interrupted=|title: detail|
    /// E  <at_ms> |title: detail|
    /// T  <at_ms> <tool-name> |summary|
    /// ```
    ///
    /// Text rides between pipes with `\` `|` and newline backslash-escaped,
    /// so ONE LINE PER EVENT holds for stream/grep consumers. The rendering
    /// is a pure function of the projection in `seq` order: re-exporting a
    /// session that only grew yields the previous bytes plus new lines, and
    /// a `--since` export's BODY lines are exactly the suffix the full
    /// export appends after that cursor.
    #[must_use]
    pub fn to_pipe(&self, masked: bool) -> String {
        fn field(text: &str) -> String {
            let mut out = String::with_capacity(text.len() + 2);
            out.push('|');
            for character in text.chars() {
                match character {
                    '\\' => out.push_str("\\\\"),
                    '|' => out.push_str("\\|"),
                    '\n' => out.push_str("\\n"),
                    '\r' => {}
                    other => out.push(other),
                }
            }
            out.push('|');
            out
        }
        let mut lines = Vec::with_capacity(self.turns.len() + 1);
        lines.push(format!(
            "pipe-export/v1 session={} provider={} model={} created_ms={} head_seq={} cwd={} title={}",
            self.meta.session_id,
            self.meta.provider,
            self.meta.model,
            self.meta.created_at_ms,
            self.head_seq,
            field(&self.meta.cwd),
            field(&self.title(masked).unwrap_or_default()),
        ));
        for turn in &self.turns {
            lines.push(match turn {
                Turn::User { text, at_ms, seq } => {
                    format!("U  {seq} {at_ms} {}", field(&self.text(text, masked)))
                }
                Turn::Assistant { text, at_ms, seq } => {
                    format!("A  {seq} {at_ms} {}", field(&self.text(text, masked)))
                }
                Turn::AssistantIncomplete {
                    text,
                    interruption,
                    at_ms,
                    seq,
                } => format!(
                    "A! {seq} {at_ms} {} interrupted={}",
                    field(&self.text(text, masked)),
                    field(&self.text(
                        &format!("{}: {}", interruption.title, interruption.detail),
                        masked
                    )),
                ),
                Turn::Error {
                    presentation,
                    at_ms,
                    seq,
                } => format!(
                    "E  {seq} {at_ms} {}",
                    field(&self.text(
                        &format!("{}: {}", presentation.title, presentation.detail),
                        masked
                    )),
                ),
                Turn::Tool {
                    name,
                    summary,
                    at_ms,
                    seq,
                } => format!(
                    "T  {seq} {at_ms} {name} {}",
                    field(&self.text(summary, masked))
                ),
            });
        }
        let mut body = lines.join("\n");
        body.push('\n');
        body
    }

    /// A readable markdown transcript — the shareable artifact.
    #[must_use]
    pub fn to_markdown(&self, masked: bool) -> String {
        let mut out = String::new();
        out.push_str(&format!("# Session {}\n\n", self.meta.session_id));
        if let Some(title) = self.title(masked) {
            out.push_str(&format!("**{title}**\n\n"));
        }
        out.push_str(&format!(
            "- workspace: `{}`\n- model: `{}` · `{}`\n- created: {}\n\n",
            self.meta.cwd,
            self.meta.provider,
            self.meta.model,
            iso8601_ms(self.meta.created_at_ms),
        ));
        out.push_str("---\n\n");
        for turn in &self.turns {
            match turn {
                Turn::User { text, at_ms, .. } => {
                    out.push_str(&format!("## User · {}\n\n", iso8601_ms(*at_ms)));
                    out.push_str(&self.text(text, masked));
                    out.push_str("\n\n");
                }
                Turn::Assistant { text, at_ms, .. } => {
                    out.push_str(&format!("## Assistant · {}\n\n", iso8601_ms(*at_ms)));
                    out.push_str(&self.text(text, masked));
                    out.push_str("\n\n");
                }
                Turn::AssistantIncomplete {
                    text,
                    interruption,
                    at_ms,
                    ..
                } => {
                    out.push_str(&format!("## Assistant · {}\n\n", iso8601_ms(*at_ms)));
                    out.push_str(&self.text(text, masked));
                    out.push_str(&format!(
                        "\n\n> ⚠ incomplete — stream interrupted (`{}`)\n\n",
                        interruption.subcode.as_str()
                    ));
                }
                Turn::Error {
                    presentation,
                    at_ms,
                    ..
                } => out.push_str(&format!(
                    "## Error · {}\n\n**{}** — {} (`{}`)\n\nActions: {}\n\n",
                    iso8601_ms(*at_ms),
                    self.text(&presentation.title, masked),
                    self.text(&presentation.detail, masked),
                    presentation.subcode.as_str(),
                    serde_json::to_string(&presentation.allowed_actions)
                        .unwrap_or_else(|_| "[]".into())
                )),
                Turn::Tool { name, summary, .. } => {
                    out.push_str(&format!(
                        "> tool `{}` — {}\n\n",
                        name,
                        self.text(summary, masked)
                    ));
                }
            }
        }
        out
    }

    /// The structured export schema — a documented, stable public shape (NOT
    /// the raw envelope).
    #[must_use]
    pub fn to_json(&self, masked: bool) -> String {
        let turns: Vec<Value> = self
            .turns
            .iter()
            .map(|turn| match turn {
                Turn::User { text, at_ms, seq } => json!({
                    "role": "user",
                    "text": self.text(text, masked),
                    "at_ms": at_ms,
                    "seq": seq,
                }),
                Turn::Assistant { text, at_ms, seq } => json!({
                    "role": "assistant",
                    "text": self.text(text, masked),
                    "at_ms": at_ms,
                    "seq": seq,
                }),
                Turn::AssistantIncomplete {
                    text,
                    interruption,
                    at_ms,
                    seq,
                } => json!({
                    "role": "assistant",
                    "text": self.text(text, masked),
                    "incomplete": true,
                    "interruption": interruption,
                    "at_ms": at_ms,
                    "seq": seq,
                }),
                Turn::Error {
                    presentation,
                    at_ms,
                    seq,
                } => json!({
                    "role": "error",
                    "presentation": presentation,
                    "at_ms": at_ms,
                    "seq": seq,
                }),
                Turn::Tool {
                    name,
                    summary,
                    at_ms,
                    seq,
                } => json!({
                    "role": "tool",
                    "name": name,
                    "summary": self.text(summary, masked),
                    "at_ms": at_ms,
                    "seq": seq,
                }),
            })
            .collect();
        let document = json!({
            "schema": "haider.export.v1",
            "session_id": self.meta.session_id,
            "title": self.title(masked),
            "cwd": self.meta.cwd,
            "provider": self.meta.provider,
            "model": self.meta.model,
            "created_at_ms": self.meta.created_at_ms,
            "head_seq": self.head_seq,
            "masked": masked,
            "turns": turns,
        });
        serde_json::to_string_pretty(&document).unwrap_or_else(|_| "{}".to_owned())
    }

    // -----------------------------------------------------------------------
    // Cross-harness renderers
    // -----------------------------------------------------------------------

    /// The codex rollout export: a `rollout-*.jsonl` (filename uuid ==
    /// `session_meta` id) plus a `history.jsonl` append.
    #[must_use]
    #[allow(clippy::expect_used)]
    pub fn to_codex(&self, masked: bool) -> CodexExport {
        let uuid = derive_uuid(&self.meta.session_id, self.meta.created_at_ms, 7);
        let stamp = filename_stamp(self.meta.created_at_ms);
        let (y, m, d) = civil_date(self.meta.created_at_ms);
        let relpath = format!("sessions/{y:04}/{m:02}/{d:02}/rollout-{stamp}-{uuid}.jsonl");
        let created_iso = iso8601_ms(self.meta.created_at_ms);

        let mut lines: Vec<String> = Vec::new();
        // Line 1: session_meta — id MUST equal the filename uuid.
        lines.push(
            json!({
                "timestamp": created_iso,
                "type": "session_meta",
                "payload": {
                    "session_id": uuid,
                    "id": uuid,
                    "timestamp": created_iso,
                    "cwd": self.meta.cwd,
                    "originator": "haider",
                    "cli_version": self.meta.cli_version,
                    // H4: codex's resume picker filters rollouts by an
                    // interactive-source allowlist and by `model_provider`. A
                    // literal `source:"export"` is NOT on that allowlist, so an
                    // exported rollout never appears in `codex resume`. Write an
                    // ACCEPTED interactive source (`cli`) and the provider the
                    // resuming runtime actually speaks (`openai`), and keep
                    // Haider's true origin in explicit provenance fields so the
                    // transcript's real lineage is never lost.
                    "source": "cli",
                    "model_provider": "openai",
                    "origin": "haider-export",
                    "origin_provider": self.meta.provider,
                    "origin_model": self.meta.model,
                    "history_mode": "legacy",
                }
            })
            .to_string(),
        );
        let mut history = Vec::new();
        for turn in &self.turns {
            let iso = iso8601_ms(turn.at_ms());
            match turn {
                Turn::User { text, .. } => {
                    let text = self.text(text, masked);
                    // event_msg feeds codex's resume-picker preview.
                    lines.push(
                        json!({
                            "timestamp": iso,
                            "type": "event_msg",
                            "payload": { "type": "user_message", "message": text },
                        })
                        .to_string(),
                    );
                    lines.push(
                        json!({
                            "timestamp": iso,
                            "type": "response_item",
                            "payload": {
                                "type": "message",
                                "role": "user",
                                "content": [{ "type": "input_text", "text": text }],
                            }
                        })
                        .to_string(),
                    );
                    history.push(
                        json!({
                            "session_id": uuid,
                            "ts": self.meta.created_at_ms / 1000,
                            "text": text,
                        })
                        .to_string(),
                    );
                }
                Turn::Assistant { .. } | Turn::AssistantIncomplete { .. } | Turn::Error { .. } => {
                    let text = self
                        .foreign_assistant_text(turn, masked)
                        .expect("assistant-like export turn");
                    lines.push(
                        json!({
                            "timestamp": iso,
                            "type": "response_item",
                            "payload": {
                                "type": "message",
                                "role": "assistant",
                                "id": format!("msg_{}", short_hash(&format!("{uuid}{iso}"))),
                                "content": [{ "type": "output_text", "text": text }],
                            }
                        })
                        .to_string(),
                    );
                }
                Turn::Tool { name, summary, .. } => {
                    // M1: a `function_call` with no matching `function_call_output`
                    // is a malformed turn a resumed codex session chokes on, and
                    // our projected `summary` is not guaranteed-valid JSON for the
                    // `arguments` slot. Emit a VALID paired call + output: wrap the
                    // summary in a well-formed JSON object for `arguments`, then
                    // close the call with its `function_call_output` (same call_id).
                    let summary = self.text(summary, masked);
                    let call_id = format!("call_{}", short_hash(&format!("{name}{iso}")));
                    lines.push(
                        json!({
                            "timestamp": iso,
                            "type": "response_item",
                            "payload": {
                                "type": "function_call",
                                "name": name,
                                "arguments": json!({ "summary": summary }).to_string(),
                                "call_id": call_id,
                            }
                        })
                        .to_string(),
                    );
                    lines.push(
                        json!({
                            "timestamp": iso,
                            "type": "response_item",
                            "payload": {
                                "type": "function_call_output",
                                "call_id": call_id,
                                "output": summary,
                            }
                        })
                        .to_string(),
                    );
                }
            }
        }
        CodexExport {
            uuid,
            rollout_relpath: relpath,
            rollout_jsonl: with_trailing_newline(&lines.join("\n")),
            history_jsonl: with_trailing_newline(&history.join("\n")),
        }
    }

    /// The claude-code export: `projects/<cwd-slug>/<sessionId>.jsonl`, a
    /// uuid/parentUuid chain of user + assistant records in the native
    /// Anthropic message shape, plus ai-title / last-prompt picker rows.
    #[must_use]
    #[allow(clippy::expect_used)]
    pub fn to_claude_code(&self, masked: bool) -> ClaudeCodeExport {
        let session_uuid = derive_uuid(&self.meta.session_id, self.meta.created_at_ms, 4);
        let slug = claude_slug(&self.meta.cwd);
        let relpath = format!("projects/{slug}/{session_uuid}.jsonl");

        let mut lines: Vec<String> = Vec::new();
        let mut parent: Option<String> = None;
        let mut leaf = session_uuid.clone();
        for (index, turn) in self.turns.iter().enumerate() {
            let uuid = derive_uuid(
                &format!("{session_uuid}:{index}"),
                self.meta.created_at_ms,
                4,
            );
            let iso = iso8601_ms(turn.at_ms());
            let (kind, message) = match turn {
                Turn::User { text, .. } => (
                    "user",
                    json!({ "role": "user", "content": self.text(text, masked) }),
                ),
                Turn::Assistant { .. } | Turn::AssistantIncomplete { .. } | Turn::Error { .. } => (
                    "assistant",
                    json!({
                        "role": "assistant",
                        "model": self.meta.model,
                        "id": format!("msg_{}", short_hash(&uuid)),
                        "type": "message",
                        "content": [{ "type": "text", "text": self.foreign_assistant_text(turn, masked).expect("assistant-like export turn") }],
                        "stop_reason": "end_turn",
                    }),
                ),
                Turn::Tool { name, summary, .. } => (
                    "assistant",
                    json!({
                        "role": "assistant",
                        "model": self.meta.model,
                        "id": format!("msg_{}", short_hash(&uuid)),
                        "type": "message",
                        "content": [{
                            "type": "text",
                            "text": format!("[tool {}] {}", name, self.text(summary, masked)),
                        }],
                        "stop_reason": "end_turn",
                    }),
                ),
            };
            lines.push(
                json!({
                    "parentUuid": parent,
                    "isSidechain": false,
                    "type": kind,
                    "uuid": uuid,
                    "timestamp": iso,
                    "sessionId": session_uuid,
                    "cwd": self.meta.cwd,
                    "version": self.meta.cli_version,
                    "userType": "external",
                    "message": message,
                })
                .to_string(),
            );
            parent = Some(uuid.clone());
            leaf = uuid;
        }
        // Session-state rows (no uuid chain): a good picker row.
        if let Some(title) = self.title(masked) {
            lines.push(json!({ "type": "ai-title", "aiTitle": title }).to_string());
        }
        if let Some(prompt) = self.last_user_prompt(masked) {
            lines.push(
                json!({ "type": "last-prompt", "lastPrompt": prompt, "leafUuid": leaf })
                    .to_string(),
            );
        }
        ClaudeCodeExport {
            session_uuid,
            relpath,
            jsonl: with_trailing_newline(&lines.join("\n")),
        }
    }

    /// The opencode row set: a session, its messages, and text parts — ids in
    /// COLUMNS (opencode's shape). The db INSERT is a separate guarded step.
    #[must_use]
    #[allow(clippy::expect_used)]
    pub fn to_opencode(&self, masked: bool) -> OpenCodeExport {
        let session_id = format!("ses_{}", short_hash(&self.meta.session_id));
        let mut messages = Vec::new();
        for (index, turn) in self.turns.iter().enumerate() {
            let seed = format!("{session_id}:{index}");
            let message_id = format!("msg_{}", short_hash(&seed));
            let part_id = format!("prt_{}", short_hash(&format!("{seed}:part")));
            let (role, text) = match turn {
                Turn::User { text, .. } => ("user", self.text(text, masked)),
                Turn::Assistant { .. } | Turn::AssistantIncomplete { .. } | Turn::Error { .. } => (
                    "assistant",
                    self.foreign_assistant_text(turn, masked)
                        .expect("assistant-like export turn"),
                ),
                Turn::Tool { name, summary, .. } => (
                    "assistant",
                    format!("[tool {}] {}", name, self.text(summary, masked)),
                ),
            };
            let message_data = json!({
                "role": role,
                "time": { "created": turn.at_ms() },
                "modelID": self.meta.model,
                "providerID": self.meta.provider,
            })
            .to_string();
            let part_data = json!({ "type": "text", "text": text }).to_string();
            // H1: the message/part rows carry their own NOT NULL time columns
            // (opencode's real schema). Use the turn's own timestamp.
            let at = i64::try_from(turn.at_ms()).unwrap_or(0);
            messages.push(OpenCodeMessage {
                id: message_id,
                data: message_data,
                part_id,
                part_data,
                time_created: at,
                time_updated: at,
            });
        }
        OpenCodeExport {
            session_id,
            project_id: "global".to_owned(),
            slug: claude_slug(&self.meta.cwd),
            directory: self.meta.cwd.clone(),
            title: self
                .title(masked)
                .unwrap_or_else(|| "haider export".to_owned()),
            version: self.meta.cli_version.clone(),
            time_created: i64::try_from(self.meta.created_at_ms).unwrap_or(0),
            time_updated: i64::try_from(self.meta.created_at_ms).unwrap_or(0),
            messages,
        }
    }
}

/// The codex rollout output.
#[derive(Debug, Clone)]
pub struct CodexExport {
    pub uuid: String,
    pub rollout_relpath: String,
    pub rollout_jsonl: String,
    pub history_jsonl: String,
}

/// The claude-code output.
#[derive(Debug, Clone)]
pub struct ClaudeCodeExport {
    pub session_uuid: String,
    pub relpath: String,
    pub jsonl: String,
}

/// One opencode message plus its single text part.
#[derive(Debug, Clone)]
pub struct OpenCodeMessage {
    pub id: String,
    pub data: String,
    pub part_id: String,
    pub part_data: String,
    /// The per-message `time_created`/`time_updated` (ms). opencode's
    /// `message` AND `part` tables carry these as NOT NULL columns; omitting
    /// them from the INSERT makes the first row fail on a real store and rolls
    /// the whole export back (H1).
    pub time_created: i64,
    pub time_updated: i64,
}

/// The opencode row set to INSERT.
#[derive(Debug, Clone)]
pub struct OpenCodeExport {
    pub session_id: String,
    pub project_id: String,
    pub slug: String,
    pub directory: String,
    pub title: String,
    pub version: String,
    pub time_created: i64,
    pub time_updated: i64,
    pub messages: Vec<OpenCodeMessage>,
}

/// A report of what an opencode INSERT wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCodeWriteReport {
    pub session_id: String,
    pub messages: usize,
    pub parts: usize,
}

/// INSERT the row set into an existing opencode SQLite db (WAL). REFUSES if
/// the db is missing (never creates a harness's store) or locked (never risks
/// corrupting a live opencode), and refuses a session-id collision.
pub fn write_opencode(
    db_path: &Path,
    rows: &OpenCodeExport,
) -> Result<OpenCodeWriteReport, ExportError> {
    use rusqlite::{Connection, OpenFlags, params};
    if !db_path.is_file() {
        return Err(ExportError::OpenCodeMissing(db_path.display().to_string()));
    }
    // READ_WRITE without CREATE — we never bring a harness store into being.
    let connection = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|error| classify_sqlite(error, db_path))?;
    // Fail fast on a lock rather than blocking behind a live opencode.
    connection
        .busy_timeout(std::time::Duration::from_millis(0))
        .map_err(|error| classify_sqlite(error, db_path))?;

    let exists: bool = connection
        .query_row(
            "SELECT 1 FROM session WHERE id = ?1",
            params![rows.session_id],
            |_| Ok(true),
        )
        .optional_lock(db_path)?
        .unwrap_or(false);
    if exists {
        return Err(ExportError::Collision(format!(
            "opencode session {}",
            rows.session_id
        )));
    }

    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| classify_sqlite(error, db_path))?;
    transaction
        .execute(
            "INSERT INTO session(id, project_id, slug, directory, title, version, time_created, time_updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                rows.session_id,
                rows.project_id,
                rows.slug,
                rows.directory,
                rows.title,
                rows.version,
                rows.time_created,
                rows.time_updated,
            ],
        )
        .map_err(|error| classify_sqlite(error, db_path))?;
    let mut parts = 0usize;
    for message in &rows.messages {
        transaction
            .execute(
                "INSERT INTO message(id, session_id, data, time_created, time_updated)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    message.id,
                    rows.session_id,
                    message.data,
                    message.time_created,
                    message.time_updated,
                ],
            )
            .map_err(|error| classify_sqlite(error, db_path))?;
        transaction
            .execute(
                "INSERT INTO part(id, message_id, session_id, data, time_created, time_updated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    message.part_id,
                    message.id,
                    rows.session_id,
                    message.part_data,
                    message.time_created,
                    message.time_updated,
                ],
            )
            .map_err(|error| classify_sqlite(error, db_path))?;
        parts += 1;
    }
    transaction
        .commit()
        .map_err(|error| classify_sqlite(error, db_path))?;
    Ok(OpenCodeWriteReport {
        session_id: rows.session_id.clone(),
        messages: rows.messages.len(),
        parts,
    })
}

/// Map a rusqlite error to a lock vs generic IO failure.
fn classify_sqlite(error: rusqlite::Error, db_path: &Path) -> ExportError {
    if is_lock_error(&error) {
        ExportError::OpenCodeLocked(db_path.display().to_string())
    } else {
        ExportError::Io(format!("opencode sqlite: {error}"))
    }
}

fn is_lock_error(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ffi::ErrorCode::DatabaseBusy
                    | rusqlite::ffi::ErrorCode::DatabaseLocked,
                ..
            },
            _
        )
    )
}

/// A tiny extension turning a `SELECT` that itself hit a lock into the typed
/// locked error rather than a generic IO one.
trait OptionalLock<T> {
    fn optional_lock(self, db_path: &Path) -> Result<Option<T>, ExportError>;
}

impl<T> OptionalLock<T> for rusqlite::Result<T> {
    fn optional_lock(self, db_path: &Path) -> Result<Option<T>, ExportError> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(classify_sqlite(error, db_path)),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers: masking, slugs, ids, time
// ---------------------------------------------------------------------------

fn apply_mask(text: &str, masked: bool) -> String {
    if masked {
        mask_text(text)
    } else {
        text.to_owned()
    }
}

/// The claude-code / opencode cwd slug: `/` and `.` → `-`.
#[must_use]
pub fn claude_slug(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect()
}

fn with_trailing_newline(text: &str) -> String {
    if text.is_empty() {
        String::new()
    } else if text.ends_with('\n') {
        text.to_owned()
    } else {
        format!("{text}\n")
    }
}

/// A stable 64-bit FNV-1a hash, hex-formatted — deterministic ids without a
/// wall clock or an RNG (tests need reproducible bytes).
fn fnv1a(seed: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in seed.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn short_hash(seed: &str) -> String {
    format!("{:016x}", fnv1a(seed))
}

/// Derive a valid-shaped uuid (`version` in {4,7}) deterministically from a
/// seed + the session's `created_at` ms. v7 stamps the 48-bit ms prefix; both
/// set the RFC-4122 variant. Deterministic so a session always exports to the
/// same filename (which is what makes collision refusal meaningful).
#[must_use]
pub fn derive_uuid(seed: &str, ms: u64, version: u8) -> String {
    let mut bytes = [0u8; 16];
    let a = fnv1a(seed).to_be_bytes();
    let b = fnv1a(&format!("{seed}:{ms}:salt")).to_be_bytes();
    bytes[..8].copy_from_slice(&a);
    bytes[8..].copy_from_slice(&b);
    if version == 7 {
        let stamp = (ms & 0xffff_ffff_ffff).to_be_bytes();
        bytes[..6].copy_from_slice(&stamp[2..]);
    }
    // Version nibble and RFC-4122 variant (10xx).
    bytes[6] = (bytes[6] & 0x0f) | (version << 4);
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32],
    )
}

/// Civil (year, month, day) UTC from unix ms — Howard Hinnant's algorithm.
fn civil_date(ms: u64) -> (i64, u32, u32) {
    let (y, m, d, _, _, _, _) = civil_from_ms(ms);
    (y, m, d)
}

#[allow(clippy::many_single_char_names)]
fn civil_from_ms(ms: u64) -> (i64, u32, u32, u32, u32, u32, u32) {
    let secs = (ms / 1000) as i64;
    let millis = (ms % 1000) as u32;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let hour = (rem / 3600) as u32;
    let minute = ((rem % 3600) / 60) as u32;
    let second = (rem % 60) as u32;
    // days since 1970-01-01 → civil date.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day, hour, minute, second, millis)
}

/// ISO-8601 UTC with milliseconds: `YYYY-MM-DDTHH:MM:SS.mmmZ`.
#[must_use]
pub fn iso8601_ms(ms: u64) -> String {
    let (y, mo, d, h, mi, s, milli) = civil_from_ms(ms);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{milli:03}Z")
}

/// The codex filename time stamp: `YYYY-MM-DDTHH-MM-SS` (dashes in the time).
fn filename_stamp(ms: u64) -> String {
    let (y, mo, d, h, mi, s, _) = civil_from_ms(ms);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}-{mi:02}-{s:02}")
}

// ---------------------------------------------------------------------------
// File-writing (native + foreign), with collision refusal
// ---------------------------------------------------------------------------

/// Write `contents` to `path`, refusing to overwrite an existing file.
pub fn write_new_file(path: &Path, contents: &str) -> Result<(), ExportError> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            ExportError::Io(format!("cannot create {}: {error}", parent.display()))
        })?;
    }
    // M2: `create_new` makes existence-check + create ONE atomic, symlink-safe
    // syscall (O_CREAT|O_EXCL) — no check-then-write TOCTOU window where a
    // foreign file (or a symlink to one) could slip in between. An existing
    // target is the collision refusal; any other error is a plain IO failure.
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(ExportError::Collision(path.display().to_string()));
        }
        Err(error) => {
            return Err(ExportError::Io(format!(
                "cannot write {}: {error}",
                path.display()
            )));
        }
    };
    file.write_all(contents.as_bytes())
        .map_err(|error| ExportError::Io(format!("cannot write {}: {error}", path.display())))
}

/// M3: write the codex rollout and append its `history.jsonl` as a RECOVERABLE
/// pair. The rollout is created first (collision-refusing); if the history
/// append then fails, the just-created rollout is REMOVED so a retry is not
/// blocked by a half-written export (a stranded rollout would make the next
/// attempt a false collision). On success both sit on disk.
pub fn write_codex_pair(
    rollout_path: &Path,
    rollout_jsonl: &str,
    history_path: &Path,
    history_jsonl: &str,
) -> Result<(), ExportError> {
    write_new_file(rollout_path, rollout_jsonl)?;
    if let Err(error) = append_file(history_path, history_jsonl) {
        // `write_new_file` returns Ok only when it CREATED the file, so removing
        // it here can never delete a pre-existing foreign rollout.
        let _ = std::fs::remove_file(rollout_path);
        return Err(error);
    }
    Ok(())
}

/// Append `contents` to `path` (used for codex `history.jsonl`).
pub fn append_file(path: &Path, contents: &str) -> Result<(), ExportError> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            ExportError::Io(format!("cannot create {}: {error}", parent.display()))
        })?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| ExportError::Io(format!("cannot open {}: {error}", path.display())))?;
    file.write_all(contents.as_bytes())
        .map_err(|error| ExportError::Io(format!("cannot append {}: {error}", path.display())))
}

/// The default codex sessions root (`~/.codex`).
#[must_use]
pub fn codex_root(home: &Path) -> PathBuf {
    home.join(".codex")
}

/// The default claude-code projects root (`~/.claude`).
#[must_use]
pub fn claude_root(home: &Path) -> PathBuf {
    home.join(".claude")
}

/// The default opencode db path (`~/.local/share/opencode/opencode.db`).
#[must_use]
pub fn opencode_db_path(home: &Path) -> PathBuf {
    home.join(".local")
        .join("share")
        .join("opencode")
        .join("opencode.db")
}

// ---------------------------------------------------------------------------
// CLI command
// ---------------------------------------------------------------------------

use std::process::ExitCode;

const EX_USAGE: u8 = 2;
const EX_UNAVAILABLE: u8 = 69;
const EX_IOERR: u8 = 74;
const EX_BLOCKED: u8 = 77;

/// A hard cap on the envelopes retained for one export replay (M4). Export is
/// a full, bounded, follow-off replay; a hostile or enormous session must not
/// be able to OOM the exporter by streaming an unbounded number of facts.
const MAX_REPLAY_EVENTS: usize = 500_000;

/// Drain `receiver` into a BOUNDED buffer of at most `max_events` items,
/// returning the retained items and whether the replay was truncated. The
/// channel keeps being drained past the bound (so the unbounded producer never
/// wedges on a full queue), the surplus items are simply not retained — the
/// peak memory is `max_events`, not the whole session. Generic so the bound is
/// exercised by a law without a live daemon.
pub async fn collect_bounded_replay<T>(
    mut receiver: tokio::sync::mpsc::UnboundedReceiver<T>,
    max_events: usize,
) -> (Vec<T>, bool) {
    let mut events = Vec::new();
    let mut truncated = false;
    while let Some(item) = receiver.recv().await {
        if events.len() >= max_events {
            truncated = true;
            continue;
        }
        events.push(item);
    }
    (events, truncated)
}

/// Parsed `haider export` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportOptions {
    pub session_id: String,
    pub format: Format,
    pub out: Option<PathBuf>,
    pub masked: bool,
    pub confirm: bool,
    pub no_spawn: bool,
    /// Incremental cursor (pipe/json only): render only turns with
    /// `seq > since`. The header still carries the CURRENT head_seq, so
    /// each catch-up call yields the next cursor.
    pub since: Option<u64>,
}

/// Parse `<session-id> [--format FMT] [--out PATH] [--masked] [--confirm]
/// [--no-spawn]`.
pub fn parse_export_options(rest: &[String]) -> Result<ExportOptions, String> {
    let mut session_id: Option<String> = None;
    let mut format = Format::Markdown;
    let mut out: Option<PathBuf> = None;
    let mut masked = false;
    let mut confirm = false;
    let mut no_spawn = false;
    let mut since: Option<u64> = None;
    let mut index = 0;
    while index < rest.len() {
        match rest[index].as_str() {
            "--format" => {
                index += 1;
                let value = rest
                    .get(index)
                    .ok_or_else(|| "--format requires a value".to_owned())?;
                format = Format::parse(value)?;
            }
            "--out" => {
                index += 1;
                let value = rest
                    .get(index)
                    .ok_or_else(|| "--out requires a path".to_owned())?;
                if value.is_empty() {
                    return Err("--out requires a non-empty path".into());
                }
                out = Some(PathBuf::from(value));
            }
            "--since" => {
                index += 1;
                let value = rest
                    .get(index)
                    .ok_or_else(|| "--since requires a journal seq".to_owned())?;
                let cursor: u64 = value
                    .parse()
                    .map_err(|_| format!("--since takes a journal seq, got `{value}`"))?;
                since = Some(cursor);
            }
            "--masked" if !masked => masked = true,
            "--confirm" if !confirm => confirm = true,
            "--no-spawn" if !no_spawn => no_spawn = true,
            "--masked" | "--confirm" | "--no-spawn" => {
                return Err(format!("duplicate {} flag", rest[index]));
            }
            flag if flag.starts_with("--") => return Err(format!("unknown flag `{flag}`")),
            value if session_id.is_none() && !value.is_empty() => {
                session_id = Some(value.to_owned());
            }
            _ => {
                return Err(
                    "usage: haider export <session-id> [--format FMT] [--out PATH] [--masked] [--confirm]"
                        .into(),
                );
            }
        }
        index += 1;
    }
    Ok(ExportOptions {
        session_id: session_id.ok_or_else(|| {
            "usage: haider export <session-id> [--format markdown|json|codex|claude-code|opencode|pipe] [--since SEQ] [--out PATH] [--masked] [--confirm]"
                .to_owned()
        })?,
        format,
        out,
        masked,
        confirm,
        no_spawn,
        since,
    })
}

/// `haider export` — render a session's durable journal to a chosen format.
pub async fn export_command(rest: &[String]) -> ExitCode {
    let options = match parse_export_options(rest) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("haider export: {message}");
            return ExitCode::from(EX_USAGE);
        }
    };
    let profile = match haider_client::resolve_profile(&haider_client::ProfileEnv::capture()) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("haider: {error}");
            return ExitCode::from(EX_UNAVAILABLE);
        }
    };
    let session_id = haider_protocol::ids::SessionId::new(options.session_id.clone());

    // 1) Session-level facts from the daemon digest (cwd/provider/model/created).
    let observer = match haider_client::ObserveClient::connect(&profile, !options.no_spawn).await {
        Ok(observer) => observer,
        Err(error) => {
            eprintln!("haider export: {error}");
            return ExitCode::from(EX_UNAVAILABLE);
        }
    };
    let digest = observer.session(session_id.clone(), 0).await;
    observer.close();
    let digest = match digest {
        Ok(digest) => digest,
        Err(error) => {
            eprintln!("haider export: {error}");
            return ExitCode::from(EX_UNAVAILABLE);
        }
    };
    let Some(metadata) = digest.metadata.clone() else {
        eprintln!(
            "haider export: session {} has no durable metadata to export",
            options.session_id
        );
        return ExitCode::from(EX_UNAVAILABLE);
    };
    let title = if digest.title.trim().is_empty() {
        metadata.title.clone()
    } else {
        Some(digest.title.clone())
    };
    let meta = ExportMeta {
        session_id: options.session_id.clone(),
        title,
        cwd: metadata.cwd.clone(),
        provider: metadata.provider.clone(),
        model: metadata.model.clone(),
        created_at_ms: metadata.created_at_ms,
        cli_version: env!("CARGO_PKG_VERSION").to_owned(),
    };

    // 2) The durable envelopes (a bounded full replay, follow off). The client
    // API requires an UNBOUNDED sender, so the BOUNDED buffer lives in the
    // collector: it keeps draining the channel (the producer never wedges on a
    // full queue) but retains at most `MAX_REPLAY_EVENTS`, so a huge or hostile
    // session can never OOM the exporter (M4).
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let collector = tokio::spawn(collect_bounded_replay(receiver, MAX_REPLAY_EVENTS));
    // Verify round 2 (F1): collection starts AT the cursor, so the bounded
    // collector is a sliding WINDOW — with `--since`, every suffix of an
    // over-long session is reachable across successive calls; nothing is
    // ever stranded behind the cap.
    let stream = haider_client::observe_stream_session_after(
        &profile,
        !options.no_spawn,
        session_id,
        false,
        sender,
        options.since.unwrap_or(0),
    )
    .await;
    let (events, truncated) = collector.await.unwrap_or_default();
    if let Err(error) = stream {
        eprintln!("haider export: {error}");
        return ExitCode::from(EX_UNAVAILABLE);
    }
    if truncated {
        // The window slid from the cursor, so head_seq below is the honest
        // cursor of THIS window — the named follow-up reaches the rest.
        eprintln!(
            "haider export: replay window capped at {MAX_REPLAY_EVENTS} events — continue with \
             `haider export {} --format pipe --since <head_seq from this export's header>`",
            options.session_id
        );
    }

    // 3) Project + render + write.
    let mut export = SessionExport::project(meta, &events);
    // `--since <seq>`: the incremental cursor. Filtering the projection (not
    // the render) keeps every format's body exactly the suffix of the full
    // export's body — the append-only law does the rest. Foreign-store
    // writers (codex/claude-code/opencode) build complete files; a partial
    // one would corrupt a foreign app's history, so the cursor refuses there.
    if let Some(since) = options.since {
        if matches!(
            options.format,
            Format::Codex | Format::ClaudeCode | Format::OpenCode
        ) {
            eprintln!(
                "haider export: --since is a pipe/json/markdown cursor; foreign-store formats \
                 write complete files"
            );
            return ExitCode::from(EX_USAGE);
        }
        export.turns.retain(|turn| turn.seq() > since);
    }
    match write_export(&export, &options) {
        Ok(summary) => {
            if !summary.is_empty() {
                eprintln!("haider export: {summary}");
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("haider export: {error}");
            ExitCode::from(match error {
                ExportError::Collision(_)
                | ExportError::OpenCodeUnconfirmed
                | ExportError::OpenCodeLocked(_) => EX_BLOCKED,
                ExportError::OpenCodeMissing(_) => EX_UNAVAILABLE,
                ExportError::Io(_) => EX_IOERR,
            })
        }
    }
}

/// Render the chosen format and write it to `--out`, stdout, or the target
/// harness dir. Returns a short human summary of what was written (empty for
/// a native stdout render).
pub fn write_export(
    export: &SessionExport,
    options: &ExportOptions,
) -> Result<String, ExportError> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match options.format {
        Format::Markdown | Format::Json | Format::Pipe => {
            let body = match options.format {
                Format::Markdown => export.to_markdown(options.masked),
                Format::Json => export.to_json(options.masked),
                _ => export.to_pipe(options.masked),
            };
            match &options.out {
                Some(path) => {
                    std::fs::write(path, &body).map_err(|error| {
                        ExportError::Io(format!("cannot write {}: {error}", path.display()))
                    })?;
                    Ok(format!("wrote {}", path.display()))
                }
                None => {
                    print!("{body}");
                    Ok(String::new())
                }
            }
        }
        Format::Codex => {
            let codex = export.to_codex(options.masked);
            let (rollout_path, history_path) = match &options.out {
                Some(path) => {
                    let history = path.parent().map_or_else(
                        || PathBuf::from("history.jsonl"),
                        |parent| parent.join("history.jsonl"),
                    );
                    (path.clone(), history)
                }
                None => {
                    let root = codex_root(require_home(home.as_ref())?);
                    (
                        root.join(&codex.rollout_relpath),
                        root.join("history.jsonl"),
                    )
                }
            };
            write_codex_pair(
                &rollout_path,
                &codex.rollout_jsonl,
                &history_path,
                &codex.history_jsonl,
            )?;
            Ok(format!(
                "wrote codex rollout {} (id {})",
                rollout_path.display(),
                codex.uuid
            ))
        }
        Format::ClaudeCode => {
            let claude = export.to_claude_code(options.masked);
            let path = match &options.out {
                Some(path) => path.clone(),
                None => claude_root(require_home(home.as_ref())?).join(&claude.relpath),
            };
            write_new_file(&path, &claude.jsonl)?;
            Ok(format!(
                "wrote claude-code session {} ({})",
                path.display(),
                claude.session_uuid
            ))
        }
        Format::OpenCode => {
            if !options.confirm {
                return Err(ExportError::OpenCodeUnconfirmed);
            }
            let rows = export.to_opencode(options.masked);
            let db_path = match &options.out {
                Some(path) => path.clone(),
                None => opencode_db_path(require_home(home.as_ref())?),
            };
            let report = write_opencode(&db_path, &rows)?;
            Ok(format!(
                "inserted opencode session {} ({} messages, {} parts) into {}",
                report.session_id,
                report.messages,
                report.parts,
                db_path.display(),
            ))
        }
    }
}

fn require_home(home: Option<&PathBuf>) -> Result<&Path, ExportError> {
    home.map(PathBuf::as_path)
        .ok_or_else(|| ExportError::Io("HOME is not set; pass --out with an explicit path".into()))
}
