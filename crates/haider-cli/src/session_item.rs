//! On-demand transcript detail for one durable journal sequence.

use std::io::{self, Write};
use std::process::ExitCode;

use haider_client::{ObserveClient, ProfileEnv, resolve_profile};
use haider_protocol::EventPayload;
use haider_protocol::envelope::RawEnvelope;
use haider_protocol::history::NodeKind;
use haider_protocol::ids::SessionId;
use haider_protocol::pipe::{ToolExchangeJoin, TranscriptJoiner};
use haider_protocol::tool::BoundedResult;
use haider_tui::notify::mask_text;
use serde_json::{Map, Value, json};

use super::run::{EX_IOERR, EX_PROTOCOL, EX_UNAVAILABLE, EX_USAGE};

const ITEM_SCHEMA: &str = "haider.item.v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ItemOptions {
    pub(crate) seq: u64,
    pub(crate) masked: bool,
    pub(crate) no_spawn: bool,
}

#[derive(Debug)]
enum ItemError {
    Unavailable(String),
    Missing { seq: u64, head: u64 },
    Protocol(&'static str),
    Io(String),
}

impl std::fmt::Display for ItemError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(message) => formatter.write_str(message),
            Self::Missing { seq, head } => write!(
                formatter,
                "item_not_found: journal seq {seq} is absent (head_seq={head})"
            ),
            Self::Protocol(message) => formatter.write_str(message),
            Self::Io(message) => formatter.write_str(message),
        }
    }
}

pub(crate) async fn session_item_command(session_id: &str, seq: &str, rest: &[String]) -> ExitCode {
    let options = match parse_options(seq, rest) {
        Ok(Some(options)) => options,
        Ok(None) => {
            println!(
                "usage: haider session <session-id> item <seq> --json [--masked] [--no-spawn]"
            );
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("haider session item: {message}");
            return ExitCode::from(EX_USAGE);
        }
    };
    match execute(SessionId::new(session_id), options).await {
        Ok(document) => write_document(&document),
        Err(error) => {
            eprintln!("haider session item: {error}");
            ExitCode::from(match error {
                ItemError::Unavailable(_) | ItemError::Missing { .. } => EX_UNAVAILABLE,
                ItemError::Protocol(_) => EX_PROTOCOL,
                ItemError::Io(_) => EX_IOERR,
            })
        }
    }
}

pub(crate) fn parse_options(seq: &str, rest: &[String]) -> Result<Option<ItemOptions>, String> {
    if matches!(rest, [flag] if matches!(flag.as_str(), "--help" | "-h")) {
        return Ok(None);
    }
    let seq = seq
        .parse::<u64>()
        .map_err(|_| format!("item seq must be an unsigned journal sequence, got `{seq}`"))?;
    let mut json = false;
    let mut masked = false;
    let mut no_spawn = false;
    for flag in rest {
        match flag.as_str() {
            "--json" if !json => json = true,
            "--masked" if !masked => masked = true,
            "--no-spawn" if !no_spawn => no_spawn = true,
            "--json" | "--masked" | "--no-spawn" => {
                return Err(format!("duplicate {flag} flag"));
            }
            other => return Err(format!("unknown flag `{other}`")),
        }
    }
    if !json {
        return Err("the item detail door requires --json".into());
    }
    Ok(Some(ItemOptions {
        seq,
        masked,
        no_spawn,
    }))
}

