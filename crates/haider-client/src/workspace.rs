//! Dated default-workspace resolver (`docs/design/dated-workspace-v1.md` §2–3).
//!
//! Resolution is a pure decision over injected inputs plus read-only
//! project inspection: it NEVER creates a directory. The dated plan is
//! resolvable without being created (L1/L6); the only eager creation the
//! contract permits is the organizing root `<base>/Haider/<hijri-date>/`
//! via [`materialize_daily_root`], and the session leaf materialises on
//! first write only via [`materialize_leaf`] (L2). A session that writes
//! nothing therefore leaves nothing on disk (L3).

#[cfg(test)]
#[path = "workspace_tests.rs"]
mod tests;

use std::path::{Component, Path, PathBuf};

use crate::hijri::{HIJRI_CALENDAR_ID, HijriError, hijri_from_gregorian};
use haider_platform::{CivilDateSample, DocumentsLookup, documents_directory};
use haider_protocol::session::WorkspaceAllocationV1;

/// Exact existing absolute workspace path (environment selector).
pub const WORKSPACE_ENV: &str = "HAIDER_WORKSPACE";
/// Workspace mode (`auto`/`cwd`/`dated`) environment selector.
pub const WORKSPACE_MODE_ENV: &str = "HAIDER_WORKSPACE_MODE";
/// Absolute base directory that owns the literal `Haider` organizing child.
pub const WORKSPACE_BASE_ENV: &str = "HAIDER_WORKSPACE_BASE";
/// `local` (default) or `UTC` sampling policy.
pub const WORKSPACE_TIMEZONE_ENV: &str = "HAIDER_WORKSPACE_TIMEZONE";

/// Workspace selection mode (W2/W3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceMode {
    /// Preserve recognised projects and existing allocations; otherwise date.
    Auto,
    /// Preserve launch cwd (the one-invocation opt-out).
    Cwd,
    /// Force a dated allocation even inside a project.
    Dated,
}

impl WorkspaceMode {
    /// Parses the flag/env/config vocabulary; anything else is an error.
    pub fn parse(value: &str) -> Result<Self, WorkspaceError> {
        match value {
            "auto" => Ok(Self::Auto),
            "cwd" => Ok(Self::Cwd),
            "dated" => Ok(Self::Dated),
            other => Err(WorkspaceError::InvalidMode(other.to_string())),
        }
    }
}

/// Which entrypoint is asking (W1/W2): interactive defaults to `auto`,
/// headless `haider run` defaults to `cwd`. Peer/fleet/remote/Android
/// paths never construct a request at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceInvocation {
    Interactive,
    Headless,
}

/// Timezone policy for sampling "today" (C5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WorkspaceTimezone {
    #[default]
    Local,
    Utc,
}

impl WorkspaceTimezone {
    pub fn parse(value: &str) -> Result<Self, WorkspaceError> {
        match value {
            "local" => Ok(Self::Local),
            "UTC" => Ok(Self::Utc),
            other => Err(WorkspaceError::InvalidTimezone(other.to_string())),
        }
    }
}

/// Injected environment snapshot so tests never mutate process state.
#[derive(Debug, Clone, Default)]
pub struct WorkspaceEnvironment {
    /// `HAIDER_WORKSPACE`.
    pub workspace: Option<String>,
    /// `HAIDER_WORKSPACE_MODE`.
    pub mode: Option<String>,
    /// `HAIDER_WORKSPACE_BASE`.
    pub base: Option<String>,
    /// `HAIDER_WORKSPACE_TIMEZONE`.
    pub timezone: Option<String>,
    /// Whether any `CI` value is set (W3: implicit interactive -> `cwd`).
    pub ci: bool,
    /// Whether `HAIDER_PROFILE_DIR` was explicitly set (W6: harness scope).
    pub explicit_profile_dir: bool,
    /// Resolved home directory for Documents lookup and redaction.
    pub home: Option<PathBuf>,
}

impl WorkspaceEnvironment {
    /// Snapshots the real process environment.
    #[must_use]
    pub fn capture() -> Self {
        let nonempty = |name: &str| {
            std::env::var(name)
                .ok()
                .map(|value| value.trim().to_string())
        };
        Self {
            workspace: nonempty(WORKSPACE_ENV),
            mode: nonempty(WORKSPACE_MODE_ENV),
            base: nonempty(WORKSPACE_BASE_ENV),
            timezone: nonempty(WORKSPACE_TIMEZONE_ENV),
            ci: std::env::var_os("CI").is_some(),
            explicit_profile_dir: std::env::var_os(crate::profile::PROFILE_DIR_ENV).is_some(),
            home: std::env::var_os("HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from)),
        }
    }
}

/// Additive typed `workspace` object from the profile `config.json` (§2 W3,
/// W6). Absent keys mean "no opinion"; an invalid object only blocks a NEW
/// workspace selection, never status/recovery/opening existing sessions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceConfig {
    pub mode: Option<WorkspaceMode>,
    pub base: Option<PathBuf>,
    pub timezone: Option<WorkspaceTimezone>,
}

