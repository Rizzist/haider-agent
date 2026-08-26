//! Execution contracts for durable, capability-scoped typed agents.
//!
//! [`crate::loom::LoomAgentType`] remains the authoring and registry record.
//! This module derives the immutable execution snapshot a daemon owns: an
//! explicit specialist role, the programs which must be installed before the
//! specialist may run, and the durable state of that installation work.
//! Required programs are names, never shell fragments or installation
//! commands. The daemon resolves them through its own trusted installer
//! catalog and executes structured argv only.

use crate::loom::LoomAgentType;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt::{Display, Formatter};

/// Maximum bytes in a registered typed-agent identifier.
pub const TYPED_AGENT_ID_MAX_BYTES: usize = 64;
/// Maximum bytes in a typed role's display name.
pub const TYPED_AGENT_ROLE_NAME_MAX_BYTES: usize = 120;
/// Maximum bytes in a typed role's scoped instructions.
pub const TYPED_AGENT_ROLE_INSTRUCTIONS_MAX_BYTES: usize = 4 * 1024;
/// Maximum number of programs one typed agent may require.
pub const TYPED_AGENT_REQUIRED_CLI_MAX: usize = 32;
/// Maximum bytes in one required program token.
pub const TYPED_AGENT_CLI_MAX_BYTES: usize = 128;
/// Maximum bytes in one durable install-job identity.
pub const TYPED_AGENT_INSTALL_JOB_ID_MAX_BYTES: usize = 128;
/// Maximum bytes retained for a durable installation failure.
pub const TYPED_AGENT_INSTALL_ERROR_MAX_BYTES: usize = 512;
/// Maximum number of historical jobs returned by one reconnectable status
/// snapshot. Exact-job polling is unaffected; every inventory query (global
/// or type-filtered) is bounded so an old profile cannot exceed the frame.
pub const TYPED_AGENT_INSTALL_STATUS_MAX_JOBS: usize = 32;

/// Stable machine-readable classification for contract validation failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypedAgentContractErrorCode {
    InvalidRole,
    InvalidRequiredCli,
    InvalidContract,
    InvalidInstallJob,
    InvalidInstallProgress,
    IllegalInstallTransition,
}

/// A typed validation error. Callers branch on `code`; `message` is bounded
/// diagnostic prose and must never be parsed for control flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypedAgentContractError {
    pub code: TypedAgentContractErrorCode,
    pub message: String,
}

impl TypedAgentContractError {
    fn new(code: TypedAgentContractErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl Display for TypedAgentContractError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TypedAgentContractError {}

/// The specialist identity injected by the daemon for one typed execution.
///
/// `scope` is the registry type id, not a privilege role such as Head or
/// Subagent. It prevents a specialist prompt from becoming an unscoped global
/// instruction and lets manifests pin exactly which role was executed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypedAgentScopedRole {
    pub scope: String,
    pub name: String,
    pub instructions: String,
}

impl TypedAgentScopedRole {
    /// Validate all role material before it reaches a prompt or durable job.
    pub fn validate(&self) -> Result<(), TypedAgentContractError> {
        if !is_identifier(&self.scope) {
            return Err(TypedAgentContractError::new(
                TypedAgentContractErrorCode::InvalidRole,
                "typed-agent role scope must be a 1..=64 byte identifier",
            ));
        }
        if !is_bounded_nonempty_text(&self.name, TYPED_AGENT_ROLE_NAME_MAX_BYTES) {
            return Err(TypedAgentContractError::new(
                TypedAgentContractErrorCode::InvalidRole,
                "typed-agent role name must be bounded, non-empty text",
            ));
        }
        if !is_bounded_nonempty_text(&self.instructions, TYPED_AGENT_ROLE_INSTRUCTIONS_MAX_BYTES) {
            return Err(TypedAgentContractError::new(
                TypedAgentContractErrorCode::InvalidRole,
                "typed-agent role instructions must be 1..=4096 bytes of safe text",
            ));
        }
        Ok(())
    }
}

/// One executable which must be present before a typed agent may dispatch.
///
/// This is intentionally not an install command. `program` is both the exact
/// executable token admitted by the runtime fence and the key the daemon uses
/// to resolve a trusted installation recipe.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TypedAgentRequiredCli {
    pub program: String,
}

