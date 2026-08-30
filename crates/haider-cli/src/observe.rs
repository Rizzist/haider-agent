//! Scriptable `haider.observe.v1` snapshots and raw-envelope streams.

use std::io::{self, Write};
use std::process::ExitCode;

use haider_client::{
    ConnectError, EnsureError, ObserveClient, ObserveError, ProfileEnv, ResolvedProfile,
    observe_stream_all, observe_stream_session, resolve_profile,
};
use haider_protocol::context::ContextFootprintTruth;
use haider_protocol::ids::SessionId;
use haider_protocol::session_fork::SessionForkProvenance;
use haider_rpc::{ObserveRunStateWire, SessionFleetSnapshot, SessionObserveDigest};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::run::{
    EX_BLOCKED, EX_IOERR, EX_PROTOCOL, EX_PROVIDER, EX_SOFTWARE, EX_TIMEOUT, EX_UNAVAILABLE,
    EX_USAGE,
};

const OBSERVE_SCHEMA: &str = "haider.observe.v1";
const LAST_EVENT_LIMIT: u32 = 20;

const STREAM_HELP: &str = "Streams are LF-framed raw event envelopes. Event payload kinds and fields are additive; consumers must tolerate unknown kinds and fields.";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SnapshotOptions {
    pub json: bool,
    pub no_spawn: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SessionsOptions {
    pub json: bool,
    pub no_spawn: bool,
    pub recovery: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionOptions {
    pub session_id: String,
    pub json: bool,
    pub watch: bool,
    pub no_spawn: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FleetOptions {
    pub session_id: Option<String>,
    pub json: bool,
    pub no_spawn: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct EventsOptions {
    pub follow: bool,
    pub no_spawn: bool,
}

pub(crate) enum Parsed<T> {
    Run(T),
    Help,
}

pub(crate) struct StatusDocument {
    pub schema: &'static str,
    pub kind: &'static str,
    pub daemon: DaemonView,
    pub update: UpdateView,
    pub features: Vec<String>,
    pub account: Option<AccountView>,
    pub session_count: u64,
    pub profile_path: String,
    pub runtime_dir: String,
    pub adoption_available: Vec<haider_rpc::AccountAdoptionAvailable>,
}

pub(crate) struct DaemonView {
    pub version: String,
    pub generation: u64,
    pub pid: Option<u32>,
    pub socket_path: String,
    pub pid_file_path: Option<String>,
    pub ready: bool,
}

pub(crate) struct UpdateView {
    pub status: &'static str,
    pub current_version: String,
    pub latest_version: Option<String>,
    pub error: Option<&'static str>,
}

pub(crate) struct AccountView {
    pub provider: String,
    pub alias: String,
}

#[derive(serde::Serialize)]
struct StatusDocumentWire<'a> {
    account: Option<StatusAccountWire<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    account_adoption_available: Option<&'a [haider_rpc::AccountAdoptionAvailable]>,
    daemon: StatusDaemonWire<'a>,
    features: &'a [String],
    kind: &'static str,
    profile_path: &'a str,
    runtime_dir: &'a str,
    schema: &'static str,
    session_count: u64,
    update: StatusUpdateWire<'a>,
}

#[derive(serde::Serialize)]
struct StatusDaemonWire<'a> {
    generation: u64,
    pid: Option<u32>,
    pid_file_path: Option<&'a str>,
    pipe_dir: String,
    ready: bool,
    socket_path: &'a str,
    version: &'a str,
}

#[derive(serde::Serialize)]
struct StatusUpdateWire<'a> {
    current_version: &'a str,
    error: Option<&'static str>,
    latest_version: Option<&'a str>,
    status: &'static str,
}

#[derive(serde::Serialize)]
struct StatusAccountWire<'a> {
    alias: &'a str,
    provider: &'a str,
}

pub(crate) struct SessionsDocument {
    pub schema: &'static str,
    pub kind: &'static str,
    pub sessions: Vec<SessionSummaryView>,
}

pub(crate) struct SessionDocument {
    pub schema: &'static str,
    pub kind: &'static str,
    pub session: SessionDepthView,
}

pub(crate) struct FleetListEntry {
    pub id: String,
    pub title: String,
    pub snapshot: SessionFleetSnapshot,
}

pub(crate) struct CacheView {
    pub lifetime_basis_points: Option<u32>,
    pub reread_basis_points: Option<u32>,
}

pub(crate) struct SessionSummaryView {
    pub id: String,
    pub title: String,
    pub run_state: &'static str,
    pub active_branch: String,
    pub branches: Vec<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub footprint: Option<FootprintView>,
    pub subagent_count: usize,
    pub updated_at: u64,
    pub parked_since: Option<u64>,
    /// Additive roster scalars merged from `session.list` (v0.0.936): the
    /// digest path predates them, so they are optional and omitted when the
    /// daemon or the merge has nothing to say.
    /// The cancel coordinates (v0.0.938 `session_run_id_v1`). `turn.cancel`
    /// takes BOTH, so a projection reporting neither leaves every session
    /// uncancellable from anything reading this JSON. Read them off the SAME
    /// summary as `run_state` — the pair is one observation.
    pub run_id: Option<String>,
    pub worker_generation: Option<u64>,
    /// Prompt-cache health, projected from the promoted roster scalars with
    /// `agent_metrics.usage` retained only as a pre-promotion-daemon fallback.
    /// TWO numbers because either alone misleads: the lifetime ratio counts the
    /// first send of new content as a miss (definitionally, so it can never
    /// reach 100%), and the re-read rate hides real cold-start cost. `None` on
    /// the re-read rate is NOT zero — a session with nothing to re-read has no
    /// rate at all.
    pub cache: Option<CacheView>,
    pub effort: Option<String>,
    pub fast: Option<bool>,
    pub agent_type: Option<String>,
    pub seen_at_ms: Option<u64>,
    pub last_activity_ms: Option<u64>,
    pub waiting_why: Option<haider_rpc::WaitingWhyWire>,
    pub needs_input: Option<haider_rpc::NeedsInputWire>,
    /// Prompt-oriented fork provenance from `session.list`. Unlike delegation
    /// lineage, this carries the exact source-prompt sequence selected as the
    /// child's editable draft.
    pub forked_from: Option<SessionForkProvenance>,
}

