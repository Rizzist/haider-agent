#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Resolver, precedence, non-hijack, and anti-littering tests
//! (`docs/design/dated-workspace-v1.md` §2–3).

use std::path::{Path, PathBuf};

use haider_platform::CivilDateSample;

use super::{
    CwdReason, MaterializeError, WORKSPACE_BASE_ENV, WorkspaceConfig, WorkspaceEnvironment,
    WorkspaceError, WorkspaceInvocation, WorkspaceMode, WorkspaceRequest, WorkspaceSelection,
    materialize_daily_root, materialize_leaf, path_is_inside_dated_allocation, resolve_workspace,
};

const ENTROPY: [u8; 16] = [
    0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd,
    0xef,
];

fn reference_sample() -> CivilDateSample {
    CivilDateSample {
        year: 2026,
        month: 9,
        day: 13,
        utc_ms: 1_789_257_600_000,
        offset_seconds: 0,
    }
}

fn environment_with_base(base: &Path) -> WorkspaceEnvironment {
    WorkspaceEnvironment {
        base: Some(base.display().to_string()),
        ..WorkspaceEnvironment::default()
    }
}

fn request<'a>(
    invocation: WorkspaceInvocation,
    launch_cwd: Option<&'a Path>,
    environment: &'a WorkspaceEnvironment,
    config: &'a WorkspaceConfig,
    store_dir: &'a Path,
) -> WorkspaceRequest<'a> {
    WorkspaceRequest {
        invocation,
        explicit_workspace: None,
        explicit_mode: None,
        launch_cwd,
        environment,
        config,
        store_dir,
        sample: reference_sample(),
        allocation_entropy: ENTROPY,
    }
}

/// Recursive snapshot of every path under a directory.
fn snapshot(dir: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    fn walk(dir: &Path, paths: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            paths.push(path.clone());
            if path.is_dir() {
                walk(&path, paths);
            }
        }
    }
    walk(dir, &mut paths);
    paths.sort();
    paths
}

#[test]
fn dated_plan_is_resolvable_without_creation() {
    let temp = tempfile::tempdir().unwrap();
    let launch = temp.path().join("empty-launch");
    std::fs::create_dir(&launch).unwrap();
    let base = temp.path().join("base");
    std::fs::create_dir(&base).unwrap();
    let environment = environment_with_base(&base);
    let config = WorkspaceConfig::default();
    let before = snapshot(temp.path());

    let selection = resolve_workspace(&request(
        WorkspaceInvocation::Interactive,
        Some(&launch),
        &environment,
        &config,
        temp.path(),
    ))
    .unwrap();

    let WorkspaceSelection::Dated(plan) = selection else {
        panic!("expected a dated plan, got {selection:?}");
    };
    assert_eq!(plan.hijri_label, "1448-03-30");
    assert_eq!(plan.gregorian_label, "2026-09-13");
    assert_eq!(plan.calendar_id, "islamic-civil");
    assert_eq!(plan.daily_root, base.join("Haider").join("1448-03-30"));
    assert_eq!(
        plan.leaf,
        plan.daily_root.join("s-0123456789abcdef0123456789abcdef")
    );
    // L1/L6: resolution created NOTHING.
    assert_eq!(snapshot(temp.path()), before);
    assert!(!plan.daily_root.exists());
    assert!(!plan.leaf.exists());
}

/// L3: a session that writes nothing (never materialises) is disk-invisible.
#[test]
fn no_write_session_leaves_zero_filesystem_residue() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("base");
    std::fs::create_dir(&base).unwrap();
    let environment = environment_with_base(&base);
    let config = WorkspaceConfig::default();
    let before = snapshot(temp.path());

    let selection = resolve_workspace(&request(
        WorkspaceInvocation::Interactive,
        Some(temp.path()),
        &environment,
        &config,
        temp.path(),
    ))
    .unwrap();
    assert!(matches!(selection, WorkspaceSelection::Dated(_)));

    // Simulated chat-only session lifetime: resolve, display, record, exit —
    // no write ever happens, so materialize is never called.
    assert_eq!(snapshot(temp.path()), before, "zero residue expected");
}