impl TypedAgentRequiredCli {
    pub fn validate(&self) -> Result<(), TypedAgentContractError> {
        if !is_valid_required_program(&self.program) {
            return Err(TypedAgentContractError::new(
                TypedAgentContractErrorCode::InvalidRequiredCli,
                "required CLI must be a bounded concrete program token, never a shell or dispatcher",
            ));
        }
        Ok(())
    }
}

/// Immutable execution-facing snapshot derived from a registered Loom type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypedAgentContract {
    pub agent_type_id: String,
    pub agent_type_rev: u32,
    pub agent_type_digest: String,
    pub role: TypedAgentScopedRole,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_clis: Vec<TypedAgentRequiredCli>,
}

impl TypedAgentContract {
    /// Derive the explicit role and required-program contract from the frozen
    /// registry record. Repeated CLI grants collapse to one install item while
    /// preserving first-declaration order.
    pub fn from_loom_agent_type(record: &LoomAgentType) -> Result<Self, TypedAgentContractError> {
        let mut seen = HashSet::new();
        let required_clis = record
            .clis
            .iter()
            .filter(|program| seen.insert(program.as_str()))
            .map(|program| TypedAgentRequiredCli {
                program: program.clone(),
            })
            .collect();
        let contract = Self {
            agent_type_id: record.id.clone(),
            agent_type_rev: record.rev,
            agent_type_digest: record.digest(),
            role: TypedAgentScopedRole {
                scope: record.id.clone(),
                name: record.name.clone(),
                instructions: record.job.clone(),
            },
            required_clis,
        };
        contract.validate()?;
        Ok(contract)
    }

    pub fn validate(&self) -> Result<(), TypedAgentContractError> {
        if !is_identifier(&self.agent_type_id) || self.agent_type_id != self.role.scope {
            return Err(TypedAgentContractError::new(
                TypedAgentContractErrorCode::InvalidContract,
                "typed-agent contract id must be bounded and equal its role scope",
            ));
        }
        if self.agent_type_rev == 0 {
            return Err(TypedAgentContractError::new(
                TypedAgentContractErrorCode::InvalidContract,
                "typed-agent contract revision must be positive",
            ));
        }
        if !is_digest(&self.agent_type_digest) {
            return Err(TypedAgentContractError::new(
                TypedAgentContractErrorCode::InvalidContract,
                "typed-agent contract digest must be 32 lowercase hexadecimal bytes",
            ));
        }
        self.role.validate()?;
        if self.required_clis.len() > TYPED_AGENT_REQUIRED_CLI_MAX {
            return Err(TypedAgentContractError::new(
                TypedAgentContractErrorCode::InvalidRequiredCli,
                "typed-agent required CLI list exceeds 32 entries",
            ));
        }
        let mut seen = HashSet::new();
        for required in &self.required_clis {
            required.validate()?;
            if !seen.insert(required.program.as_str()) {
                return Err(TypedAgentContractError::new(
                    TypedAgentContractErrorCode::InvalidRequiredCli,
                    format!("required CLI `{}` is duplicated", required.program),
                ));
            }
        }
        Ok(())
    }
}

/// Durable installation lifecycle. Succeeded and failed are terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypedAgentInstallState {
    Queued,
    Installing,
    Verifying,
    Succeeded,
    Failed,
}

impl TypedAgentInstallState {
    /// Same-state writes are legal progress updates; terminal states cannot
    /// reopen. A changed agent-type revision creates a new durable job rather
    /// than rewriting the terminal history of an older contract.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Queued, Self::Queued | Self::Installing | Self::Failed)
                | (
                    Self::Installing,
                    Self::Installing | Self::Verifying | Self::Failed
                )
                | (
                    Self::Verifying,
                    Self::Verifying | Self::Succeeded | Self::Failed
                )
                | (Self::Succeeded, Self::Succeeded)
                | (Self::Failed, Self::Failed)
        )
    }

    pub fn validate_transition_to(self, next: Self) -> Result<(), TypedAgentContractError> {
        if self.can_transition_to(next) {
            return Ok(());
        }
        Err(TypedAgentContractError::new(
            TypedAgentContractErrorCode::IllegalInstallTransition,
            format!("illegal typed-agent install transition: {self:?} -> {next:?}"),
        ))
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed)
    }
}

/// Bounded progress retained with one durable install job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypedAgentInstallProgress {
    pub total: u16,
    pub completed: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_cli: Option<String>,
}

