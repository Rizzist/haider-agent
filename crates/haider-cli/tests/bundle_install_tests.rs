//! Native filesystem coverage for required and optional installer executables.
//! Production smoke/signature verification is tested separately; these byte
//! fixtures exercise real rename, readonly, rollback and marker recovery paths.
#![allow(clippy::expect_used)]
#![allow(dead_code)]

#[path = "../src/lib.rs"]
mod cli_main;

use cli_main::update::UpdateError;
use cli_main::update::members::{ALL_BUNDLE_MEMBERS, BUNDLE_MEMBERS, BundleMember};
use cli_main::update::staging::{VerifiedStagedPair, verified_bundle_for_test};
use cli_main::update::transaction::{
    CommitBoundary, FaultInjector, InstallLayout, InstalledPairVerifier, NoFaults,
    PreparedTransaction, commit_pair, marker_path,
};
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

const BOUNDARIES: [CommitBoundary; 11] = [
    CommitBoundary::BackupDaemon,
    CommitBoundary::BackupTui,
    CommitBoundary::BackupCli,
    CommitBoundary::RenameDaemon,
    CommitBoundary::RenameTui,
    CommitBoundary::RenameCli,
    CommitBoundary::ChmodDaemon,
    CommitBoundary::ChmodTui,
    CommitBoundary::ChmodCli,
    CommitBoundary::InstallDirFsync,
    CommitBoundary::InstalledPairVerify,
];

const PORTAL_BOUNDARIES: [(CommitBoundary, &str); 3] = [
    (CommitBoundary::BackupPortal, "backup_portal"),
    (CommitBoundary::RenamePortal, "portal_installed"),
    (CommitBoundary::ChmodPortal, "portal_writable"),
];

const PORTAL_INSTALL_STATES: [(&str, &[BundleMember]); 4] = [
    ("fresh", &[]),
    ("historic pair", &[BundleMember::Daemon, BundleMember::Cli]),
    ("split without portal", &BUNDLE_MEMBERS),
    ("existing portal", &ALL_BUNDLE_MEMBERS),
];

struct FailAfter(CommitBoundary);

impl FaultInjector for FailAfter {
    fn after(&self, boundary: CommitBoundary) -> Result<(), UpdateError> {
        if boundary == self.0 {
            Err(UpdateError::Internal(format!("injected {boundary:?}")))
        } else {
            Ok(())
        }
    }
}

struct InterruptAfter(CommitBoundary);

impl FaultInjector for InterruptAfter {
    fn after(&self, boundary: CommitBoundary) -> Result<(), UpdateError> {
        if boundary == self.0 {
            // Unwind past commit_pair's error rollback, leaving its durable
            // marker and real backups for the next lock acquisition. This
            // models an abandoned transaction, not an OS power-loss test.
            panic!("interrupted {boundary:?}");
        }
        Ok(())
    }
}

struct VerifyBytes;

impl InstalledPairVerifier for VerifyBytes {
    fn verify(
        &self,
        layout: &InstallLayout,
        bundle: &VerifiedStagedPair,
    ) -> Result<(), UpdateError> {
        for member in bundle.members() {
            if cli_main::update::staging::sha256_file(layout.path(member))? != bundle.digest(member)
            {
                return Err(UpdateError::Internal(
                    "fixture member digest mismatch".into(),
                ));
            }
        }
        Ok(())
    }
}

fn fixture_bundle(install: &Path) -> VerifiedStagedPair {
    fixture_bundle_members(install, &BUNDLE_MEMBERS)
}

fn fixture_bundle_members(install: &Path, members: &[BundleMember]) -> VerifiedStagedPair {
    let sources = tempfile::tempdir().expect("source fixture");
    let paths: Vec<_> = members
        .iter()
        .map(|&member| (member, sources.path().join(member.file_name())))
        .collect();
    for (member, source) in &paths {
        fs::write(source, format!("new {}", member.name())).expect("source member");
        haider_platform::set_mode(source, 0o700).expect("executable source");
    }
    verified_bundle_for_test(
        install,
        &paths
            .iter()
            .map(|(member, path)| (*member, path.as_path()))
            .collect::<Vec<_>>(),
        "9.0.0",
    )
    .expect("verified fixture capability")
}

fn seed_old(layout: &InstallLayout) {
    seed_old_members(layout, &BUNDLE_MEMBERS);
}

fn seed_old_members(layout: &InstallLayout, members: &[BundleMember]) {
    for &member in members {
        fs::write(layout.path(member), format!("old {}", member.name())).expect("installed member");
        let mode = if member == BundleMember::WaylandPortal {
            0o750
        } else {
            0o700
        };
        haider_platform::set_mode(layout.path(member), mode).expect("installed mode");
    }
}

