//! W-C M3 — session export: native markdown/json from a fixture journal,
//! `--masked` identity hiding, and the cross-harness writers (codex rollout
//! id==filename, claude-code cwd-slug + uuid chain, opencode INSERT/readback,
//! collision refusal, missing/locked db refusal).
#![allow(clippy::expect_used)]
#![allow(dead_code)]

use haider_protocol::EventPayload;
use haider_protocol::envelope::{EventEnvelope, PromptRender, RawEnvelope, RenderTargets};
use haider_protocol::history::{NodeKind, TreeNode};
use haider_protocol::ids::{DeviceId, EventId, NodeId, SessionId};
use haider_protocol::verify::VerifyVerdict;
use serde_json::Value;

#[path = "../src/export.rs"]
mod export;

use export::{ExportMeta, ExportOptions, Format, SessionExport};

const CREATED_MS: u64 = 1_754_000_000_000; // a fixed instant (2025-ish), UTC.

fn node_env(seq: u64, at_ms: u64, kind: NodeKind) -> RawEnvelope {
    let node = TreeNode {
        node: NodeId::new(format!("n-{seq}")),
        parent: None,
        kind,
    };
    let payload = serde_json::to_value(EventPayload::NodeCommitted(node)).expect("payload");
    EventEnvelope {
        schema_version: 1,
        event_id: EventId::new(format!("e-{seq}")),
        seq,
        session_id: SessionId::new("sess-abc"),
        branch_id: None,
        run_id: None,
        agent_id: None,
        device_id: DeviceId::new("dev"),
        authority_epoch: 0,
        worker_generation: 1,
        causation_id: None,
        correlation_id: None,
        committed_at_ms: at_ms,
        render: RenderTargets {
            ui: true,
            durable: true,
            prompt: PromptRender::Omit,
        },
        payload,
    }
}

fn fixture_events() -> Vec<RawEnvelope> {
    vec![
        node_env(
            2,
            CREATED_MS + 1_000,
            NodeKind::UserTurn {
                text: "ping alice@example.com about the deploy".to_owned(),
                attachments: Vec::new(),
            },
        ),
        node_env(
            3,
            CREATED_MS + 2_000,
            NodeKind::AssistantCommit {
                text: "On it — deploying now.".into(),
                verdict: VerifyVerdict::Unverified,
            },
        ),
        node_env(
            4,
            CREATED_MS + 3_000,
            NodeKind::ToolExchange {
                tool: "bash".to_owned(),
                summary: "ran the deploy script".to_owned(),
                artifact: None,
            },
        ),
    ]
}

fn fixture_meta() -> ExportMeta {
    ExportMeta {
        session_id: "sess-abc".to_owned(),
        title: Some("Deploy chat".to_owned()),
        cwd: "/Users/dev/proj.rs".to_owned(),
        provider: "anthropic".to_owned(),
        model: "fable-5".to_owned(),
        created_at_ms: CREATED_MS,
        cli_version: "0.0.80".to_owned(),
    }
}

fn fixture_export() -> SessionExport {
    SessionExport::project(fixture_meta(), &fixture_events())
}

// ---------------------------------------------------------------------------
// Native
// ---------------------------------------------------------------------------

#[test]
fn markdown_renders_from_a_fixture_journal() {
    let md = fixture_export().to_markdown(false);
    assert!(md.contains("# Session sess-abc"), "{md}");
    assert!(md.contains("## User"), "{md}");
    assert!(
        md.contains("ping alice@example.com about the deploy"),
        "{md}"
    );
    assert!(md.contains("## Assistant"), "{md}");
    assert!(md.contains("On it — deploying now."), "{md}");
    assert!(md.contains("tool `bash`"), "{md}");
    // Timestamps are ISO-8601 UTC.
    assert!(md.contains("2025-"), "iso timestamp: {md}");
}

#[test]
fn json_renders_a_stable_public_schema() {
    let rendered = fixture_export().to_json(false);
    let value: Value = serde_json::from_str(&rendered).expect("json");
    assert_eq!(value["schema"], "haider.export.v1");
    assert_eq!(value["session_id"], "sess-abc");
    assert_eq!(value["cwd"], "/Users/dev/proj.rs");
    let turns = value["turns"].as_array().expect("turns");
    assert_eq!(turns.len(), 3);
    assert_eq!(turns[0]["role"], "user");
    assert_eq!(turns[0]["ordinal"], 0);
    assert!(turns[0].get("branch_id").is_none());
    assert_eq!(turns[1]["role"], "assistant");
    assert_eq!(turns[2]["role"], "tool");

    // EQUALITY LAW applies to this bounded export window. The native sidecar
    // covers the full journal; successive `--since` windows cover any suffix
    // beyond the CLI's one-shot replay cap.
    let shared_rows: Vec<String> = fixture_events()
        .iter()
        .filter_map(haider_protocol::pipe::sidecar_row_line)
        .collect();
    let compact = compact_json_whitespace(&rendered);
    assert!(
        compact.contains(&format!("\"turns\":[{}]", shared_rows.join(","))),
        "each unmasked JSON turn must preserve the shared sidecar row bytes: {rendered}"
    );
}