#[test]
fn materialize_daily_root_creates_only_the_carveout() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("base");
    std::fs::create_dir(&base).unwrap();
    let environment = environment_with_base(&base);
    let config = WorkspaceConfig::default();
    let selection = resolve_workspace(&request(
        WorkspaceInvocation::Interactive,
        Some(temp.path()),
        &environment,
        &config,
        temp.path(),
    ))
    .unwrap();
    let WorkspaceSelection::Dated(plan) = selection else {
        panic!("expected dated");
    };

    materialize_daily_root(&plan).unwrap();
    assert!(plan.daily_root.is_dir());
    assert!(!plan.leaf.exists(), "leaf must stay lazy (L2)");
    assert_eq!(
        std::fs::read_dir(&plan.daily_root).unwrap().count(),
        0,
        "daily root must be empty"
    );
}

#[test]
fn materialize_leaf_is_first_write_only_and_private() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("base");
    std::fs::create_dir(&base).unwrap();
    let environment = environment_with_base(&base);
    let config = WorkspaceConfig::default();
    let WorkspaceSelection::Dated(plan) = resolve_workspace(&request(
        WorkspaceInvocation::Interactive,
        Some(temp.path()),
        &environment,
        &config,
        temp.path(),
    ))
    .unwrap() else {
        panic!("expected dated");
    };

    let materialized = materialize_leaf(&plan).unwrap();
    assert_eq!(materialized.leaf, plan.leaf);
    assert_eq!(materialized.retries, 0);
    assert!(plan.leaf.is_dir());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&plan.leaf).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "leaf must be owner-private");
    }
}

#[test]
fn leaf_collision_retries_with_fresh_id() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("base");
    std::fs::create_dir(&base).unwrap();
    let environment = environment_with_base(&base);
    let config = WorkspaceConfig::default();
    let WorkspaceSelection::Dated(plan) = resolve_workspace(&request(
        WorkspaceInvocation::Interactive,
        Some(temp.path()),
        &environment,
        &config,
        temp.path(),
    ))
    .unwrap() else {
        panic!("expected dated");
    };

    // Occupy the planned leaf to force a genuine collision.
    std::fs::create_dir_all(&plan.leaf).unwrap();
    let materialized = materialize_leaf(&plan).unwrap();
    assert_ne!(materialized.leaf, plan.leaf);
    assert_ne!(materialized.allocation_id, plan.allocation_id);
    assert_eq!(materialized.retries, 1);
    assert!(materialized.leaf.is_dir());
}

#[test]
fn materialize_without_base_is_a_visible_error() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("missing-base");
    let environment = environment_with_base(&base);
    let config = WorkspaceConfig::default();
    let WorkspaceSelection::Dated(plan) = resolve_workspace(&request(
        WorkspaceInvocation::Interactive,
        Some(temp.path()),
        &environment,
        &config,
        temp.path(),
    ))
    .unwrap() else {
        panic!("expected dated");
    };
    assert!(matches!(
        materialize_daily_root(&plan),
        Err(MaterializeError::BaseUnavailable(_, _))
    ));
    assert!(!base.exists(), "no silent base creation");
}

#[test]
fn vcs_root_is_never_hijacked_and_subdir_scope_is_kept() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let subdir = repo.join("crates").join("a");
    std::fs::create_dir_all(&subdir).unwrap();
    std::fs::create_dir(repo.join(".git")).unwrap();
    let base = temp.path().join("base");
    std::fs::create_dir(&base).unwrap();
    let environment = environment_with_base(&base);
    let config = WorkspaceConfig::default();

    let selection = resolve_workspace(&request(
        WorkspaceInvocation::Interactive,
        Some(&subdir),
        &environment,
        &config,
        temp.path(),
    ))
    .unwrap();
    let WorkspaceSelection::LaunchCwd { path, reason } = selection else {
        panic!("project must be preserved");
    };
    // W4: keep LAUNCH CWD, not the boundary root.
    assert_eq!(path, subdir);
    assert_eq!(
        reason,
        CwdReason::ProjectDetected {
            marker: ".git".to_string(),
            boundary: repo.clone(),
        }
    );
}