pub(crate) struct SessionDepthView {
    pub summary: SessionSummaryView,
    pub pending_menus: Vec<haider_rpc::ObserveMenuWire>,
    pub parked_permissions: Vec<String>,
    pub subagents: Vec<SubagentView>,
    pub branch_heads: Vec<BranchView>,
    pub last_event_kinds: Vec<String>,
}

pub(crate) struct FootprintView {
    pub truth: &'static str,
    pub tokens: u64,
}

pub(crate) struct SubagentView {
    pub id: String,
    pub callsign: Option<String>,
    pub task: String,
    pub state: String,
}

pub(crate) struct BranchView {
    pub id: Option<String>,
    pub name: String,
    pub head_node_id: Option<String>,
    pub head_seq: u64,
}

pub(crate) trait ObserveJson {
    fn json(&self) -> Value;
}

impl ObserveJson for StatusDocument {
    fn json(&self) -> Value {
        let mut document = json!({
            "schema": self.schema,
            "kind": self.kind,
            "daemon": {
                "version": self.daemon.version,
                "generation": self.daemon.generation,
                "pid": self.daemon.pid,
                "socket_path": self.daemon.socket_path,
                "pid_file_path": self.daemon.pid_file_path,
                "ready": self.daemon.ready,
                "pipe_dir": std::path::Path::new(&self.profile_path)
                    .join("pipe")
                    .display()
                    .to_string(),
            },
            "update": {
                "status": self.update.status,
                "current_version": self.update.current_version,
                "latest_version": self.update.latest_version,
                "error": self.update.error,
            },
            "features": self.features,
            "account": self.account.as_ref().map(|account| json!({
                "provider": account.provider,
                "alias": account.alias,
            })),
            "session_count": self.session_count,
            "profile_path": self.profile_path,
            "runtime_dir": self.runtime_dir,
        });
        if !self.adoption_available.is_empty() {
            document["account_adoption_available"] = json!(self.adoption_available);
        }
        document
    }
}

impl ObserveJson for SessionsDocument {
    fn json(&self) -> Value {
        json!({
            "schema": self.schema,
            "kind": self.kind,
            "sessions": self.sessions.iter().map(SessionSummaryView::json).collect::<Vec<_>>(),
        })
    }
}

impl ObserveJson for SessionDocument {
    fn json(&self) -> Value {
        json!({
            "schema": self.schema,
            "kind": self.kind,
            "session": self.session.json(),
        })
    }
}

impl ObserveJson for SessionSummaryView {
    fn json(&self) -> Value {
        let mut object = json!({
            // `id` is the historical CLI name; `session_id` is what every RPC
            // surface calls the same field. A consumer reading both surfaces
            // would otherwise see two key names for one thing and silently
            // fail to join the rows — which is exactly how a bridge lost
            // run_id for hours. Emit both; `id` stays for existing readers.
            "id": self.id,
            "session_id": self.id,
            "title": self.title,
            "run_state": self.run_state,
            "active_branch": self.active_branch,
            "branches": self.branches,
            "provider": self.provider,
            "model": self.model,
            "footprint": self.footprint.as_ref().map(|footprint| json!({
                "truth": footprint.truth,
                "tokens": footprint.tokens,
            })),
            "subagent_count": self.subagent_count,
            "updated_at": self.updated_at,
        });
        if let Some(run_id) = &self.run_id {
            object["run_id"] = json!(run_id);
        }
        if let Some(generation) = self.worker_generation {
            object["worker_generation"] = json!(generation);
        }
        if let Some(cache) = &self.cache {
            // Each rate is emitted only when the daemon measured it. An absent
            // rate means "not measured", which must never be readable as 0%.
            let mut view = serde_json::Map::new();
            if let Some(points) = cache.lifetime_basis_points {
                view.insert("lifetime_basis_points".into(), json!(points));
            }
            if let Some(points) = cache.reread_basis_points {
                view.insert("reread_basis_points".into(), json!(points));
            }
            if !view.is_empty() {
                object["cache"] = Value::Object(view);
            }
        }
        if let Some(parked_since) = self.parked_since {
            object["parked_since"] = json!(parked_since);
        }
        if let Some(effort) = &self.effort {
            object["effort"] = json!(effort);
        }
        if let Some(fast) = self.fast {
            object["fast"] = json!(fast);
        }
        if let Some(agent_type) = &self.agent_type {
            object["agent_type"] = json!(agent_type);
        }
        if let Some(seen_at_ms) = self.seen_at_ms {
            object["seen_at_ms"] = json!(seen_at_ms);
        }
        if let Some(last_activity_ms) = self.last_activity_ms {
            object["last_activity_ms"] = json!(last_activity_ms);
        }
        if let Some(waiting_why) = &self.waiting_why {
            object["waiting_why"] = serde_json::to_value(waiting_why).unwrap_or(Value::Null);
        }
        if let Some(needs_input) = &self.needs_input {
            object["needs_input"] = serde_json::to_value(needs_input).unwrap_or(Value::Null);
        }
        if let Some(provenance) = &self.forked_from {
            object["forked_from"] = json!({
                "session_id": provenance.session_id.as_str(),
                "seq": provenance.seq,
            });
        }
        object
    }
}

impl ObserveJson for SessionDepthView {
    fn json(&self) -> Value {
        let mut object = self.summary.json().as_object().cloned().unwrap_or_default();
        object.insert(
            "pending_menus".into(),
            Value::Array(
                self.pending_menus
                    .iter()
                    .map(|menu| {
                        json!({
                            "kind": menu.kind,
                            "title": menu.title,
                            "permission_description": menu.permission_description,
                        })
                    })
                    .collect(),
            ),
        );
        object.insert("parked_permissions".into(), json!(self.parked_permissions));
        object.insert(
            "subagents".into(),
            Value::Array(
                self.subagents
                    .iter()
                    .map(|subagent| {
                        json!({
                            "id": subagent.id,
                            "callsign": subagent.callsign,
                            "task": subagent.task,
                            "state": subagent.state,
                        })
                    })
                    .collect(),
            ),
        );
        object.insert(
            "branch_heads".into(),
            Value::Array(
                self.branch_heads
                    .iter()
                    .map(|branch| {
                        json!({
                            "id": branch.id,
                            "name": branch.name,
                            "head_node_id": branch.head_node_id,
                            "head_seq": branch.head_seq,
                        })
                    })
                    .collect(),
            ),
        );
        object.insert("last_event_kinds".into(), json!(self.last_event_kinds));
        Value::Object(object)
    }
}