fn compact_json_whitespace(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_string = false;
    let mut escaped = false;
    for character in input.chars() {
        if in_string {
            output.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
        } else if character == '"' {
            in_string = true;
            output.push(character);
        } else if !character.is_whitespace() {
            output.push(character);
        }
    }
    output
}

#[test]
fn masked_hides_identity() {
    let md = fixture_export().to_markdown(true);
    assert!(!md.contains("alice@example.com"), "raw email leaked: {md}");
    let value: Value = serde_json::from_str(&fixture_export().to_json(true)).expect("json");
    assert_eq!(value["masked"], true);
    let user = value["turns"][0]["text"].as_str().unwrap_or_default();
    assert!(
        !user.contains("alice@example.com"),
        "raw email in json: {user}"
    );
    assert!(
        user.contains("about the deploy"),
        "non-identity words kept: {user}"
    );
}

// ---------------------------------------------------------------------------
// codex
// ---------------------------------------------------------------------------

#[test]
fn codex_rollout_id_equals_filename_uuid() {
    let codex = fixture_export().to_codex(false);
    // Extract the uuid from the filename: rollout-<stamp>-<uuid>.jsonl.
    let file = codex.rollout_relpath.rsplit('/').next().expect("filename");
    let uuid_in_name = file
        .trim_start_matches("rollout-")
        .trim_end_matches(".jsonl")
        .rsplit_once('T')
        .map(|(_, rest)| rest.splitn(4, '-').skip(3).collect::<String>())
        .unwrap_or_default();
    assert_eq!(uuid_in_name, codex.uuid, "filename uuid == returned uuid");

    // Line 1 is session_meta with id == uuid; a message response_item follows.
    let mut lines = codex.rollout_jsonl.lines();
    let meta: Value = serde_json::from_str(lines.next().expect("meta line")).expect("meta json");
    assert_eq!(meta["type"], "session_meta");
    assert_eq!(meta["payload"]["id"], codex.uuid);
    assert_eq!(meta["payload"]["session_id"], codex.uuid);
    assert_eq!(meta["payload"]["cwd"], "/Users/dev/proj.rs");
    assert!(meta["payload"]["cli_version"].is_string());

    let has_message = codex.rollout_jsonl.lines().any(|line| {
        serde_json::from_str::<Value>(line)
            .map(|value| value["type"] == "response_item" && value["payload"]["type"] == "message")
            .unwrap_or(false)
    });
    assert!(has_message, "a response_item message record exists");
    // No reasoning encrypted_content is ever fabricated.
    assert!(!codex.rollout_jsonl.contains("encrypted_content"));
    // The history.jsonl append carries a session_id + text line.
    let history: Value =
        serde_json::from_str(codex.history_jsonl.lines().next().expect("history")).expect("hist");
    assert_eq!(history["session_id"], codex.uuid);
}

#[test]
fn codex_export_is_deterministic() {
    // Same session → same uuid/filename (what makes collision refusal real).
    assert_eq!(
        fixture_export().to_codex(false).rollout_relpath,
        fixture_export().to_codex(false).rollout_relpath
    );
}

#[test]
fn codex_session_meta_source_is_an_accepted_interactive_value() {
    // H4: codex's `codex resume` picker filters rollouts by an interactive
    // source allowlist AND by `model_provider`. A literal `source:"export"`
    // (or an Anthropic `model_provider`) is filtered out, so the export never
    // lists. We write an ACCEPTED source + the resuming runtime's provider,
    // and keep Haider's true origin in explicit provenance fields.
    let codex = fixture_export().to_codex(false);
    let meta: Value =
        serde_json::from_str(codex.rollout_jsonl.lines().next().expect("meta")).expect("meta json");
    // codex's interactive/resumable sources.
    const ACCEPTED: &[&str] = &["cli", "vscode", "exec"];
    let source = meta["payload"]["source"].as_str().expect("source string");
    assert!(
        ACCEPTED.contains(&source),
        "session_meta source must be codex-accepted, got {source:?}"
    );
    // The resuming runtime's provider is what codex lists on.
    assert_eq!(meta["payload"]["model_provider"], "openai");
    // Provenance is preserved — Haider's true origin is never lost.
    assert_eq!(meta["payload"]["origin_provider"], "anthropic");
    assert_eq!(meta["payload"]["origin_model"], "fable-5");
    assert_eq!(meta["payload"]["originator"], "haider");
}