impl TypedAgentInstallProgress {
    pub fn validate(&self) -> Result<(), TypedAgentContractError> {
        if self.total == 0 || usize::from(self.total) > TYPED_AGENT_REQUIRED_CLI_MAX {
            return Err(TypedAgentContractError::new(
                TypedAgentContractErrorCode::InvalidInstallProgress,
                "typed-agent install total must be 1..=32",
            ));
        }
        if self.completed > self.total {
            return Err(TypedAgentContractError::new(
                TypedAgentContractErrorCode::InvalidInstallProgress,
                "typed-agent install completed count exceeds its total",
            ));
        }
        if let Some(current) = &self.current_cli {
            TypedAgentRequiredCli {
                program: current.clone(),
            }
            .validate()?;
        }
        Ok(())
    }
}

/// Durable, reconnectable installation job for one immutable agent-type rev.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypedAgentInstallJob {
    pub job_id: String,
    pub agent_type_id: String,
    pub agent_type_rev: u32,
    pub agent_type_digest: String,
    pub state: TypedAgentInstallState,
    pub progress: TypedAgentInstallProgress,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl TypedAgentInstallJob {
    /// Construct the initial durable job for a contract with required CLIs.
    pub fn queued(
        job_id: impl Into<String>,
        contract: &TypedAgentContract,
        now_ms: u64,
    ) -> Result<Self, TypedAgentContractError> {
        contract.validate()?;
        let total = u16::try_from(contract.required_clis.len()).map_err(|_| {
            TypedAgentContractError::new(
                TypedAgentContractErrorCode::InvalidInstallProgress,
                "typed-agent required CLI count is out of range",
            )
        })?;
        let job = Self {
            job_id: job_id.into(),
            agent_type_id: contract.agent_type_id.clone(),
            agent_type_rev: contract.agent_type_rev,
            agent_type_digest: contract.agent_type_digest.clone(),
            state: TypedAgentInstallState::Queued,
            progress: TypedAgentInstallProgress {
                total,
                completed: 0,
                current_cli: None,
            },
            error: None,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        };
        job.validate()?;
        Ok(job)
    }

    pub fn validate(&self) -> Result<(), TypedAgentContractError> {
        if !is_job_id(&self.job_id) {
            return Err(TypedAgentContractError::new(
                TypedAgentContractErrorCode::InvalidInstallJob,
                "typed-agent install job id must be 1..=128 safe ASCII bytes",
            ));
        }
        if !is_identifier(&self.agent_type_id)
            || self.agent_type_rev == 0
            || !is_digest(&self.agent_type_digest)
        {
            return Err(TypedAgentContractError::new(
                TypedAgentContractErrorCode::InvalidInstallJob,
                "typed-agent install job has invalid type coordinates",
            ));
        }
        if self.updated_at_ms < self.created_at_ms {
            return Err(TypedAgentContractError::new(
                TypedAgentContractErrorCode::InvalidInstallJob,
                "typed-agent install job update predates its creation",
            ));
        }
        self.progress.validate()?;
        validate_error(&self.error)?;

        let state_progress_is_valid = match self.state {
            TypedAgentInstallState::Queued => {
                self.progress.completed == 0
                    && self.progress.current_cli.is_none()
                    && self.error.is_none()
            }
            TypedAgentInstallState::Installing => {
                self.progress.completed < self.progress.total
                    && self.progress.current_cli.is_some()
                    && self.error.is_none()
            }
            TypedAgentInstallState::Verifying => {
                self.progress.completed == self.progress.total && self.error.is_none()
            }
            TypedAgentInstallState::Succeeded => {
                self.progress.completed == self.progress.total
                    && self.progress.current_cli.is_none()
                    && self.error.is_none()
            }
            TypedAgentInstallState::Failed => self.error.is_some(),
        };
        if !state_progress_is_valid {
            return Err(TypedAgentContractError::new(
                TypedAgentContractErrorCode::InvalidInstallProgress,
                "typed-agent install state, progress, and error are inconsistent",
            ));
        }
        Ok(())
    }

    /// Validate a proposed durable update without mutating the current record.
    /// Identity, totals, completion, timestamps, and lifecycle are monotonic.
    pub fn validate_update(&self, next: &Self) -> Result<(), TypedAgentContractError> {
        self.validate()?;
        next.validate()?;
        if self.job_id != next.job_id
            || self.agent_type_id != next.agent_type_id
            || self.agent_type_rev != next.agent_type_rev
            || self.agent_type_digest != next.agent_type_digest
            || self.created_at_ms != next.created_at_ms
        {
            return Err(TypedAgentContractError::new(
                TypedAgentContractErrorCode::InvalidInstallJob,
                "typed-agent install job identity is immutable",
            ));
        }
        if self.progress.total != next.progress.total
            || next.progress.completed < self.progress.completed
            || next.updated_at_ms < self.updated_at_ms
        {
            return Err(TypedAgentContractError::new(
                TypedAgentContractErrorCode::InvalidInstallProgress,
                "typed-agent install progress and timestamps must be monotonic",
            ));
        }
        self.state.validate_transition_to(next.state)
    }
}