impl WorkspaceConfig {
    /// Reads the `workspace` object from an already-parsed profile config
    /// value. Unknown sibling keys are preserved elsewhere; unknown keys
    /// INSIDE `workspace` are tolerated for forward compatibility.
    pub fn from_config_value(value: &serde_json::Value) -> Result<Self, WorkspaceError> {
        let Some(workspace) = value.get("workspace") else {
            return Ok(Self::default());
        };
        let Some(object) = workspace.as_object() else {
            return Err(WorkspaceError::InvalidConfig(
                "`workspace` must be an object".to_string(),
            ));
        };
        let mode = match object.get("mode") {
            None => None,
            Some(serde_json::Value::String(text)) => Some(WorkspaceMode::parse(text)?),
            Some(_) => {
                return Err(WorkspaceError::InvalidConfig(
                    "`workspace.mode` must be a string".to_string(),
                ));
            }
        };
        let base = match object.get("base") {
            None => None,
            Some(serde_json::Value::String(text)) => {
                let path = PathBuf::from(text);
                if text.trim().is_empty() || !path.is_absolute() || has_parent_component(&path) {
                    return Err(WorkspaceError::InvalidConfig(
                        "`workspace.base` must be a nonempty absolute path without `..`"
                            .to_string(),
                    ));
                }
                Some(path)
            }
            Some(_) => {
                return Err(WorkspaceError::InvalidConfig(
                    "`workspace.base` must be a string".to_string(),
                ));
            }
        };
        let timezone = match object.get("timezone") {
            None => None,
            Some(serde_json::Value::String(text)) => Some(WorkspaceTimezone::parse(text)?),
            Some(_) => {
                return Err(WorkspaceError::InvalidConfig(
                    "`workspace.timezone` must be a string".to_string(),
                ));
            }
        };
        Ok(Self {
            mode,
            base,
            timezone,
        })
    }

    /// Loads the selected profile's `config.json` workspace object. A
    /// missing or unreadable file is simply "no opinion"; malformed JSON or
    /// an invalid workspace object is the explicit error W3 requires.
    pub fn load(store_dir: &Path) -> Result<Self, WorkspaceError> {
        let path = store_dir.join("config.json");
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(_) => return Ok(Self::default()),
        };
        let value: serde_json::Value = serde_json::from_str(&content).map_err(|error| {
            WorkspaceError::InvalidConfig(format!("config.json is not valid JSON: {error}"))
        })?;
        Self::from_config_value(&value)
    }
}

/// One workspace-resolution request. `launch_cwd` is the canonical
/// captured launch path (`None` when unavailable/non-UTF-8: the resolver
/// then refuses `cwd` outcomes rather than silently selecting `/`).
#[derive(Debug, Clone)]
pub struct WorkspaceRequest<'a> {
    pub invocation: WorkspaceInvocation,
    /// Explicit `--workspace <path>` (mutually exclusive with mode flag).
    pub explicit_workspace: Option<&'a str>,
    /// Explicit `--workspace-mode`.
    pub explicit_mode: Option<WorkspaceMode>,
    pub launch_cwd: Option<&'a Path>,
    pub environment: &'a WorkspaceEnvironment,
    pub config: &'a WorkspaceConfig,
    /// Canonical profile store directory (for the harness-scoped base).
    pub store_dir: &'a Path,
    /// The already-sampled civil date (policy-applied by the caller via
    /// [`sample_civil_date`]).
    pub sample: CivilDateSample,
    /// 16 random bytes for the allocation id (injected for determinism).
    pub allocation_entropy: [u8; 16],
}

/// Why launch cwd was preserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CwdReason {
    ExplicitWorkspace,
    ModeCwd,
    HeadlessDefault,
    CiDefault,
    /// A project marker matched at this ancestor.
    ProjectDetected {
        marker: String,
        boundary: PathBuf,
    },
    /// Launch cwd is inside an existing `<base>/Haider/<date>` allocation.
    InsideDatedAllocation,
    /// Ancestor inspection failed; conservatively preserve readable cwd.
    InspectionError(String),
}

/// The resolver's decision. `Dated` carries a PLAN: nothing exists on disk
/// until the caller materialises it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceSelection {
    LaunchCwd { path: PathBuf, reason: CwdReason },
    Dated(DatedWorkspacePlan),
}

/// Where the dated base came from (recorded for evidence/origin display).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceBaseSource {
    EnvBase,
    ConfigBase,
    ProfileStoreWorkspaces,
    PlatformDocuments,
    HomeFallback,
}

/// A resolved-but-unmaterialised dated allocation (L1/L6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatedWorkspacePlan {
    pub base: PathBuf,
    pub base_source: WorkspaceBaseSource,
    /// `<base>/Haider/<hijri-date>` — the only eager-creation carve-out.
    pub daily_root: PathBuf,
    /// `<daily_root>/s-<32 lowercase hex>` — the session tool workspace.
    pub leaf: PathBuf,
    /// 128-bit lowercase-hex allocation id (never derived from user data).
    pub allocation_id: String,
    pub calendar_id: &'static str,
    /// islamic-civil `YYYY-MM-DD` label.
    pub hijri_label: String,
    /// Sampled Gregorian `YYYY-MM-DD` label.
    pub gregorian_label: String,
    pub sampled_utc_ms: u64,
    pub offset_seconds: i32,
}

/// Explicit refusals; nothing here falls back silently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceError {
    InvalidMode(String),
    InvalidTimezone(String),
    InvalidConfig(String),
    /// `--workspace` and `--workspace-mode` on the same invocation.
    ConflictingSelectors,
    /// Both `HAIDER_WORKSPACE` and `HAIDER_WORKSPACE_MODE` are set.
    AmbiguousEnvironment,
    /// A selector value was empty.
    EmptySelector(&'static str),
    /// `HAIDER_WORKSPACE` must be an exact existing absolute directory.
    EnvWorkspaceInvalid(String),
    /// Explicit workspace path does not exist or is not a directory.
    ExplicitWorkspaceMissing(String),
    /// Launch cwd is unavailable and the outcome would need it.
    LaunchCwdUnavailable,
    /// The calendar refused the sampled date.
    Calendar(HijriError),
    /// `HAIDER_WORKSPACE_BASE`/config base must be nonempty and absolute.
    InvalidBase(String),
    /// A resolved platform path cannot be represented on the JSON wire.
    NonUtf8Path(PathBuf),
}

