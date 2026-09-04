#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Supervised-agent account plumbing. Nothing here touches the network, a
//! real Google binary, or a credential: the install half is a fixture tree
//! built in a temp directory, and every adapter is exercised WITHOUT launching
//! the agent (the fixture executable is a text file — a test that spawned it
//! would fail, which is exactly what makes the "no launch" claims real).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use haider_protocol::error::ErrorCode;
use haider_protocol::ids::CredentialAlias;
use haider_provider::Provider as _;

use crate::antigravity_install::{ACTIVE_POINTER, ANTIGRAVITY_VERSION, VERSIONS_DIRECTORY};
use crate::antigravity_session::{
    ANTIGRAVITY_HOME_ENV, AntigravityAdapterFactory, AntigravityRuntimeRoot, INSTALL_DIRECTORY,
    ModelChoice, PREFERRED_NEW_SESSION_MODEL, PROFILES_DIRECTORY, launch_spec, leased_installation,
    profile_directory_name, resolve_session_model,
};

/// The catalog observed on the live 1.1.1 agent, verbatim from
/// `docs/testing/v0.0.970/_acp-wire-facts.md`. `gemini-pro-agent` is kept
/// because it is the entry that proves a slug cannot be parsed structurally.
const OBSERVED_CATALOG: &[&str] = &[
    "gemini-3.8-flash-high",
    "gemini-3.8-flash-medium",
    "gemini-3.8-flash-low",
    "gemini-3.7-flash-high",
    "gemini-3.7-flash-medium",
    "gemini-3.7-flash-low",
    "gemini-3.6-flash-high",
    "gemini-3.6-flash-medium",
    "gemini-3.6-flash-low",
    "gemini-pro-agent",
    "gemini-3.1-pro-low",
];

/// The default the live agent declared for itself.
const OBSERVED_AGENT_DEFAULT: &str = "gemini-3.7-flash-high";

fn catalog(slugs: &[&str]) -> Vec<String> {
    slugs.iter().map(|slug| (*slug).to_owned()).collect()
}

fn owner_only_mode(path: &Path) -> u32 {
    let metadata = std::fs::metadata(path).expect("stat path");
    haider_platform::metadata_mode(&metadata) & 0o777
}

/// Builds the minimal tree `AntigravityInstaller::resolve` accepts for the
/// HOST pin: an `active` pointer plus the pinned executable and its sibling
/// helper, all owner-only. The file bodies are not real binaries — nothing in
/// these tests executes them.
fn install_host_fixture(root: &AntigravityRuntimeRoot) -> PathBuf {
    let pin = crate::antigravity_install::pin_for_host().expect("this host has a release pin");
    let install_root = root.install_root();
    let version_dir = install_root
        .join(VERSIONS_DIRECTORY)
        .join(ANTIGRAVITY_VERSION);
    std::fs::create_dir_all(&version_dir).expect("create the fixture version directory");
    for directory in [
        install_root.as_path(),
        install_root.join(VERSIONS_DIRECTORY).as_path(),
        version_dir.as_path(),
    ] {
        haider_platform::set_mode(directory, 0o700).expect("secure the fixture directory");
    }
    for name in [pin.executable_name(), pin.helper_name()] {
        let path = version_dir.join(name);
        std::fs::write(&path, b"antigravity fixture, never executed").expect("write fixture file");
        haider_platform::set_mode(&path, 0o700).expect("secure the fixture file");
    }
    let pointer = install_root.join(ACTIVE_POINTER);
    std::fs::write(&pointer, ANTIGRAVITY_VERSION).expect("write the active pointer");
    haider_platform::set_mode(&pointer, 0o600).expect("secure the active pointer");
    version_dir
}

// ---------------------------------------------------------------------------
// Runtime root and per-account profiles
// ---------------------------------------------------------------------------

/// The runtime root is home-relative and its two subtrees are separate: the
/// shared ~900 MiB install never lives inside an account's private profile.
///
/// MUTATION CHECK: make `install_root` return the root itself. Expected
/// runtime failure: the install root then equals the root and the
/// install-versus-profile inequality below fails.
#[test]
fn the_runtime_root_is_home_relative_with_separate_install_and_profile_subtrees() {
    let root = AntigravityRuntimeRoot::from_home(Path::new("/home/golden"));
    assert_eq!(
        root.path(),
        Path::new("/home/golden/.haider/antigravity"),
        "the root is Haider's own directory inside the home, never the home itself"
    );
    assert_eq!(
        root.install_root(),
        Path::new("/home/golden/.haider/antigravity").join(INSTALL_DIRECTORY)
    );
    let alias = CredentialAlias::new("google-work");
    assert_eq!(
        root.profile_dir(&alias),
        Path::new("/home/golden/.haider/antigravity")
            .join(PROFILES_DIRECTORY)
            .join(profile_directory_name(&alias))
    );
    assert_ne!(root.install_root(), root.profile_dir(&alias));
    assert_eq!(ANTIGRAVITY_HOME_ENV, "HAIDER_ANTIGRAVITY_HOME");
}

