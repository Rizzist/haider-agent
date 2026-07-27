//! CHARTER — the connection's request surface: transport in, semantics down.
//!
//! What lives here: [`HubConnection`]'s method handlers — capability and
//! control-attachment policy checks, argument validation, receipt-first
//! command orchestration (R2/R3/R5), workspace validation, and wire
//! error-code mapping. What may NOT live here: durable mutation (the store
//! owns every transaction; the session actor serializes it — actor.rs),
//! delivery pacing (replay.rs), and provider/tool work (`worker.rs`; a
//! request handler hands the manager a COMMITTED acceptance and returns).
//! Requests on one connection are handled inline by the connection task, so
//! nothing here may await provider work — the longest await is one store
//! transaction or one workspace `spawn_blocking`.

use super::*;

// ─────────── connection RPC surface: list/read/attach/detach/menu ───────────

impl HubConnection {
    /// Handles one request and enqueues its correlated response.
    pub async fn request(
        &self,
        request_id: RequestId,
        body: RequestBody,
    ) -> Result<(), SessionHubError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(SessionHubError::Closed);
        }
        if self.hub.inner.draining.load(Ordering::Acquire) {
            return self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "daemon is draining",
                true,
                None,
            );
        }
        match body {
            RequestBody::SessionCreate {
                command_id,
                cwd,
                provider,
                model,
                max_tokens,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_create(request_id, command_id, cwd, provider, model, max_tokens)
                    .await
            }
            RequestBody::SessionList { cursor, limit } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_list(request_id, cursor, limit).await
            }
            RequestBody::SessionRead { session_id, range } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_read(request_id, session_id, range).await
            }
            RequestBody::SessionAttach {
                session_id,
                after_seq,
                mode,
            } => {
                let operation = match mode {
                    AttachMode::View => Operation::View,
                    AttachMode::Control => Operation::Control,
                    // `Unknown` and any future mode: never guess an
                    // authorization level for a mode this daemon predates.
                    _ => {
                        return self.respond_error(
                            request_id,
                            ERROR_CODE_INVALID_ARGUMENT,
                            "unknown attachment mode",
                            false,
                            None,
                        );
                    }
                };
                if let Err(message) = authorize(&self.capabilities, operation) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_attach(request_id, session_id, after_seq, mode)
                    .await
            }
            RequestBody::SessionDetach { attachment_id } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.session_detach(request_id, attachment_id).await
            }
            RequestBody::TurnSubmit {
                command_id,
                session_id,
                worker_generation,
                text,
                attachments,
                mode,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "turn submission requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.turn_submit(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    text,
                    attachments,
                    mode,
                )
                .await
            }
            RequestBody::TurnCancel {
                command_id,
                session_id,
                worker_generation,
                run_id,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                if !self
                    .hub
                    .holds_control_attachment(&self.connection_id, &session_id)?
                {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        "turn cancellation requires a control attachment to this session",
                        false,
                        None,
                    );
                }
                self.turn_cancel(
                    request_id,
                    command_id,
                    session_id,
                    worker_generation,
                    run_id,
                )
                .await
            }
            RequestBody::VaultStage {
                stage_id,
                purpose,
                secret,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.vault_stage(request_id, stage_id, purpose, secret)
            }
            RequestBody::AccountLoginApi {
                command_id,
                provider,
                alias,
                vault_reference,
                validation_model,
            } => {
                if let Err(message) = authorize(&self.capabilities, Operation::Control) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_login(
                    request_id,
                    command_id,
                    provider,
                    alias,
                    vault_reference,
                    validation_model,
                )
            }
            RequestBody::AccountList { provider } => {
                if let Err(message) = authorize(&self.capabilities, Operation::View) {
                    return self.respond_error(
                        request_id,
                        ERROR_CODE_CAPABILITY_DENIED,
                        message,
                        false,
                        None,
                    );
                }
                self.account_list(request_id, provider)
            }
            // `Unknown` and any future method decode alike: a typed,
            // correlated rejection instead of a dropped request.
            _ => self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "unknown session method",
                false,
                None,
            ),
        }
    }

    /// The transport + vault gate shared by `vault.stage` and
    /// `account.login_api` (R7/R10): Control alone must not expose raw-secret
    /// staging to a remote transport, and a vaultless platform answers the
    /// stable `vault_unsupported` BEFORE staging/validation.
    fn secret_surface_facade(
        &self,
        request_id: &RequestId,
    ) -> Result<Option<crate::accounts::AccountsFacade>, SessionHubError> {
        if self.transport != crate::accounts::ConnectionTransport::LocalSameUid {
            self.respond_error(
                request_id.clone(),
                ERROR_CODE_CAPABILITY_DENIED,
                "secret staging is only served on authenticated same-UID local connections",
                false,
                None,
            )?;
            return Ok(None);
        }
        let facade = self.hub.accounts()?;
        match facade {
            Some(facade) if facade.vault_supported => Ok(Some(facade)),
            _ => {
                self.respond_error(
                    request_id.clone(),
                    haider_rpc::ERROR_CODE_VAULT_UNSUPPORTED,
                    "this platform has no supported secret vault (W3c supports macOS Keychain)",
                    false,
                    None,
                )?;
                Ok(None)
            }
        }
    }

    /// `vault.stage`: connection-scoped, non-durable, inline (no I/O). The
    /// secret enters zeroizing storage here and the wire frame drops
    /// (zeroized) with this call.
    fn vault_stage(
        &self,
        request_id: RequestId,
        stage_id: String,
        purpose: haider_rpc::StagePurpose,
        secret: haider_rpc::SecretWire,
    ) -> Result<(), SessionHubError> {
        if self.secret_surface_facade(&request_id)?.is_none() {
            return Ok(());
        }
        if stage_id.trim().is_empty() || secret.is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "stage id and secret must not be empty",
                false,
                None,
            );
        }
        if matches!(purpose, haider_rpc::StagePurpose::Unknown) {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "unknown stage purpose",
                false,
                None,
            );
        }
        let staged = {
            let mut stages = lock(&self.stages)?;
            stages.stage(&stage_id, purpose, secret.expose_secret().as_bytes())
        };
        match staged {
            Ok((vault_reference, expires_at_ms)) => self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::VaultStage {
                    stage_id,
                    vault_reference,
                    expires_at_ms,
                },
            }),
            Err(crate::accounts::StageError::Mismatch) => self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "stage id was already used with different secret bytes",
                false,
                None,
            ),
            Err(crate::accounts::StageError::Mint(message)) => self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                &format!("cannot mint stage reference: {message}"),
                true,
                None,
            ),
        }
    }

    /// `account.login_api`: claims the stage and HANDS OFF to the account
    /// actor (R7: the connection task never awaits validation/Keychain work
    /// inline). The correlated response arrives from the actor through this
    /// connection's sink; disconnect drops only that route, never the
    /// durable command.
    fn account_login(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        provider: String,
        alias: Option<String>,
        vault_reference: String,
        validation_model: Option<String>,
    ) -> Result<(), SessionHubError> {
        let Some(facade) = self.secret_surface_facade(&request_id)? else {
            return Ok(());
        };
        if command_id.as_str().trim().is_empty()
            || provider.trim().is_empty()
            || vault_reference.trim().is_empty()
        {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "login command id, provider, and vault reference must not be empty",
                false,
                None,
            );
        }
        let claimed = {
            let mut stages = lock(&self.stages)?;
            stages.claim(&vault_reference)
        };
        let secret = match claimed {
            Some((haider_rpc::StagePurpose::ApiKey, secret)) => Some(secret),
            Some(_) => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    "staged secret was not staged for api_key use",
                    false,
                    None,
                );
            }
            // Unknown/expired reference: the actor may still hold the
            // pending command's secret (retry-after-retryable), else it
            // answers restage_required.
            None => None,
        };
        let Some(login) = facade.login else {
            return self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_VAULT_UNSUPPORTED,
                "this platform has no supported secret vault (W3c supports macOS Keychain)",
                false,
                None,
            );
        };
        let job = crate::accounts::LoginJob {
            command_id: command_id.0,
            provider,
            display_alias: alias.filter(|value| !value.trim().is_empty()),
            validation_model: validation_model.filter(|value| !value.trim().is_empty()),
            secret,
            route: crate::accounts::LoginRoute {
                request_id: request_id.clone(),
                sink: Arc::clone(&self.sink),
            },
        };
        match login.try_send(crate::accounts::AccountCommand::Login(Box::new(job))) {
            Ok(()) => Ok(()),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => self.respond_error(
                request_id,
                haider_rpc::ERROR_CODE_BUSY,
                "account actor is busy; retry shortly",
                true,
                None,
            ),
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => self.respond_error(
                request_id,
                ERROR_CODE_DRAINING,
                "account actor is shut down",
                true,
                None,
            ),
        }
    }

    /// `account.list`: inline snapshot read (short command; the actor is the
    /// only writer, so a queued login never head-of-line-blocks listing).
    fn account_list(
        &self,
        request_id: RequestId,
        provider: Option<String>,
    ) -> Result<(), SessionHubError> {
        let Some(facade) = self.hub.accounts()? else {
            return self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::AccountList {
                    descriptors: Vec::new(),
                },
            });
        };
        let descriptors = facade
            .snapshot
            .lock()
            .map(|view| {
                view.iter()
                    .filter(|descriptor| {
                        provider
                            .as_deref()
                            .is_none_or(|provider| descriptor.provider == provider)
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::AccountList { descriptors },
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn turn_submit(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        text: String,
        attachments: Vec<haider_protocol::tool::AttachmentBlock>,
        mode: haider_protocol::DeliveryMode,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().is_empty() || text.trim().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "turn command id and text must not be empty",
                false,
                None,
            );
        }
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "text": &text,
            "attachments": &attachments,
            "mode": mode,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!("cannot encode turn-submit coordinates: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        match self
            .hub
            .turn_accept_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(accepted)) => {
                if accepted.worker_generation == self.hub.inner.store.worker_generation()
                    && let Err(error) = self.hub.worker_manager()?.submit(accepted.clone()).await
                {
                    return self.respond_turn_error(request_id, error);
                }
                return self.respond_turn_accepted(request_id, accepted);
            }
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        }
        let command = TurnAcceptCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id: session_id.clone(),
            worker_generation,
            run_id: haider_protocol::ids::RunId::new(random_id("run")?),
            text,
            attachments,
            mode,
            queued_event_id: EventId::new(random_id("turn-queued")?),
            user_event_id: EventId::new(random_id("turn-user")?),
            active_event_id: EventId::new(random_id("session-active")?),
            device_id: self.hub.inner.device_id.clone(),
        };
        let accepted = match self.hub.accept_turn(command).await {
            Ok(TurnAcceptOutcome::Committed { accepted, .. })
            | Ok(TurnAcceptOutcome::IdempotentReplay { accepted }) => accepted,
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        // Durable-before-provider: the manager sees this only after the actor
        // committed and synchronously published the acceptance transaction.
        if let Err(error) = self.hub.worker_manager()?.submit(accepted.clone()).await {
            return self.respond_turn_error(request_id, error);
        }
        self.respond_turn_accepted(request_id, accepted)
    }

    async fn turn_cancel(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        session_id: SessionId,
        worker_generation: u64,
        run_id: haider_protocol::ids::RunId,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "turn-cancel command id must not be empty",
                false,
                None,
            );
        }
        let request_json = serde_json::to_string(&serde_json::json!({
            "session_id": &session_id,
            "worker_generation": worker_generation,
            "run_id": &run_id,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!("cannot encode turn-cancel coordinates: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();
        let cancelled = match self
            .hub
            .turn_cancel_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(cancelled)) => cancelled,
            Ok(None) => {
                let command = TurnCancelCommand {
                    command_id: command_id.0,
                    request_digest,
                    request_json,
                    session_id: session_id.clone(),
                    worker_generation,
                    run_id: run_id.clone(),
                    cancelling_event_id: EventId::new(random_id("turn-cancelling")?),
                    device_id: self.hub.inner.device_id.clone(),
                };
                match self.hub.cancel_turn(command).await {
                    Ok(TurnCancelOutcome::Committed { cancelled, .. })
                    | Ok(TurnCancelOutcome::IdempotentReplay { cancelled }) => cancelled,
                    Err(SessionHubError::Store(error)) => {
                        return self.respond_turn_error(request_id, error);
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(SessionHubError::Store(error)) => {
                return self.respond_turn_error(request_id, error);
            }
            Err(error) => return Err(error),
        };
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::TurnCancel {
                session_id: cancelled.session_id,
                run_id: cancelled.run_id,
                status: match cancelled.status {
                    TurnCancellationStatus::Accepted => CancelStatus::Accepted,
                    TurnCancellationStatus::AlreadyTerminal => CancelStatus::AlreadyTerminal,
                },
                terminal_seq: cancelled.terminal_seq,
            },
        })
    }

    fn respond_turn_accepted(
        &self,
        request_id: RequestId,
        accepted: AcceptedTurn,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::TurnSubmit {
                session_id: accepted.session_id,
                run_id: accepted.run_id,
                accepted_seq: accepted.accepted_seq,
                worker_generation: accepted.worker_generation,
                disposition: match accepted.disposition {
                    TurnAdmissionDisposition::Started => SubmitDisposition::Started,
                    TurnAdmissionDisposition::Queued => SubmitDisposition::Queued,
                    TurnAdmissionDisposition::SteerPending => SubmitDisposition::SteerPending,
                },
            },
        })
    }

    fn respond_turn_error(
        &self,
        request_id: RequestId,
        error: HaiderError,
    ) -> Result<(), SessionHubError> {
        let code = match error.code {
            ErrorCode::SingleWriterViolation => ERROR_CODE_STALE_GENERATION,
            ErrorCode::SessionNotFound => ERROR_CODE_NOT_FOUND,
            ErrorCode::RunNotActive => ERROR_CODE_RUN_NOT_ACTIVE,
            ErrorCode::Busy => ERROR_CODE_OVERLOADED,
            _ => ERROR_CODE_INVALID_ARGUMENT,
        };
        self.respond_error(request_id, code, &error.message, error.retryable, None)
    }

    #[allow(clippy::too_many_arguments)]
    async fn session_create(
        &self,
        request_id: RequestId,
        command_id: CommandId,
        cwd: String,
        provider: String,
        model: String,
        max_tokens: u64,
    ) -> Result<(), SessionHubError> {
        if command_id.as_str().is_empty() {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session-create command id must not be empty",
                false,
                None,
            );
        }
        let request_json = serde_json::to_string(&serde_json::json!({
            "cwd": &cwd,
            "provider": &provider,
            "model": &model,
            "max_tokens": max_tokens,
        }))
        .map_err(|error| {
            SessionHubError::Task(format!("cannot encode session-create coordinates: {error}"))
        })?;
        let request_digest = blake3::hash(request_json.as_bytes()).to_hex().to_string();

        // Receipt lookup deliberately precedes path validation. A response
        // lost after commit remains recoverable even if the workspace was
        // deleted before the retry reached a new connection.
        match self
            .hub
            .session_create_receipt(&command_id, &request_digest, &request_json)
            .await
        {
            Ok(Some(created)) => return self.respond_created(request_id, created),
            Ok(None) => {}
            Err(SessionHubError::Store(error)) => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    &error.message,
                    error.retryable,
                    None,
                );
            }
            Err(error) => return Err(error),
        }

        if !matches!(provider.as_str(), "anthropic" | "fake") {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "unsupported session provider",
                false,
                None,
            );
        }
        if model.trim().is_empty() || max_tokens == 0 {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session model must be non-empty and max_tokens must be positive",
                false,
                None,
            );
        }
        let workspace = match validate_workspace(cwd).await {
            Ok(workspace) => workspace,
            Err(message) => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    &message,
                    false,
                    None,
                );
            }
        };
        let command = SessionCreateCommand {
            command_id: command_id.0,
            request_digest,
            request_json,
            session_id: SessionId::new(random_id("session")?),
            cwd: workspace.canonical,
            provider,
            model,
            max_tokens,
            system_prompt_version: crate::worker::SystemPromptBuilder::VERSION.into(),
            event_id: EventId::new(random_id("session-created")?),
            device_id: self.hub.inner.device_id.clone(),
        };
        // Keep the opened directory descriptor alive until the transaction
        // returns. M3 transfers the same canonical identity into its broker.
        let _descriptor = workspace.descriptor;
        match self.hub.create_session(command).await {
            Ok(SessionCreateOutcome::Committed { created, .. })
            | Ok(SessionCreateOutcome::IdempotentReplay { created }) => {
                self.respond_created(request_id, created)
            }
            Err(SessionHubError::Store(error)) => self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                &error.message,
                error.retryable,
                None,
            ),
            Err(error) => Err(error),
        }
    }

    fn respond_created(
        &self,
        request_id: RequestId,
        created: CreatedSession,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionCreate {
                session_id: created.session_id,
                created_seq: created.created_seq,
                worker_generation: created.worker_generation,
                metadata: created.metadata,
            },
        })
    }

    async fn session_list(
        &self,
        request_id: RequestId,
        cursor: Option<String>,
        limit: u32,
    ) -> Result<(), SessionHubError> {
        let after = match cursor.as_deref().map(decode_cursor).transpose() {
            Ok(after) => after,
            Err(()) => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_INVALID_CURSOR,
                    "session-list cursor is invalid",
                    false,
                    None,
                );
            }
        };
        let limit = usize::try_from(limit)
            .unwrap_or(usize::MAX)
            .min(MAX_LIST_PAGE);
        if limit == 0 {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session-list limit must be greater than zero",
                false,
                None,
            );
        }
        let ids = self.hub.inner.store.session_ids().await?;
        let mut selected = ids
            .into_iter()
            .filter(|session_id| {
                after
                    .as_ref()
                    .is_none_or(|after| session_id.as_str() > after.as_str())
            })
            .take(limit.saturating_add(1))
            .collect::<Vec<_>>();
        let has_more = selected.len() > limit;
        if has_more {
            selected.truncate(limit);
        }
        let mut sessions = Vec::with_capacity(selected.len());
        for session_id in &selected {
            sessions.push(SessionSummary {
                session_id: session_id.clone(),
                head_seq: self.hub.inner.store.latest_seq(session_id).await?,
                worker_generation: self.hub.inner.store.worker_generation(),
                metadata: self.hub.inner.store.session_metadata(session_id).await?,
            });
        }
        let next_cursor = has_more
            .then(|| selected.last().map(encode_cursor))
            .flatten();
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionList {
                sessions,
                next_cursor,
            },
        })
    }

    async fn session_read(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        range: SeqRange,
    ) -> Result<(), SessionHubError> {
        let head = self.hub.inner.store.latest_seq(&session_id).await?;
        if head == 0 {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        }
        if range.start_seq == 0 || range.end_seq < range.start_seq {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session-read range must be non-empty and start at sequence one or later",
                false,
                None,
            );
        }
        let count = range
            .end_seq
            .saturating_sub(range.start_seq)
            .saturating_add(1);
        let limit = usize::try_from(count).unwrap_or(usize::MAX);
        if limit > MAX_READ_ENVELOPES {
            return self.respond_error(
                request_id,
                ERROR_CODE_INVALID_ARGUMENT,
                "session-read range exceeds the maximum of 1024 envelopes",
                false,
                None,
            );
        }
        let envelopes = self
            .hub
            .inner
            .store
            .read(&session_id, range.start_seq.saturating_sub(1), limit)
            .await?
            .into_iter()
            .take_while(|envelope| envelope.seq <= range.end_seq)
            .collect::<Vec<_>>();
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionRead {
                result: SessionReadResult {
                    metadata: self.hub.inner.store.session_metadata(&session_id).await?,
                    session_id,
                    range,
                    head_seq: head,
                    envelopes,
                },
            },
        })
    }

    async fn session_attach(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        after_seq: u64,
        mode: AttachMode,
    ) -> Result<(), SessionHubError> {
        if self.hub.inner.store.latest_seq(&session_id).await? == 0 {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        }
        let registration = match self
            .hub
            .register(&self.connection_id, session_id, after_seq, mode)
            .await?
        {
            RegisterResult::Registered(registration) => registration,
            RegisterResult::CursorAhead { requested, head } => {
                return self.respond_error(
                    request_id,
                    ERROR_CODE_CURSOR_AHEAD,
                    "replay cursor is beyond the committed session head",
                    false,
                    Some(ErrorData::CursorAhead { requested, head }),
                );
            }
            // Same stable code the connection cap uses (its doc names
            // admission caps as the family); correlated and retryable here.
            RegisterResult::Overloaded { message } => {
                return self.respond_error(request_id, ERROR_CODE_OVERLOADED, &message, true, None);
            }
        };
        let attachment_id = registration.attachment_id.clone();
        let attach_state = registration.attach_state.clone();
        // Close-vs-registration sweep (P2-4): `close` sets `closed` BEFORE
        // it snapshots the owners map, so a registration that landed after
        // that snapshot always observes `closed` here and detaches itself;
        // one that landed before it was swept by close. Either way no
        // attachment survives on a closed connection.
        if self.closed.load(Ordering::Acquire) {
            let _ = self.hub.detach(&attachment_id).await;
            return Err(SessionHubError::Closed);
        }
        // Response-before-first-event: the response is staged with a marker
        // that gates this attachment's event offers until it has left the
        // queue, so no replayed event can precede the response that names
        // the attachment id (and a purge that still finds it answers the
        // request — see the unknown-id rule on `lag_and_detach`).
        if self
            .sink
            .try_send_for(
                &attachment_id,
                WireFrame::Response {
                    request_id,
                    body: ResponseBody::SessionAttach {
                        attachment_id: attachment_id.clone(),
                        attach_state,
                    },
                },
            )
            .is_err()
        {
            let _ = self.hub.detach(&attachment_id).await;
            return Err(SessionHubError::Delivery);
        }
        self.hub
            .spawn_replay(registration, after_seq, Arc::clone(&self.sink))
    }

    async fn session_detach(
        &self,
        request_id: RequestId,
        attachment_id: AttachmentId,
    ) -> Result<(), SessionHubError> {
        let owner = self
            .hub
            .take_attachment(&attachment_id, Some(&self.connection_id))?;
        let Some(owner) = owner else {
            return self.respond_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "attachment was not found on this connection",
                false,
                None,
            );
        };
        // Removal/cancellation happened under the same ownership lock used by
        // replay delivery. Purging now is therefore a terminal lane barrier.
        // (The purge cannot report a pending response: the client could only
        // name this attachment id after receiving that response.)
        let _ = self.sink.purge_attachment(&attachment_id);
        SessionHub::finish_detach(&attachment_id, owner).await;
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::SessionDetach { attachment_id },
        })
    }

    /// Handles the durable top-level `MenuAnswer` command.
    ///
    /// The arbitration law — first COMMITTED answer wins, losers get the
    /// winner's `resolution_seq` — is stated on
    /// `haider_store::Store::resolve_menu`; this method adds transport
    /// concerns only: capability + attachment policy, wire error mapping, and
    /// the correlated reply. Every attachment learns the outcome from the
    /// event stream (the actor publishes the committed envelope); the reply
    /// is a convenience, never the authority.
    ///
    /// Policy decision (brief §6): answering requires a CONTROL attachment to
    /// the target session — v0.1 has no "controller without a viewport"
    /// allowance.
    #[allow(clippy::too_many_arguments)]
    pub async fn menu_answer(
        &self,
        request_id: Option<RequestId>,
        command_id: CommandId,
        session_id: SessionId,
        menu_id: haider_protocol::ids::MenuId,
        request_seq: u64,
        worker_generation: u64,
        option_key: String,
        option_index: u32,
        input: Option<MenuInput>,
    ) -> Result<(), SessionHubError> {
        if self.hub.inner.draining.load(Ordering::Acquire) {
            return self.menu_error(
                request_id,
                ERROR_CODE_DRAINING,
                "daemon is draining",
                true,
                None,
            );
        }
        if let Err(message) = authorize(&self.capabilities, Operation::Control) {
            return self.menu_error(
                request_id,
                ERROR_CODE_CAPABILITY_DENIED,
                message,
                false,
                None,
            );
        }
        if !self
            .hub
            .holds_control_attachment(&self.connection_id, &session_id)?
        {
            return self.menu_error(
                request_id,
                ERROR_CODE_CAPABILITY_DENIED,
                "menu answers require a control attachment to this session",
                false,
                None,
            );
        }
        let (value, secret_reference) = match input {
            Some(MenuInput::Text { text }) => (Some(text), false),
            Some(MenuInput::SecretVaultReference { vault_reference }) => {
                (Some(vault_reference), true)
            }
            None => (None, false),
            Some(_) => {
                return self.menu_error(
                    request_id,
                    ERROR_CODE_INVALID_ARGUMENT,
                    "unknown menu input kind",
                    false,
                    None,
                );
            }
        };
        let answer = DurableMenuAnswer {
            menu: menu_id,
            option_key: (!option_key.is_empty()).then_some(option_key),
            option_index,
            value,
            via: AnswerVia::Rpc,
        };
        // Symmetric with `session_attach` (durable existence precedes actor
        // creation), so a bad session id can never mint a permanent actor.
        // Kept after the attachment-policy check to preserve that check's
        // pinned `capability_denied` for unattached callers.
        if self.hub.inner.store.latest_seq(&session_id).await? == 0 {
            return self.menu_error(
                request_id,
                ERROR_CODE_NOT_FOUND,
                "session was not found",
                false,
                None,
            );
        }
        let actor = self.hub.actor_for(session_id.clone()).await?;
        let command = MenuResolutionCommand {
            command_id: command_id.0,
            session_id,
            request_seq,
            worker_generation,
            allow_prior_generation: false,
            answer,
            device_id: self.hub.inner.device_id.clone(),
            input_is_secret_reference: secret_reference,
        };
        let (completed, result) = oneshot::channel();
        actor
            .commands
            .send(ActorCommand::MenuAnswer { command, completed })
            .await
            .map_err(|_| SessionHubError::Closed)?;
        match result.await.map_err(|_| SessionHubError::Closed)? {
            Ok(MenuResolutionOutcome::Committed { ref envelope }) => {
                self.menu_success(request_id, envelope.seq)
            }
            Ok(MenuResolutionOutcome::IdempotentReplay { resolution_seq }) => {
                self.menu_success(request_id, resolution_seq)
            }
            Ok(MenuResolutionOutcome::AlreadyResolved { resolution_seq }) => self.menu_error(
                request_id,
                ERROR_CODE_ALREADY_RESOLVED,
                "menu was already resolved",
                false,
                Some(ErrorData::AlreadyResolved { resolution_seq }),
            ),
            Err(error) => {
                let code = match error.code {
                    ErrorCode::SingleWriterViolation => ERROR_CODE_STALE_GENERATION,
                    ErrorCode::MenuAlreadyAnswered => ERROR_CODE_ALREADY_RESOLVED,
                    ErrorCode::MenuNotFound | ErrorCode::SessionNotFound => ERROR_CODE_NOT_FOUND,
                    _ => ERROR_CODE_INVALID_ARGUMENT,
                };
                self.menu_error(request_id, code, &error.message, error.retryable, None)
            }
        }
    }

    fn menu_success(
        &self,
        request_id: Option<RequestId>,
        resolution_seq: u64,
    ) -> Result<(), SessionHubError> {
        match request_id {
            Some(request_id) => self.send(WireFrame::Response {
                request_id,
                body: ResponseBody::MenuAnswer { resolution_seq },
            }),
            None => Ok(()),
        }
    }

    fn menu_error(
        &self,
        request_id: Option<RequestId>,
        code: &str,
        message: &str,
        retryable: bool,
        data: Option<ErrorData>,
    ) -> Result<(), SessionHubError> {
        match request_id {
            Some(request_id) => self.respond_error(request_id, code, message, retryable, data),
            None => self.send(WireFrame::ProtocolError(ProtocolError {
                code: code.into(),
                message: message.into(),
                fatal: false,
            })),
        }
    }

    fn respond_error(
        &self,
        request_id: RequestId,
        code: &str,
        message: &str,
        retryable: bool,
        data: Option<ErrorData>,
    ) -> Result<(), SessionHubError> {
        self.send(WireFrame::Response {
            request_id,
            body: ResponseBody::Error {
                code: code.into(),
                message: message.into(),
                retryable,
                data,
            },
        })
    }

    fn send(&self, frame: WireFrame) -> Result<(), SessionHubError> {
        self.sink
            .try_send(frame)
            .map_err(|_| SessionHubError::Delivery)
    }

    /// Detaches every attachment owned by this connection and wipes every
    /// staged secret (R7: disconnect wipes all staged secrets; a secret a
    /// login command already claimed lives on with the command).
    pub async fn close(&self) -> Result<(), SessionHubError> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        if let Ok(mut stages) = self.stages.lock() {
            *stages = crate::accounts::StagedSecrets::default();
        }
        self.hub.detach_connection(&self.connection_id).await
    }
}