async fn execute(session_id: SessionId, options: ItemOptions) -> Result<Value, ItemError> {
    let profile = resolve_profile(&ProfileEnv::capture())
        .map_err(|error| ItemError::Unavailable(error.to_string()))?;
    let observer = ObserveClient::connect(&profile, !options.no_spawn)
        .await
        .map_err(|error| ItemError::Unavailable(error.to_string()))?;
    let digest = observer
        .session(session_id.clone(), 0)
        .await
        .map_err(|error| ItemError::Unavailable(error.to_string()));
    observer.close();
    let head = digest?.head_seq;
    if options.seq > head {
        return Err(ItemError::Missing {
            seq: options.seq,
            head,
        });
    }

    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let collector = tokio::spawn(collect_target(receiver, options.seq));
    // The call item is structurally adjacent to its committed exchange node.
    // Start one envelope before it; if the result was committed earlier, a
    // second constant-memory pass below resolves that call id exactly.
    let after_seq = options.seq.saturating_sub(2);
    let stream = haider_client::observe_stream_session_after(
        &profile,
        !options.no_spawn,
        session_id.clone(),
        false,
        sender,
        after_seq,
    )
    .await;
    let (envelope, mut join) = collector
        .await
        .map_err(|_| ItemError::Protocol("item journal collector failed"))?;
    stream.map_err(|error| ItemError::Unavailable(error.to_string()))?;
    let envelope = envelope.ok_or(ItemError::Missing {
        seq: options.seq,
        head,
    })?;
    if let Some(unresolved) = join.as_ref().filter(|join| join.result.is_none()) {
        let call_id = unresolved.call_id.clone();
        let branch_id = envelope
            .branch_id
            .as_ref()
            .map(|branch| branch.as_str().to_owned());
        let run_id = envelope.run_id.as_ref().map(|run| run.as_str().to_owned());
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let collector = tokio::spawn(collect_result(
            receiver,
            call_id,
            branch_id,
            run_id,
            unresolved.tool_call_seq,
        ));
        haider_client::observe_stream_session_after(
            &profile,
            !options.no_spawn,
            session_id.clone(),
            false,
            sender,
            0,
        )
        .await
        .map_err(|error| ItemError::Unavailable(error.to_string()))?;
        if let Some((seq, result)) = collector
            .await
            .map_err(|_| ItemError::Protocol("tool result collector failed"))?
            && let Some(join) = join.as_mut()
        {
            join.tool_result_seq = Some(seq);
            join.result = Some(result);
        }
    }
    Ok(item_document(&session_id, &envelope, join, options.masked))
}

async fn collect_result(
    mut receiver: tokio::sync::mpsc::UnboundedReceiver<RawEnvelope>,
    wanted_call_id: String,
    wanted_branch_id: Option<String>,
    wanted_run_id: Option<String>,
    before_seq: u64,
) -> Option<(u64, BoundedResult)> {
    let mut found = None;
    while let Some(envelope) = receiver.recv().await {
        if envelope.seq < before_seq
            && let Ok(EventPayload::ToolResult { call_id, result }) =
                serde_json::from_value::<EventPayload>(envelope.payload.clone())
            && call_id == wanted_call_id
            && envelope.branch_id.as_ref().map(|branch| branch.as_str())
                == wanted_branch_id.as_deref()
            && envelope.run_id.as_ref().map(|run| run.as_str()) == wanted_run_id.as_deref()
        {
            // The closest preceding result is the only eligible ordinary
            // tool result; never bind an earlier reused call id.
            found = Some((envelope.seq, result));
        }
    }
    found
}

async fn collect_target(
    mut receiver: tokio::sync::mpsc::UnboundedReceiver<RawEnvelope>,
    target: u64,
) -> (Option<RawEnvelope>, Option<ToolExchangeJoin>) {
    let mut joiner = TranscriptJoiner::default();
    let mut found = None;
    let mut found_join = None;
    while let Some(envelope) = receiver.recv().await {
        if envelope.seq <= target {
            let join = joiner.observe(&envelope);
            if envelope.seq == target {
                found = Some(envelope);
                found_join = join;
            }
        } else if let Some(join) = found_join.as_mut()
            && join.result.is_none()
            && let Ok(EventPayload::ToolResult { call_id, result }) =
                serde_json::from_value::<EventPayload>(envelope.payload.clone())
            && call_id == join.call_id
            && found
                .as_ref()
                .and_then(|target| target.branch_id.as_ref())
                .map(|branch| branch.as_str())
                == envelope.branch_id.as_ref().map(|branch| branch.as_str())
            && found
                .as_ref()
                .and_then(|target| target.run_id.as_ref())
                .map(|run| run.as_str())
                == envelope.run_id.as_ref().map(|run| run.as_str())
        {
            join.tool_result_seq = Some(envelope.seq);
            join.result = Some(result);
        }
    }
    (found, found_join)
}