/// Two aliases get two private profile directories, `0700`, and neither name
/// is derived from the other.
///
/// MUTATION CHECK: derive the profile directory from the alias text instead of
/// the digest. Expected runtime failure: the traversal-shaped alias in the
/// next test escapes the profiles root, and two aliases that share a prefix
/// stop being independent here.
#[test]
fn two_aliases_get_two_private_profile_directories() {
    let temp = tempfile::tempdir().expect("temp root");
    let root = AntigravityRuntimeRoot::new(temp.path());
    let work = CredentialAlias::new("google-work");
    let personal = CredentialAlias::new("google-personal");

    let work_dir = root.ensure_profile_dir(&work).expect("work profile");
    let personal_dir = root
        .ensure_profile_dir(&personal)
        .expect("personal profile");

    assert_ne!(
        work_dir, personal_dir,
        "two Google accounts must never share a GEMINI_HOME"
    );
    assert!(work_dir.is_dir() && personal_dir.is_dir());
    for directory in [temp.path(), work_dir.as_path(), personal_dir.as_path()] {
        assert_eq!(
            owner_only_mode(directory),
            0o700,
            "{} must be owner-only",
            directory.display()
        );
    }
    // Idempotent: resolving the same alias twice reaches the SAME profile, so
    // a session survives a daemon restart.
    assert_eq!(
        root.ensure_profile_dir(&work).expect("work again"),
        work_dir
    );
}

/// The profile directory name is a fixed-length digest, so no alias text — not
/// a traversal, not a separator, not an arbitrary length — can decide a path.
///
/// MUTATION CHECK: use the alias verbatim as the directory name. Expected
/// runtime failure: the name is not 32 hex characters and the containment
/// assertion rejects the escaped path.
#[test]
fn a_profile_directory_name_is_a_digest_never_the_alias_text() {
    let hostile = CredentialAlias::new("../../etc/passwd");
    let name = profile_directory_name(&hostile);
    assert_eq!(name.len(), 32, "128 bits of digest, fixed length");
    assert!(
        name.chars().all(|character| character.is_ascii_hexdigit()),
        "a profile name is hex only: {name}"
    );
    assert!(!name.contains(".."));
    assert!(!name.contains('/'));
    assert!(!name.contains(std::path::MAIN_SEPARATOR));

    // Deterministic across calls, and distinct for a one-character change.
    assert_eq!(profile_directory_name(&hostile), name);
    assert_ne!(
        profile_directory_name(&CredentialAlias::new("../../etc/passwe")),
        name
    );

    let root = AntigravityRuntimeRoot::from_home(Path::new("/home/golden"));
    let profile = root.profile_dir(&hostile);
    assert!(
        profile.starts_with(root.path()),
        "a hostile alias cannot escape the profiles root: {}",
        profile.display()
    );
    assert_eq!(
        profile.components().count(),
        root.path().components().count() + 2,
        "exactly `profiles/<digest>` is appended"
    );
}

// ---------------------------------------------------------------------------
// Install resolution
// ---------------------------------------------------------------------------

/// A missing install is a typed, actionable refusal — and the refusal is all
/// that happens: no version tree appears, so nothing was downloaded.
///
/// MUTATION CHECK: make `leased_installation` install on a miss. Expected
/// runtime failure: the `versions/` assertion below finds a tree that no test
/// created.
#[test]
fn a_missing_install_refuses_with_a_typed_remedy_and_downloads_nothing() {
    let temp = tempfile::tempdir().expect("temp root");
    let root = AntigravityRuntimeRoot::new(temp.path());

    let error = leased_installation(&root).expect_err("nothing is installed");
    assert_eq!(error.code, ErrorCode::ProviderError);
    assert!(
        error.message.contains("not installed"),
        "the refusal must name the cause: {}",
        error.message
    );
    assert!(
        error.message.contains("install it"),
        "the refusal must name the remedy: {}",
        error.message
    );
    assert!(
        !error.retryable,
        "retrying will not make an uninstalled agent appear"
    );
    assert!(
        !root.install_root().join(VERSIONS_DIRECTORY).exists(),
        "a turn must never trigger a download"
    );
}