pub(crate) fn parse_snapshot_options(
    rest: &[String],
    command: &str,
) -> Result<Parsed<SnapshotOptions>, String> {
    let mut options = SnapshotOptions::default();
    for flag in rest {
        match flag.as_str() {
            "--json" if !options.json => options.json = true,
            "--no-spawn" if !options.no_spawn => options.no_spawn = true,
            "--help" | "-h" if rest.len() == 1 => return Ok(Parsed::Help),
            "--json" | "--no-spawn" => return Err(format!("duplicate {flag} flag")),
            _ => return Err(format!("usage: haider {command} [--json] [--no-spawn]")),
        }
    }
    Ok(Parsed::Run(options))
}

pub(crate) fn parse_session_options(rest: &[String]) -> Result<Parsed<SessionOptions>, String> {
    if matches!(rest, [flag] if matches!(flag.as_str(), "--help" | "-h")) {
        return Ok(Parsed::Help);
    }
    let mut session_id = None;
    let mut json = false;
    let mut watch = false;
    let mut no_spawn = false;
    for value in rest {
        match value.as_str() {
            "--json" if !json => json = true,
            "--watch" if !watch => watch = true,
            "--no-spawn" if !no_spawn => no_spawn = true,
            "--json" | "--watch" | "--no-spawn" => {
                return Err(format!("duplicate {value} flag"));
            }
            flag if flag.starts_with('-') => return Err(format!("unknown flag `{flag}`")),
            id if session_id.is_none() && !id.is_empty() => session_id = Some(id.to_owned()),
            _ => return Err("usage: haider session <id> [--json|--watch] [--no-spawn]".into()),
        }
    }
    if json && watch {
        return Err("--json and --watch are mutually exclusive; watch is raw JSONL".into());
    }
    Ok(Parsed::Run(SessionOptions {
        session_id: session_id
            .ok_or_else(|| "usage: haider session <id> [--json|--watch] [--no-spawn]".to_owned())?,
        json,
        watch,
        no_spawn,
    }))
}

pub(crate) fn parse_sessions_options(rest: &[String]) -> Result<Parsed<SessionsOptions>, String> {
    if matches!(rest, [flag] if matches!(flag.as_str(), "--help" | "-h")) {
        return Ok(Parsed::Help);
    }
    let mut options = SessionsOptions::default();
    for flag in rest {
        match flag.as_str() {
            "--json" if !options.json => options.json = true,
            "--no-spawn" if !options.no_spawn => options.no_spawn = true,
            "--recovery" if !options.recovery => options.recovery = true,
            "--json" | "--no-spawn" | "--recovery" => {
                return Err(format!("duplicate {flag} flag"));
            }
            _ => {
                return Err("usage: haider sessions [--recovery] [--json] [--no-spawn]".into());
            }
        }
    }
    Ok(Parsed::Run(options))
}

pub(crate) fn parse_fleet_options(rest: &[String]) -> Result<Parsed<FleetOptions>, String> {
    if matches!(rest, [flag] if matches!(flag.as_str(), "--help" | "-h")) {
        return Ok(Parsed::Help);
    }
    let mut session_id = None;
    let mut json = false;
    let mut no_spawn = false;
    for value in rest {
        match value.as_str() {
            "--json" if !json => json = true,
            "--no-spawn" if !no_spawn => no_spawn = true,
            "--json" | "--no-spawn" => return Err(format!("duplicate {value} flag")),
            flag if flag.starts_with('-') => return Err(format!("unknown flag `{flag}`")),
            id if session_id.is_none() && !id.is_empty() => session_id = Some(id.to_owned()),
            _ => {
                return Err("usage: haider fleet [<session-id>] [--json] [--no-spawn]".into());
            }
        }
    }
    if json && session_id.is_none() {
        return Err("--json requires a session id for the raw fleet response".into());
    }
    Ok(Parsed::Run(FleetOptions {
        session_id,
        json,
        no_spawn,
    }))
}

pub(crate) fn parse_events_options(rest: &[String]) -> Result<Parsed<EventsOptions>, String> {
    if matches!(rest, [flag] if matches!(flag.as_str(), "--help" | "-h")) {
        return Ok(Parsed::Help);
    }
    let mut options = EventsOptions::default();
    for flag in rest {
        match flag.as_str() {
            "--follow" if !options.follow => options.follow = true,
            "--no-spawn" if !options.no_spawn => options.no_spawn = true,
            "--follow" | "--no-spawn" => return Err(format!("duplicate {flag} flag")),
            _ => return Err("usage: haider events [--follow] [--no-spawn]".into()),
        }
    }
    Ok(Parsed::Run(options))
}

pub(crate) async fn status_command(rest: &[String]) -> ExitCode {
    let options = match parse_snapshot_options(rest, "status") {
        Ok(Parsed::Run(options)) => options,
        Ok(Parsed::Help) => return write_help("usage: haider status [--json] [--no-spawn]"),
        Err(error) => return usage("status", &error),
    };
    let profile = match profile() {
        Ok(profile) => profile,
        Err(code) => return code,
    };
    let observer = match ObserveClient::connect_one_shot(&profile, !options.no_spawn).await {
        Ok(observer) => observer,
        Err(error) => return observe_failure("status", &error),
    };
    let snapshot = match observer.status_snapshot().await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let _ = observer.close();
            return observe_failure("status", &error);
        }
    };
    let account = snapshot.active_account.map(|descriptor| AccountView {
        provider: descriptor.provider,
        alias: descriptor.alias.as_str().to_owned(),
    });
    let socket_path = snapshot
        .socket_path
        .unwrap_or_else(|| profile.endpoint_path.display().to_string());
    let pid_file_path = snapshot.pid_file_path;
    let welcome = observer.into_welcome();
    let update = stamp_update_view(&profile.store_dir);
    let document = StatusDocument {
        schema: OBSERVE_SCHEMA,
        kind: "status",
        daemon: DaemonView {
            version: welcome.daemon_version,
            generation: welcome.daemon_generation,
            pid: snapshot.daemon_pid,
            socket_path,
            pid_file_path,
            ready: snapshot.ready,
        },
        update,
        features: welcome.features.into_iter().collect(),
        account,
        session_count: snapshot.session_count,
        profile_path: profile.store_dir.display().to_string(),
        runtime_dir: profile.runtime_dir.display().to_string(),
        adoption_available: snapshot.adoption_available,
    };
    if options.json {
        write_status_document(&document)
    } else {
        write_status_human(&document)
    }
}

