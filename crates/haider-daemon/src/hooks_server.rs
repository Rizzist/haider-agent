//! Long-lived request/response hook processes using bounded JSONL framing.

use super::*;
use tokio::io::{AsyncBufReadExt, BufReader};

const SERVER_QUEUE: usize = 8;

#[derive(Debug)]
pub(super) enum ServerDispatchError {
    QueueFull,
    Closed,
}

impl ServerDispatchError {
    pub(super) fn process_result(&self) -> HookProcessResult {
        match self {
            Self::QueueFull => failed_process_output("hook server queue is full"),
            Self::Closed => failed_process_output("hook server is unavailable"),
        }
    }
}

pub(super) enum ServerReply {
    Result(HookProcessResult),
    DefinitionChanged,
}

struct ServerRequest {
    input: Arc<[u8]>,
    response: oneshot::Sender<ServerReply>,
}

struct ServerHandle {
    sender: mpsc::Sender<ServerRequest>,
    task: JoinHandle<()>,
    definition_key: String,
    digest: String,
    workspace_cwd: PathBuf,
    run_scope: Option<(SessionId, RunId)>,
}

#[derive(Clone, Default)]
pub(super) struct HookServerRegistry {
    handles: Arc<Mutex<HashMap<String, ServerHandle>>>,
}

impl HookServerRegistry {
    pub(super) fn try_dispatch(
        &self,
        service: HookService,
        definition: HookDefinition,
        input: Arc<[u8]>,
        run_scope: Option<(SessionId, RunId)>,
    ) -> Result<oneshot::Receiver<ServerReply>, ServerDispatchError> {
        let definition_key = definition.subscriber_key();
        let key = match &run_scope {
            Some((session_id, run_id)) => {
                format!("{definition_key}\0run\0{session_id}\0{run_id}")
            }
            None => format!("{definition_key}\0profile"),
        };
        for _ in 0..2 {
            let sender = {
                let mut handles = self
                    .handles
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                handles
                    .entry(key.clone())
                    .or_insert_with(|| {
                        let (sender, receiver) = mpsc::channel(SERVER_QUEUE);
                        let actor_service = service.clone();
                        let actor_definition = definition.clone();
                        let actor_run_override = run_scope.is_some();
                        let task = tokio::spawn(async move {
                            run_server_actor(
                                actor_service,
                                actor_definition,
                                receiver,
                                actor_run_override,
                            )
                            .await;
                        });
                        ServerHandle {
                            sender,
                            task,
                            definition_key: definition_key.clone(),
                            digest: definition.digest.clone(),
                            workspace_cwd: definition.workspace_cwd.clone(),
                            run_scope: run_scope.clone(),
                        }
                    })
                    .sender
                    .clone()
            };
            let (response, wait) = oneshot::channel();
            match sender.try_send(ServerRequest {
                input: Arc::clone(&input),
                response,
            }) {
                Ok(()) => return Ok(wait),
                Err(mpsc::error::TrySendError::Full(_)) => {
                    return Err(ServerDispatchError::QueueFull);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    if let Some(handle) = self
                        .handles
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&key)
                    {
                        handle.task.abort();
                    }
                }
            }
        }
        Err(ServerDispatchError::Closed)
    }

    pub(super) fn kill_digest(&self, digest: &str) {
        self.remove_where(|handle| handle.digest == digest);
    }

    pub(super) fn kill_scope(&self, scope: &(SessionId, RunId)) {
        self.remove_where(|handle| handle.run_scope.as_ref() == Some(scope));
    }

    pub(super) fn reconcile_workspace(
        &self,
        cwd: &Path,
        current: &HashSet<String>,
        trusted_profile: &HashSet<String>,
        trusted_runs: Option<&HashSet<(SessionId, RunId)>>,
    ) {
        self.remove_where(|handle| {
            if handle.workspace_cwd != cwd {
                return false;
            }
            if !current.contains(&handle.definition_key) {
                return true;
            }
            match &handle.run_scope {
                Some(scope) => trusted_runs.is_some_and(|trusted| !trusted.contains(scope)),
                None => !trusted_profile.contains(&handle.definition_key),
            }
        });
    }

    pub(super) async fn shutdown(&self) {
        let tasks = {
            let mut handles = self
                .handles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            handles
                .drain()
                .map(|(_, handle)| {
                    handle.task.abort();
                    handle.task
                })
                .collect::<Vec<_>>()
        };
        for task in tasks {
            let _ = task.await;
        }
    }

    pub(super) fn abort_all(&self) {
        self.remove_where(|_| true);
    }

    fn remove_where(&self, mut remove: impl FnMut(&ServerHandle) -> bool) {
        let mut handles = self
            .handles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        handles.retain(|_, handle| {
            if remove(handle) {
                handle.task.abort();
                false
            } else {
                true
            }
        });
    }
}

