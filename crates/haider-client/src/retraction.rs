//! Receipt-backed prompt retraction with a run-pinned plain-cancel fallback.

use crate::{ClientError, RpcClient};
use haider_protocol::ids::{RunId, SessionId};
use haider_protocol::retraction::RetractedTurn;
use haider_rpc::{CancelStatus, CommandId, RequestBody, ResponseBody};

/// Retain this value across reconnects: command identities, generation and run
/// never change, including when a too-late retraction becomes ordinary cancel.
#[derive(Debug, Clone)]
pub struct TurnRetraction {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub worker_generation: u64,
    retract_command_id: CommandId,
    cancel_command_id: CommandId,
    cancel_fallback: bool,
}

#[derive(Debug, Clone)]
pub enum TurnRetractionOutcome {
    Retracted(RetractedTurn),
    /// Retraction returned typed `too_late`; ordinary cancellation was then
    /// durably acknowledged for the same run, preserving its transcript.
    Cancelled {
        session_id: SessionId,
        run_id: RunId,
        status: CancelStatus,
        terminal_seq: Option<u64>,
    },
}

#[derive(Debug)]
pub enum TurnRetractionError {
    Client(ClientError),
    Daemon {
        code: String,
        message: String,
        retryable: bool,
    },
    Protocol(&'static str),
}

impl std::fmt::Display for TurnRetractionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Client(error) => write!(formatter, "{error}"),
            Self::Daemon { code, message, .. } => write!(formatter, "{code}: {message}"),
            Self::Protocol(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for TurnRetractionError {}

impl TurnRetraction {
    #[must_use]
    pub fn new(
        session_id: SessionId,
        run_id: RunId,
        worker_generation: u64,
        command_id: CommandId,
    ) -> Self {
        Self {
            session_id,
            run_id,
            worker_generation,
            cancel_command_id: CommandId::new(format!("{}:cancel", command_id.as_str())),
            retract_command_id: command_id,
            cancel_fallback: false,
        }
    }

    fn request_body(&self) -> RequestBody {
        if self.cancel_fallback {
            RequestBody::TurnCancel {
                command_id: self.cancel_command_id.clone(),
                session_id: self.session_id.clone(),
                worker_generation: self.worker_generation,
                run_id: self.run_id.clone(),
            }
        } else {
            RequestBody::TurnRetract {
                command_id: self.retract_command_id.clone(),
                session_id: self.session_id.clone(),
                worker_generation: self.worker_generation,
                run_id: self.run_id.clone(),
            }
        }
    }

    fn apply_response(
        &mut self,
        response: ResponseBody,
    ) -> Result<Option<TurnRetractionOutcome>, TurnRetractionError> {
        match response {
            ResponseBody::TurnRetract {
                session_id,
                run_id,
                prompt_seq,
                retracted_seq,
                text,
                attachments,
            } if !self.cancel_fallback
                && session_id == self.session_id
                && run_id == self.run_id
                && prompt_seq > 0
                && retracted_seq > prompt_seq =>
            {
                Ok(Some(TurnRetractionOutcome::Retracted(RetractedTurn {
                    session_id,
                    run_id,
                    prompt_seq,
                    retracted_seq,
                    text,
                    attachments,
                })))
            }
            ResponseBody::TurnCancel {
                session_id,
                run_id,
                status,
                terminal_seq,
            } if self.cancel_fallback
                && session_id == self.session_id
                && run_id == self.run_id
                && matches!(
                    status,
                    CancelStatus::Accepted | CancelStatus::AlreadyTerminal
                ) =>
            {
                Ok(Some(TurnRetractionOutcome::Cancelled {
                    session_id,
                    run_id,
                    status,
                    terminal_seq,
                }))
            }
            ResponseBody::Error { code, .. }
                if !self.cancel_fallback && code == haider_rpc::ERROR_CODE_TOO_LATE =>
            {
                self.cancel_fallback = true;
                Ok(None)
            }
            ResponseBody::Error {
                code,
                message,
                retryable,
                ..
            } => Err(TurnRetractionError::Daemon {
                code,
                message,
                retryable,
            }),
            _ => Err(TurnRetractionError::Protocol(
                "turn retraction response does not match the pinned request",
            )),
        }
    }

    /// The caller must hold a control attachment. On transport failure, keep
    /// this value, reconnect and attach again, then retry this method. RpcClient
    /// services heartbeat traffic independently throughout these waits.
    pub async fn execute(
        &mut self,
        client: &RpcClient,
    ) -> Result<TurnRetractionOutcome, TurnRetractionError> {
        if !client
            .welcome()
            .features
            .contains(haider_rpc::FEATURE_TURN_RETRACT_V1)
        {
            return Err(TurnRetractionError::Client(ClientError::MissingFeature(
                haider_rpc::FEATURE_TURN_RETRACT_V1,
            )));
        }
        loop {
            let response = client
                .request(self.request_body())
                .await
                .map_err(TurnRetractionError::Client)?;
            if let Some(outcome) = self.apply_response(response)? {
                return Ok(outcome);
            }
        }
    }
}

#[cfg(test)]
#[path = "retraction_tests.rs"]
mod tests;