pub(crate) async fn sessions_command(rest: &[String]) -> ExitCode {
    let options = match parse_sessions_options(rest) {
        Ok(Parsed::Run(options)) => options,
        Ok(Parsed::Help) => {
            return write_help("usage: haider sessions [--recovery] [--json] [--no-spawn]");
        }
        Err(error) => return usage("sessions", &error),
    };
    let profile = match profile() {
        Ok(profile) => profile,
        Err(code) => return code,
    };
    let observer = match ObserveClient::connect(&profile, !options.no_spawn).await {
        Ok(observer) => observer,
        Err(error) => return observe_failure("sessions", &error),
    };
    let digests = if options.recovery {
        match observer.require_effect_recovery_feature() {
            Ok(()) => {
                let summaries = match observer.session_summaries().await {
                    Ok(summaries) => summaries,
                    Err(error) => {
                        let _ = observer.close();
                        return observe_failure("sessions", &error);
                    }
                };
                let mut digests = Vec::new();
                for summary in summaries
                    .into_iter()
                    .filter(|summary| summary.run_state == Some(ObserveRunStateWire::EffectUnknown))
                {
                    let title = summary.title;
                    match observer.session(summary.session_id, 0).await {
                        Ok(mut digest) => {
                            if let Some(title) = title {
                                digest.title = title;
                            }
                            digests.push(digest);
                        }
                        Err(error) => {
                            let _ = observer.close();
                            return observe_failure("sessions", &error);
                        }
                    }
                }
                Ok(digests)
            }
            Err(error) => Err(error),
        }
    } else {
        observer.sessions(0).await
    };
    // Roster scalars are additive garnish: an error here must not fail the
    // listing (an older daemon simply has none).
    let observer_summaries = observer.session_summaries().await.ok();
    let _ = observer.close();
    let digests = match digests {
        Ok(digests) => digests,
        Err(error) => return observe_failure("sessions", &error),
    };
    // The digest path predates the roster scalars; one session.list call
    // fetches them for every row (tolerating an older daemon by leaving the
    // fields absent, exactly like the wire).
    let roster: std::collections::HashMap<_, _> = match observer_summaries {
        Some(summaries) => summaries
            .into_iter()
            .map(|summary| (summary.session_id.clone(), summary))
            .collect(),
        None => std::collections::HashMap::new(),
    };
    let sessions = digests
        .into_iter()
        .filter(|digest| {
            !options.recovery || digest.run_state == ObserveRunStateWire::EffectUnknown
        })
        .map(|digest| {
            let scalars = roster.get(&digest.session_id);
            let mut view = summary_view(digest);
            if let Some(summary) = scalars {
                merge_roster_summary(&mut view, summary);
            }
            view
        })
        .collect();
    let document = SessionsDocument {
        schema: OBSERVE_SCHEMA,
        kind: "sessions",
        sessions,
    };
    if options.json {
        write_document(&document)
    } else if options.recovery {
        write_human(recovery_sessions_human_text(&document))
    } else {
        write_sessions_human(&document)
    }
}

pub(crate) async fn session_command(rest: &[String]) -> ExitCode {
    let options = match parse_session_options(rest) {
        Ok(Parsed::Run(options)) => options,
        Ok(Parsed::Help) => {
            return write_help(&format!(
                "usage: haider session <id> [--json|--watch] [--no-spawn]\n{STREAM_HELP}"
            ));
        }
        Err(error) => return usage("session", &error),
    };
    let profile = match profile() {
        Ok(profile) => profile,
        Err(code) => return code,
    };
    if options.watch {
        return stream_session(&profile, &options).await;
    }
    let observer = match ObserveClient::connect(&profile, !options.no_spawn).await {
        Ok(observer) => observer,
        Err(error) => return observe_failure("session", &error),
    };
    let digest = observer
        .session(SessionId::new(options.session_id), LAST_EVENT_LIMIT)
        .await;
    let _ = observer.close();
    let digest = match digest {
        Ok(digest) => digest,
        Err(error) => return observe_failure("session", &error),
    };
    let document = SessionDocument {
        schema: OBSERVE_SCHEMA,
        kind: "session",
        session: depth_view(digest),
    };
    if options.json {
        write_document(&document)
    } else {
        write_session_human(&document)
    }
}

pub(crate) async fn fleet_command(rest: &[String]) -> ExitCode {
    let options = match parse_fleet_options(rest) {
        Ok(Parsed::Run(options)) => options,
        Ok(Parsed::Help) => {
            return write_help("usage: haider fleet [<session-id>] [--json] [--no-spawn]");
        }
        Err(error) => return usage("fleet", &error),
    };
    let profile = match profile() {
        Ok(profile) => profile,
        Err(code) => return code,
    };
    let observer = match ObserveClient::connect(&profile, !options.no_spawn).await {
        Ok(observer) => observer,
        Err(error) => return observe_failure("fleet", &error),
    };
    if let Err(error) = observer.require_fleet_feature() {
        let _ = observer.close();
        return observe_failure("fleet", &error);
    }
    if let Some(session_id) = options.session_id {
        let snapshot = observer.fleet(SessionId::new(session_id)).await;
        let _ = observer.close();
        let snapshot = match snapshot {
            Ok(snapshot) => snapshot,
            Err(error) => return observe_failure("fleet", &error),
        };
        return if options.json {
            write_fleet_json(&snapshot)
        } else {
            write_human(fleet_human_text(&snapshot))
        };
    }

    let digests = observer.sessions(0).await;
    let digests = match digests {
        Ok(digests) => fleet_candidates(digests),
        Err(error) => {
            let _ = observer.close();
            return observe_failure("fleet", &error);
        }
    };
    let mut entries = Vec::with_capacity(digests.len());
    for digest in digests {
        let snapshot = match observer.fleet(digest.session_id.clone()).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let _ = observer.close();
                return observe_failure("fleet", &error);
            }
        };
        entries.push(FleetListEntry {
            id: digest.session_id.as_str().to_owned(),
            title: digest.title,
            snapshot,
        });
    }
    let _ = observer.close();
    write_human(fleet_list_human_text(&entries))
}

