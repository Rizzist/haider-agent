//! Exercise every publication entry point with sandbox-denied hard links.
#![allow(clippy::expect_used)]
use super::*;
use std::sync::Arc;

fn with_link_hook<T>(hook: CasLinkTestHook, action: impl FnOnce() -> T) -> T {
    struct Restore(Option<CasLinkTestHook>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CAS_LINK_TEST_HOOK.with(|slot| slot.replace(self.0.take()));
        }
    }
    let _restore = Restore(CAS_LINK_TEST_HOOK.with(|slot| slot.replace(Some(hook))));
    action()
}

fn put(cas: &FileCas, route: usize, bytes: &[u8]) -> StoreResult<ArtifactRef> {
    match route {
        0 => cas.put(bytes),
        1 => cas.put_reader(Cursor::new(bytes), Path::new("publication-test-reader")),
        2 => {
            let mut upload = cas.begin_put(bytes.len() as u64)?;
            upload.write_chunk(bytes)?;
            upload.finish(&artifact_for(bytes))
        }
        3 => {
            let artifact =
                cas.put_batched_stream(&artifact_for(bytes), bytes.len() as u64, |sink| {
                    sink.write_all(bytes)
                })?;
            cas.finish_ordered_batched_puts()?;
            Ok(artifact)
        }
        _ => unreachable!("four publication routes"),
    }
}

#[test]
fn denied_links_publish_and_deduplicate_all_cas_routes() {
    for errno in [1, 13] {
        // EPERM and EACCES, as returned by Android sandbox policy.
        for route in 0..4 {
            let root = tempfile::tempdir().expect("CAS root");
            let cas = FileCas::open(root.path()).expect("open CAS");
            let calls = Arc::new(AtomicU64::new(0));
            let observed = Arc::clone(&calls);
            with_link_hook(
                Box::new(move |_, _| {
                    observed.fetch_add(1, Ordering::Relaxed);
                    Err(std::io::Error::from_raw_os_error(errno))
                }),
                || {
                    let bytes = b"complete immutable sandbox object";
                    let artifact = put(&cas, route, bytes).expect("publish via rename");
                    let path = cas.path_for(&artifact).expect("object path");
                    let original = fs::metadata(&path).expect("published metadata");
                    assert_eq!(put(&cas, route, bytes).expect("deduplicate"), artifact);
                    use std::os::unix::fs::MetadataExt as _;
                    assert_eq!(
                        fs::metadata(path).expect("dedup metadata").ino(),
                        original.ino()
                    );
                    assert_eq!(cas.get(&artifact).expect("read"), bytes);
                    assert_eq!(calls.load(Ordering::Relaxed), 1);
                },
            );
        }
    }
}

#[test]
fn denied_link_concurrent_publishers_never_expose_partial_bytes() {
    let root = tempfile::tempdir().expect("CAS root");
    let cas = FileCas::open(root.path()).expect("open CAS");
    let bytes = vec![0x5a; 256 * 1024];
    let expected = artifact_for(&bytes);
    let path = cas.path_for(&expected).expect("object path");
    let ready = Arc::new(std::sync::Barrier::new(9));
    let release = Arc::new(std::sync::Barrier::new(9));
    std::thread::scope(|scope| {
        let publishers = (0..8)
            .map(|index| {
                let ready = Arc::clone(&ready);
                let release = Arc::clone(&release);
                let cas = &cas;
                let bytes = &bytes;
                scope.spawn(move || {
                    with_link_hook(
                        Box::new(move |_, _| {
                            ready.wait();
                            release.wait();
                            Err(std::io::Error::from_raw_os_error(13))
                        }),
                        || put(cas, index % 4, bytes).expect("concurrent publication"),
                    )
                })
            })
            .collect::<Vec<_>>();
        ready.wait();
        assert!(!path.exists(), "staged bytes are not public");
        release.wait();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while publishers.iter().any(|thread| !thread.is_finished()) {
            match fs::read(&path) {
                Ok(actual) => assert_eq!(actual, bytes),
                Err(error) => assert_eq!(error.kind(), ErrorKind::NotFound),
            }
            assert!(std::time::Instant::now() < deadline, "publication stalled");
            std::thread::yield_now();
        }
        for publisher in publishers {
            assert_eq!(publisher.join().expect("publisher thread"), expected);
        }
    });
    assert_eq!(cas.get(&expected).expect("complete winner"), bytes);
}

#[test]
fn denied_link_rename_never_replaces_a_corrupt_racing_winner() {
    for route in 0..4 {
        let root = tempfile::tempdir().expect("CAS root");
        let cas = FileCas::open(root.path()).expect("open CAS");
        let bytes = b"expected complete bytes";
        let result = with_link_hook(
            Box::new(|_, target| {
                fs::write(target, b"corrupt racing winner").expect("seed concurrent object");
                Err(std::io::Error::from_raw_os_error(13))
            }),
            || put(&cas, route, bytes),
        );
        assert_eq!(
            result.expect_err("corruption fails closed").code,
            ErrorCode::StoreCorrupt
        );
        let path = cas.path_for(&artifact_for(bytes)).expect("object path");
        assert_eq!(
            fs::read(path).expect("winner retained"),
            b"corrupt racing winner"
        );
    }
}

#[test]
fn crash_before_rename_replays_all_cas_publication_routes() {
    const ROOT: &str = "HAIDER_CAS_CRASH_ROOT";
    const ROUTE: &str = "HAIDER_CAS_CRASH_ROUTE";
    let bytes = b"durable staged bytes before process death";
    if let Some(root) = std::env::var_os(ROOT) {
        let route = std::env::var(ROUTE)
            .expect("child route")
            .parse()
            .expect("route number");
        let cas = FileCas::open(root).expect("child CAS");
        with_link_hook(
            Box::new(|source, target| {
                assert!(source.exists());
                assert!(!target.exists());
                // Real process death skips every staging Drop and directory cleanup.
                std::process::exit(73);
            }),
            || put(&cas, route, bytes).expect("child must exit before publication"),
        );
        unreachable!("child must exit in publication hook");
    }
    for route in 0..4 {
        let root = tempfile::tempdir().expect("CAS root");
        let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", "cas::cas_publication_tests::crash_before_rename_replays_all_cas_publication_routes", "--nocapture"])
            .env(ROOT, root.path()).env(ROUTE, route.to_string())
            .status().expect("run crash child");
        assert_eq!(status.code(), Some(73));
        let cas = FileCas::open(root.path()).expect("reopen after crash");
        let artifact = artifact_for(bytes);
        let path = cas.path_for(&artifact).expect("object path");
        assert!(
            !path.exists(),
            "unacknowledged staging is invisible after restart"
        );
        let staging = if matches!(route, 1 | 2) {
            cas.root.clone()
        } else {
            path.parent().expect("shard").to_path_buf()
        };
        assert!(
            fs::read_dir(staging)
                .expect("staging directory")
                .any(|entry| {
                    entry
                        .expect("staging entry")
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".tmp-")
                }),
            "crash really left an orphaned temporary"
        );
        with_link_hook(
            Box::new(|_, _| Err(std::io::Error::from_raw_os_error(13))),
            || {
                assert_eq!(
                    put(&cas, route, bytes).expect("replay after crash"),
                    artifact
                );
            },
        );
        assert_eq!(cas.get(&artifact).expect("replayed bytes"), bytes);
    }
}