impl std::fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidMode(mode) => {
                write!(f, "invalid workspace mode {mode:?} (auto|cwd|dated)")
            }
            Self::InvalidTimezone(tz) => {
                write!(f, "invalid workspace timezone {tz:?} (local|UTC)")
            }
            Self::InvalidConfig(reason) => write!(f, "invalid workspace config: {reason}"),
            Self::ConflictingSelectors => {
                f.write_str("--workspace and --workspace-mode are mutually exclusive")
            }
            Self::AmbiguousEnvironment => {
                f.write_str("HAIDER_WORKSPACE and HAIDER_WORKSPACE_MODE are both set; unset one")
            }
            Self::EmptySelector(name) => write!(f, "{name} is set but empty"),
            Self::EnvWorkspaceInvalid(path) => write!(
                f,
                "HAIDER_WORKSPACE must be an existing absolute directory: {path}"
            ),
            Self::ExplicitWorkspaceMissing(path) => {
                write!(f, "workspace path does not exist: {path}")
            }
            Self::LaunchCwdUnavailable => f.write_str(
                "launch directory is unavailable; pass --workspace or use --workspace-mode dated",
            ),
            Self::Calendar(error) => write!(f, "calendar error: {error}"),
            Self::InvalidBase(reason) => write!(f, "invalid workspace base: {reason}"),
            Self::NonUtf8Path(path) => {
                write!(f, "workspace path is not valid UTF-8: {}", path.display())
            }
        }
    }
}

impl std::error::Error for WorkspaceError {}

impl From<HijriError> for WorkspaceError {
    fn from(error: HijriError) -> Self {
        Self::Calendar(error)
    }
}

/// Samples "today" under the requested policy (env outranks config); a
/// local-offset failure REQUIRES the explicit UTC policy rather than
/// silently changing dates (C5).
pub fn sample_civil_date(
    environment: &WorkspaceEnvironment,
    config: &WorkspaceConfig,
) -> Result<CivilDateSample, WorkspaceError> {
    let timezone = match environment.timezone.as_deref() {
        Some("") => return Err(WorkspaceError::EmptySelector(WORKSPACE_TIMEZONE_ENV)),
        Some(text) => WorkspaceTimezone::parse(text)?,
        None => config.timezone.unwrap_or_default(),
    };
    let sample = match timezone {
        WorkspaceTimezone::Local => haider_platform::sample_local_civil_date(),
        WorkspaceTimezone::Utc => haider_platform::sample_utc_civil_date(),
    };
    sample.map_err(|error| WorkspaceError::InvalidTimezone(error.to_string()))
}

/// Resolves the workspace selection for one NEW local desktop session.
/// Read-only: performs no directory creation (L1). Existing sessions never
/// call this (their stored workspace is authoritative).
pub fn resolve_workspace(
    request: &WorkspaceRequest<'_>,
) -> Result<WorkspaceSelection, WorkspaceError> {
    if request.explicit_workspace.is_some() && request.explicit_mode.is_some() {
        return Err(WorkspaceError::ConflictingSelectors);
    }

    // 1. Explicit path: exact, must exist, relative resolves against launch cwd.
    if let Some(explicit) = request.explicit_workspace {
        if explicit.trim().is_empty() {
            return Err(WorkspaceError::EmptySelector("--workspace"));
        }
        let candidate = PathBuf::from(explicit);
        let path = if candidate.is_absolute() {
            candidate
        } else {
            let Some(launch) = request.launch_cwd else {
                return Err(WorkspaceError::LaunchCwdUnavailable);
            };
            launch.join(candidate)
        };
        if !path.is_dir() {
            return Err(WorkspaceError::ExplicitWorkspaceMissing(
                path.display().to_string(),
            ));
        }
        return Ok(WorkspaceSelection::LaunchCwd {
            path,
            reason: CwdReason::ExplicitWorkspace,
        });
    }

    // 2..4. Mode selection: flag > env > config > invocation default.
    let mode = if let Some(mode) = request.explicit_mode {
        mode
    } else {
        match (
            request.environment.workspace.as_deref(),
            request.environment.mode.as_deref(),
        ) {
            (Some(_), Some(_)) => return Err(WorkspaceError::AmbiguousEnvironment),
            (Some(""), None) => return Err(WorkspaceError::EmptySelector(WORKSPACE_ENV)),
            (Some(path), None) => {
                let env_path = PathBuf::from(path);
                if !env_path.is_absolute() || !env_path.is_dir() {
                    return Err(WorkspaceError::EnvWorkspaceInvalid(path.to_string()));
                }
                return Ok(WorkspaceSelection::LaunchCwd {
                    path: env_path,
                    reason: CwdReason::ExplicitWorkspace,
                });
            }
            (None, Some("")) => return Err(WorkspaceError::EmptySelector(WORKSPACE_MODE_ENV)),
            (None, Some(mode)) => WorkspaceMode::parse(mode)?,
            (None, None) => match request.config.mode {
                Some(mode) => mode,
                None => match request.invocation {
                    WorkspaceInvocation::Headless => WorkspaceMode::Cwd,
                    WorkspaceInvocation::Interactive if request.environment.ci => {
                        WorkspaceMode::Cwd
                    }
                    WorkspaceInvocation::Interactive => WorkspaceMode::Auto,
                },
            },
        }
    };

    let ci_implicit = request.explicit_mode.is_none()
        && request.environment.mode.is_none()
        && request.config.mode.is_none()
        && request.environment.ci
        && request.invocation == WorkspaceInvocation::Interactive;

    match mode {
        WorkspaceMode::Cwd => {
            let Some(launch) = request.launch_cwd else {
                return Err(WorkspaceError::LaunchCwdUnavailable);
            };
            let reason = if ci_implicit {
                CwdReason::CiDefault
            } else if request.explicit_mode.is_none()
                && request.environment.mode.is_none()
                && request.config.mode.is_none()
                && request.invocation == WorkspaceInvocation::Headless
            {
                CwdReason::HeadlessDefault
            } else {
                CwdReason::ModeCwd
            };
            Ok(WorkspaceSelection::LaunchCwd {
                path: launch.to_path_buf(),
                reason,
            })
        }
        WorkspaceMode::Dated => Ok(WorkspaceSelection::Dated(dated_plan(request)?)),
        WorkspaceMode::Auto => {
            if let Some(launch) = request.launch_cwd {
                match detect_preserved_root(launch, request) {
                    PreservationOutcome::Preserve(reason) => Ok(WorkspaceSelection::LaunchCwd {
                        path: launch.to_path_buf(),
                        reason,
                    }),
                    PreservationOutcome::Allocate => {
                        Ok(WorkspaceSelection::Dated(dated_plan(request)?))
                    }
                }
            } else {
                // No cwd: interactive auto may still allocate (never `/`).
                Ok(WorkspaceSelection::Dated(dated_plan(request)?))
            }
        }
    }
}

