//! whisper-cli discovery-order, install-driver, and zip-slip laws.

#![allow(clippy::expect_used)]

mod common;

#[cfg(windows)]
use std::ffi::OsStr;
use std::ffi::OsString;
// Only the macOS well-known-paths test constructs PathBuf values.
#[cfg(target_os = "macos")]
use std::path::PathBuf;

use common::{StubBehavior, build_stored_zip, write_stub_command};
use haider_stt::SttError;
#[cfg(windows)]
use haider_stt::runtime::find_on_path_with_pathext;
#[cfg(target_os = "macos")]
use haider_stt::runtime::well_known_runtime_paths;
use haider_stt::runtime::{
    discover_runtime_with, extract_runtime_zip, find_on_path, find_runtime_in_dir,
    install_runtime_with_homebrew, reject_unsafe_zip_entries, runtime_directory,
    runtime_executable_names,
};

/// The ADE discovery ladder is pinned: managed `<whisper>/runtime/` beats
/// PATH, PATH beats the well-known list, and within one location the name
/// order is `whisper-cli` → `main` → `whisper`.
///
/// MUTATION CHECK: swap the managed-dir and PATH tiers, or probe `whisper`
/// before `whisper-cli`. Expected runtime failure: the wrong path wins one
/// of the assertions below.
#[test]
fn discovery_order_is_managed_dir_then_path_then_well_known() {
    let whisper = tempfile::tempdir().expect("whisper dir");
    let path_dir = tempfile::tempdir().expect("path dir");
    let known_dir = tempfile::tempdir().expect("well-known dir");
    let managed = runtime_directory(whisper.path()).join("bin");
    std::fs::create_dir_all(&managed).expect("managed dir");
    let managed_cli = write_stub_command(&managed, "whisper-cli", StubBehavior::Success);
    let path_cli = write_stub_command(path_dir.path(), "whisper-cli", StubBehavior::Success);
    let known_cli = write_stub_command(known_dir.path(), "whisper-cli", StubBehavior::Success);
    let path_value = OsString::from(path_dir.path().display().to_string());
    let well_known = vec![known_cli.clone()];
    // All three tiers present: the managed runtime wins (recursive walk).
    assert_eq!(
        discover_runtime_with(whisper.path(), Some(&path_value), &well_known),
        Some(managed_cli.clone())
    );
    // Managed gone: PATH wins.
    std::fs::remove_file(&managed_cli).expect("evict managed");
    assert_eq!(
        discover_runtime_with(whisper.path(), Some(&path_value), &well_known),
        Some(path_cli.clone())
    );
    // PATH gone too: the well-known list answers.
    std::fs::remove_file(&path_cli).expect("evict path");
    assert_eq!(
        discover_runtime_with(whisper.path(), Some(&path_value), &well_known),
        Some(known_cli)
    );
}

/// Name precedence within one directory: `whisper-cli` beats `main` beats
/// `whisper` (ADE name order).
#[test]
fn name_order_prefers_whisper_cli_then_main_then_whisper() {
    let dir = tempfile::tempdir().expect("dir");
    let whisper = write_stub_command(dir.path(), "whisper", StubBehavior::Success);
    let path_value = OsString::from(dir.path().display().to_string());
    assert_eq!(
        find_on_path(runtime_executable_names(), Some(&path_value)),
        Some(whisper.clone())
    );
    let main = write_stub_command(dir.path(), "main", StubBehavior::Success);
    assert_eq!(
        find_on_path(runtime_executable_names(), Some(&path_value)),
        Some(main)
    );
    let cli = write_stub_command(dir.path(), "whisper-cli", StubBehavior::Success);
    assert_eq!(
        find_on_path(runtime_executable_names(), Some(&path_value)),
        Some(cli)
    );
    assert!(whisper.is_file(), "lower-precedence names stay in place");
}

/// Windows PATH lookup applies PATHEXT inside each canonical executable name,
/// without disturbing PATH-directory or whisper/main name precedence.
#[cfg(windows)]
#[test]
fn windows_path_resolution_honors_pathext_order() {
    let dir = tempfile::tempdir().expect("dir");
    let com = write_stub_command(dir.path(), "whisper-cli.com", StubBehavior::Success);
    let exe = write_stub_command(dir.path(), "whisper-cli.exe", StubBehavior::Success);
    let path_value = OsString::from(dir.path().display().to_string());

    assert_eq!(
        find_on_path_with_pathext(
            &["whisper-cli"],
            Some(&path_value),
            Some(OsStr::new(".COM;.EXE")),
        ),
        Some(com)
    );
    assert_eq!(
        find_on_path_with_pathext(
            &["whisper-cli"],
            Some(&path_value),
            Some(OsStr::new(".EXE;.COM")),
        ),
        Some(exe)
    );
}

