//! Account-side plumbing for the supervised Google Antigravity agent.
//!
//! [`antigravity_install`](crate::antigravity_install) owns the archive and
//! the on-disk version tree; `haider_provider::acp` owns the protocol. This
//! module is the seam between them and the account layer: it turns one account
//! alias into a private `GEMINI_HOME`, resolves the already-installed
//! executable into a launch spec, decides which model a turn runs on, and
//! wraps the result in a [`Provider`] the factory can hand back synchronously.
//!
//! Three laws shape everything here.
//!
//! - **Haider holds no Google credential.** Google's agent owns its own OAuth
//!   material under `$GEMINI_HOME`, so this lane resolves NO vault secret and
//!   mints no placeholder [`haider_accounts::SecretHandle`]. A handle that
//!   existed would be a credential Haider does not have.
//! - **One alias, one profile.** Two Google accounts must never share a
//!   profile directory, and therefore never share sessions, auth state or the
//!   effect of a logout. The directory name is a digest of the alias, never
//!   raw user text, so no alias can name a path.
//! - **Never install inside a turn.** A missing install is a typed, actionable
//!   refusal. A download is a big, slow, explicitly operator-approved act; it
//!   is not something a prompt triggers.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use haider_accounts::CredentialAlias;
use haider_protocol::error::{ErrorCode, HaiderError};
use haider_protocol::provider::CapabilityDoc;
use haider_provider::acp::client::AcpLaunchSpec;
use haider_provider::{
    AntigravityAcpProvider, AntigravitySessionConfig, Provider, ProviderError, ProviderStream,
    TurnRequest,
};

use crate::antigravity_install::{
    ANTIGRAVITY_VERSION, AntigravityInstallError, AntigravityInstallation, AntigravityInstaller,
    AntigravityLease, pin_for_host,
};

/// Mode for every directory this module creates. Matches
/// [`crate::antigravity_install::DIRECTORY_MODE`]: the profile holds Google's
/// own token file, so it is owner-only exactly like the install tree.
pub(crate) const PROFILE_DIRECTORY_MODE: u32 = 0o700;

/// Operator override for the Antigravity runtime root.
///
/// Present for the same reason the OAuth import sources have one: an operator
/// may need the ~900 MiB install and the per-account profiles on a different
/// volume from `$HOME`.
pub(crate) const ANTIGRAVITY_HOME_ENV: &str = "HAIDER_ANTIGRAVITY_HOME";

/// Root-relative directory holding the pinned install tree.
pub(crate) const INSTALL_DIRECTORY: &str = "install";

/// Root-relative directory holding one private `GEMINI_HOME` per alias.
pub(crate) const PROFILES_DIRECTORY: &str = "profiles";

/// The model a NEW session prefers — and ONLY when the authenticated agent
/// offers this exact slug.
///
/// Antigravity's catalog drifts server-side and carries irregular slugs
/// (`gemini-pro-agent` is not `<family>-<tier>` shaped at all), so membership
/// is decided by exact string equality and a slug is never parsed
/// structurally.
pub(crate) const PREFERRED_NEW_SESSION_MODEL: &str = "gemini-3.8-flash-high";

/// Hex characters kept from the alias digest when naming a profile directory.
///
/// Derivation. The digest exists to be collision-free and path-safe, not to be
/// short. 32 hex characters is 128 bits, so the birthday bound for a
/// collision across N aliases is N^2 / 2^129; even an absurd 2^20 aliases on
/// one machine leaves ~2^-89. It also matches the 32-hex account-alias
/// suffix this codebase already uses for exactly this job
/// (`anthropic-0123456789abcdef01234567` style aliases), so the two
/// name-derivation sites stay consistent.
const PROFILE_DIGEST_HEX_CHARS: usize = 32;

/// Domain separator for the profile-name digest, so a digest computed here can
/// never collide with a digest this codebase computes for another purpose.
const PROFILE_DIGEST_DOMAIN: &[u8] = b"haider.antigravity.profile.v1\0";

// ---------------------------------------------------------------------------
// Runtime root
// ---------------------------------------------------------------------------

