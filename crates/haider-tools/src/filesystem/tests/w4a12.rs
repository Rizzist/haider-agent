use super::*;
use std::os::unix::fs::MetadataExt;

/// MUTATION CHECK: remove the before/after content-metadata comparison.
/// Expected failure: the torn `a…a/c…c` mix is returned even though it was
/// never a completed file state. Verified by revert in W4a1.2.
#[test]
fn mid_read_in_place_edit_never_false_passes_a_torn_content_hash() {
    const FIRST_READ_BYTES: usize = 64 * 1024;

    let directory = tempfile::tempdir().expect("temporary directory");
    let target = directory.path().join("target.txt");
    let prefix_before = vec![b'a'; FIRST_READ_BYTES];
    let suffix_before = vec![b'b'; FIRST_READ_BYTES];
    let prefix_after = vec![b'd'; FIRST_READ_BYTES];
    let suffix_after = vec![b'c'; FIRST_READ_BYTES];
    let initial = [prefix_before.as_slice(), suffix_before.as_slice()].concat();
    let final_bytes = [prefix_after.as_slice(), suffix_after.as_slice()].concat();
    let torn_mix = [prefix_before.as_slice(), suffix_after.as_slice()].concat();
    fs::write(&target, initial).expect("seed target");
    let initial_inode = fs::metadata(&target).expect("initial metadata").ino();
    let mut source = fs::File::open(&target).expect("open target");
    let mut edited = false;

    let snapshot =
        metadata_guarded_file_snapshot_with_reader(&mut source, &target, |source, buffer| {
            let prefix_read = source.read_at(&mut buffer[..FIRST_READ_BYTES], 0)?;
            if !edited {
                fs::write(&target, &final_bytes).expect("rewrite target in place");
                assert_eq!(
                    fs::metadata(&target).expect("rewritten metadata").ino(),
                    initial_inode,
                    "reproduction must preserve the target inode"
                );
                edited = true;
            }
            let suffix_read = source.read_at(
                &mut buffer[prefix_read..],
                u64::try_from(prefix_read).expect("prefix offset"),
            )?;
            Ok(prefix_read + suffix_read)
        })
        .expect("retry after the observable metadata change");

    assert_ne!(
        blake3::hash(&snapshot),
        blake3::hash(&torn_mix),
        "a torn hash must never false-pass"
    );
    assert_eq!(snapshot, final_bytes, "retry returns one coherent state");
    assert_eq!(fs::read(&target).expect("read final target"), final_bytes);
}

/// MUTATION CHECK: remove the final anchored identity check after content
/// verification. Expected failure: the edit replaces the editor's newly
/// renamed inode. Verified by revert in W4a1.2.
#[test]
fn leaf_replacement_after_content_verify_is_typed_path_change() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace_path = fs::canonicalize(directory.path()).expect("canonical workspace");
    let target = workspace_path.join("target.txt");
    let parked = workspace_path.join("parked.txt");
    fs::write(&target, "before").expect("seed target");
    let workspace = rustix::fs::open(
        &workspace_path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("open workspace");
    let operation = FsEdit::new(&target, "before", "haider");
    let expected = mutation_digest(b"before");

    let result = apply_edit_at_with_commit_hooks(
        workspace,
        Path::new("target.txt"),
        &operation,
        Some(&expected),
        || {},
        || {
            fs::rename(&target, &parked).expect("park verified target");
            fs::write(&target, "editor").expect("install editor replacement");
        },
    );

    assert!(matches!(result, Err(ToolError::PathChanged { .. })));
    assert_eq!(
        fs::read_to_string(&target).expect("read editor target"),
        "editor"
    );
    assert_eq!(
        fs::read_to_string(&parked).expect("read parked target"),
        "before"
    );
    assert!(
        fs::read_dir(&workspace_path)
            .expect("read workspace")
            .all(|entry| !entry
                .expect("directory entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".haider-patch-"))
    );
}

/// MUTATION CHECK (layered): remove the exclusive `source.lock()` in
/// `open_locked_current_at`. Expected failure: the second patch finishes while
/// the first is paused, so the serialization assertion fails; the later
/// identity recheck still makes the first refuse in this choreography.
/// Verified by revert in W4a1.2.
#[test]
fn cooperating_edits_serialize_across_verify_and_rename() {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    let directory = tempfile::tempdir().expect("temporary directory");
    let workspace_path = fs::canonicalize(directory.path()).expect("canonical workspace");
    let target = workspace_path.join("target.txt");
    fs::write(&target, "before").expect("seed target");
    let first_workspace = rustix::fs::open(
        &workspace_path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("open first workspace");
    let second_workspace = rustix::fs::open(
        &workspace_path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .expect("open second workspace");
    let first_operation = FsEdit::new(&target, "before", "first");
    let second_operation = FsEdit::new(&target, "before", "second");
    let expected = mutation_digest(b"before");
    let first_expected = expected.clone();
    let (first_at_commit_sender, first_at_commit_receiver) = mpsc::channel();
    let (release_first_sender, release_first_receiver) = mpsc::channel();

    let first = thread::spawn(move || {
        apply_edit_at_with_commit_hooks(
            first_workspace,
            Path::new("target.txt"),
            &first_operation,
            Some(&first_expected),
            || {},
            || {
                first_at_commit_sender
                    .send(())
                    .expect("signal first verified");
                release_first_receiver.recv().expect("release first rename");
            },
        )
    });
    first_at_commit_receiver
        .recv()
        .expect("first patch reaches final locked span");

    let (second_started_sender, second_started_receiver) = mpsc::channel();
    let (second_result_sender, second_result_receiver) = mpsc::channel();
    let second = thread::spawn(move || {
        second_started_sender
            .send(())
            .expect("signal second started");
        let result = apply_edit_at(
            second_workspace,
            Path::new("target.txt"),
            &second_operation,
            Some(&expected),
        );
        second_result_sender
            .send(result)
            .expect("send second result");
    });
    second_started_receiver.recv().expect("second patch starts");
    let early_second = second_result_receiver
        .recv_timeout(Duration::from_millis(500))
        .ok();
    let second_finished_early = early_second.is_some();
    release_first_sender
        .send(())
        .expect("allow first patch to rename");
    let first_result = first.join().expect("join first patch");
    let second_result = match early_second {
        Some(result) => result,
        None => second_result_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("second patch finishes after first unlocks"),
    };
    second.join().expect("join second patch");

    assert!(
        !second_finished_early,
        "the cooperating second edit must wait across verify→rename"
    );
    assert!(first_result.is_ok(), "first patch must apply");
    assert!(
        matches!(second_result, Err(ToolError::StaleRead { .. })),
        "second edit must re-read the winner before deciding"
    );
    assert_eq!(
        fs::read_to_string(&target).expect("read serialized target"),
        "first"
    );
}