#[test]
fn git_indirection_file_and_bare_repo_and_marker_are_detected() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("base");
    std::fs::create_dir(&base).unwrap();
    let config = WorkspaceConfig::default();
    let environment = environment_with_base(&base);

    // Linked worktree: `.git` is a regular file.
    let worktree = temp.path().join("worktree");
    std::fs::create_dir(&worktree).unwrap();
    std::fs::write(worktree.join(".git"), "gitdir: /elsewhere\n").unwrap();
    let selection = resolve_workspace(&request(
        WorkspaceInvocation::Interactive,
        Some(&worktree),
        &environment,
        &config,
        temp.path(),
    ))
    .unwrap();
    assert!(matches!(selection, WorkspaceSelection::LaunchCwd { .. }));

    // Bare repository: objects/ + refs/ + regular HEAD.
    let bare = temp.path().join("bare.git");
    std::fs::create_dir_all(bare.join("objects")).unwrap();
    std::fs::create_dir_all(bare.join("refs")).unwrap();
    std::fs::write(bare.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    let selection = resolve_workspace(&request(
        WorkspaceInvocation::Interactive,
        Some(&bare),
        &environment,
        &config,
        temp.path(),
    ))
    .unwrap();
    let WorkspaceSelection::LaunchCwd { reason, .. } = selection else {
        panic!("bare repo must be preserved");
    };
    assert_eq!(
        reason,
        CwdReason::ProjectDetected {
            marker: "bare-git".to_string(),
            boundary: bare.clone(),
        }
    );

    // Unversioned project escape hatch.
    let marked = temp.path().join("marked");
    std::fs::create_dir(&marked).unwrap();
    std::fs::write(marked.join(".haider-project"), "").unwrap();
    let selection = resolve_workspace(&request(
        WorkspaceInvocation::Interactive,
        Some(&marked),
        &environment,
        &config,
        temp.path(),
    ))
    .unwrap();
    assert!(matches!(selection, WorkspaceSelection::LaunchCwd { .. }));
}

/// W1: headless `haider run` keeps cwd with an empty environment.
#[test]
fn headless_default_keeps_cwd() {
    let temp = tempfile::tempdir().unwrap();
    let environment = WorkspaceEnvironment::default();
    let config = WorkspaceConfig::default();
    let before = snapshot(temp.path());
    let selection = resolve_workspace(&request(
        WorkspaceInvocation::Headless,
        Some(temp.path()),
        &environment,
        &config,
        temp.path(),
    ))
    .unwrap();
    assert_eq!(
        selection,
        WorkspaceSelection::LaunchCwd {
            path: temp.path().to_path_buf(),
            reason: CwdReason::HeadlessDefault,
        }
    );
    assert_eq!(snapshot(temp.path()), before);
}

#[test]
fn ci_environment_makes_implicit_interactive_default_cwd() {
    let temp = tempfile::tempdir().unwrap();
    let environment = WorkspaceEnvironment {
        ci: true,
        ..WorkspaceEnvironment::default()
    };
    let config = WorkspaceConfig::default();
    let selection = resolve_workspace(&request(
        WorkspaceInvocation::Interactive,
        Some(temp.path()),
        &environment,
        &config,
        temp.path(),
    ))
    .unwrap();
    assert_eq!(
        selection,
        WorkspaceSelection::LaunchCwd {
            path: temp.path().to_path_buf(),
            reason: CwdReason::CiDefault,
        }
    );

    // An explicit choice still wins under CI.
    let base = temp.path().join("base");
    std::fs::create_dir(&base).unwrap();
    let environment = WorkspaceEnvironment {
        ci: true,
        base: Some(base.display().to_string()),
        ..WorkspaceEnvironment::default()
    };
    let mut explicit = request(
        WorkspaceInvocation::Interactive,
        Some(temp.path()),
        &environment,
        &config,
        temp.path(),
    );
    explicit.explicit_mode = Some(WorkspaceMode::Dated);
    assert!(matches!(
        resolve_workspace(&explicit).unwrap(),
        WorkspaceSelection::Dated(_)
    ));
}