/// The daemon-owned root under which Antigravity's install tree and the
/// per-account profiles live.
#[derive(Debug, Clone)]
pub(crate) struct AntigravityRuntimeRoot {
    root: PathBuf,
}

impl AntigravityRuntimeRoot {
    /// Binds a root explicitly. Tests use this with a temporary directory;
    /// production reaches it through [`Self::resolve`].
    pub(crate) fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The root Haider owns inside a home directory. Kept separate from
    /// [`Self::resolve`] so the layout is pinnable without reading the
    /// environment.
    pub(crate) fn from_home(home: impl AsRef<Path>) -> Self {
        Self::new(home.as_ref().join(".haider").join("antigravity"))
    }

    /// Resolves the production root: the operator override when set, else
    /// `$HOME/.haider/antigravity`.
    pub(crate) fn resolve() -> Result<Self, HaiderError> {
        if let Some(path) = std::env::var_os(ANTIGRAVITY_HOME_ENV).filter(|value| !value.is_empty())
        {
            return Ok(Self::new(path));
        }
        let Some(home) = crate::oauth::oauth_home_dir() else {
            return Err(HaiderError::new(
                ErrorCode::InvalidArgument,
                format!(
                    "cannot locate the Antigravity runtime root: the home directory and {ANTIGRAVITY_HOME_ENV} are unset"
                ),
                false,
            ));
        };
        Ok(Self::from_home(PathBuf::from(home)))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.root
    }

    /// Install root handed to [`AntigravityInstaller`].
    pub(crate) fn install_root(&self) -> PathBuf {
        self.root.join(INSTALL_DIRECTORY)
    }

    /// The private `GEMINI_HOME` for one alias. Pure: computing a path never
    /// touches the filesystem, so a caller can compare two aliases' profiles
    /// without creating either.
    pub(crate) fn profile_dir(&self, alias: &CredentialAlias) -> PathBuf {
        self.root
            .join(PROFILES_DIRECTORY)
            .join(profile_directory_name(alias))
    }

    /// Creates the alias's profile directory `0700` and returns it.
    ///
    /// The mode is forced after creation because `mkdir` masks its argument
    /// with the process umask, so an explicit `set_mode` is what makes `0700`
    /// a guarantee rather than a request.
    pub(crate) fn ensure_profile_dir(
        &self,
        alias: &CredentialAlias,
    ) -> Result<PathBuf, HaiderError> {
        let profile = self.profile_dir(alias);
        for directory in [
            self.root.as_path(),
            &self.root.join(PROFILES_DIRECTORY),
            profile.as_path(),
        ] {
            create_private_directory(directory)?;
        }
        Ok(profile)
    }
}

fn create_private_directory(path: &Path) -> Result<(), HaiderError> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    haider_platform::configure_directory_mode(&mut builder, PROFILE_DIRECTORY_MODE);
    if let Err(error) = builder.create(path)
        && error.kind() != std::io::ErrorKind::AlreadyExists
    {
        return Err(filesystem_error("create", path, &error));
    }
    haider_platform::set_mode(path, PROFILE_DIRECTORY_MODE)
        .map_err(|error| filesystem_error("secure", path, &error))
}

/// Path errors name the OPERATION and the kind, never the caller's alias.
fn filesystem_error(operation: &str, path: &Path, error: &std::io::Error) -> HaiderError {
    HaiderError::new(
        ErrorCode::ProviderError,
        format!(
            "cannot {operation} the Antigravity profile directory `{}`: {}",
            path.display(),
            error.kind()
        ),
        false,
    )
}

/// The directory name for one alias: a domain-separated digest, never the
/// alias text.
///
/// An alias is operator-supplied, so using it verbatim would let a name like
/// `../..` or a very long string decide a path. A digest is fixed-length,
/// path-safe by construction, and still deterministic — the same alias always
/// resolves to the same profile across restarts, which is what makes a session
/// resumable.
pub(crate) fn profile_directory_name(alias: &CredentialAlias) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(PROFILE_DIGEST_DOMAIN);
    hasher.update(alias.as_str().as_bytes());
    hasher
        .finalize()
        .to_hex()
        .chars()
        .take(PROFILE_DIGEST_HEX_CHARS)
        .collect()
}