/// An installed tree resolves to the pinned executable, holds a live lease on
/// it, and produces a launch spec confined to the account's own profile.
///
/// MUTATION CHECK: point `home_dir` at the operator's real home instead of the
/// profile. Expected runtime failure: the `home_dir == profile_dir` assertion.
#[test]
fn an_installed_tree_resolves_to_a_leased_executable_confined_to_the_profile() {
    let temp = tempfile::tempdir().expect("temp root");
    let root = AntigravityRuntimeRoot::new(temp.path());
    let version_dir = install_host_fixture(&root);
    let pin = crate::antigravity_install::pin_for_host().expect("host pin");

    let leased = leased_installation(&root).expect("the fixture install resolves");
    let installation = leased.installation();
    assert_eq!(installation.version(), ANTIGRAVITY_VERSION);
    assert_eq!(
        installation.executable(),
        version_dir.join(pin.executable_name())
    );

    let installer = crate::antigravity_install::AntigravityInstaller::new(root.install_root());
    assert!(
        installer
            .is_version_leased(ANTIGRAVITY_VERSION)
            .expect("lease probe"),
        "a resolved install is leased so an update cannot replace a running child"
    );

    let alias = CredentialAlias::new("google-work");
    let profile = root.ensure_profile_dir(&alias).expect("profile");
    let spec = launch_spec(installation, &profile, Path::new("/tmp/workspace"));
    assert_eq!(spec.program, version_dir.join(pin.executable_name()));
    assert_eq!(
        spec.args,
        pin.extra_args()
            .iter()
            .map(std::ffi::OsString::from)
            .collect::<Vec<_>>(),
        "argv comes from the release pin, never from a caller"
    );
    assert_eq!(spec.profile_dir, profile);
    assert_eq!(
        spec.home_dir, profile,
        "the child's HOME is its own profile, so it cannot read the operator's"
    );
    assert_eq!(spec.working_dir, Path::new("/tmp/workspace"));

    drop(leased);
    assert!(
        !installer
            .is_version_leased(ANTIGRAVITY_VERSION)
            .expect("lease probe after drop"),
        "dropping the installation releases the version"
    );
}

// ---------------------------------------------------------------------------
// Model policy
// ---------------------------------------------------------------------------

/// A new session takes `gemini-3.8-flash-high` only when the authenticated
/// agent offers that EXACT slug, and otherwise the agent's own default.
///
/// MUTATION CHECK: match the preferred model by prefix instead of equality.
/// Expected runtime failure: the second case picks `gemini-3.8-flash-medium`
/// and the `AgentDefault` assertion fails.
#[test]
fn a_new_session_prefers_flash_high_only_on_an_exact_offer() {
    let offered = catalog(OBSERVED_CATALOG);
    assert_eq!(
        resolve_session_model(&offered, Some(OBSERVED_AGENT_DEFAULT), "").expect("preferred"),
        ModelChoice::Preferred(PREFERRED_NEW_SESSION_MODEL.to_owned())
    );

    // The same catalog WITHOUT the exact preferred slug: the nearest
    // neighbours are still there, and none of them is taken.
    let without_preferred: Vec<String> = offered
        .iter()
        .filter(|slug| slug.as_str() != PREFERRED_NEW_SESSION_MODEL)
        .cloned()
        .collect();
    let choice = resolve_session_model(&without_preferred, Some(OBSERVED_AGENT_DEFAULT), "")
        .expect("agent default");
    assert_eq!(
        choice,
        ModelChoice::AgentDefault(OBSERVED_AGENT_DEFAULT.to_owned())
    );
    assert_eq!(
        choice.slug(),
        OBSERVED_AGENT_DEFAULT,
        "the model actually resolved is recorded, not the one that was wanted"
    );
}