#[test]
fn codex_tool_turn_emits_a_paired_call_and_output() {
    // M1: a `function_call` with no matching `function_call_output` breaks a
    // resumed codex turn, and the projected summary is not guaranteed-valid
    // JSON for `arguments`. Every function_call must be paired with a
    // function_call_output on the SAME call_id, and `arguments` must parse.
    let codex = fixture_export().to_codex(false);
    let items: Vec<Value> = codex
        .rollout_jsonl
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| value["type"] == "response_item")
        .map(|value| value["payload"].clone())
        .collect();
    let calls: Vec<&Value> = items
        .iter()
        .filter(|p| p["type"] == "function_call")
        .collect();
    assert!(!calls.is_empty(), "the fixture has a tool turn");
    for call in &calls {
        let call_id = call["call_id"].as_str().expect("call_id");
        // `arguments` is well-formed JSON, never a bare summary string.
        let args = call["arguments"].as_str().expect("arguments string");
        serde_json::from_str::<Value>(args).expect("arguments parse as JSON");
        // A matching function_call_output closes the call.
        let paired = items
            .iter()
            .any(|p| p["type"] == "function_call_output" && p["call_id"].as_str() == Some(call_id));
        assert!(paired, "function_call {call_id} has a matching output");
    }
}

// ---------------------------------------------------------------------------
// claude-code
// ---------------------------------------------------------------------------

#[test]
fn claude_code_slug_and_uuid_chain() {
    let claude = fixture_export().to_claude_code(false);
    // Slug: `/` and `.` → `-`.
    assert!(
        claude.relpath.starts_with("projects/-Users-dev-proj-rs/"),
        "slug: {}",
        claude.relpath
    );
    assert!(
        claude
            .relpath
            .ends_with(&format!("{}.jsonl", claude.session_uuid))
    );

    let lines: Vec<Value> = claude
        .jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("json"))
        .collect();
    // First conversation record: parentUuid null, isSidechain false.
    assert_eq!(lines[0]["parentUuid"], Value::Null);
    assert_eq!(lines[0]["isSidechain"], false);
    assert_eq!(lines[0]["type"], "user");
    assert_eq!(lines[0]["sessionId"], claude.session_uuid);
    assert_eq!(lines[0]["cwd"], "/Users/dev/proj.rs");
    // Second record chains onto the first.
    assert_eq!(lines[1]["parentUuid"], lines[0]["uuid"]);
    assert_eq!(lines[1]["type"], "assistant");
    // The assistant message is the native Anthropic shape.
    assert_eq!(lines[1]["message"]["type"], "message");
    assert_eq!(lines[1]["message"]["content"][0]["type"], "text");
    // Picker rows: ai-title + last-prompt present.
    assert!(claude.jsonl.contains("\"ai-title\""), "ai-title row");
    assert!(claude.jsonl.contains("\"last-prompt\""), "last-prompt row");
}

// ---------------------------------------------------------------------------
// opencode (temp db only — never the real one)
// ---------------------------------------------------------------------------

fn make_opencode_db(path: &std::path::Path) {
    let conn = rusqlite::Connection::open(path).expect("open");
    // H1: mirror the REAL opencode 1.17.x schema — `message` AND `part` carry
    // NOT NULL `time_created`/`time_updated` columns. The old reduced fixture
    // dropped them, which is exactly what hid the writer omitting them (the
    // first message INSERT fails on a real db and rolls the whole export back).
    conn.execute_batch(
        "CREATE TABLE session(id TEXT PRIMARY KEY, project_id TEXT, slug TEXT, directory TEXT, title TEXT, version TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
         CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, data TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
         CREATE TABLE part(id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, data TEXT, time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);",
    )
    .expect("schema");
}