// ---------------------------------------------------------------------------
// Install resolution
// ---------------------------------------------------------------------------

/// A verified install plus the live lease that keeps it from being replaced
/// while a child is running on it.
#[derive(Debug)]
pub(crate) struct LeasedInstallation {
    installation: AntigravityInstallation,
    // Held for the lifetime of the value. The lease's `Drop` releases the
    // advisory lock, which is exactly what makes a crashed holder reclaimable
    // by the kernel rather than by a timeout.
    _lease: AntigravityLease,
}

impl LeasedInstallation {
    pub(crate) fn installation(&self) -> &AntigravityInstallation {
        &self.installation
    }
}

/// Resolves the ALREADY-INSTALLED pinned agent and takes a lease on it.
///
/// This never downloads. A turn that discovered a missing install would
/// otherwise stall for a multi-hundred-megabyte transfer it never asked for,
/// so the absence is a typed refusal naming the remedy instead.
pub(crate) fn leased_installation(
    root: &AntigravityRuntimeRoot,
) -> Result<LeasedInstallation, HaiderError> {
    let pin = pin_for_host().map_err(install_error)?;
    let installer = AntigravityInstaller::new(root.install_root());
    let Some(installation) = installer.resolve(pin).map_err(install_error)? else {
        return Err(HaiderError::new(
            ErrorCode::ProviderError,
            format!(
                "the Google Antigravity agent {ANTIGRAVITY_VERSION} is not installed for this host; \
                 install it from the account card before starting a turn — Haider never downloads it mid-turn"
            ),
            false,
        ));
    };
    let lease = installer
        .acquire_lease(installation.version())
        .map_err(install_error)?;
    Ok(LeasedInstallation {
        installation,
        _lease: lease,
    })
}

/// Installer failures reach the account layer as typed, bounded provider
/// errors. The installer's own `Display` is already archive-name sanitized and
/// carries no URL, so it is forwarded verbatim.
fn install_error(error: AntigravityInstallError) -> HaiderError {
    let code = match error {
        // A host Google publishes no build for is a permanent, actionable
        // configuration fact, not a provider outage.
        AntigravityInstallError::UnsupportedPlatform { .. } => ErrorCode::InvalidArgument,
        _ => ErrorCode::ProviderError,
    };
    HaiderError::new(code, format!("Antigravity install: {error}"), false)
}

/// Builds the launch spec for one account.
///
/// `home_dir` is the account's OWN profile directory, not the operator's home:
/// the child sees a `HOME` it cannot read anything else from, so a misbehaving
/// agent cannot reach `~/.config`, another account's profile, or Haider's own
/// state.
pub(crate) fn launch_spec(
    installation: &AntigravityInstallation,
    profile_dir: &Path,
    working_dir: &Path,
) -> AcpLaunchSpec {
    AcpLaunchSpec {
        program: installation.executable().to_path_buf(),
        args: installation.args().iter().map(OsString::from).collect(),
        profile_dir: profile_dir.to_path_buf(),
        home_dir: profile_dir.to_path_buf(),
        working_dir: working_dir.to_path_buf(),
    }
}

// ---------------------------------------------------------------------------
// Model policy
// ---------------------------------------------------------------------------

/// What a session's model resolution decided, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelChoice {
    /// The caller pinned a model and the agent still offers it.
    Pinned(String),
    /// A new session took the preferred slug because the agent offered that
    /// exact id.
    Preferred(String),
    /// A new session took the agent's own designated default.
    AgentDefault(String),
}

impl ModelChoice {
    /// The slug the session actually runs on. Recorded, never inferred.
    pub(crate) fn slug(&self) -> &str {
        match self {
            Self::Pinned(slug) | Self::Preferred(slug) | Self::AgentDefault(slug) => slug,
        }
    }
}