enum PreservationOutcome {
    Preserve(CwdReason),
    Allocate,
}

/// W4 project markers, checked by name/type only.
const PROJECT_FILE_MARKERS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    "CMakeLists.txt",
    "build.gradle",
    "build.gradle.kts",
    "Makefile",
    ".haider-project",
];

fn detect_preserved_root(launch: &Path, request: &WorkspaceRequest<'_>) -> PreservationOutcome {
    // Existing dated allocation of this profile's resolved base (W2): a
    // reopened leaf must not nest dates endlessly.
    if let Ok((base, _)) = resolve_base(request)
        && path_is_inside_dated_allocation(launch, &base)
    {
        return PreservationOutcome::Preserve(CwdReason::InsideDatedAllocation);
    }
    for ancestor in launch.ancestors() {
        // VCS directories, including `.git` files (worktrees/submodules).
        for vcs in [".git", ".hg", ".svn"] {
            let candidate = ancestor.join(vcs);
            match std::fs::symlink_metadata(&candidate) {
                Ok(metadata) if metadata.is_dir() || (vcs == ".git" && metadata.is_file()) => {
                    return PreservationOutcome::Preserve(CwdReason::ProjectDetected {
                        marker: vcs.to_string(),
                        boundary: ancestor.to_path_buf(),
                    });
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return PreservationOutcome::Preserve(CwdReason::InspectionError(
                        error.to_string(),
                    ));
                }
            }
        }
        for marker in PROJECT_FILE_MARKERS {
            let candidate = ancestor.join(marker);
            match std::fs::symlink_metadata(&candidate) {
                Ok(metadata) if metadata.is_file() => {
                    return PreservationOutcome::Preserve(CwdReason::ProjectDetected {
                        marker: (*marker).to_string(),
                        boundary: ancestor.to_path_buf(),
                    });
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return PreservationOutcome::Preserve(CwdReason::InspectionError(
                        error.to_string(),
                    ));
                }
            }
        }
        // Bare Git repository: objects/ + refs/ directories + regular HEAD.
        let bare = ancestor.join("objects").is_dir()
            && ancestor.join("refs").is_dir()
            && ancestor
                .join("HEAD")
                .symlink_metadata()
                .map(|m| m.is_file())
                .unwrap_or(false);
        if bare {
            return PreservationOutcome::Preserve(CwdReason::ProjectDetected {
                marker: "bare-git".to_string(),
                boundary: ancestor.to_path_buf(),
            });
        }
    }
    PreservationOutcome::Allocate
}

/// Whether `path` sits inside `<base>/Haider/<date-label>/...`.
#[must_use]
pub fn path_is_inside_dated_allocation(path: &Path, base: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(base) else {
        return false;
    };
    let mut components = relative.components();
    let Some(Component::Normal(first)) = components.next() else {
        return false;
    };
    if first != std::ffi::OsStr::new("Haider") {
        return false;
    }
    let Some(Component::Normal(label)) = components.next() else {
        return false;
    };
    label.to_str().map(is_date_label_like).unwrap_or(false)
}

fn is_date_label_like(label: &str) -> bool {
    let bytes = label.as_bytes();
    if bytes.len() < 10 {
        return false;
    }
    let (year, rest) = bytes.split_at(bytes.len() - 6);
    year.iter().all(u8::is_ascii_digit)
        && year.len() >= 4
        && rest[0] == b'-'
        && rest[1].is_ascii_digit()
        && rest[2].is_ascii_digit()
        && rest[3] == b'-'
        && rest[4].is_ascii_digit()
        && rest[5].is_ascii_digit()
}