/// Reasoning variants are DISTINCT catalog entries and an irregular slug is
/// carried opaquely: selection never parses a slug's shape.
///
/// MUTATION CHECK: normalize slugs by stripping a `-{high,medium,low}` suffix
/// before comparing. Expected runtime failure: `gemini-3.8-flash-low` resolves
/// as pinned against a catalog that only offers `-high`.
#[test]
fn reasoning_variants_stay_distinct_and_slugs_are_never_parsed_structurally() {
    let only_high = catalog(&["gemini-3.8-flash-high"]);
    for sibling in ["gemini-3.8-flash-medium", "gemini-3.8-flash-low"] {
        let error = resolve_session_model(&only_high, Some("gemini-3.8-flash-high"), sibling)
            .expect_err("a sibling variant is a different model");
        assert_eq!(error.code, ErrorCode::InvalidArgument);
    }
    // The irregular slug is a first-class entry, selectable by exact id.
    let irregular = catalog(&["gemini-pro-agent"]);
    assert_eq!(
        resolve_session_model(&irregular, Some("gemini-pro-agent"), "gemini-pro-agent")
            .expect("irregular slug"),
        ModelChoice::Pinned("gemini-pro-agent".to_owned())
    );
    // ... and prefix-shaped near misses are not it.
    assert!(resolve_session_model(&irregular, Some("gemini-pro-agent"), "gemini-pro").is_err());
    // Every observed reasoning variant survives as its own selectable entry.
    let offered = catalog(OBSERVED_CATALOG);
    for slug in OBSERVED_CATALOG {
        assert_eq!(
            resolve_session_model(&offered, Some(OBSERVED_AGENT_DEFAULT), slug)
                .expect("each catalog entry is selectable")
                .slug(),
            *slug
        );
    }
}

/// A resumed session whose stored model the agent no longer offers is refused
/// with a model-selection remedy — never silently moved to another model.
///
/// MUTATION CHECK: fall back to the agent default when the pinned model is
/// gone. Expected runtime failure: the call returns `Ok` and the refusal
/// assertions below fail.
#[test]
fn a_withdrawn_model_is_refused_with_a_selection_remedy_not_substituted() {
    let offered = catalog(&["gemini-3.8-flash-high", "gemini-3.7-flash-high"]);
    let error = resolve_session_model(
        &offered,
        Some("gemini-3.7-flash-high"),
        "gemini-3.6-flash-high",
    )
    .expect_err("the stored model is gone");
    assert_eq!(error.code, ErrorCode::InvalidArgument);
    assert!(
        error.message.contains("gemini-3.6-flash-high"),
        "the refusal names the model that vanished: {}",
        error.message
    );
    assert!(
        error.message.contains("pick a model"),
        "the refusal names the remedy: {}",
        error.message
    );
    assert!(
        error.message.contains("does not substitute"),
        "the refusal states the law: {}",
        error.message
    );
    // A model that IS still offered resumes unchanged.
    assert_eq!(
        resolve_session_model(
            &offered,
            Some("gemini-3.7-flash-high"),
            "gemini-3.8-flash-high"
        )
        .expect("still offered"),
        ModelChoice::Pinned("gemini-3.8-flash-high".to_owned())
    );
}

/// An empty catalog, or a default the catalog does not contain, is a refusal.
/// The agent's own list is the only inventory truth, so there is nothing to
/// fall back to.
///
/// MUTATION CHECK: return the preferred slug when the catalog is empty.
/// Expected runtime failure: the first call returns `Ok`.
#[test]
fn an_absent_catalog_is_refused_rather_than_fabricated() {
    let empty: Vec<String> = Vec::new();
    let error = resolve_session_model(&empty, Some(OBSERVED_AGENT_DEFAULT), "")
        .expect_err("no catalog, no session");
    assert_eq!(error.code, ErrorCode::ProviderError);
    assert!(error.message.contains("no model catalog"));

    // A default the catalog does not list is not usable either.
    let offered = catalog(&["gemini-3.7-flash-low"]);
    let error = resolve_session_model(&offered, Some("gemini-3.7-flash-high"), "")
        .expect_err("the declared default is not on offer");
    assert_eq!(error.code, ErrorCode::ProviderError);
    assert!(error.message.contains("no default model"));

    // No default at all is the same refusal.
    assert!(resolve_session_model(&offered, None, "").is_err());
}

// ---------------------------------------------------------------------------
// Adapter factory
// ---------------------------------------------------------------------------