/// Chooses the model for one Antigravity session.
///
/// `offered` is the catalog the AUTHENTICATED agent published for this
/// account; `agent_default` is the id that same catalog designated as its
/// default. Nothing here invents a slug: an empty catalog is a refusal, not a
/// guess.
///
/// - A pinned `requested` model that is no longer offered is REFUSED with a
///   model-selection remedy. Silently substituting would answer a resumed
///   session on a different model than its transcript was built with.
/// - An unpinned session prefers [`PREFERRED_NEW_SESSION_MODEL`] only when
///   that EXACT id is offered, and otherwise takes the agent's designated
///   default.
pub(crate) fn resolve_session_model(
    offered: &[String],
    agent_default: Option<&str>,
    requested: &str,
) -> Result<ModelChoice, HaiderError> {
    if offered.is_empty() {
        return Err(HaiderError::new(
            ErrorCode::ProviderError,
            "the Google Antigravity agent published no model catalog for this account; \
             sign in again so the agent can publish its models",
            false,
        ));
    }
    let requested = requested.trim();
    if !requested.is_empty() {
        if offers(offered, requested) {
            return Ok(ModelChoice::Pinned(requested.to_owned()));
        }
        return Err(HaiderError::new(
            ErrorCode::InvalidArgument,
            format!(
                "the Google Antigravity agent no longer offers model `{requested}` for this \
                 account; pick a model this account still offers — Haider does not substitute one"
            ),
            false,
        ));
    }
    if offers(offered, PREFERRED_NEW_SESSION_MODEL) {
        return Ok(ModelChoice::Preferred(
            PREFERRED_NEW_SESSION_MODEL.to_owned(),
        ));
    }
    if let Some(default) = agent_default.map(str::trim).filter(|slug| !slug.is_empty())
        && offers(offered, default)
    {
        return Ok(ModelChoice::AgentDefault(default.to_owned()));
    }
    Err(HaiderError::new(
        ErrorCode::ProviderError,
        "the Google Antigravity agent designated no default model for this account; \
         pick one of the models it offers",
        false,
    ))
}

/// EXACT membership. A slug is opaque: `gemini-pro-agent` proves the catalog
/// carries ids that no `<family>-<tier>` parse would survive, and the
/// `-{high,medium,low}` reasoning variants are DISTINCT entries that a prefix
/// match would collapse into one.
fn offers(offered: &[String], slug: &str) -> bool {
    offered.iter().any(|model| model == slug)
}

// ---------------------------------------------------------------------------
// The account-backed adapter
// ---------------------------------------------------------------------------

/// A [`Provider`] bound to one account's profile and the installed executable.
///
/// The agent is launched LAZILY, on the first turn. Cold start to the
/// `initialize` response was measured at 14.75 s with a ~225 MiB child, so
/// paying that to answer `capabilities()` — which the daemon asks on every
/// resolution, including pair-switch probing that may never run a turn — would
/// spend fifteen seconds and a quarter gigabyte on a metadata question.
pub(crate) struct AntigravityAccountProvider {
    spec: AcpLaunchSpec,
    config: AntigravitySessionConfig,
    installation: LeasedInstallation,
    session: tokio::sync::OnceCell<Arc<AntigravityAcpProvider>>,
}

impl std::fmt::Debug for AntigravityAccountProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AntigravityAccountProvider")
            .field("version", &self.version())
            .field("model", &self.model())
            .field("profile_dir", &self.profile_dir())
            .field("launched", &self.session.initialized())
            .finish_non_exhaustive()
    }
}

impl AntigravityAccountProvider {
    pub(crate) fn new(
        spec: AcpLaunchSpec,
        config: AntigravitySessionConfig,
        installation: LeasedInstallation,
    ) -> Self {
        Self {
            spec,
            config,
            installation,
            session: tokio::sync::OnceCell::new(),
        }
    }

    /// The model this account's session runs on.
    pub(crate) fn model(&self) -> &str {
        &self.config.model
    }

    /// The private `GEMINI_HOME` this account's child is confined to.
    pub(crate) fn profile_dir(&self) -> &Path {
        &self.spec.profile_dir
    }