#[derive(Debug, Clone, Copy)]
enum Operation {
    View,
    Control,
}

fn authorize(capabilities: &CapabilitySet, operation: Operation) -> Result<(), &'static str> {
    let allowed = match operation {
        Operation::View => {
            capabilities.contains(&Capability::View) || capabilities.contains(&Capability::Control)
        }
        Operation::Control => capabilities.contains(&Capability::Control),
    };
    allowed.then_some(()).ok_or(match operation {
        Operation::View => "this method requires the view capability",
        Operation::Control => "this method requires the control capability",
    })
}

struct ValidatedWorkspace {
    canonical: String,
    descriptor: std::fs::File,
}

async fn validate_workspace(cwd: String) -> Result<ValidatedWorkspace, String> {
    if !std::path::Path::new(&cwd).is_absolute() {
        return Err("session cwd must be an absolute path".into());
    }
    tokio::task::spawn_blocking(move || {
        let canonical = std::fs::canonicalize(&cwd)
            .map_err(|error| format!("cannot canonicalize session cwd: {error}"))?;
        let canonical_text = canonical
            .to_str()
            .ok_or_else(|| "canonical session cwd is not valid UTF-8".to_owned())?
            .to_owned();
        let metadata = std::fs::metadata(&canonical)
            .map_err(|error| format!("cannot inspect session cwd: {error}"))?;
        if !metadata.is_dir() {
            return Err("session cwd must identify a directory".into());
        }
        let descriptor = std::fs::File::open(&canonical)
            .map_err(|error| format!("cannot open session cwd: {error}"))?;
        Ok(ValidatedWorkspace {
            canonical: canonical_text,
            descriptor,
        })
    })
    .await
    .map_err(|error| format!("session cwd validation task failed: {error}"))?
}