#[test]
fn selector_precedence_and_conflicts() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("base");
    std::fs::create_dir(&base).unwrap();
    let config = WorkspaceConfig::default();

    // Both env selectors set is ambiguous.
    let environment = WorkspaceEnvironment {
        workspace: Some(temp.path().display().to_string()),
        mode: Some("cwd".to_string()),
        ..WorkspaceEnvironment::default()
    };
    assert_eq!(
        resolve_workspace(&request(
            WorkspaceInvocation::Interactive,
            Some(temp.path()),
            &environment,
            &config,
            temp.path(),
        )),
        Err(WorkspaceError::AmbiguousEnvironment)
    );

    // An explicit mode flag supersedes the ambiguous environment.
    let mut with_flag = request(
        WorkspaceInvocation::Interactive,
        Some(temp.path()),
        &environment,
        &config,
        temp.path(),
    );
    with_flag.explicit_mode = Some(WorkspaceMode::Cwd);
    assert!(matches!(
        resolve_workspace(&with_flag).unwrap(),
        WorkspaceSelection::LaunchCwd {
            reason: CwdReason::ModeCwd,
            ..
        }
    ));

    // --workspace and --workspace-mode are mutually exclusive.
    let mut conflicting = request(
        WorkspaceInvocation::Interactive,
        Some(temp.path()),
        &environment,
        &config,
        temp.path(),
    );
    conflicting.explicit_workspace = Some(".");
    conflicting.explicit_mode = Some(WorkspaceMode::Cwd);
    assert_eq!(
        resolve_workspace(&conflicting),
        Err(WorkspaceError::ConflictingSelectors)
    );

    // Empty env selector values are explicit errors.
    let environment = WorkspaceEnvironment {
        mode: Some(String::new()),
        ..WorkspaceEnvironment::default()
    };
    assert!(matches!(
        resolve_workspace(&request(
            WorkspaceInvocation::Interactive,
            Some(temp.path()),
            &environment,
            &config,
            temp.path(),
        )),
        Err(WorkspaceError::EmptySelector(_))
    ));

    // HAIDER_WORKSPACE must be absolute and existing.
    let environment = WorkspaceEnvironment {
        workspace: Some("relative/path".to_string()),
        ..WorkspaceEnvironment::default()
    };
    assert!(matches!(
        resolve_workspace(&request(
            WorkspaceInvocation::Interactive,
            Some(temp.path()),
            &environment,
            &config,
            temp.path(),
        )),
        Err(WorkspaceError::EnvWorkspaceInvalid(_))
    ));
}

#[test]
fn config_mode_applies_to_headless_as_explicit_opt_in() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("base");
    std::fs::create_dir(&base).unwrap();
    let environment = environment_with_base(&base);
    let config = WorkspaceConfig {
        mode: Some(WorkspaceMode::Dated),
        ..WorkspaceConfig::default()
    };
    let selection = resolve_workspace(&request(
        WorkspaceInvocation::Headless,
        Some(temp.path()),
        &environment,
        &config,
        temp.path(),
    ))
    .unwrap();
    assert!(matches!(selection, WorkspaceSelection::Dated(_)));
}

#[test]
fn dated_mode_overrides_project_detection() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    let base = temp.path().join("base");
    std::fs::create_dir(&base).unwrap();
    let environment = environment_with_base(&base);
    let config = WorkspaceConfig::default();
    let mut forced = request(
        WorkspaceInvocation::Interactive,
        Some(&repo),
        &environment,
        &config,
        temp.path(),
    );
    forced.explicit_mode = Some(WorkspaceMode::Dated);
    assert!(matches!(
        resolve_workspace(&forced).unwrap(),
        WorkspaceSelection::Dated(_)
    ));
}