    /// The installed version the lease is held on.
    pub(crate) fn version(&self) -> &str {
        self.installation.installation().version()
    }

    /// Launches the supervised agent once and reuses the session afterwards.
    ///
    /// `get_or_try_init` deliberately does NOT cache a failure: a launch that
    /// failed because the operator had not finished the browser login must be
    /// retryable on the next turn without rebuilding the account.
    async fn session(&self) -> Result<&Arc<AntigravityAcpProvider>, ProviderError> {
        self.session
            .get_or_try_init(|| async {
                AntigravityAcpProvider::launch(
                    &self.spec,
                    &self.config,
                    AntigravityAcpProvider::refusing_handler(),
                )
                .await
                .map(Arc::new)
            })
            .await
    }
}

#[async_trait::async_trait]
impl Provider for AntigravityAccountProvider {
    /// The supervised agent reaches Google over the network, so a confirmed
    /// missing OS default route is authoritative for it. Forwarded from the
    /// inner adapter's own declaration rather than restated.
    fn trusts_default_route_absence(&self) -> bool {
        true
    }

    /// Haider holds NO Google credential for this provider: the child owns its
    /// OAuth material under `$GEMINI_HOME` and nothing token-shaped crosses
    /// the ACP wire.
    fn credential_surface(&self) -> haider_provider::ProviderCredentialSurface {
        haider_provider::ProviderCredentialSurface::Opaque
    }

    fn usage_lane_dimensions(&self) -> haider_protocol::provider::UsageLaneDimensions {
        haider_protocol::provider::UsageLaneDimensions {
            api_family: Some("acp_antigravity".into()),
            effort: None,
            speed: None,
        }
    }

    async fn capabilities(&self) -> CapabilityDoc {
        // Answered from the adapter's own static declaration: it depends on
        // nothing a live session carries, so no child is spawned.
        AntigravityAcpProvider::declared_capabilities()
    }

    async fn stream_turn(&self, request: TurnRequest) -> Result<ProviderStream, ProviderError> {
        self.session().await?.stream_turn(request).await
    }
}

// ---------------------------------------------------------------------------
// Adapter factory
// ---------------------------------------------------------------------------

/// Maximum supervised-agent adapters one factory retains.
///
/// Derivation. This is a MEMORY bound, not a latency one. Each retained entry
/// owns an install lease and, once a turn runs, one supervised child measured
/// first-hand at 230,176 KiB RSS (~225 MiB) on darwin-arm64, so
/// 4 * 225 MiB = 900 MiB is the worst case this cache can pin — already the
/// largest per-provider footprint in the daemon. Four is chosen because the
/// entries are keyed by (alias, model, workspace) and a person signs in with
/// Google accounts in ones or twos, not dozens; a fifth combination evicts the
/// least recently used rather than adding another quarter gigabyte.
const AGENT_OWNED_ADAPTER_CACHE_CAPACITY: usize = 4;

/// What makes two supervised sessions interchangeable.
///
/// The workspace is part of the identity because `session/new` is opened
/// against one `cwd` and the agent's own filesystem tools act relative to it;
/// reusing another workspace's session would run a turn against the wrong
/// tree. The alias is part of it because two Google accounts must never share
/// a session.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentOwnedAdapterKey {
    alias: CredentialAlias,
    model: String,
    workspace: String,
}

/// Builds — and reuses — the supervised adapter for one account.
///
/// Reuse is not an optimization here: a cache miss spawns a fresh agent, and
/// cold start to the `initialize` response was measured at 14.75 s. Rebuilding
/// per turn would put that on every prompt.
pub(crate) struct AntigravityAdapterFactory {
    /// `None` resolves the production root on first use. An explicit root is
    /// how an operator relocates the install and how tests stay hermetic.
    root: Option<AntigravityRuntimeRoot>,
    entries: std::sync::Mutex<Vec<(AgentOwnedAdapterKey, Arc<AntigravityAccountProvider>)>>,
}