/// Two aliases build two adapters bound to two profiles, and neither is a
/// cache hit for the other.
///
/// MUTATION CHECK: drop the alias from the adapter cache key. Expected
/// runtime failure: the second `build` returns the first alias's adapter and
/// the distinct-profile assertion fails.
#[test]
fn two_aliases_build_two_independent_supervised_adapters() {
    let temp = tempfile::tempdir().expect("temp root");
    let root = AntigravityRuntimeRoot::new(temp.path());
    install_host_fixture(&root);
    let factory = AntigravityAdapterFactory::with_root(root.clone());
    let offered = catalog(OBSERVED_CATALOG);

    let work = CredentialAlias::new("google-work");
    let personal = CredentialAlias::new("google-personal");
    let build = |alias: &CredentialAlias| {
        factory
            .build(
                alias,
                &offered,
                Some(OBSERVED_AGENT_DEFAULT),
                "",
                "/tmp/workspace",
            )
            .expect("build the supervised adapter")
    };
    let work_adapter = build(&work);
    let personal_adapter = build(&personal);

    assert_eq!(factory.retained_len(), 2);
    assert!(
        !std::sync::Arc::ptr_eq(&work_adapter, &personal_adapter),
        "two aliases must not share one supervised session"
    );
    assert!(
        format!("{work_adapter:?}").contains(&root.profile_dir(&work).display().to_string()),
        "each adapter is bound to its own alias profile"
    );
    assert!(
        format!("{personal_adapter:?}")
            .contains(&root.profile_dir(&personal).display().to_string())
    );

    // Same alias, same model, same workspace: the SAME adapter, so a second
    // turn never pays another ~15 s cold start.
    assert!(std::sync::Arc::ptr_eq(&work_adapter, &build(&work)));
    assert_eq!(factory.retained_len(), 2);
}

/// Removing one Google alias releases only that alias's supervised adapter;
/// every other alias keeps its live session.
///
/// MUTATION CHECK: make `retain_aliases` clear the whole cache. Expected
/// runtime failure: the surviving alias's adapter is rebuilt and the
/// `Arc::ptr_eq` assertion fails.
#[test]
fn removing_one_alias_cannot_disturb_another() {
    let temp = tempfile::tempdir().expect("temp root");
    let root = AntigravityRuntimeRoot::new(temp.path());
    install_host_fixture(&root);
    let factory = AntigravityAdapterFactory::with_root(root);
    let offered = catalog(OBSERVED_CATALOG);
    let removed = CredentialAlias::new("google-removed");
    let kept = CredentialAlias::new("google-kept");
    let build = |alias: &CredentialAlias| {
        factory
            .build(
                alias,
                &offered,
                Some(OBSERVED_AGENT_DEFAULT),
                "",
                "/tmp/workspace",
            )
            .expect("build the supervised adapter")
    };
    let _removed_adapter = build(&removed);
    let kept_adapter = build(&kept);
    assert_eq!(factory.retained_len(), 2);

    let live: HashSet<CredentialAlias> = HashSet::from([kept.clone()]);
    factory.retain_aliases(&live);

    assert_eq!(factory.retained_len(), 1);
    assert!(
        std::sync::Arc::ptr_eq(&kept_adapter, &build(&kept)),
        "the surviving alias keeps its exact session"
    );
}

/// Capabilities are answered from the adapter's static declaration, so asking
/// what a Google account can do never spawns the ~225 MiB agent.
///
/// MUTATION CHECK: make `capabilities` launch the session first. Expected
/// runtime failure: the fixture executable is a text file, so the launch
/// fails and the adapter reports `launched: true` before erroring.
#[tokio::test]
async fn capabilities_are_answered_without_launching_the_agent() {
    let temp = tempfile::tempdir().expect("temp root");
    let root = AntigravityRuntimeRoot::new(temp.path());
    install_host_fixture(&root);
    let factory = AntigravityAdapterFactory::with_root(root);
    let adapter = factory
        .build(
            &CredentialAlias::new("google-work"),
            &catalog(OBSERVED_CATALOG),
            Some(OBSERVED_AGENT_DEFAULT),
            "",
            "/tmp/workspace",
        )
        .expect("build the supervised adapter");

    let capabilities = adapter.capabilities().await;
    assert_eq!(capabilities.provider, "google-antigravity");
    assert_eq!(
        capabilities.thinking_visible,
        haider_protocol::provider::FeatureResolve::Native
    );
    assert_eq!(
        capabilities.parallel_tools,
        haider_protocol::provider::FeatureResolve::Unsupported,
        "the agent runs its own tools; Haider never dispatches one for it"
    );
    assert_eq!(
        adapter.credential_surface(),
        haider_provider::ProviderCredentialSurface::Opaque,
        "Haider holds no Google credential for this account"
    );
    assert!(
        format!("{adapter:?}").contains("launched: false"),
        "no child was spawned: {adapter:?}"
    );
}
