#![allow(clippy::unwrap_used)]
use std::os::unix::fs::symlink;

use super::open_absolute_directory;

#[test]
fn absolute_directory_retains_readable_final_handle_after_rename() {
    let temporary = tempfile::tempdir().unwrap();
    let base = temporary.path().canonicalize().unwrap();
    let original = base.join("original");
    std::fs::create_dir(&original).unwrap();
    let directory = open_absolute_directory(&original).unwrap();
    let moved = base.join("moved");
    std::fs::rename(&original, &moved).unwrap();
    std::fs::create_dir(&original).unwrap();
    rustix::fs::mkdirat(
        &directory,
        "retained",
        rustix::fs::Mode::from_raw_mode(0o700),
    )
    .unwrap();
    // An O_PATH final descriptor would reject both listing and fsync.
    let mut entries = rustix::fs::Dir::read_from(&directory).unwrap();
    assert!(entries.any(|entry| entry.unwrap().file_name().to_bytes() == b"retained"));
    rustix::fs::fsync(&directory).unwrap();
    assert!(moved.join("retained").is_dir());
    assert!(!original.join("retained").exists());
}

#[test]
fn absolute_directory_refuses_symlink_ancestors_and_nonabsolute_paths() {
    let temporary = tempfile::tempdir().unwrap();
    let base = temporary.path().canonicalize().unwrap();
    let real = base.join("real");
    std::fs::create_dir_all(real.join("child")).unwrap();
    symlink(&real, base.join("alias")).unwrap();
    assert!(open_absolute_directory(&base.join("alias")).is_err());
    assert!(open_absolute_directory(&base.join("alias/child")).is_err());
    assert!(open_absolute_directory(&real.join("../real")).is_err());
    assert!(open_absolute_directory(std::path::Path::new("relative")).is_err());
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn absolute_directory_traverses_search_only_ancestors() {
    use std::os::unix::fs::PermissionsExt;
    let temporary = tempfile::tempdir().unwrap();
    let base = temporary.path().canonicalize().unwrap();
    let ancestor = base.join("search-only");
    let leaf = ancestor.join("private");
    std::fs::create_dir_all(&leaf).unwrap();
    std::fs::set_permissions(&ancestor, std::fs::Permissions::from_mode(0o111)).unwrap();
    let result = open_absolute_directory(&leaf);
    std::fs::set_permissions(&ancestor, std::fs::Permissions::from_mode(0o700)).unwrap();
    let directory = result.unwrap();
    rustix::fs::fsync(&directory).unwrap();
}

#[test]
fn workspace_directory_duplicates_are_close_on_exec_and_retain_the_anchor() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().canonicalize().unwrap();
    let owner = super::open_workspace_directory(&path).unwrap();
    let duplicate = super::duplicate_workspace_directory(&owner).unwrap();
    assert!(
        rustix::io::fcntl_getfd(&owner)
            .unwrap()
            .contains(rustix::io::FdFlags::CLOEXEC)
    );
    assert!(
        rustix::io::fcntl_getfd(&duplicate)
            .unwrap()
            .contains(rustix::io::FdFlags::CLOEXEC)
    );
    let original = rustix::fs::fstat(&owner).unwrap();
    drop(owner);
    let retained = rustix::fs::fstat(&duplicate).unwrap();
    assert_eq!(
        (original.st_dev, original.st_ino),
        (retained.st_dev, retained.st_ino)
    );
    rustix::fs::mkdirat(
        &duplicate,
        "through-duplicate",
        rustix::fs::Mode::from_raw_mode(0o700),
    )
    .unwrap();
    assert!(path.join("through-duplicate").is_dir());
}

#[test]
#[allow(unsafe_code)]
fn duplicated_workspace_anchor_is_absent_after_spawn() {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::process::CommandExt as _;
    const HELPER: &str = "HAIDER_DIRECTORY_DUPLICATE_HELPER";
    if let Some(identity) = std::env::var_os(HELPER) {
        let identity = identity
            .to_str()
            .unwrap()
            .split(':')
            .map(|part| part.parse::<u64>().unwrap())
            .collect::<Vec<_>>();
        // An exec-time fd reuse is allowed; retaining this exact directory is not.
        let fd = libc::c_int::try_from(identity[0]).unwrap();
        if let Some(actual) = crate::file_lock::test_descriptor_identity(fd).unwrap() {
            assert_ne!(
                actual,
                (identity[1], identity[2]),
                "workspace anchor survived exec"
            );
        }
        return;
    }
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().canonicalize().unwrap();
    let owner = super::open_workspace_directory(&path).unwrap();
    let duplicate = super::duplicate_workspace_directory(&owner).unwrap();
    let metadata = rustix::fs::fstat(&duplicate).unwrap();
    for fork_exec in [false, true] {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "duplicated_workspace_anchor_is_absent_after_spawn",
                "--nocapture",
            ])
            .env(
                HELPER,
                format!(
                    "{}:{}:{}",
                    duplicate.as_raw_fd(),
                    metadata.st_dev,
                    metadata.st_ino
                ),
            );
        if fork_exec {
            // SAFETY: the empty callback performs no post-fork work. It forces
            // fork/exec so spawn's own descriptor-closing defaults cannot hide
            // a missing CLOEXEC flag on the duplicated directory.
            unsafe { command.pre_exec(|| Ok(())) };
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "fork_exec={fork_exec}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(
        rustix::fs::fstat(&duplicate).unwrap().st_ino,
        metadata.st_ino
    );
}