fn item_document(
    session_id: &SessionId,
    envelope: &RawEnvelope,
    join: Option<ToolExchangeJoin>,
    masked: bool,
) -> Value {
    let base = || {
        Map::from_iter([
            ("schema".into(), ITEM_SCHEMA.into()),
            ("session_id".into(), session_id.as_str().into()),
            ("seq".into(), envelope.seq.into()),
        ])
    };
    let Ok(payload) = serde_json::from_value::<EventPayload>(envelope.payload.clone()) else {
        return other_document(base(), payload_kind(&envelope.payload));
    };
    match payload {
        EventPayload::NodeCommitted(node) => match node.kind {
            NodeKind::ToolExchange { tool, summary, .. } => {
                let mut document = base();
                document.insert("kind".into(), "tool_exchange".into());
                document.insert("name".into(), tool.into());
                document.insert("summary".into(), masked_text(&summary, masked).into());
                if let Some(join) = join {
                    document.insert("args".into(), masked_json(join.args, masked));
                    document.insert("tool_call_seq".into(), join.tool_call_seq.into());
                    if let Some(seq) = join.tool_result_seq {
                        document.insert("tool_result_seq".into(), seq.into());
                    }
                    if let Some(result) = join.result {
                        document.insert("result".into(), bounded_result_value(result, masked));
                    }
                }
                Value::Object(document)
            }
            NodeKind::UserTurn { text, .. } => text_document(base(), "user", &text, masked),
            NodeKind::AssistantCommit { text, .. } => {
                text_document(base(), "assistant", &text, masked)
            }
            _ => other_document(base(), "node_committed"),
        },
        _ => other_document(base(), payload_kind(&envelope.payload)),
    }
}

fn text_document(mut document: Map<String, Value>, kind: &str, text: &str, masked: bool) -> Value {
    document.insert("kind".into(), kind.into());
    document.insert("text".into(), masked_text(text, masked).into());
    Value::Object(document)
}

fn other_document(mut document: Map<String, Value>, payload_kind: &str) -> Value {
    document.insert("kind".into(), "other".into());
    document.insert("payload_kind".into(), payload_kind.into());
    Value::Object(document)
}

fn payload_kind(payload: &Value) -> &str {
    payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
}

fn bounded_result_value(mut result: BoundedResult, masked: bool) -> Value {
    if masked {
        result.preview = mask_text(&result.preview);
        result.reason = result.reason.map(|reason| mask_text(&reason));
        if let Some(presentation) = result.presentation.as_mut() {
            presentation.title = mask_text(&presentation.title);
            presentation.detail = mask_text(&presentation.detail);
        }
    }
    json!({
        "preview": result.preview,
        "truncated": result.truncated,
        "status": result.status,
        "reason": result.reason,
        "presentation": result.presentation,
        "artifact": result.artifact,
        "images": result.images,
        "cursor": result.cursor,
    })
}

fn masked_json(value: Value, masked: bool) -> Value {
    if !masked {
        return value;
    }
    match value {
        Value::String(value) => mask_text(&value).into(),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| masked_json(value, true))
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, masked_json(value, true)))
                .collect(),
        ),
        other => other,
    }
}

fn masked_text(value: &str, masked: bool) -> String {
    if masked {
        mask_text(value)
    } else {
        value.to_owned()
    }
}

fn write_document(document: &Value) -> ExitCode {
    let bytes = match serde_json::to_vec(document) {
        Ok(mut bytes) => {
            bytes.push(b'\n');
            bytes
        }
        Err(error) => {
            eprintln!("haider session item: JSON serialization failed: {error}");
            return ExitCode::from(EX_PROTOCOL);
        }
    };
    if let Err(error) = io::stdout().write_all(&bytes) {
        let failure = ItemError::Io(format!("stdout failed: {error}"));
        eprintln!("haider session item: {failure}");
        return ExitCode::from(EX_IOERR);
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    /// MUTATION CHECK: accept a non-numeric seq, allow duplicate flags, or
    /// swallow unknown flags. Expected runtime failure: one of the typed
    /// usage errors below turns into `Ok`.
    #[test]
    fn parse_options_is_strict_and_typed() {
        let parsed = parse_options("41", &["--json".into(), "--masked".into()])
            .expect("valid options")
            .expect("json mode");
        assert_eq!(parsed.seq, 41);
        assert!(parsed.masked);
        assert!(!parsed.no_spawn);

        assert!(parse_options("not-a-seq", &["--json".into()]).is_err());
        assert!(
            parse_options("41", &["--json".into(), "--json".into()]).is_err(),
            "duplicate flags refuse"
        );
        assert!(
            parse_options("41", &["--json".into(), "--future".into()]).is_err(),
            "unknown flags refuse instead of being ignored"
        );
    }

    /// The absent-seq refusal names the head so a drawer can say what the
    /// journal actually holds.
    #[test]
    fn item_not_found_names_the_head() {
        let error = ItemError::Missing { seq: 99, head: 41 };
        assert_eq!(
            error.to_string(),
            "item_not_found: journal seq 99 is absent (head_seq=41)"
        );
    }
}