/// The macOS well-known list is the ADE's literal list, in order.
#[cfg(target_os = "macos")]
#[test]
fn macos_well_known_paths_are_the_ade_literals() {
    assert_eq!(
        well_known_runtime_paths(),
        vec![
            PathBuf::from("/opt/homebrew/bin/whisper-cli"),
            PathBuf::from("/usr/local/bin/whisper-cli"),
            PathBuf::from("/opt/homebrew/bin/whisper"),
            PathBuf::from("/usr/local/bin/whisper"),
            PathBuf::from("/opt/homebrew/bin/main"),
            PathBuf::from("/usr/local/bin/main"),
        ]
    );
}

/// The Homebrew driver: success is silent; a non-zero exit surfaces the
/// FIRST non-empty output line, never the full log.
///
/// MUTATION CHECK: ignore the exit status or report raw combined output.
/// Expected runtime failure: the failing stub stops erroring, or the error
/// carries more than the first line.
#[tokio::test]
async fn homebrew_driver_maps_exit_status_and_first_output_line() {
    let dir = tempfile::tempdir().expect("dir");
    let ok_brew = write_stub_command(dir.path(), "brew-ok", StubBehavior::Success);
    install_runtime_with_homebrew(&ok_brew)
        .await
        .expect("zero exit is success");
    let failing = write_stub_command(
        dir.path(),
        "brew-fail",
        StubBehavior::Failure {
            stderr: vec![
                "Error: no bottle available".to_owned(),
                "second line".to_owned(),
            ],
            exit_code: 1,
        },
    );
    let error = install_runtime_with_homebrew(&failing)
        .await
        .expect_err("non-zero exit fails");
    match error {
        SttError::Endpoint(message) => {
            assert!(message.contains("Error: no bottle available"), "{message}");
            assert!(!message.contains("second line"), "{message}");
        }
        other => panic!("expected Endpoint error, got {other:?}"),
    }
}

/// Zip-slip guard: absolute paths, drive prefixes, and `..` components are
/// refused; ordinary nested entries pass.
#[test]
fn zip_entry_guard_refuses_escaping_names() {
    reject_unsafe_zip_entries("Release/whisper-cli.exe\nRelease/lib/ggml.dll\n")
        .expect("nested entries pass");
    for evil in [
        "../evil.exe",
        "/etc/passwd",
        "C:\\Windows\\evil.exe",
        "a/../../evil",
        "a\\..\\evil",
    ] {
        assert!(
            reject_unsafe_zip_entries(evil).is_err(),
            "entry `{evil}` must be refused"
        );
    }
}

