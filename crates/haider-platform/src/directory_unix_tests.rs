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