fn snapshot(layout: &InstallLayout) -> Vec<Option<(Vec<u8>, u32)>> {
    snapshot_members(layout, &BUNDLE_MEMBERS)
}

fn snapshot_members(
    layout: &InstallLayout,
    members: &[BundleMember],
) -> Vec<Option<(Vec<u8>, u32)>> {
    members
        .iter()
        .copied()
        .map(|member| {
            let path = layout.path(member);
            match fs::metadata(path) {
                Ok(metadata) => Some((
                    fs::read(path).expect("member bytes"),
                    haider_platform::metadata_mode(&metadata) & 0o777,
                )),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => panic!("member metadata: {error}"),
            }
        })
        .collect()
}

fn assert_transaction_clean(layout: &InstallLayout) {
    assert!(!marker_path(layout).exists());
    for entry in fs::read_dir(&layout.dir).expect("install entries") {
        let name = entry.expect("entry").file_name();
        let name = name.to_string_lossy();
        assert!(
            !name.contains("-old-") && !name.starts_with(".haider-update-stage-"),
            "leftover {name}"
        );
    }
}

#[test]
fn bundle_member_projection_uses_native_executable_names() {
    let expected = if cfg!(windows) {
        ["haiderd.exe", "haider-tui.exe", "haider.exe"]
    } else {
        ["haiderd", "haider-tui", "haider"]
    };
    assert_eq!(BUNDLE_MEMBERS.map(BundleMember::file_name), expected);
}

/// MUTATION CHECK: omit any member's rollback, or fail to clear a read-only
/// replacement on Windows. Each native failure must restore all old bytes and
/// modes, or complete absence for a fresh install, without leaving a marker.
#[test]
fn fresh_and_existing_installs_roll_back_at_every_member_boundary() {
    for fresh in [true, false] {
        for boundary in BOUNDARIES {
            let install = tempfile::tempdir().expect("install fixture");
            let layout = InstallLayout::for_test(install.path().to_path_buf());
            if !fresh {
                seed_old(&layout);
            }
            let before = snapshot(&layout);
            let bundle = fixture_bundle(install.path());
            let prepared = PreparedTransaction::acquire(layout.clone()).expect("prepare fixture");
            let result = commit_pair(
                prepared,
                bundle,
                &FailAfter(boundary),
                &VerifyBytes,
                "1.0.0",
            );
            assert!(
                matches!(result, Err(UpdateError::Internal(message)) if message == format!("injected {boundary:?}")),
                "fresh={fresh} boundary={boundary:?}"
            );
            assert_eq!(
                snapshot(&layout),
                before,
                "fresh={fresh} boundary={boundary:?}"
            );
            assert!(!marker_path(&layout).exists());
            for entry in fs::read_dir(install.path()).expect("install entries") {
                let name = entry.expect("entry").file_name();
                let name = name.to_string_lossy();
                assert!(
                    !name.contains("-old-") && !name.starts_with(".haider-update-stage-"),
                    "leftover {name}"
                );
            }
        }
    }
}

#[test]
fn fresh_install_crash_recovers_to_absent_bundle() {
    let install = tempfile::tempdir().expect("fresh install");
    let layout = InstallLayout::for_test(install.path().to_path_buf());
    let before = snapshot(&layout);
    let bundle = fixture_bundle(install.path());
    let prepared = PreparedTransaction::acquire(layout.clone()).expect("prepare fresh");
    let committed =
        commit_pair(prepared, bundle, &NoFaults, &VerifyBytes, "1.0.0").expect("publish fresh");
    assert!(snapshot(&layout).iter().all(Option::is_some));
    drop(committed);
    let recovered = PreparedTransaction::acquire(layout.clone()).expect("recover fresh install");
    assert_eq!(snapshot(&layout), before);
    assert!(!marker_path(&layout).exists());
    drop(recovered);
}

/// MUTATION CHECK: omit the optional member from publication,
/// or overwrite its incumbent mode instead of preserving it. Both migrations
/// introduce the portal; an existing four-member install replaces its old bytes.
#[test]
fn optional_portal_publishes_with_fresh_and_existing_bundle_access_policies() {
    for (state, old_members) in PORTAL_INSTALL_STATES {
        let install = tempfile::tempdir().expect("portal install");
        let layout = InstallLayout::for_test(install.path().to_path_buf());
        seed_old_members(&layout, old_members);
        let bundle = fixture_bundle_members(install.path(), &ALL_BUNDLE_MEMBERS);
        assert_eq!(bundle.members().collect::<Vec<_>>(), ALL_BUNDLE_MEMBERS);
        let prepared = PreparedTransaction::acquire(layout.clone()).expect("prepare portal bundle");
        let mut committed = commit_pair(prepared, bundle, &NoFaults, &VerifyBytes, "1.0.0")
            .expect("publish portal bundle");
        for member in ALL_BUNDLE_MEMBERS {
            assert_eq!(
                fs::read(layout.path(member)).expect("published bytes"),
                format!("new {}", member.name()).as_bytes(),
                "{state}: {member:?}",
            );
            let expected_mode = if cfg!(windows) {
                0o700
            } else if old_members.is_empty() {
                0o755
            } else if member == BundleMember::WaylandPortal && old_members.contains(&member) {
                0o750
            } else {
                0o700
            };
            assert_eq!(
                haider_platform::metadata_mode(
                    &fs::metadata(layout.path(member)).expect("published metadata")
                ) & 0o777,
                expected_mode,
                "{state}: {member:?}",
            );
        }
        committed.finalize().expect("finalize portal bundle");
        assert_transaction_clean(&layout);
    }
}