#[test]
fn opencode_inserts_and_reads_back() {
    let temp = tempfile::tempdir().expect("temp");
    let db = temp.path().join("opencode.db");
    make_opencode_db(&db);
    let rows = fixture_export().to_opencode(false);
    let report = export::write_opencode(&db, &rows).expect("insert");
    assert_eq!(report.messages, 3);
    assert_eq!(report.parts, 3);

    let conn = rusqlite::Connection::open(&db).expect("reopen");
    let session_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM session WHERE id = ?1",
            [&rows.session_id],
            |r| r.get(0),
        )
        .expect("session count");
    assert_eq!(session_count, 1);
    let part_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM part WHERE session_id = ?1",
            [&rows.session_id],
            |r| r.get(0),
        )
        .expect("part count");
    assert_eq!(part_count, 3);
    // The text part round-trips the transcript.
    let first_part: String = conn
        .query_row(
            "SELECT data FROM part WHERE session_id = ?1 ORDER BY id LIMIT 1",
            [&rows.session_id],
            |r| r.get(0),
        )
        .expect("part data");
    assert!(
        first_part.contains("\"type\":\"text\""),
        "part is a text part: {first_part}"
    );
    // H1: the message AND part time columns are POPULATED from the turn
    // timestamp — never left out (a NOT NULL omission would have failed the
    // INSERT above and rolled the whole export back on a real store).
    let message_time: i64 = conn
        .query_row(
            "SELECT time_created FROM message ORDER BY time_created LIMIT 1",
            [],
            |r| r.get(0),
        )
        .expect("message time_created");
    assert!(
        message_time >= CREATED_MS as i64,
        "message time_created populated from the turn ts: {message_time}"
    );
    let part_time: i64 = conn
        .query_row(
            "SELECT time_created FROM part ORDER BY time_created LIMIT 1",
            [],
            |r| r.get(0),
        )
        .expect("part time_created");
    assert!(
        part_time >= CREATED_MS as i64,
        "part time_created populated: {part_time}"
    );
}

#[test]
fn opencode_refuses_a_session_collision() {
    let temp = tempfile::tempdir().expect("temp");
    let db = temp.path().join("opencode.db");
    make_opencode_db(&db);
    let rows = fixture_export().to_opencode(false);
    export::write_opencode(&db, &rows).expect("first insert");
    // A second export of the same session must refuse, not duplicate.
    let err = export::write_opencode(&db, &rows).expect_err("collision");
    assert!(matches!(err, export::ExportError::Collision(_)), "{err}");
}

#[test]
fn opencode_refuses_a_missing_db() {
    let temp = tempfile::tempdir().expect("temp");
    let db = temp.path().join("does-not-exist.db");
    let rows = fixture_export().to_opencode(false);
    let err = export::write_opencode(&db, &rows).expect_err("missing");
    assert!(
        matches!(err, export::ExportError::OpenCodeMissing(_)),
        "{err}"
    );
}

#[test]
fn opencode_refuses_a_locked_db() {
    let temp = tempfile::tempdir().expect("temp");
    let db = temp.path().join("opencode.db");
    make_opencode_db(&db);
    // Hold an EXCLUSIVE lock on the db from another connection.
    let locker = rusqlite::Connection::open(&db).expect("locker");
    locker.execute_batch("BEGIN EXCLUSIVE").expect("lock");
    let rows = fixture_export().to_opencode(false);
    let err = export::write_opencode(&db, &rows).expect_err("locked");
    assert!(
        matches!(err, export::ExportError::OpenCodeLocked(_)),
        "{err}"
    );
}