#[test]
fn reopened_allocation_leaf_is_preserved_not_nested() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("base");
    let leaf = base
        .join("Haider")
        .join("1448-03-30")
        .join("s-0123456789abcdef0123456789abcdef");
    std::fs::create_dir_all(&leaf).unwrap();
    let environment = environment_with_base(&base);
    let config = WorkspaceConfig::default();
    let selection = resolve_workspace(&request(
        WorkspaceInvocation::Interactive,
        Some(&leaf),
        &environment,
        &config,
        temp.path(),
    ))
    .unwrap();
    assert_eq!(
        selection,
        WorkspaceSelection::LaunchCwd {
            path: leaf.clone(),
            reason: CwdReason::InsideDatedAllocation,
        }
    );
    assert!(path_is_inside_dated_allocation(&leaf, &base));
    assert!(!path_is_inside_dated_allocation(&base.join("Haider"), &base));
    assert!(!path_is_inside_dated_allocation(
        &base.join("Haider").join("not-a-date").join("x"),
        &base
    ));
}

#[test]
fn same_day_sessions_get_distinct_leaves_under_one_daily_root() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("base");
    std::fs::create_dir(&base).unwrap();
    let environment = environment_with_base(&base);
    let config = WorkspaceConfig::default();
    let first = request(
        WorkspaceInvocation::Interactive,
        Some(temp.path()),
        &environment,
        &config,
        temp.path(),
    );
    let mut second = first.clone();
    second.allocation_entropy = [0xff; 16];
    let (WorkspaceSelection::Dated(a), WorkspaceSelection::Dated(b)) = (
        resolve_workspace(&first).unwrap(),
        resolve_workspace(&second).unwrap(),
    ) else {
        panic!("expected dated plans");
    };
    assert_eq!(a.daily_root, b.daily_root);
    assert_ne!(a.leaf, b.leaf);
    assert_ne!(a.allocation_id, b.allocation_id);
}

#[test]
fn day_rollover_changes_new_plans_only() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("base");
    std::fs::create_dir(&base).unwrap();
    let environment = environment_with_base(&base);
    let config = WorkspaceConfig::default();
    let mut before_midnight = request(
        WorkspaceInvocation::Interactive,
        Some(temp.path()),
        &environment,
        &config,
        temp.path(),
    );
    before_midnight.sample = CivilDateSample {
        year: 2026,
        month: 9,
        day: 13,
        utc_ms: 0,
        offset_seconds: 0,
    };
    let mut after_midnight = before_midnight.clone();
    after_midnight.sample = CivilDateSample {
        year: 2026,
        month: 9,
        day: 14,
        utc_ms: 0,
        offset_seconds: 0,
    };
    let (WorkspaceSelection::Dated(a), WorkspaceSelection::Dated(b)) = (
        resolve_workspace(&before_midnight).unwrap(),
        resolve_workspace(&after_midnight).unwrap(),
    ) else {
        panic!("expected dated plans");
    };
    assert_eq!(a.hijri_label, "1448-03-30");
    assert_eq!(b.hijri_label, "1448-04-01");
    assert_ne!(a.daily_root, b.daily_root);
}