pub(crate) async fn events_command(rest: &[String]) -> ExitCode {
    let options = match parse_events_options(rest) {
        Ok(Parsed::Run(options)) => options,
        Ok(Parsed::Help) => {
            return write_help(&format!(
                "usage: haider events [--follow] [--no-spawn]\n{STREAM_HELP}"
            ));
        }
        Err(error) => return usage("events", &error),
    };
    let profile = match profile() {
        Ok(profile) => profile,
        Err(code) => return code,
    };
    stream_all(&profile, options).await
}

async fn stream_session(profile: &ResolvedProfile, options: &SessionOptions) -> ExitCode {
    let (sender, receiver) = mpsc::unbounded_channel();
    let adapter = tokio::task::spawn_blocking(move || write_envelopes(receiver));
    let result = observe_stream_session(
        profile,
        !options.no_spawn,
        SessionId::new(options.session_id.clone()),
        true,
        sender,
    )
    .await;
    finish_stream("session", result, adapter).await
}

async fn stream_all(profile: &ResolvedProfile, options: EventsOptions) -> ExitCode {
    let (sender, receiver) = mpsc::unbounded_channel();
    let adapter = tokio::task::spawn_blocking(move || write_envelopes(receiver));
    let result = observe_stream_all(profile, !options.no_spawn, options.follow, sender).await;
    finish_stream("events", result, adapter).await
}

async fn finish_stream(
    command: &str,
    result: Result<(), ObserveError>,
    adapter: tokio::task::JoinHandle<io::Result<()>>,
) -> ExitCode {
    match adapter.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            eprintln!("haider {command}: stdout failed: {error}");
            return ExitCode::from(EX_IOERR);
        }
        Err(error) => {
            eprintln!("haider {command}: output adapter failed: {error}");
            return ExitCode::from(EX_SOFTWARE);
        }
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => observe_failure(command, &error),
    }
}

fn write_envelopes(
    mut receiver: mpsc::UnboundedReceiver<haider_protocol::envelope::RawEnvelope>,
) -> io::Result<()> {
    let stdout = io::stdout();
    let mut output = io::BufWriter::new(stdout.lock());
    while let Some(envelope) = receiver.blocking_recv() {
        write_raw_envelope_jsonl(&mut output, &envelope)?;
    }
    Ok(())
}

/// Writes exactly one raw envelope followed by one LF. No observation wrapper
/// is permitted on the streaming contract.
pub(crate) fn write_raw_envelope_jsonl(
    mut output: impl Write,
    envelope: &haider_protocol::envelope::RawEnvelope,
) -> io::Result<()> {
    serde_json::to_writer(&mut output, envelope).map_err(io::Error::other)?;
    output.write_all(b"\n")?;
    output.flush()
}

fn profile() -> Result<ResolvedProfile, ExitCode> {
    resolve_profile(&ProfileEnv::capture()).map_err(|error| {
        eprintln!("haider: {error}");
        ExitCode::from(EX_PROTOCOL)
    })
}

/// v0.0.935: `haider status` never runs the release check itself — the
/// synchronous discovery curl cost seconds per invocation. It reports the
/// profile's six-hour stamp cache instead: `checked_recently` when an
/// automatic check ran within the interval, `check_due` when the next TUI
/// start or an explicit `haider update` will perform one. Status therefore
/// completes without any network traffic.
pub(crate) fn stamp_update_view(profile_dir: &std::path::Path) -> UpdateView {
    use super::update::check_policy::{check_due, last_check_stamp, unix_timestamp_now};
    let last = last_check_stamp(profile_dir);
    let status = if check_due(last, unix_timestamp_now()) {
        "check_due"
    } else {
        "checked_recently"
    };
    UpdateView {
        status,
        current_version: super::VERSION.to_owned(),
        latest_version: None,
        error: None,
    }
}

pub(crate) fn summary_view(digest: SessionObserveDigest) -> SessionSummaryView {
    let branches = std::iter::once("main".to_owned())
        .chain(
            digest
                .branches
                .iter()
                .map(|branch| branch.branch_id.as_str().to_owned()),
        )
        .collect();
    SessionSummaryView {
        id: digest.session_id.as_str().to_owned(),
        effort: None,
        fast: None,
        agent_type: None,
        seen_at_ms: None,
        last_activity_ms: None,
        waiting_why: None,
        needs_input: None,
        forked_from: None,
        title: digest.title,
        run_state: run_state_name(digest.run_state),
        run_id: digest.run_id.as_ref().map(|run| run.as_str().to_owned()),
        worker_generation: Some(digest.worker_generation),
        cache: digest
            .agent_metrics
            .as_ref()
            .and_then(|metrics| metrics.usage.as_ref())
            .map(|usage| CacheView {
                lifetime_basis_points: usage.cache_hit_basis_points,
                reread_basis_points: usage.cache_reread_hit_basis_points,
            }),
        active_branch: digest
            .active_branch_id
            .as_ref()
            .map_or_else(|| "main".to_owned(), |branch| branch.as_str().to_owned()),
        branches,
        provider: digest
            .metadata
            .as_ref()
            .map(|metadata| metadata.provider.clone()),
        model: digest
            .metadata
            .as_ref()
            .map(|metadata| metadata.model.clone()),
        footprint: digest
            .latest_context_footprint
            .map(|footprint| FootprintView {
                truth: match footprint.truth {
                    ContextFootprintTruth::Exact => "exact",
                    ContextFootprintTruth::Estimated => "estimated",
                },
                tokens: footprint.used_tokens,
            }),
        subagent_count: digest.subagents.len(),
        updated_at: digest.updated_at_ms,
        parked_since: digest
            .pending_menus
            .iter()
            .filter(|menu| menu.kind == "recovery")
            .filter_map(|menu| menu.opened_at_ms)
            .min(),
    }
}