impl Default for AntigravityAdapterFactory {
    fn default() -> Self {
        Self {
            root: None,
            entries: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl std::fmt::Debug for AntigravityAdapterFactory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AntigravityAdapterFactory")
            .field(
                "root",
                &self.root.as_ref().map(AntigravityRuntimeRoot::path),
            )
            .finish_non_exhaustive()
    }
}

impl AntigravityAdapterFactory {
    /// Binds the factory to an explicit runtime root. Production resolves the
    /// root lazily from `HAIDER_ANTIGRAVITY_HOME` or `$HOME`; this is the
    /// hermetic seam tests bind to a temporary directory.
    #[cfg(test)]
    pub(crate) fn with_root(root: AntigravityRuntimeRoot) -> Self {
        Self {
            root: Some(root),
            entries: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn root(&self) -> Result<AntigravityRuntimeRoot, HaiderError> {
        match &self.root {
            Some(root) => Ok(root.clone()),
            None => AntigravityRuntimeRoot::resolve(),
        }
    }

    /// Builds the credential-free adapter for one account and workspace.
    ///
    /// `offered` and `agent_default` are the AUTHENTICATED agent's own
    /// published catalog, projected onto the provider summary by discovery.
    /// Nothing here consults a vault, and no [`haider_accounts::SecretHandle`]
    /// is created: the agent owns the credential.
    pub(crate) fn build(
        &self,
        alias: &CredentialAlias,
        offered: &[String],
        agent_default: Option<&str>,
        requested_model: &str,
        workspace: &str,
    ) -> Result<Arc<AntigravityAccountProvider>, HaiderError> {
        let choice = resolve_session_model(offered, agent_default, requested_model)?;
        let key = AgentOwnedAdapterKey {
            alias: alias.clone(),
            model: choice.slug().to_owned(),
            workspace: workspace.to_owned(),
        };
        if let Some(adapter) = self.cached(&key) {
            return Ok(adapter);
        }
        let root = self.root()?;
        // The profile is created BEFORE the install is resolved so a missing
        // install cannot leave an account half-provisioned: both steps are
        // idempotent, and the directory is the thing a later install needs.
        let profile_dir = root.ensure_profile_dir(alias)?;
        let installation = leased_installation(&root)?;
        let spec = launch_spec(
            installation.installation(),
            &profile_dir,
            std::path::Path::new(workspace),
        );
        let config = AntigravitySessionConfig {
            cwd: workspace.to_owned(),
            // Haider brokers no extra roots into the supervised agent in this
            // slice: every additional directory would widen what the agent's
            // own filesystem tools may touch.
            additional_directories: Vec::new(),
            model: key.model.clone(),
        };
        let adapter = Arc::new(AntigravityAccountProvider::new(spec, config, installation));
        self.retain(key, Arc::clone(&adapter));
        Ok(adapter)
    }

    fn cached(&self, key: &AgentOwnedAdapterKey) -> Option<Arc<AntigravityAccountProvider>> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let index = entries.iter().position(|(cached, _)| cached == key)?;
        let entry = entries.remove(index);
        let adapter = Arc::clone(&entry.1);
        entries.push(entry);
        Some(adapter)
    }

    fn retain(&self, key: AgentOwnedAdapterKey, adapter: Arc<AntigravityAccountProvider>) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|(cached, _)| cached != &key);
        if entries.len() >= AGENT_OWNED_ADAPTER_CACHE_CAPACITY {
            entries.remove(0);
        }
        entries.push((key, adapter));
    }

    /// Drops every retained adapter whose alias is no longer signed in.
    ///
    /// Removing or logging out one Google account must release ONLY that
    /// account's supervised child and its install lease; every other alias
    /// keeps its live session, which is what makes two Google accounts
    /// genuinely independent. Dropping the adapter drops the child (the ACP
    /// connection arms `kill_on_drop`) and the lease, so a removed account
    /// stops holding a version pinned.
    pub(crate) fn retain_aliases(&self, live: &std::collections::HashSet<CredentialAlias>) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|(cached, _)| live.contains(&cached.alias));
    }

    #[cfg(test)]
    pub(crate) fn retained_len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}