struct ServerProcess {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    group: ProcessGroupGuard,
}

impl ServerProcess {
    async fn kill(mut self) {
        self.group.kill();
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

async fn run_server_actor(
    service: HookService,
    definition: HookDefinition,
    mut requests: mpsc::Receiver<ServerRequest>,
    run_override: bool,
) {
    let HookKind::Server { idle_timeout_ms } = definition.kind else {
        return;
    };
    let idle_timeout = Duration::from_millis(idle_timeout_ms);
    let mut process = None::<ServerProcess>;
    let mut crash_backoff = None::<Duration>;
    let mut next_backoff = SUBSCRIBE_BACKOFF_MIN;
    let mut shutdown = service.inner.shutdown.subscribe();
    // A registry drained by `servers.shutdown()` can still receive one late
    // `try_dispatch` from an in-flight engine message; `subscribe()` marks
    // the already-flipped flag as seen, so `changed()` alone would never
    // fire for this receiver. Check the flag once at actor start so a
    // post-shutdown actor exits before its first spawn instead of outliving
    // the clean-shutdown path.
    if *shutdown.borrow() {
        return;
    }
    loop {
        let request = if process.is_some() && !idle_timeout.is_zero() {
            tokio::select! {
                biased;
                request = requests.recv() => request,
                _ = tokio::time::sleep(idle_timeout) => {
                    if let Some(process) = process.take() {
                        process.kill().await;
                    }
                    crash_backoff = None;
                    next_backoff = SUBSCRIBE_BACKOFF_MIN;
                    continue;
                }
                _ = shutdown.changed() => None,
            }
        } else {
            tokio::select! {
                biased;
                request = requests.recv() => request,
                _ = shutdown.changed() => None,
            }
        };
        let Some(request) = request else {
            if let Some(process) = process.take() {
                process.kill().await;
            }
            return;
        };

        if !definition_current(&service, &definition, run_override).await {
            if let Some(process) = process.take() {
                process.kill().await;
            }
            let _ = request.response.send(ServerReply::DefinitionChanged);
            return;
        }
        if let Some(active) = process.as_mut() {
            match active.child.try_wait() {
                Ok(None) => {}
                Ok(Some(_)) => {
                    if let Some(process) = process.take() {
                        // The leader is gone, but descendants may still own
                        // the process group. Kill the group before respawn.
                        process.kill().await;
                    }
                    crash_backoff = Some(next_backoff);
                    next_backoff = next_subscriber_backoff(next_backoff);
                }
                Err(_) => {
                    if let Some(process) = process.take() {
                        process.kill().await;
                    }
                    crash_backoff = Some(next_backoff);
                    next_backoff = next_subscriber_backoff(next_backoff);
                }
            }
        }
        if let Some(backoff) = crash_backoff.take() {
            let proceed = tokio::select! {
                _ = tokio::time::sleep(backoff) => true,
                _ = shutdown.changed() => false,
            };
            if !proceed {
                let _ = request
                    .response
                    .send(ServerReply::Result(cancelled_process_output()));
                return;
            }
        }
        // Trust and the bound definition may change while crash backoff is
        // sleeping. Revalidate at the last async seam before every respawn.
        if !definition_current(&service, &definition, run_override).await {
            if let Some(process) = process.take() {
                process.kill().await;
            }
            let _ = request.response.send(ServerReply::DefinitionChanged);
            return;
        }
        if process.is_none() {
            match spawn_server(&definition) {
                Ok(spawned) => process = Some(spawned),
                Err(reason) => {
                    let _ = request
                        .response
                        .send(ServerReply::Result(failed_process_output(&reason)));
                    crash_backoff = Some(next_backoff);
                    next_backoff = next_subscriber_backoff(next_backoff);
                    continue;
                }
            }
        }

        let Some(active) = process.as_mut() else {
            let _ = request.response.send(ServerReply::Result(
                ServerDispatchError::Closed.process_result(),
            ));
            continue;
        };
        let outcome = run_server_event(
            active,
            &request.input,
            definition.timeout,
            &service.inner.store,
            &mut shutdown,
        )
        .await;
        if outcome.crashed {
            if let Some(process) = process.take() {
                process.kill().await;
            }
            crash_backoff = Some(next_backoff);
            next_backoff = next_subscriber_backoff(next_backoff);
        } else {
            crash_backoff = None;
            next_backoff = SUBSCRIBE_BACKOFF_MIN;
        }
        let cancelled = outcome.result.cancelled;
        let _ = request.response.send(ServerReply::Result(outcome.result));
        if cancelled {
            if let Some(process) = process.take() {
                process.kill().await;
            }
            return;
        }
    }
}

struct ServerEventOutcome {
    result: HookProcessResult,
    crashed: bool,
}

async fn run_server_event(
    process: &mut ServerProcess,
    input: &[u8],
    timeout: Duration,
    store: &haider_core::SqliteStoreHandle,
    shutdown: &mut watch::Receiver<bool>,
) -> ServerEventOutcome {
    let exchange = async {
        process.stdin.write_all(input).await?;
        process.stdin.write_all(b"\n").await?;
        process.stdin.flush().await?;
        read_bounded_json_line(&mut process.stdout, MAX_STREAM_OUTPUT_BYTES).await
    };
    let outcome = tokio::select! {
        outcome = tokio::time::timeout(timeout, exchange) => Some(outcome),
        _ = shutdown.changed() => None,
    };
    match outcome {
        None => ServerEventOutcome {
            result: cancelled_process_output(),
            crashed: true,
        },
        Some(Err(_)) => ServerEventOutcome {
            result: HookProcessResult {
                exit_code: None,
                timed_out: true,
                cancelled: false,
                stdout: empty_output(),
                stderr: empty_output(),
                proposed_decision: None,
            },
            crashed: true,
        },
        Some(Ok(Err(error))) => ServerEventOutcome {
            result: failed_process_output(&format!("hook server exchange failed: {error}")),
            crashed: true,
        },
        Some(Ok(Ok(bytes))) => ServerEventOutcome {
            result: HookProcessResult {
                exit_code: Some(0),
                timed_out: false,
                cancelled: false,
                stdout: make_output(
                    store,
                    CapturedBytes {
                        bytes,
                        truncated: false,
                    },
                )
                .await,
                stderr: empty_output(),
                proposed_decision: None,
            },
            crashed: false,
        },
    }
}

fn spawn_server(definition: &HookDefinition) -> Result<ServerProcess, String> {
    let cwd_fd = open_canonical_directory(&definition.workspace_cwd)
        .ok_or_else(|| "workspace cwd is no longer canonical".to_owned())?;
    let mut command = hook_command(&definition.command);
    command
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    haider_platform::configure_process_environment(&mut command);
    for name in ENV_ALLOWLIST {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    haider_platform::configure_process_group(&mut command);
    #[cfg(unix)]
    configure_hook_cwd(&mut command, cwd_fd);
    #[cfg(windows)]
    configure_hook_cwd(&mut command, &cwd_fd);
    haider_platform::configure_background_process(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("hook server spawn failed: {error}"))?;
    let raw_pid = child.id().ok_or_else(|| {
        let _ = child.start_kill();
        "hook server did not expose a process id".to_owned()
    })?;
    let group = haider_platform::register_process_group(raw_pid).map_err(|error| {
        let _ = child.start_kill();
        format!("hook server process-group registration failed: {error}")
    })?;
    let mut group = ProcessGroupGuard { pid: Some(group) };
    let stdin = child.stdin.take().ok_or_else(|| {
        group.kill();
        let _ = child.start_kill();
        "hook server stdin unavailable".to_owned()
    })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        group.kill();
        let _ = child.start_kill();
        "hook server stdout unavailable".to_owned()
    })?;
    Ok(ServerProcess {
        child,
        stdin,
        stdout: BufReader::new(stdout),
        group,
    })
}

pub(super) async fn read_bounded_json_line(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
    cap: usize,
) -> std::io::Result<Vec<u8>> {
    let mut line = Vec::with_capacity(cap.min(INLINE_OUTPUT_BYTES));
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "hook server closed before its response line",
            ));
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let take = newline.unwrap_or(available.len());
        if line.len().saturating_add(take) > cap {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "hook server response exceeded the output limit",
            ));
        }
        line.extend_from_slice(&available[..take]);
        let consumed = take + usize::from(newline.is_some());
        reader.consume(consumed);
        if newline.is_some() {
            break;
        }
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    let value: Value = serde_json::from_slice(&line).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("hook server response is not JSON: {error}"),
        )
    })?;
    match value {
        Value::String(value) => Ok(value.into_bytes()),
        value => serde_json::to_vec(&value).map_err(std::io::Error::other),
    }
}