#[test]
fn base_precedence_env_config_profile_store() {
    let temp = tempfile::tempdir().unwrap();
    let env_base = temp.path().join("env-base");
    let config_base = temp.path().join("config-base");
    let store = temp.path().join("store");
    std::fs::create_dir_all(&env_base).unwrap();
    std::fs::create_dir_all(&config_base).unwrap();
    std::fs::create_dir_all(&store).unwrap();
    let config = WorkspaceConfig {
        base: Some(config_base.clone()),
        ..WorkspaceConfig::default()
    };

    // Env base wins.
    let environment = environment_with_base(&env_base);
    let WorkspaceSelection::Dated(plan) = resolve_workspace(&request(
        WorkspaceInvocation::Interactive,
        Some(temp.path()),
        &environment,
        &config,
        &store,
    ))
    .unwrap() else {
        panic!("expected dated");
    };
    assert_eq!(plan.base, env_base);

    // Config base next.
    let environment = WorkspaceEnvironment::default();
    let WorkspaceSelection::Dated(plan) = resolve_workspace(&request(
        WorkspaceInvocation::Interactive,
        Some(temp.path()),
        &environment,
        &config,
        &store,
    ))
    .unwrap() else {
        panic!("expected dated");
    };
    assert_eq!(plan.base, config_base);

    // Explicit profile dir scopes generated work to the store.
    let environment = WorkspaceEnvironment {
        explicit_profile_dir: true,
        ..WorkspaceEnvironment::default()
    };
    let no_config = WorkspaceConfig::default();
    let WorkspaceSelection::Dated(plan) = resolve_workspace(&request(
        WorkspaceInvocation::Interactive,
        Some(temp.path()),
        &environment,
        &no_config,
        &store,
    ))
    .unwrap() else {
        panic!("expected dated");
    };
    assert_eq!(plan.base, store.join("workspaces"));

    // Relative env base is refused, never reinterpreted.
    let environment = WorkspaceEnvironment {
        base: Some("relative/base".to_string()),
        ..WorkspaceEnvironment::default()
    };
    assert!(matches!(
        resolve_workspace(&request(
            WorkspaceInvocation::Interactive,
            Some(temp.path()),
            &environment,
            &no_config,
            &store,
        )),
        Err(WorkspaceError::InvalidBase(_))
    ));
    let _ = WORKSPACE_BASE_ENV;
}

#[test]
fn relative_explicit_workspace_resolves_against_launch_cwd() {
    let temp = tempfile::tempdir().unwrap();
    let child = temp.path().join("child");
    std::fs::create_dir(&child).unwrap();
    let environment = WorkspaceEnvironment::default();
    let config = WorkspaceConfig::default();
    let mut explicit = request(
        WorkspaceInvocation::Interactive,
        Some(temp.path()),
        &environment,
        &config,
        temp.path(),
    );
    explicit.explicit_workspace = Some("child");
    let WorkspaceSelection::LaunchCwd { path, reason } =
        resolve_workspace(&explicit).unwrap()
    else {
        panic!("expected explicit path");
    };
    assert_eq!(path, child);
    assert_eq!(reason, CwdReason::ExplicitWorkspace);

    // A missing explicit path is an error, not an allocation.
    let mut missing = request(
        WorkspaceInvocation::Interactive,
        Some(temp.path()),
        &environment,
        &config,
        temp.path(),
    );
    missing.explicit_workspace = Some("does-not-exist");
    assert!(matches!(
        resolve_workspace(&missing),
        Err(WorkspaceError::ExplicitWorkspaceMissing(_))
    ));
}

#[test]
fn workspace_config_parses_and_rejects() {
    let valid: serde_json::Value = serde_json::json!({
        "default_model": "some-model",
        "workspace": {"mode": "dated", "base": "/abs/base", "timezone": "UTC"}
    });
    let config = WorkspaceConfig::from_config_value(&valid).unwrap();
    assert_eq!(config.mode, Some(WorkspaceMode::Dated));
    assert_eq!(config.base, Some(PathBuf::from("/abs/base")));
    assert_eq!(config.timezone, Some(super::WorkspaceTimezone::Utc));

    // Absent object means no opinion.
    let absent: serde_json::Value = serde_json::json!({"default_model": "m"});
    assert_eq!(
        WorkspaceConfig::from_config_value(&absent).unwrap(),
        WorkspaceConfig::default()
    );

    for invalid in [
        serde_json::json!({"workspace": {"mode": "sometimes"}}),
        serde_json::json!({"workspace": {"base": "relative"}}),
        serde_json::json!({"workspace": {"timezone": "utc"}}),
        serde_json::json!({"workspace": "auto"}),
    ] {
        assert!(
            WorkspaceConfig::from_config_value(&invalid).is_err(),
            "accepted invalid config: {invalid}"
        );
    }
}