/// MUTATION CHECK: leave behind an introduced portal or omit its old backup
/// from rollback. Every portal-specific fault restores all four paths' exact
/// bytes, modes and absence, including the historic pair's absent TUI.
#[test]
fn optional_portal_failure_restores_fresh_migration_and_existing_installs() {
    for (state, old_members) in PORTAL_INSTALL_STATES {
        for (boundary, _) in PORTAL_BOUNDARIES {
            let install = tempfile::tempdir().expect("portal rollback install");
            let layout = InstallLayout::for_test(install.path().to_path_buf());
            seed_old_members(&layout, old_members);
            let before = snapshot_members(&layout, &ALL_BUNDLE_MEMBERS);
            let bundle = fixture_bundle_members(install.path(), &ALL_BUNDLE_MEMBERS);
            let prepared =
                PreparedTransaction::acquire(layout.clone()).expect("prepare portal rollback");
            let result = commit_pair(
                prepared,
                bundle,
                &FailAfter(boundary),
                &VerifyBytes,
                "1.0.0",
            );
            assert!(
                matches!(result, Err(UpdateError::Internal(message)) if message == format!("injected {boundary:?}")),
                "{state}: {boundary:?}",
            );
            assert_eq!(
                snapshot_members(&layout, &ALL_BUNDLE_MEMBERS),
                before,
                "{state}: {boundary:?}"
            );
            assert_transaction_clean(&layout);
        }
    }
}

/// MUTATION CHECK: fail to parse a portal phase/member, or lose an optional
/// member's previous absence in the persisted marker. Recovery must use the
/// abandoned transaction's actual hard-link backups on the native filesystem.
#[test]
fn optional_portal_interruption_recovers_at_each_portal_phase() {
    for (state, old_members) in PORTAL_INSTALL_STATES {
        for (boundary, phase) in PORTAL_BOUNDARIES {
            let install = tempfile::tempdir().expect("portal recovery install");
            let layout = InstallLayout::for_test(install.path().to_path_buf());
            seed_old_members(&layout, old_members);
            let before = snapshot_members(&layout, &ALL_BUNDLE_MEMBERS);
            let bundle = fixture_bundle_members(install.path(), &ALL_BUNDLE_MEMBERS);
            let prepared =
                PreparedTransaction::acquire(layout.clone()).expect("prepare portal interruption");
            let interrupted = catch_unwind(AssertUnwindSafe(|| {
                commit_pair(
                    prepared,
                    bundle,
                    &InterruptAfter(boundary),
                    &VerifyBytes,
                    "1.0.0",
                )
            }));
            let panic = match interrupted {
                Err(panic) => panic,
                Ok(_) => panic!("{state}: {boundary:?} did not interrupt the transaction"),
            };
            assert_eq!(
                panic.downcast_ref::<String>(),
                Some(&format!("interrupted {boundary:?}"))
            );
            let marker: serde_json::Value = serde_json::from_slice(
                &fs::read(marker_path(&layout)).expect("abandoned transaction marker"),
            )
            .expect("persisted marker JSON");
            assert_eq!(marker["phase"], phase, "{state}: {boundary:?}");
            assert_eq!(
                marker["members"].as_array().expect("member ledger").len(),
                4
            );
            let recovered =
                PreparedTransaction::acquire(layout.clone()).expect("recover portal transaction");
            assert_eq!(
                snapshot_members(&layout, &ALL_BUNDLE_MEMBERS),
                before,
                "{state}: {boundary:?}"
            );
            assert_transaction_clean(&layout);
            drop(recovered);
        }
    }
}

/// Changing publication back to 0700 makes a root-owned shared-prefix install
/// unusable to its users. Only canonical executables receive public access;
/// staging capabilities, the OS lock and recovery markers stay private.
#[test]
fn fresh_bundle_publishes_public_executables_with_private_transaction_state() {
    assert_publication_modes(None, false);
}