fn resolve_base(
    request: &WorkspaceRequest<'_>,
) -> Result<(PathBuf, WorkspaceBaseSource), WorkspaceError> {
    if let Some(base) = request.environment.base.as_deref() {
        if base.trim().is_empty() {
            return Err(WorkspaceError::EmptySelector(WORKSPACE_BASE_ENV));
        }
        let path = PathBuf::from(base);
        validate_selected_base(&path, WORKSPACE_BASE_ENV)?;
        return Ok((path, WorkspaceBaseSource::EnvBase));
    }
    if let Some(base) = request.config.base.as_deref() {
        validate_selected_base(base, "workspace.base")?;
        return Ok((base.to_path_buf(), WorkspaceBaseSource::ConfigBase));
    }
    if request.environment.explicit_profile_dir {
        return Ok((
            request.store_dir.join("workspaces"),
            WorkspaceBaseSource::ProfileStoreWorkspaces,
        ));
    }
    let Some(home) = request.environment.home.as_deref() else {
        return Err(WorkspaceError::InvalidBase(
            "no home directory available for the Documents convention".to_string(),
        ));
    };
    match documents_directory(home) {
        DocumentsLookup::Documents(path) => Ok((path, WorkspaceBaseSource::PlatformDocuments)),
        DocumentsLookup::HomeFallback { home, .. } => Ok((home, WorkspaceBaseSource::HomeFallback)),
    }
}

fn validate_selected_base(path: &Path, source: &str) -> Result<(), WorkspaceError> {
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err(WorkspaceError::InvalidBase(format!(
            "{source} must be a nonempty absolute path: {}",
            path.display()
        )));
    }
    if has_parent_component(path) {
        return Err(WorkspaceError::InvalidBase(format!(
            "{source} must not contain `..`: {}",
            path.display()
        )));
    }
    Ok(())
}

fn has_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| component == Component::ParentDir)
}

fn dated_plan(request: &WorkspaceRequest<'_>) -> Result<DatedWorkspacePlan, WorkspaceError> {
    let (base, base_source) = resolve_base(request)?;
    let sample = request.sample;
    let hijri = hijri_from_gregorian(sample.year, sample.month, sample.day)?;
    let hijri_label = hijri.label();
    let gregorian_label = format!("{:04}-{:02}-{:02}", sample.year, sample.month, sample.day);
    let allocation_id = hex_lower(&request.allocation_entropy);
    let daily_root = base.join("Haider").join(&hijri_label);
    let leaf = daily_root.join(format!("s-{allocation_id}"));
    Ok(DatedWorkspacePlan {
        base,
        base_source,
        daily_root,
        leaf,
        allocation_id,
        calendar_id: HIJRI_CALENDAR_ID,
        hijri_label,
        gregorian_label,
        sampled_utc_ms: sample.utc_ms,
        offset_seconds: sample.offset_seconds,
    })
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// Generates fresh allocation entropy from the OS CSPRNG.
pub fn allocation_entropy() -> Result<[u8; 16], WorkspaceError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| WorkspaceError::InvalidBase(format!("entropy unavailable: {error}")))?;
    Ok(bytes)
}

/// Converts a resolved dated plan into the additive create-request metadata.
/// This is a representation step only and performs no filesystem mutation.
pub fn workspace_allocation(
    plan: &DatedWorkspacePlan,
) -> Result<WorkspaceAllocationV1, WorkspaceError> {
    let daily_root_path = std::fs::canonicalize(&plan.daily_root).map_err(|error| {
        WorkspaceError::InvalidBase(format!(
            "cannot canonicalize daily workspace root {}: {error}",
            plan.daily_root.display()
        ))
    })?;
    let leaf_name = plan
        .leaf
        .file_name()
        .ok_or_else(|| WorkspaceError::InvalidBase("workspace leaf has no filename".into()))?;
    let leaf_path = daily_root_path.join(leaf_name);
    let daily_root = daily_root_path
        .to_str()
        .ok_or_else(|| WorkspaceError::NonUtf8Path(daily_root_path.clone()))?;
    let leaf = leaf_path
        .to_str()
        .ok_or_else(|| WorkspaceError::NonUtf8Path(leaf_path.clone()))?;
    Ok(WorkspaceAllocationV1 {
        daily_root: daily_root.to_owned(),
        leaf: leaf.to_owned(),
        allocation_id: plan.allocation_id.clone(),
        calendar_id: plan.calendar_id.to_owned(),
        hijri_date: plan.hijri_label.clone(),
        gregorian_date: plan.gregorian_label.clone(),
        allocated_at_ms: plan.sampled_utc_ms,
        offset_seconds: plan.offset_seconds,
    })
}

/// Mints a sibling allocation for the next session created by the same TUI
/// process. The launch-time date/root remain stable, while allocation identity
/// and timestamp are fresh. This is pure: the new leaf is not created here.
pub fn renew_workspace_allocation(
    previous: &WorkspaceAllocationV1,
) -> Result<WorkspaceAllocationV1, MaterializeError> {
    let previous_plan = plan_from_workspace_allocation(previous)?;
    let allocation_id = hex_lower(
        &allocation_entropy().map_err(|error| MaterializeError::InvalidPlan(error.to_string()))?,
    );
    let leaf = previous_plan.daily_root.join(format!("s-{allocation_id}"));
    let leaf = leaf
        .to_str()
        .ok_or_else(|| MaterializeError::InvalidPlan("workspace leaf is not valid UTF-8".into()))?;
    let allocated_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| MaterializeError::InvalidPlan(error.to_string()))?
        .as_millis()
        .try_into()
        .map_err(|_| MaterializeError::InvalidPlan("allocation timestamp overflow".into()))?;
    Ok(WorkspaceAllocationV1 {
        daily_root: previous.daily_root.clone(),
        leaf: leaf.to_owned(),
        allocation_id,
        calendar_id: previous.calendar_id.clone(),
        hijri_date: previous.hijri_date.clone(),
        gregorian_date: previous.gregorian_date.clone(),
        allocated_at_ms,
        offset_seconds: previous.offset_seconds,
    })
}