/// Durable per-program row belonging to an install job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypedAgentInstallItem {
    pub job_id: String,
    pub ordinal: u16,
    pub required_cli: TypedAgentRequiredCli,
    pub state: TypedAgentInstallState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl TypedAgentInstallItem {
    pub fn validate(&self) -> Result<(), TypedAgentContractError> {
        if !is_job_id(&self.job_id)
            || usize::from(self.ordinal) >= TYPED_AGENT_REQUIRED_CLI_MAX
            || self.updated_at_ms < self.created_at_ms
        {
            return Err(TypedAgentContractError::new(
                TypedAgentContractErrorCode::InvalidInstallJob,
                "typed-agent install item has invalid identity, ordinal, or timestamps",
            ));
        }
        self.required_cli.validate()?;
        validate_error(&self.error)?;
        if (self.state == TypedAgentInstallState::Failed) != self.error.is_some() {
            return Err(TypedAgentContractError::new(
                TypedAgentContractErrorCode::InvalidInstallProgress,
                "only a failed typed-agent install item may carry an error",
            ));
        }
        Ok(())
    }

    pub fn validate_update(&self, next: &Self) -> Result<(), TypedAgentContractError> {
        self.validate()?;
        next.validate()?;
        if self.job_id != next.job_id
            || self.ordinal != next.ordinal
            || self.required_cli != next.required_cli
            || self.created_at_ms != next.created_at_ms
        {
            return Err(TypedAgentContractError::new(
                TypedAgentContractErrorCode::InvalidInstallJob,
                "typed-agent install item identity is immutable",
            ));
        }
        if next.updated_at_ms < self.updated_at_ms {
            return Err(TypedAgentContractError::new(
                TypedAgentContractErrorCode::InvalidInstallProgress,
                "typed-agent install item timestamp must be monotonic",
            ));
        }
        self.state.validate_transition_to(next.state)
    }
}

fn validate_error(error: &Option<String>) -> Result<(), TypedAgentContractError> {
    if let Some(error) = error
        && !is_bounded_nonempty_text(error, TYPED_AGENT_INSTALL_ERROR_MAX_BYTES)
    {
        return Err(TypedAgentContractError::new(
            TypedAgentContractErrorCode::InvalidInstallJob,
            "typed-agent install error must be bounded, non-empty safe text",
        ));
    }
    Ok(())
}

fn is_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= TYPED_AGENT_ID_MAX_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn is_digest(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_bounded_nonempty_text(value: &str, max_bytes: usize) -> bool {
    !value.trim().is_empty()
        && value.len() <= max_bytes
        && !value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
}

fn is_job_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= TYPED_AGENT_INSTALL_JOB_ID_MAX_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

fn is_valid_required_program(program: &str) -> bool {
    const DISPATCHERS: [&str; 26] = [
        ".", "source", "eval", "exec", "command", "builtin", "env", "xargs", "sh", "bash", "zsh",
        "dash", "ksh", "csh", "tcsh", "fish", "nohup", "time", "nice", "sudo", "doas", "su",
        "setsid", "stdbuf", "busybox", "toybox",
    ];
    let byte_is_safe =
        |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+' | b'/');
    if program.is_empty()
        || program.len() > TYPED_AGENT_CLI_MAX_BYTES
        || program.starts_with('-')
        || !program.bytes().all(byte_is_safe)
        || (program.contains('/') && !program.starts_with('/'))
        || program.contains("//")
        || program
            .split('/')
            .any(|component| matches!(component, "." | ".."))
    {
        return false;
    }
    let basename = program.rsplit('/').next().unwrap_or(program);
    !basename.is_empty()
        && !DISPATCHERS.contains(&basename)
        && basename.bytes().any(|byte| byte.is_ascii_alphanumeric())
}