/// Publishing every member as 0755 would expose an intentionally private
/// existing install. Preserve each incumbent mode, and let a newly introduced
/// payload inherit the historic CLI's access policy.
#[test]
fn existing_bundle_preserves_member_modes_and_private_migration_policy() {
    assert_publication_modes(Some([0o700, 0o700, 0o700]), false);
    assert_publication_modes(Some([0o755, 0o750, 0o700]), false);
    assert_publication_modes(Some([0o700, 0o700, 0o700]), true);
}

fn assert_publication_modes(previous: Option<[u32; 3]>, omit_payload: bool) {
    let install = tempfile::tempdir().expect("permission install");
    haider_platform::set_mode(install.path(), 0o755).expect("shared prefix");
    let layout = InstallLayout::for_test(install.path().to_path_buf());
    if let Some(modes) = previous {
        seed_old(&layout);
        for (member, mode) in BUNDLE_MEMBERS.into_iter().zip(modes) {
            haider_platform::set_mode(layout.path(member), mode).expect("incumbent mode");
        }
        if omit_payload {
            fs::remove_file(&layout.haider_tui).expect("historic pair has no payload");
        }
    }
    let bundle = fixture_bundle(install.path());
    for member in BUNDLE_MEMBERS {
        assert_eq!(
            haider_platform::metadata_mode(&fs::metadata(bundle.path(member)).expect("stage mode"))
                & 0o777,
            0o500,
        );
    }
    let stage_root = bundle
        .path(BundleMember::Cli)
        .parent()
        .expect("binary dir")
        .parent()
        .expect("private stage root");
    assert_eq!(
        haider_platform::metadata_mode(&fs::metadata(stage_root).expect("private stage")) & 0o077,
        0,
    );
    let prepared =
        PreparedTransaction::acquire(layout.clone()).expect("prepare permission install");
    let mut committed = commit_pair(prepared, bundle, &NoFaults, &VerifyBytes, "1.0.0")
        .expect("publish permission install");
    for (index, member) in BUNDLE_MEMBERS.into_iter().enumerate() {
        let expected = if cfg!(windows) {
            0o700
        } else if omit_payload && member == BundleMember::Tui {
            previous.expect("migration has incumbent modes")[2]
        } else {
            previous.map_or(0o755, |modes| modes[index])
        };
        assert_eq!(
            haider_platform::metadata_mode(
                &fs::metadata(layout.path(member)).expect("published mode")
            ) & 0o777,
            expected,
            "{member:?}",
        );
    }
    for private in [
        marker_path(&layout),
        install.path().join(".haider-update.lock"),
    ] {
        assert_eq!(
            haider_platform::metadata_mode(
                &fs::metadata(private).expect("private transaction state")
            ) & 0o077,
            0,
        );
    }
    committed.finalize().expect("finalize permission install");
}

#[test]
fn partial_existing_bundle_is_refused_before_lock_or_mutation() {
    for only in BUNDLE_MEMBERS {
        let install = tempfile::tempdir().expect("partial install");
        let layout = InstallLayout::for_test(install.path().to_path_buf());
        fs::write(layout.path(only), b"only installed member").expect("partial member");
        haider_platform::set_mode(layout.path(only), 0o700).expect("partial executable");
        let before = snapshot(&layout);
        assert!(PreparedTransaction::acquire(layout.clone()).is_err());
        assert_eq!(snapshot(&layout), before);
        assert!(!install.path().join(".haider-update.lock").exists());
    }
}

#[test]
fn directory_installer_requires_complete_sources_before_install_mutation() {
    let source = tempfile::tempdir().expect("incomplete source");
    fs::write(
        source.path().join(BundleMember::Cli.file_name()),
        b"source CLI",
    )
    .expect("source CLI");
    let install = tempfile::tempdir().expect("installation");
    let layout = InstallLayout::for_test(install.path().to_path_buf());
    let before = snapshot(&layout);
    assert!(
        cli_main::update::install_bundle_from_directory(source.path(), install.path()).is_err()
    );
    assert_eq!(snapshot(&layout), before);
    assert_eq!(
        fs::read_dir(install.path()).expect("empty install").count(),
        0
    );
}

#[test]
fn directory_installer_refuses_nonmatching_real_executables_before_install_mutation() {
    let source = tempfile::tempdir().expect("incorrect executable source");
    let test_executable = std::env::current_exe().expect("test executable");
    for member in BUNDLE_MEMBERS {
        fs::copy(&test_executable, source.path().join(member.file_name()))
            .expect("source executable copy");
    }
    let install = tempfile::tempdir().expect("installation");
    let layout = InstallLayout::for_test(install.path().to_path_buf());
    let before = snapshot(&layout);
    assert!(
        cli_main::update::install_bundle_from_directory(source.path(), install.path()).is_err()
    );
    assert_eq!(snapshot(&layout), before);
    assert_eq!(
        fs::read_dir(install.path()).expect("empty install").count(),
        0
    );
}