#[test]
fn opencode_requires_confirmation() {
    let temp = tempfile::tempdir().expect("temp");
    let db = temp.path().join("opencode.db");
    make_opencode_db(&db);
    let options = ExportOptions {
        session_id: "sess-abc".to_owned(),
        format: Format::OpenCode,
        out: Some(db),
        masked: false,
        confirm: false,
        no_spawn: true,
        since: None,
    };
    // The write pathway refuses without --confirm BEFORE touching the db.
    let err = export::write_export(&fixture_export(), &options).expect_err("unconfirmed");
    assert!(
        matches!(err, export::ExportError::OpenCodeUnconfirmed),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// collision refusal (foreign files)
// ---------------------------------------------------------------------------

#[test]
fn never_overwrites_an_existing_foreign_file() {
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("nested/rollout.jsonl");
    export::write_new_file(&path, "first").expect("first write");
    let err = export::write_new_file(&path, "second").expect_err("collision");
    assert!(matches!(err, export::ExportError::Collision(_)), "{err}");
    // The original bytes are untouched.
    assert_eq!(std::fs::read_to_string(&path).expect("read"), "first");
}

#[test]
#[cfg(unix)]
fn write_new_file_refuses_to_follow_a_symlink() {
    // M2: `create_new` (O_CREAT|O_EXCL) refuses a symlink atomically. The old
    // exists()+write followed a DANGLING symlink and wrote THROUGH it to an
    // arbitrary target (a symlink-swap TOCTOU). With the fix the write is
    // refused and no file is created behind the link.
    let temp = tempfile::tempdir().expect("temp");
    let link = temp.path().join("rollout.jsonl");
    let target = temp.path().join("outside.jsonl");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    assert!(!target.exists(), "target starts absent");
    let err = export::write_new_file(&link, "payload").expect_err("must refuse the symlink");
    assert!(matches!(err, export::ExportError::Collision(_)), "{err}");
    assert!(
        !target.exists(),
        "no file was created through the dangling symlink"
    );
}

#[test]
fn codex_pair_rolls_back_the_rollout_when_history_fails() {
    // M3: the rollout + history.jsonl are two ops. If the history append fails,
    // the just-created rollout must be REMOVED so a retry isn't blocked by a
    // false collision. We force the append to fail by pointing history at a
    // DIRECTORY (open-for-append on a dir is an IO error).
    let temp = tempfile::tempdir().expect("temp");
    let rollout = temp.path().join("rollout.jsonl");
    let history_dir = temp.path().join("history.jsonl");
    std::fs::create_dir(&history_dir).expect("dir in the history file's place");
    let err = export::write_codex_pair(&rollout, "rollout-body", &history_dir, "hist-line")
        .expect_err("history append must fail");
    assert!(matches!(err, export::ExportError::Io(_)), "{err}");
    assert!(
        !rollout.exists(),
        "the rollout was rolled back so a retry is not a false collision"
    );
}

#[tokio::test]
async fn replay_buffer_is_bounded() {
    // M4: the export replay is capped so a huge/hostile session cannot OOM the
    // exporter. Past the bound the channel keeps draining but items stop being
    // retained (truncated=true) — peak memory is the bound, not the session.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<u32>();
    for i in 0..10u32 {
        tx.send(i).expect("send");
    }
    drop(tx);
    let (events, truncated) = export::collect_bounded_replay(rx, 3).await;
    assert_eq!(
        events,
        vec![0, 1, 2],
        "only the first max_events are retained"
    );
    assert!(truncated, "the surplus is reported as truncated");
    // Under the bound, everything is kept and nothing is flagged.
    let (tx2, rx2) = tokio::sync::mpsc::unbounded_channel::<u32>();
    for i in 0..3u32 {
        tx2.send(i).expect("send");
    }
    drop(tx2);
    let (all, trunc2) = export::collect_bounded_replay(rx2, 100).await;
    assert_eq!(all.len(), 3);
    assert!(!trunc2, "under the bound → not truncated");
}

#[test]
fn parse_defaults_to_markdown_and_reads_flags() {
    let options =
        export::parse_export_options(&["sess-1".to_owned(), "--masked".to_owned()]).expect("parse");
    assert_eq!(options.session_id, "sess-1");
    assert_eq!(options.format, Format::Markdown);
    assert!(options.masked);

    let codex = export::parse_export_options(&[
        "sess-2".to_owned(),
        "--format".to_owned(),
        "codex".to_owned(),
    ])
    .expect("parse");
    assert_eq!(codex.format, Format::Codex);
    assert!(export::parse_export_options(&["--format".to_owned(), "bogus".to_owned()]).is_err());
}

/// MUTATION CHECK (pipe/instructpipe export): break the one-line-per-event
/// law (stop escaping newlines/pipes), drop a turn variant from the
/// renderer, or make the rendering depend on anything but the projection.
/// Expected RUNTIME failures below.
#[test]
fn pipe_export_holds_the_line_law_and_is_append_only() {
    let export = fixture_export();
    let pipe = export.to_pipe(false);
    // Header names the session facts.
    let mut lines = pipe.lines();
    let header = lines.next().expect("header line");
    assert!(header.starts_with("pipe-export/v1 session="), "{header}");
    assert!(header.contains("provider="), "{header}");
    // One line per turn, exactly.
    assert_eq!(
        pipe.lines().count(),
        1 + export.turns.len(),
        "one line per event:\n{pipe}"
    );
    // The format keyword parses.
    assert_eq!(Format::parse("pipe"), Ok(Format::Pipe));
    assert_eq!(Format::parse("instructpipe"), Ok(Format::Pipe));

    let shared_body: String = fixture_events()
        .iter()
        .filter_map(haider_protocol::pipe::pipe_body_line)
        .map(|line| format!("{line}\n"))
        .collect();
    assert_eq!(
        pipe.split_once('\n').map_or("", |(_, body)| body),
        shared_body,
        "the CLI's unmasked body must be exactly the shared native projection"
    );

    // The line law survives hostile text and tool names: newlines and pipes
    // escape instead of splitting or breaking a field.
    let mut hostile_events = fixture_events();
    hostile_events.push(node_env(
        999,
        99,
        NodeKind::UserTurn {
            text: "line one\nline two |with pipes| and \\backslash".into(),
            attachments: Vec::new(),
        },
    ));
    hostile_events.push(node_env(
        1_000,
        100,
        NodeKind::ToolExchange {
            tool: "bad tool|\n\\name".into(),
            summary: "hostile tool name".into(),
            artifact: None,
        },
    ));
    let hostile = SessionExport::project(fixture_meta(), &hostile_events);
    let rendered = hostile.to_pipe(false);
    assert_eq!(
        rendered.lines().count(),
        1 + hostile.turns.len(),
        "hostile text must not add lines:\n{rendered}"
    );
    assert!(
        rendered.contains("line one\\nline two \\|with pipes\\|"),
        "escapes must be visible:\n{rendered}"
    );
    assert!(
        rendered.contains("T  1000 100 |bad tool\\|\\n\\\\name| |hostile tool name|"),
        "tool-name field must use the same one-line escaping:\n{rendered}"
    );

    // APPEND-ONLY LAW: exporting a session that only GREW yields the
    // previous bytes plus suffix lines — the same prefix stability the
    // journal has, so periodic exports diff and cache cleanly.
    let shorter = export.to_pipe(false);
    let longer = hostile.to_pipe(false);
    // Header carries the catch-up cursor; the grown export's BODY extends
    // the earlier BODY byte-for-byte (headers differ by head_seq — split
    // them off before comparing).
    let body = |rendered: &str| {
        rendered
            .split_once('\n')
            .map(|(_, body)| body.to_owned())
            .unwrap_or_default()
    };
    assert!(
        longer.contains("head_seq="),
        "header must carry the cursor:\n{longer}"
    );
    assert!(
        body(&longer).starts_with(&body(&shorter)),
        "a grown session's export body must extend the earlier body byte-for-byte"
    );

    // Every body line leads with `<kind> <seq> <at_ms>` — the row identity a
    // live subscriber keys by, so cold-rebuilt rows match live rows.
    for line in body(&longer).lines() {
        let mut parts = line.split_whitespace();
        let _kind = parts.next().expect("kind");
        let seq: u64 = parts.next().expect("seq").parse().expect("numeric seq");
        let _at: u64 = parts.next().expect("at_ms").parse().expect("numeric at");
        assert!(seq > 0 || line.starts_with("U  0"), "seq column: {line}");
    }
}

/// MUTATION CHECK (`--since <seq>`): stop filtering the projection, or
/// filter by anything but strict `seq > since`. Expected RUNTIME failure:
/// the incremental body is not exactly the suffix the full body appends
/// after the cursor.
#[test]
fn since_cursor_yields_the_exact_suffix() {
    let export = fixture_export();
    assert!(export.turns.len() >= 2, "fixture needs at least two turns");
    let cursor = export.turns[0].seq();
    let full = export.to_pipe(false);
    let mut incremental = export.clone();
    incremental.retain_after(cursor);
    let suffix = incremental.to_pipe(false);
    let body = |rendered: &str| {
        rendered
            .split_once('\n')
            .map(|(_, body)| body.to_owned())
            .unwrap_or_default()
    };
    let full_body = body(&full);
    let suffix_body = body(&suffix);
    assert!(
        full_body.ends_with(&suffix_body),
        "since-body must be the exact tail:\nfull:\n{full_body}\nsuffix:\n{suffix_body}"
    );
    assert!(
        !suffix_body.contains(&full_body.lines().next().unwrap_or_default().to_owned()),
        "the cursor'd row must not repeat"
    );
    // head_seq stays the TRUE head in both — the next cursor is always
    // current regardless of how much body was skipped.
    assert_eq!(
        full.lines().next().and_then(|line| line
            .split_whitespace()
            .find(|part| part.starts_with("head_seq="))
            .map(str::to_owned)),
        suffix.lines().next().and_then(|line| line
            .split_whitespace()
            .find(|part| part.starts_with("head_seq="))
            .map(str::to_owned)),
    );
}