/// Returns the supplied unmaterialised allocation when its leaf is absent,
/// otherwise mints bounded fresh siblings. This is the pre-session collision
/// pass; it never creates the leaf and the daemon still revalidates the exact
/// result at admission to close the race window safely.
pub fn available_workspace_allocation(
    initial: WorkspaceAllocationV1,
) -> Result<WorkspaceAllocationV1, MaterializeError> {
    let mut candidate = initial;
    for _ in 0..8 {
        match std::fs::symlink_metadata(&candidate.leaf) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(candidate),
            Ok(_) => candidate = renew_workspace_allocation(&candidate)?,
            Err(error) => return Err(MaterializeError::Io(candidate.leaf.into(), error)),
        }
    }
    Err(MaterializeError::CollisionRetriesExhausted)
}

/// Materialisation failures (L4/L5).
#[derive(Debug)]
pub enum MaterializeError {
    /// The validated base does not exist or is not a directory; no silent
    /// fallback (W6).
    BaseUnavailable(PathBuf, std::io::Error),
    /// A public plan was inconsistent or could escape its selected base.
    InvalidPlan(String),
    /// An existing child below the anchored base was a symlink, reparse
    /// point, regular file, or otherwise not a real directory.
    UnsafeComponent(PathBuf),
    /// Creating the organizing root or leaf failed.
    Io(PathBuf, std::io::Error),
    /// Eight fresh ids all collided — something is replaying entropy.
    CollisionRetriesExhausted,
    /// The created leaf failed post-create validation and was removed.
    LeafValidationFailed(PathBuf),
    /// The exact persisted allocation leaf already exists before its first
    /// materialisation attempt.
    LeafAlreadyExists(PathBuf),
}

impl std::fmt::Display for MaterializeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BaseUnavailable(path, error) => {
                write!(
                    f,
                    "workspace base unavailable at {}: {error}",
                    path.display()
                )
            }
            Self::InvalidPlan(reason) => write!(f, "invalid workspace plan: {reason}"),
            Self::UnsafeComponent(path) => write!(
                f,
                "workspace path component is not a real directory: {}",
                path.display()
            ),
            Self::Io(path, error) => write!(f, "cannot create {}: {error}", path.display()),
            Self::CollisionRetriesExhausted => {
                f.write_str("workspace leaf collision retries exhausted")
            }
            Self::LeafValidationFailed(path) => {
                write!(f, "created leaf failed validation: {}", path.display())
            }
            Self::LeafAlreadyExists(path) => {
                write!(
                    f,
                    "workspace allocation leaf already exists: {}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for MaterializeError {}

/// Creates `<base>/Haider/<hijri-date>/` — the ONLY eager-creation
/// carve-out (L2). The base itself must already exist; its absence is a
/// visible error, never a fallback. Idempotent.
pub fn materialize_daily_root(plan: &DatedWorkspacePlan) -> Result<PathBuf, MaterializeError> {
    materialize_daily_root_directory(plan)?;
    Ok(plan.daily_root.clone())
}

/// The materialised session leaf; `allocation_id`/`leaf` may differ from
/// the plan after a genuine collision retry (L4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedLeaf {
    pub leaf: PathBuf,
    pub allocation_id: String,
    pub retries: u8,
}

/// Atomically creates the session leaf under an existing daily root: first
/// write only (L2), create-new semantics, bounded fresh-id retries on
/// genuine collision, owner-private on Unix (L4). If the created leaf
/// fails post-create validation this call removes ONLY that still-empty
/// leaf (L5); once this function returns success, no later automatic
/// cleanup ever happens.
pub fn materialize_leaf(plan: &DatedWorkspacePlan) -> Result<MaterializedLeaf, MaterializeError> {
    let daily_root = materialize_daily_root_directory(plan)?;
    let mut allocation_id = plan.allocation_id.clone();
    for retry in 0u8..8 {
        let leaf = plan.daily_root.join(format!("s-{allocation_id}"));
        let name = leaf
            .file_name()
            .ok_or_else(|| MaterializeError::InvalidPlan("leaf has no filename".into()))?;
        match create_private_leaf(&daily_root, name, &leaf) {
            Ok(directory) => {
                drop(directory);
                return Ok(MaterializedLeaf {
                    leaf,
                    allocation_id,
                    retries: retry,
                });
            }
            Err(PrivateLeafError::AlreadyExists) => {
                let entropy = allocation_entropy()
                    .map_err(|_| MaterializeError::CollisionRetriesExhausted)?;
                allocation_id = hex_lower(&entropy);
            }
            Err(PrivateLeafError::Materialize(error)) => return Err(error),
        }
    }
    Err(MaterializeError::CollisionRetriesExhausted)
}

/// An existing daily-root anchor validated for a pending allocation. Keeping
/// the descriptor alive closes the validation/commit rename race at callers.
#[derive(Debug)]
pub struct ValidatedWorkspaceAllocation {
    pub plan: DatedWorkspacePlan,
    pub daily_root: haider_platform::WorkspaceDirectory,
}

/// Validates an additive create allocation without creating its leaf.
/// The exact leaf must be absent and the daily root must be reachable through
/// a no-follow component walk.
pub fn validate_workspace_allocation(
    cwd: &Path,
    allocation: &WorkspaceAllocationV1,
) -> Result<ValidatedWorkspaceAllocation, MaterializeError> {
    let plan = plan_from_workspace_allocation(allocation)?;
    if cwd != plan.leaf {
        return Err(MaterializeError::InvalidPlan(
            "session cwd is not the allocation leaf".into(),
        ));
    }
    let daily_root = open_anchored_absolute_directory(&plan.daily_root).map_err(|error| {
        MaterializeError::Io(
            plan.daily_root.clone(),
            workspace_directory_error_to_io(error),
        )
    })?;
    match std::fs::symlink_metadata(&plan.leaf) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => return Err(MaterializeError::LeafAlreadyExists(plan.leaf.clone())),
        Err(error) => return Err(MaterializeError::Io(plan.leaf.clone(), error)),
    }
    Ok(ValidatedWorkspaceAllocation { plan, daily_root })
}