/// Merge additive `session.list` truth into the older observe-digest view.
/// Missing promoted fields preserve the digest metadata fallback so clients
/// remain useful with pre-promotion daemons.
pub(crate) fn merge_roster_summary(
    view: &mut SessionSummaryView,
    summary: &haider_rpc::SessionSummary,
) {
    // The roster summary is the LIVE observation. Take each typed value when
    // present; a 0.0.942 summary can carry `last_model` but has no promoted
    // provider, so the digest's nested metadata remains its compatibility
    // source.
    if let Some(provider) = &summary.provider {
        view.provider = Some(provider.clone());
    }
    if let Some(model) = &summary.last_model {
        view.model = Some(model.clone());
    }
    let promoted_cache = CacheView {
        lifetime_basis_points: summary.cache_lifetime_hit_basis_points,
        reread_basis_points: summary.cache_reread_hit_basis_points,
    };
    view.cache = if promoted_cache.lifetime_basis_points.is_some()
        || promoted_cache.reread_basis_points.is_some()
    {
        Some(promoted_cache)
    } else {
        // A v0.0.942 daemon has no promoted scalars. Its nested snapshot is
        // still the compatibility authority and stays readable for as long as
        // clients can connect to that daemon generation.
        summary
            .agent_metrics
            .as_ref()
            .and_then(|metrics| metrics.usage.as_ref())
            .map(|usage| CacheView {
                lifetime_basis_points: usage.cache_hit_basis_points,
                reread_basis_points: usage.cache_reread_hit_basis_points,
            })
    };
    // The cancel coordinates are another pair from the same live summary.
    view.run_id = summary.run_id.as_ref().map(|run| run.as_str().to_owned());
    view.worker_generation = Some(summary.worker_generation);
    view.effort = summary.effort.clone();
    view.fast = summary.fast;
    view.agent_type = summary.agent_type.clone();
    view.seen_at_ms = summary.seen_at_ms;
    view.last_activity_ms = summary.last_activity_ms;
    view.waiting_why = summary.waiting_why.clone();
    view.needs_input = summary.needs_input.clone();
    view.forked_from = summary.forked_from.clone();
}

pub(crate) fn depth_view(digest: SessionObserveDigest) -> SessionDepthView {
    let branch_heads = std::iter::once(BranchView {
        id: None,
        name: "main".into(),
        head_node_id: digest
            .main_head_node_id
            .as_ref()
            .map(|node| node.as_str().to_owned()),
        head_seq: digest.main_head_seq,
    })
    .chain(digest.branches.iter().map(|branch| BranchView {
        id: Some(branch.branch_id.as_str().to_owned()),
        name: branch.name.clone(),
        head_node_id: Some(branch.head_node_id.as_str().to_owned()),
        head_seq: branch.head_seq,
    }))
    .collect();
    let parked_permissions = digest
        .pending_menus
        .iter()
        .filter_map(|menu| menu.permission_description.clone())
        .collect();
    let subagents = digest
        .subagents
        .iter()
        .map(|subagent| SubagentView {
            id: subagent.agent_id.as_str().to_owned(),
            callsign: subagent.callsign.clone(),
            task: subagent.task.clone(),
            state: subagent.state.clone(),
        })
        .collect();
    let pending_menus = digest.pending_menus.clone();
    let last_event_kinds = digest.last_event_kinds.clone();
    SessionDepthView {
        summary: summary_view(digest),
        pending_menus,
        parked_permissions,
        subagents,
        branch_heads,
        last_event_kinds,
    }
}

/// The bare fleet picker's inclusion and order law: only session digests
/// with durable subagent rows, newest first with id as the stable tie-break.
pub(crate) fn fleet_candidates(
    mut digests: Vec<SessionObserveDigest>,
) -> Vec<SessionObserveDigest> {
    digests.retain(|digest| !digest.subagents.is_empty());
    digests.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| left.session_id.as_str().cmp(right.session_id.as_str()))
    });
    digests
}

pub(crate) fn fleet_human_text(snapshot: &SessionFleetSnapshot) -> String {
    use haider_tui::fleet;
    let mut text = fleet::header_line(&fleet::rollup(&snapshot.roots));
    text.push('\n');
    for row in fleet::flatten(&snapshot.roots) {
        text.push_str("  ");
        text.push_str(&" │ ".repeat(row.rel_depth));
        text.push_str(fleet::state_glyph(row.node.state));
        text.push(' ');
        text.push_str(fleet::callsign(row.node));
        if let Some(marker) = fleet::child_marker(row.node) {
            text.push(' ');
            text.push_str(&marker);
        }
        if !row.node.task.is_empty() {
            text.push_str(" — ");
            text.push_str(&row.node.task.replace('\n', " "));
        }
        let metric = fleet::node_metric(row.node);
        if !metric.is_empty() {
            text.push_str(" · ");
            text.push_str(&metric);
        }
        text.push('\n');
    }
    if let Some(footer) = fleet::truncation_footer(snapshot) {
        text.push_str(&footer);
        text.push('\n');
    }
    text
}

pub(crate) fn fleet_list_human_text(entries: &[FleetListEntry]) -> String {
    if entries.is_empty() {
        return "no sessions with subagents\n".to_owned();
    }
    let mut text = String::new();
    for entry in entries {
        text.push_str(&format!(
            "{} · {} · {}\n",
            entry.id,
            entry.title.replace('\n', " "),
            haider_tui::fleet::header_line(&haider_tui::fleet::rollup(&entry.snapshot.roots))
        ));
    }
    text
}

fn run_state_name(state: ObserveRunStateWire) -> &'static str {
    match state {
        ObserveRunStateWire::Idle => "idle",
        ObserveRunStateWire::Running => "running",
        ObserveRunStateWire::EffectUnknown => "effect_unknown",
        ObserveRunStateWire::ParkedPermission => "parked_permission",
        ObserveRunStateWire::ParkedInput => "parked_input",
        ObserveRunStateWire::Errored => "errored",
        ObserveRunStateWire::Cancelled => "cancelled",
        ObserveRunStateWire::Unknown => "unknown",
        _ => "unknown",
    }
}

fn write_document(document: &impl ObserveJson) -> ExitCode {
    write_serializable(&document.json())
}

fn write_status_document(document: &StatusDocument) -> ExitCode {
    let wire = StatusDocumentWire {
        schema: document.schema,
        kind: document.kind,
        daemon: StatusDaemonWire {
            version: &document.daemon.version,
            generation: document.daemon.generation,
            pid: document.daemon.pid,
            socket_path: &document.daemon.socket_path,
            pid_file_path: document.daemon.pid_file_path.as_deref(),
            ready: document.daemon.ready,
            pipe_dir: std::path::Path::new(&document.profile_path)
                .join("pipe")
                .display()
                .to_string(),
        },
        update: StatusUpdateWire {
            status: document.update.status,
            current_version: &document.update.current_version,
            latest_version: document.update.latest_version.as_deref(),
            error: document.update.error,
        },
        features: &document.features,
        account: document.account.as_ref().map(|account| StatusAccountWire {
            provider: &account.provider,
            alias: &account.alias,
        }),
        session_count: document.session_count,
        profile_path: &document.profile_path,
        runtime_dir: &document.runtime_dir,
        account_adoption_available: (!document.adoption_available.is_empty())
            .then_some(document.adoption_available.as_slice()),
    };
    write_serializable(&wire)
}