/// End-to-end zip extraction: a well-formed archive lands under
/// `<whisper>/runtime/` and discovery finds the executable; an archive with
/// an escaping entry is refused BEFORE extraction (nothing lands anywhere).
///
/// MUTATION CHECK: extract before screening entry names. Expected runtime
/// failure: the evil archive plants `evil.txt` outside the runtime dir (the
/// filesystem assertion below), or extraction is attempted at all.
#[tokio::test]
async fn runtime_zip_extraction_is_screened_and_lands_in_runtime_dir() {
    let whisper = tempfile::tempdir().expect("whisper dir");
    let runtime_entry = format!("Release/{}", runtime_executable_names()[0]);
    let good_zip = build_stored_zip(&[
        (runtime_entry.as_str(), b"native runtime bytes".as_slice()),
        ("Release/README.txt", b"runtime".as_slice()),
    ]);
    let good_path = whisper.path().join("whisper-bin-x64.zip");
    std::fs::write(&good_path, good_zip).expect("write zip");
    let runtime_dir = runtime_directory(whisper.path());
    let extracted = extract_runtime_zip(&good_path, &runtime_dir)
        .await
        .expect("good archive extracts");
    assert!(extracted.starts_with(&runtime_dir), "{extracted:?}");
    assert_eq!(find_runtime_in_dir(&runtime_dir), Some(extracted));
    assert_eq!(
        std::fs::read(runtime_dir.join("Release/README.txt")).expect("read extracted bytes"),
        b"runtime"
    );

    let evil_whisper = tempfile::tempdir().expect("evil whisper dir");
    let evil_zip = build_stored_zip(&[("../evil.txt", b"escaped".as_slice())]);
    let evil_path = evil_whisper.path().join("whisper-bin-x64.zip");
    std::fs::write(&evil_path, evil_zip).expect("write evil zip");
    let evil_runtime = runtime_directory(evil_whisper.path());
    let error = extract_runtime_zip(&evil_path, &evil_runtime)
        .await
        .expect_err("escaping entry refused");
    assert!(matches!(error, SttError::InvalidArgument(message) if message.contains("escapes")));
    assert!(
        !evil_whisper.path().join("evil.txt").exists(),
        "no byte of an unsafe archive may be extracted"
    );

    // This mutation subcase constructs a Unix filesystem symlink. The ZIP
    // format, path-screening, and extraction assertions above remain enabled
    // on Windows.
    #[cfg(unix)]
    {
        let linked_whisper = tempfile::tempdir().expect("linked whisper dir");
        let outside = tempfile::tempdir().expect("outside dir");
        let linked_runtime = runtime_directory(linked_whisper.path());
        std::fs::create_dir_all(&linked_runtime).expect("linked runtime dir");
        std::os::unix::fs::symlink(outside.path(), linked_runtime.join("Release"))
            .expect("destination symlink");
        let linked_zip = build_stored_zip(&[(
            "Release/whisper-cli",
            b"must stay inside runtime".as_slice(),
        )]);
        let linked_path = linked_whisper.path().join("runtime.zip");
        std::fs::write(&linked_path, linked_zip).expect("write linked zip");
        let linked_error = extract_runtime_zip(&linked_path, &linked_runtime)
            .await
            .expect_err("pre-existing destination symlink is refused");
        assert!(
            matches!(linked_error, SttError::InvalidArgument(message) if message.contains("symbolic link"))
        );
        assert!(
            !outside.path().join("whisper-cli").exists(),
            "extraction must not follow a destination symlink"
        );
    }
}

/// A verified archive with no whisper executable is an honest
/// runtime-missing state, not a silent success.
#[tokio::test]
async fn archive_without_runtime_executable_is_typed_runtime_missing() {
    let whisper = tempfile::tempdir().expect("whisper dir");
    let zip = build_stored_zip(&[("Release/README.txt", b"no binary here".as_slice())]);
    let zip_path = whisper.path().join("whisper-bin-x64.zip");
    std::fs::write(&zip_path, zip).expect("write zip");
    let error = extract_runtime_zip(&zip_path, &runtime_directory(whisper.path()))
        .await
        .expect_err("no executable");
    assert!(matches!(error, SttError::RuntimeMissing { .. }));
}

/// Archive format is selected from bytes, never the filename or the host's
/// `tar` implementation: ZIP is accepted, POSIX tar is diagnosed distinctly,
/// and arbitrary bytes receive a typed format error.
#[tokio::test]
async fn runtime_archive_format_detection_is_magic_byte_driven() {
    let root = tempfile::tempdir().expect("archive root");
    let runtime_entry = format!("Release/{}", runtime_executable_names()[0]);

    let zip_path = root.path().join("runtime.data");
    std::fs::write(
        &zip_path,
        build_stored_zip(&[(runtime_entry.as_str(), b"zip bytes".as_slice())]),
    )
    .expect("write extensionless zip");
    let zip_runtime = root.path().join("zip-runtime");
    extract_runtime_zip(&zip_path, &zip_runtime)
        .await
        .expect("ZIP magic is accepted without a zip extension");
    assert_eq!(
        std::fs::read(zip_runtime.join(&runtime_entry)).expect("read ZIP payload"),
        b"zip bytes"
    );

    let mut tar = vec![0_u8; 512];
    tar[257..262].copy_from_slice(b"ustar");
    let tar_path = root.path().join("looks-like.zip");
    std::fs::write(&tar_path, tar).expect("write tar magic");
    let tar_error = extract_runtime_zip(&tar_path, &root.path().join("tar-runtime"))
        .await
        .expect_err("tar is not passed to the ZIP reader");
    assert!(
        matches!(tar_error, SttError::InvalidArgument(message) if message.contains("archive is tar"))
    );

    let garbage_path = root.path().join("garbage.zip");
    std::fs::write(&garbage_path, b"not an archive").expect("write garbage");
    let garbage_error = extract_runtime_zip(&garbage_path, &root.path().join("garbage-runtime"))
        .await
        .expect_err("garbage is rejected before a reader is selected");
    assert!(
        matches!(garbage_error, SttError::InvalidArgument(message) if message.contains("not recognized"))
    );
}