/// Exact first-write materialisation for a persisted allocation. Unlike the
/// pre-session allocator, this never retries to a different id: the stored
/// leaf is session authority.
pub fn materialize_workspace_allocation(
    allocation: &WorkspaceAllocationV1,
) -> Result<haider_platform::WorkspaceDirectory, MaterializeError> {
    let plan = plan_from_workspace_allocation(allocation)?;
    let daily_root = open_anchored_absolute_directory(&plan.daily_root).map_err(|error| {
        MaterializeError::Io(
            plan.daily_root.clone(),
            workspace_directory_error_to_io(error),
        )
    })?;
    let name = plan
        .leaf
        .file_name()
        .ok_or_else(|| MaterializeError::InvalidPlan("leaf has no filename".into()))?;
    create_private_leaf(&daily_root, name, &plan.leaf).map_err(|error| match error {
        PrivateLeafError::AlreadyExists => MaterializeError::LeafAlreadyExists(plan.leaf),
        PrivateLeafError::Materialize(error) => error,
    })
}

fn plan_from_workspace_allocation(
    allocation: &WorkspaceAllocationV1,
) -> Result<DatedWorkspacePlan, MaterializeError> {
    if allocation.calendar_id != HIJRI_CALENDAR_ID {
        return Err(MaterializeError::InvalidPlan(
            "unsupported workspace calendar id".into(),
        ));
    }
    let daily_root = PathBuf::from(&allocation.daily_root);
    let leaf = PathBuf::from(&allocation.leaf);
    let base = daily_root
        .parent()
        .filter(|parent| parent.file_name() == Some(std::ffi::OsStr::new("Haider")))
        .and_then(Path::parent)
        .ok_or_else(|| {
            MaterializeError::InvalidPlan(
                "daily root is not below a literal Haider directory".into(),
            )
        })?
        .to_path_buf();
    let plan = DatedWorkspacePlan {
        base,
        base_source: WorkspaceBaseSource::HomeFallback,
        daily_root,
        leaf,
        allocation_id: allocation.allocation_id.clone(),
        calendar_id: HIJRI_CALENDAR_ID,
        hijri_label: allocation.hijri_date.clone(),
        gregorian_label: allocation.gregorian_date.clone(),
        sampled_utc_ms: allocation.allocated_at_ms,
        offset_seconds: allocation.offset_seconds,
    };
    validate_materialization_plan(&plan)?;
    if plan
        .daily_root
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        != Some(plan.hijri_label.as_str())
        || !is_date_label_like(&plan.gregorian_label)
    {
        return Err(MaterializeError::InvalidPlan(
            "allocation date labels are inconsistent".into(),
        ));
    }
    Ok(plan)
}

fn validate_materialization_plan(plan: &DatedWorkspacePlan) -> Result<(), MaterializeError> {
    if !plan.base.is_absolute() || has_parent_component(&plan.base) {
        return Err(MaterializeError::InvalidPlan(
            "base must be absolute and contain no `..`".into(),
        ));
    }
    if !is_date_label_like(&plan.hijri_label)
        || !matches!(
            Path::new(&plan.hijri_label)
                .components()
                .collect::<Vec<_>>()[..],
            [Component::Normal(_)]
        )
    {
        return Err(MaterializeError::InvalidPlan(
            "Hijri label must be one date component".into(),
        ));
    }
    if plan.allocation_id.len() != 32
        || !plan
            .allocation_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(MaterializeError::InvalidPlan(
            "allocation id must be 32 lowercase hexadecimal characters".into(),
        ));
    }
    let expected_daily_root = plan.base.join("Haider").join(&plan.hijri_label);
    if plan.daily_root != expected_daily_root {
        return Err(MaterializeError::InvalidPlan(
            "daily root is not the selected base's Haider/date child".into(),
        ));
    }
    if plan.leaf != expected_daily_root.join(format!("s-{}", plan.allocation_id)) {
        return Err(MaterializeError::InvalidPlan(
            "leaf is not the daily root's allocation child".into(),
        ));
    }
    Ok(())
}

fn materialize_daily_root_directory(
    plan: &DatedWorkspacePlan,
) -> Result<haider_platform::WorkspaceDirectory, MaterializeError> {
    validate_materialization_plan(plan)?;
    let mut directory = match haider_platform::open_workspace_directory(&plan.base) {
        Ok(directory) => directory,
        Err(base_error) if plan.base_source == WorkspaceBaseSource::ProfileStoreWorkspaces => {
            let parent = plan.base.parent().ok_or_else(|| {
                MaterializeError::InvalidPlan("profile workspace base has no parent".into())
            })?;
            if plan.base.file_name() != Some(std::ffi::OsStr::new("workspaces")) {
                return Err(MaterializeError::InvalidPlan(
                    "profile workspace base is not the workspaces child".into(),
                ));
            }
            let parent_directory =
                haider_platform::open_workspace_directory(parent).map_err(|_| {
                    MaterializeError::BaseUnavailable(
                        plan.base.clone(),
                        workspace_directory_error_to_io(base_error),
                    )
                })?;
            create_or_open_private_directory(
                parent_directory,
                std::ffi::OsStr::new("workspaces"),
                &plan.base,
            )?
        }
        Err(error) => {
            return Err(MaterializeError::BaseUnavailable(
                plan.base.clone(),
                workspace_directory_error_to_io(error),
            ));
        }
    };
    let haider = plan.base.join("Haider");
    directory =
        create_or_open_private_directory(directory, std::ffi::OsStr::new("Haider"), &haider)?;
    create_or_open_private_directory(
        directory,
        std::ffi::OsStr::new(&plan.hijri_label),
        &plan.daily_root,
    )
}