fn write_serializable(document: &impl serde::Serialize) -> ExitCode {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if let Err(error) = serde_json::to_writer(&mut output, document)
        .map_err(io::Error::other)
        .and_then(|()| output.write_all(b"\n"))
        .and_then(|()| output.flush())
    {
        eprintln!("haider: stdout failed: {error}");
        return ExitCode::from(EX_IOERR);
    }
    ExitCode::SUCCESS
}

fn write_fleet_json(snapshot: &SessionFleetSnapshot) -> ExitCode {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if let Err(error) = serde_json::to_writer(&mut output, snapshot)
        .map_err(io::Error::other)
        .and_then(|()| output.write_all(b"\n"))
        .and_then(|()| output.flush())
    {
        eprintln!("haider: stdout failed: {error}");
        return ExitCode::from(EX_IOERR);
    }
    ExitCode::SUCCESS
}

fn write_status_human(document: &StatusDocument) -> ExitCode {
    let account = document.account.as_ref().map_or_else(
        || "none".to_owned(),
        |account| format!("{} ({})", account.alias, account.provider),
    );
    let update = document.update.latest_version.as_ref().map_or_else(
        || document.update.status.to_owned(),
        |version| format!("available ({version})"),
    );
    let mut text = format!(
        "daemon {} (generation {}, pid {}, ready {})\nsocket: {}\npid file: {}\nupdate: {update}\naccount: {account}\nsessions: {}\nprofile: {}\nruntime: {}\nfeatures: {}\n",
        document.daemon.version,
        document.daemon.generation,
        document
            .daemon
            .pid
            .map_or_else(|| "unknown".to_owned(), |pid| pid.to_string()),
        document.daemon.ready,
        document.daemon.socket_path,
        document
            .daemon
            .pid_file_path
            .as_deref()
            .unwrap_or("unknown"),
        document.session_count,
        document.profile_path,
        document.runtime_dir,
        document.features.join(", ")
    );
    for notice in &document.adoption_available {
        let email = notice.email.as_deref().unwrap_or("unknown account");
        text.push_str(&format!(
            "account adoption available: {} ({email}) — haider account import {} --confirm\n",
            notice.source, notice.source
        ));
    }
    write_human(text)
}

fn write_sessions_human(document: &SessionsDocument) -> ExitCode {
    write_human(sessions_human_text(document))
}

pub(crate) fn sessions_human_text(document: &SessionsDocument) -> String {
    let mut text = String::new();
    for session in &document.sessions {
        let footprint = session.footprint.as_ref().map_or_else(
            || "unknown".to_owned(),
            |footprint| format!("{}:{}", footprint.truth, footprint.tokens),
        );
        text.push_str(&format!(
            "{}  {}  branch={} [{}]  {}/{}  footprint={}  subagents={}  updated_at={}  {}\n",
            session.id,
            session.run_state,
            session.active_branch,
            session.branches.join(","),
            session.provider.as_deref().unwrap_or("unknown"),
            session.model.as_deref().unwrap_or("unknown"),
            footprint,
            session.subagent_count,
            session.updated_at,
            session.title.replace('\n', " ")
        ));
    }
    text
}

pub(crate) fn recovery_sessions_human_text(document: &SessionsDocument) -> String {
    if document.sessions.is_empty() {
        return "no parked crash windows\n".to_owned();
    }
    let mut text = String::new();
    for session in &document.sessions {
        text.push_str(&format!(
            "{}  parked_since={}  {}\n",
            session.id,
            session
                .parked_since
                .map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
            session.title.replace('\n', " ")
        ));
    }
    text
}

fn write_session_human(document: &SessionDocument) -> ExitCode {
    write_human(session_human_text(document))
}

pub(crate) fn session_human_text(document: &SessionDocument) -> String {
    let session = &document.session;
    let footprint = session.summary.footprint.as_ref().map_or_else(
        || "unknown".to_owned(),
        |footprint| format!("{} {} tokens", footprint.truth, footprint.tokens),
    );
    let mut text = format!(
        "{} — {}\nstate: {}\nbranch: {}\nprovider/model: {}/{}\nfootprint: {footprint}\nsubagents: {}\nupdated_at: {}\n",
        session.summary.id,
        session.summary.title.replace('\n', " "),
        session.summary.run_state,
        session.summary.active_branch,
        session.summary.provider.as_deref().unwrap_or("unknown"),
        session.summary.model.as_deref().unwrap_or("unknown"),
        session.summary.subagent_count,
        session.summary.updated_at,
    );
    for menu in &session.pending_menus {
        let description = menu
            .permission_description
            .as_deref()
            .map_or_else(String::new, |description| format!(" — {description}"));
        text.push_str(&format!(
            "menu: {} — {}{description}\n",
            menu.kind,
            menu.title.replace('\n', " ")
        ));
    }
    for subagent in &session.subagents {
        text.push_str(&format!(
            "subagent: {} ({}) — {} — {}\n",
            subagent.callsign.as_deref().unwrap_or(&subagent.id),
            subagent.id,
            subagent.state,
            subagent.task.replace('\n', " ")
        ));
    }
    for branch in &session.branch_heads {
        text.push_str(&format!("branch: {} @ {}\n", branch.name, branch.head_seq));
    }
    if !session.last_event_kinds.is_empty() {
        text.push_str(&format!(
            "events: {}\n",
            session.last_event_kinds.join(", ")
        ));
    }
    text
}

fn write_human(text: String) -> ExitCode {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    if let Err(error) = output
        .write_all(text.as_bytes())
        .and_then(|()| output.flush())
    {
        eprintln!("haider: stdout failed: {error}");
        ExitCode::from(EX_IOERR)
    } else {
        ExitCode::SUCCESS
    }
}

fn write_help(text: &str) -> ExitCode {
    write_human(format!("{text}\n"))
}

