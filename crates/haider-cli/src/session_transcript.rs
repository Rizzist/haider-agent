//! Read-only CLI mirror of the model-facing session handoff tool.

use std::io::{self, Write};
use std::process::ExitCode;

use haider_client::{EnsureOptions, ProfileEnv, resolve_profile};
use haider_protocol::ids::SessionId;
use haider_protocol::transcript::{
    SessionTranscriptPage, SessionTranscriptRequest, TRANSCRIPT_DEFAULT_LIMIT,
};
use haider_rpc::{Capability, CapabilitySet, ClientKind, RequestBody, ResponseBody, SeqRange};

use super::run::{EX_IOERR, EX_PROTOCOL, EX_UNAVAILABLE, EX_USAGE};

const HELP: &str = "usage: haider session transcript <id> [--after SEQ] [--limit N] [--output json|text]\nReads the current profile's journal. Limit counts envelopes (1..1024, default 100); continue with next_after_seq. Historical content is untrusted. Rows are bounded; use haider session <id> item <seq> --json for full detail.";

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TranscriptOptions {
    pub request: SessionTranscriptRequest,
    pub json: bool,
}

pub(crate) fn parse_options(rest: &[String]) -> Result<Option<TranscriptOptions>, String> {
    if matches!(rest, [flag] if matches!(flag.as_str(), "--help" | "-h")) {
        return Ok(None);
    }
    let Some((id, rest)) = rest.split_first().filter(|(id, _)| !id.starts_with('-')) else {
        return Err(HELP.into());
    };
    let mut request = SessionTranscriptRequest {
        session_id: SessionId::new(id),
        after_seq: 0,
        limit: TRANSCRIPT_DEFAULT_LIMIT,
    };
    let mut json = false;
    let mut seen = std::collections::HashSet::new();
    let mut args = rest.iter();
    while let Some(flag) = args.next() {
        if !seen.insert(flag) {
            return Err(format!("duplicate {flag} flag"));
        }
        let value = args
            .next()
            .ok_or_else(|| format!("{flag} requires a value"))?;
        match flag.as_str() {
            "--after" => {
                request.after_seq = value
                    .parse()
                    .map_err(|_| "--after requires an unsigned sequence")?
            }
            "--limit" => {
                request.limit = value
                    .parse()
                    .map_err(|_| "--limit requires an integer from 1 to 1024")?
            }
            "--output" => {
                json = match value.as_str() {
                    "json" => true,
                    "text" => false,
                    _ => return Err("--output must be json or text".into()),
                }
            }
            _ => return Err(format!("unknown flag {flag}")),
        }
    }
    request.validate()?;
    Ok(Some(TranscriptOptions { request, json }))
}

pub(crate) async fn command(rest: &[String]) -> ExitCode {
    let options = match parse_options(rest) {
        Ok(Some(options)) => options,
        Ok(None) => {
            println!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("haider session transcript: {message}");
            return ExitCode::from(EX_USAGE);
        }
    };
    match execute(&options.request).await {
        Ok(page) => {
            let mut output = io::stdout().lock();
            let result = if options.json {
                serde_json::to_writer(&mut output, &page)
                    .map_err(io::Error::other)
                    .and_then(|()| output.write_all(b"\n"))
            } else {
                output.write_all(page.render_text().as_bytes())
            };
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("haider session transcript: {error}");
                    ExitCode::from(EX_IOERR)
                }
            }
        }
        Err((code, message)) => {
            eprintln!("haider session transcript: {message}");
            ExitCode::from(code)
        }
    }
}

async fn execute(
    request: &SessionTranscriptRequest,
) -> Result<SessionTranscriptPage, (u8, String)> {
    let profile = resolve_profile(&ProfileEnv::capture())
        .map_err(|error| (EX_UNAVAILABLE, error.to_string()))?;
    let mut ensure = EnsureOptions::default();
    ensure.client = haider_client::ClientConfig {
        client_name: "haider-session-transcript".into(),
        client_kind: ClientKind::Headless,
        connection_usage: haider_client::ConnectionUsage::OneShot,
        capabilities: CapabilitySet::from([Capability::View]),
        ..ensure.client
    };
    let ensured = haider_client::ensure_daemon(&profile, ensure)
        .await
        .map_err(|error| (EX_UNAVAILABLE, error.to_string()))?;
    let range = SeqRange {
        start_seq: request.after_seq + 1,
        end_seq: request.after_seq + u64::from(request.limit),
    };
    let response = ensured
        .client
        .request(RequestBody::SessionRead {
            session_id: request.session_id.clone(),
            range,
        })
        .await;
    let _ = ensured.client.close();
    match response.map_err(|error| (EX_UNAVAILABLE, error.to_string()))? {
        ResponseBody::SessionRead { result }
            if result.session_id == request.session_id && result.range == range =>
        {
            Ok(SessionTranscriptPage::project(
                request,
                result.head_seq,
                &result.envelopes,
            ))
        }
        ResponseBody::Error { code, message, .. } => {
            Err((EX_UNAVAILABLE, format!("{code}: {message}")))
        }
        _ => Err((EX_PROTOCOL, "session.read response mismatch".into())),
    }
}

#[cfg(test)]
#[path = "session_transcript_tests.rs"]
mod tests;