/// Reopens an absolute directory while retaining authority over every path
/// component. Unix can walk from `/` with `openat`; Windows uses the stage-1
/// root-to-leaf handle chain so an ancestor cannot be replaced underneath a
/// later path-based operation.
#[cfg(unix)]
fn open_anchored_absolute_directory(
    path: &Path,
) -> Result<haider_platform::WorkspaceDirectory, haider_platform::WorkspaceDirectoryError> {
    haider_platform::open_absolute_directory(path)
}

#[cfg(windows)]
fn open_anchored_absolute_directory(
    path: &Path,
) -> Result<haider_platform::WorkspaceDirectory, haider_platform::WorkspaceDirectoryError> {
    haider_platform::open_workspace_directory(path)
}

#[cfg(unix)]
fn workspace_directory_error_to_io(
    error: haider_platform::WorkspaceDirectoryError,
) -> std::io::Error {
    error.into()
}

#[cfg(windows)]
fn workspace_directory_error_to_io(
    error: haider_platform::WorkspaceDirectoryError,
) -> std::io::Error {
    error
}

#[cfg(unix)]
fn create_or_open_private_directory(
    directory: haider_platform::WorkspaceDirectory,
    name: &std::ffi::OsStr,
    path: &Path,
) -> Result<haider_platform::WorkspaceDirectory, MaterializeError> {
    use rustix::fs::{Mode, OFlags};

    match rustix::fs::mkdirat(&directory, name, Mode::from_raw_mode(0o700)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(error) => return Err(MaterializeError::Io(path.to_path_buf(), error.into())),
    }
    rustix::fs::openat(
        &directory,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| match error {
        rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR => {
            MaterializeError::UnsafeComponent(path.to_path_buf())
        }
        _ => MaterializeError::Io(path.to_path_buf(), error.into()),
    })
}

#[cfg(windows)]
fn create_or_open_private_directory(
    directory: haider_platform::WorkspaceDirectory,
    name: &std::ffi::OsStr,
    path: &Path,
) -> Result<haider_platform::WorkspaceDirectory, MaterializeError> {
    use std::os::windows::fs::MetadataExt as _;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    haider_platform::open_workspace_subdirectory(directory, Path::new(name), true).map_err(
        |error| match std::fs::symlink_metadata(path) {
            Ok(metadata)
                if !metadata.is_dir()
                    || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 =>
            {
                MaterializeError::UnsafeComponent(path.to_path_buf())
            }
            _ => MaterializeError::Io(path.to_path_buf(), error),
        },
    )
}

enum PrivateLeafError {
    AlreadyExists,
    Materialize(MaterializeError),
}

#[cfg(unix)]
fn create_private_leaf(
    daily_root: &haider_platform::WorkspaceDirectory,
    name: &std::ffi::OsStr,
    path: &Path,
) -> Result<haider_platform::WorkspaceDirectory, PrivateLeafError> {
    use rustix::fs::{AtFlags, Mode, OFlags};

    match rustix::fs::mkdirat(daily_root, name, Mode::from_raw_mode(0o700)) {
        Ok(()) => {}
        Err(rustix::io::Errno::EXIST) => return Err(PrivateLeafError::AlreadyExists),
        Err(error) => {
            return Err(PrivateLeafError::Materialize(MaterializeError::Io(
                path.to_path_buf(),
                error.into(),
            )));
        }
    }
    match rustix::fs::openat(
        daily_root,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(directory) => Ok(directory),
        Err(_) => {
            let _ = rustix::fs::unlinkat(daily_root, name, AtFlags::REMOVEDIR);
            Err(PrivateLeafError::Materialize(
                MaterializeError::LeafValidationFailed(path.to_path_buf()),
            ))
        }
    }
}

#[cfg(windows)]
fn create_private_leaf(
    daily_root: &haider_platform::WorkspaceDirectory,
    name: &std::ffi::OsStr,
    path: &Path,
) -> Result<haider_platform::WorkspaceDirectory, PrivateLeafError> {
    let leaf = daily_root.path().join(name);
    match std::fs::create_dir(&leaf) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(PrivateLeafError::AlreadyExists);
        }
        Err(error) => {
            return Err(PrivateLeafError::Materialize(MaterializeError::Io(
                path.to_path_buf(),
                error,
            )));
        }
    }
    let parent = haider_platform::duplicate_workspace_directory(daily_root).map_err(|error| {
        PrivateLeafError::Materialize(MaterializeError::Io(path.to_path_buf(), error))
    })?;
    match haider_platform::open_workspace_subdirectory(parent, Path::new(name), false) {
        Ok(directory) => Ok(directory),
        Err(_) => {
            let _ = std::fs::remove_dir(&leaf);
            Err(PrivateLeafError::Materialize(
                MaterializeError::LeafValidationFailed(path.to_path_buf()),
            ))
        }
    }
}