fn usage(command: &str, error: &str) -> ExitCode {
    eprintln!("haider {command}: {error}");
    ExitCode::from(EX_USAGE)
}

fn observe_failure(command: &str, error: &ObserveError) -> ExitCode {
    eprintln!("haider {command}: {error}");
    ExitCode::from(exit_code_for_observe_error(error))
}

pub(crate) fn exit_code_for_observe_error(error: &ObserveError) -> u8 {
    match error {
        ObserveError::NoDaemon(_) | ObserveError::NotReady(_) => EX_UNAVAILABLE,
        ObserveError::Ensure(
            EnsureError::ProtocolMismatch(_)
            | EnsureError::MissingFeatures { .. }
            | EnsureError::ProfileMismatch { .. },
        )
        | ObserveError::ProfileMismatch { .. }
        | ObserveError::MissingFeature(_)
        | ObserveError::Protocol(_) => EX_PROTOCOL,
        ObserveError::Ensure(EnsureError::Connect(
            ConnectError::Rejected(_) | ConnectError::Frame(_) | ConnectError::UnexpectedFrame,
        ))
        | ObserveError::Connect(
            ConnectError::Rejected(_) | ConnectError::Frame(_) | ConnectError::UnexpectedFrame,
        ) => EX_PROTOCOL,
        ObserveError::Ensure(
            EnsureError::Connect(_)
            | EnsureError::Spawn { .. }
            | EnsureError::DaemonExited { .. }
            | EnsureError::StartupTimeout { .. },
        )
        | ObserveError::Connect(_) => EX_UNAVAILABLE,
        ObserveError::Client(haider_client::ClientError::Disconnected(_))
        | ObserveError::OutputClosed => EX_IOERR,
        ObserveError::Client(
            haider_client::ClientError::Encode(_) | haider_client::ClientError::MissingFeature(_),
        )
        | ObserveError::StreamTask(_)
        | ObserveError::UnknownSession(_) => EX_SOFTWARE,
        ObserveError::Rpc { code, .. } if code == "timeout_before_acceptance" => EX_TIMEOUT,
        ObserveError::Rpc { code, .. }
            if matches!(
                code.as_str(),
                "provider_error"
                    | "provider_timeout"
                    | "credential_missing"
                    | "credential_limited"
                    | "unauthorized"
            ) =>
        {
            EX_PROVIDER
        }
        ObserveError::Rpc { code, .. }
            if matches!(code.as_str(), "permission_denied" | "input_required") =>
        {
            EX_BLOCKED
        }
        ObserveError::Rpc { code, .. }
            if matches!(
                code.as_str(),
                "protocol_mismatch" | "unknown_method" | "invalid_argument"
            ) =>
        {
            EX_PROTOCOL
        }
        ObserveError::Rpc { .. } => EX_SOFTWARE,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod roster_scalar_tests {
    use super::*;

    fn roster_summary(forked_from: Option<SessionForkProvenance>) -> haider_rpc::SessionSummary {
        let mut value = json!({
            "session_id": "session-view",
            "head_seq": 7,
            "worker_generation": 3,
        });
        if let Some(provenance) = forked_from {
            value["forked_from"] = json!({
                "session_id": provenance.session_id.as_str(),
                "seq": provenance.seq,
            });
        }
        serde_json::from_value(value).expect("roster summary decodes")
    }

    fn view() -> SessionSummaryView {
        SessionSummaryView {
            id: "session-view".into(),
            title: "t".into(),
            run_state: "idle",
            run_id: None,
            worker_generation: None,
            cache: None,
            active_branch: "main".into(),
            branches: vec!["main".into()],
            provider: None,
            model: None,
            footprint: None,
            subagent_count: 0,
            updated_at: 7,
            parked_since: None,
            effort: None,
            fast: None,
            agent_type: None,
            seen_at_ms: None,
            last_activity_ms: None,
            waiting_why: None,
            needs_input: None,
            forked_from: None,
        }
    }

    /// Additive roster scalars, attention fields, and prompt-fork provenance ride
    /// `sessions --json` rows ADDITIVELY — absent when the daemon (or the
    /// merge) has nothing, present with their wire shapes when populated.
    ///
    /// MUTATION CHECK (executed): serialize a field unconditionally or drop the
    /// fork-provenance merge assignment and the absent/present halves fail.
    #[test]
    fn roster_scalars_ride_sessions_json_rows_additively() {
        let mut bare = view();
        merge_roster_summary(&mut bare, &roster_summary(None));
        let bare = bare.json();
        for key in [
            "effort",
            "fast",
            "agent_type",
            "seen_at_ms",
            "last_activity_ms",
            "waiting_why",
            "needs_input",
            "forked_from",
        ] {
            assert!(bare.get(key).is_none(), "absent `{key}` must not serialize");
        }

        let mut populated = view();
        merge_roster_summary(
            &mut populated,
            &roster_summary(Some(SessionForkProvenance {
                session_id: SessionId::new("session-source"),
                seq: 42,
            })),
        );
        populated.effort = Some("high".into());
        populated.fast = Some(false);
        populated.agent_type = Some("@scout".into());
        populated.seen_at_ms = Some(1_000);
        populated.last_activity_ms = Some(2_000);
        populated.waiting_why = Some(haider_rpc::WaitingWhyWire {
            kind: haider_rpc::WaitingWhyKindWire::Permission,
            pending_menu_id: None,
        });
        populated.needs_input = Some(haider_rpc::NeedsInputWire {
            kind: haider_rpc::NeedsInputKindWire::Recovery,
            title: "Effect outcome unknown".into(),
            safe_body: Vec::new(),
            menu_id: None,
            request_seq: None,
            worker_generation: None,
            since_ms: None,
            options: Vec::new(),
            secret_answer: false,
        });
        let json = populated.json();
        assert_eq!(json["effort"], "high");
        assert_eq!(json["needs_input"]["kind"], "recovery");
        assert_eq!(json["fast"], false);
        assert_eq!(json["agent_type"], "@scout");
        assert_eq!(json["seen_at_ms"], 1_000);
        assert_eq!(json["last_activity_ms"], 2_000);
        assert_eq!(
            json["forked_from"],
            serde_json::json!({"session_id": "session-source", "seq": 42})
        );
        assert_eq!(
            json["waiting_why"],
            serde_json::json!({"kind": "permission"})
        );
    }
}
