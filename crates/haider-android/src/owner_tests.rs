#![allow(clippy::expect_used)]
use super::*;
use haider_daemon::{DaemonConfig, DaemonDependencies, VaultProvision};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Instant;

struct Context(Arc<AtomicUsize>);
impl Drop for Context {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    paths: Paths,
    host: Arc<Host<Context>>,
    drops: Arc<AtomicUsize>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.host.release(Duration::from_secs(15));
    }
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("c1")
            .tempdir_in("/tmp")
            .expect("fixture");
        let root = directory.path().canonicalize().expect("canonical fixture");
        let store = root.join("haider/profiles/default");
        let runtime = root.join("haider/runtime/android-default");
        let paths = Paths {
            profile_id: "android-default".into(),
            workspace_dir: store.join("workspace"),
            tmp_dir: runtime.join("tmp"),
            logs_dir: root.join("haider/logs"),
            store_dir: store,
            runtime_dir: runtime,
        };
        for path in [&paths.workspace_dir, &paths.tmp_dir, &paths.logs_dir] {
            std::fs::create_dir_all(path).expect("private fixture directories");
        }
        Self {
            _directory: directory,
            paths,
            host: Arc::new(Host::new()),
            drops: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn initialize(&self) {
        self.host
            .initialize(self.paths.clone(), |previous| {
                assert!(previous.is_none());
                Ok(Some(Context(Arc::clone(&self.drops))))
            })
            .expect("initialize");
    }

    fn daemon(paths: &Paths) -> DaemonTask {
        // No provider turn or credential discovery is performed in these tests.
        let mut config = DaemonConfig::new(&paths.profile_id, &paths.store_dir, &paths.runtime_dir);
        config.discovery_disabled = true;
        config.store_synchronous = Some(haider_protocol::runtime::StoreSynchronous::Normal);
        // The legacy process-global manager needs a root retained across cases.
        static LOCKDOWN: OnceLock<tempfile::TempDir> = OnceLock::new();
        config.lockdown_root_override = Some(
            LOCKDOWN
                .get_or_init(|| {
                    tempfile::Builder::new()
                        .prefix("c1-lockdown")
                        .tempdir_in("/tmp")
                        .expect("lockdown fixture")
                })
                .path()
                .to_path_buf(),
        );
        let mut dependencies = DaemonDependencies::default();
        dependencies.accounts.vault = VaultProvision::Unsupported;
        haider_daemon::spawn_with_dependencies(config, dependencies)
    }

    fn ready(&self) -> u64 {
        let until = Instant::now() + Duration::from_secs(15);
        loop {
            let observation = self.host.observe();
            if observation.phase == "Ready" {
                assert!(observation.daemon_generation > 0);
                assert_eq!(
                    observation.endpoint_path,
                    Some(self.paths.runtime_dir.join("h.sock"))
                );
                return observation.daemon_generation;
            }
            assert_eq!(observation.error_code, None);
            assert!(Instant::now() < until, "Ready deadline: {observation:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[test]
fn in_process_generations_close_all_descriptors() {
    use std::os::unix::fs::MetadataExt as _;

    // Descriptor counts are process-wide; isolate this probe from parallel tests.
    const CHILD: &str = "HAIDER_ANDROID_FD_PROBE_CHILD";
    const TEST: &str = "owner::tests::in_process_generations_close_all_descriptors";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .output()
            .expect("isolated descriptor probe");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("test result: ok. 1 passed"),
            "descriptor probe failed\n{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        print!("{stdout}");
        return;
    }

    fn descriptors() -> usize {
        std::fs::read_dir("/dev/fd")
            .expect("Unix descriptor directory")
            .inspect(|entry| {
                entry.as_ref().expect("descriptor entry");
            })
            .count()
    }

    let fixture = Fixture::new();
    // This file belongs to the caller and must survive every cleanup path.
    let before_caller = descriptors();
    let caller_file = std::fs::File::create(fixture.paths.logs_dir.join("caller-owned-fd"))
        .expect("caller-owned file");
    let caller_inode = caller_file.metadata().expect("caller file identity").ino();
    assert_eq!(descriptors(), before_caller + 1);
    // Exercise the lazy diagnostic writer as well as the native lifecycle log.
    std::fs::write(
        fixture.paths.store_dir.join("effect-diagnostics.jsonl"),
        b"",
    )
    .expect("synthetic diagnostic journal");
    let mut baseline = None;
    // The first generation initializes process-lifetime platform facilities.
    // Every subsequent generation must return to that exact descriptor count.
    for generation in 1..=13 {
        fixture.initialize();
        fixture
            .host
            .start(|owner, stop, paths| {
                run_daemon(owner, stop, async { Ok(Fixture::daemon(paths)) }, paths)
            })
            .expect("accepted generation");
        assert_eq!(fixture.ready(), generation);
        let forced = generation % 2 == 0;
        assert_eq!(
            fixture.host.shutdown(forced, Duration::from_secs(15)),
            if forced {
                Status::ShutdownForced
            } else {
                Status::Ok
            }
        );
        assert!(fixture.host.release(Duration::ZERO));
        assert_eq!(fixture.drops.load(Ordering::SeqCst), generation as usize);
        assert!(!fixture.paths.runtime_dir.join("h.sock").exists());
        assert_eq!(
            caller_file
                .metadata()
                .expect("caller file stayed open")
                .ino(),
            caller_inode
        );
        let count = descriptors();
        println!("generation={generation} forced={forced} stopped_fds={count}");
        let expected = *baseline.get_or_insert(count);
        assert_eq!(
            count, expected,
            "descriptor leak at generation {generation}"
        );
    }

    // Failed bootstrap must also destroy the runtime and close its lifecycle log.
    fixture.initialize();
    fixture
        .host
        .start(|owner, stop, paths| {
            run_daemon(
                owner,
                stop,
                async { Err(Status::StoreRecoveryFailed) },
                paths,
            )
        })
        .expect("accepted failing bootstrap");
    assert_eq!(
        fixture.host.shutdown(false, Duration::from_secs(15)),
        Status::StoreRecoveryFailed
    );
    assert!(fixture.host.release(Duration::ZERO));
    assert_eq!(descriptors(), baseline.expect("warm baseline"));

    // The daemon's abrupt in-process crash path must close the same resources.
    for _ in 0..4 {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(4)
            .thread_stack_size(8 * 1024 * 1024)
            .enable_all()
            .build()
            .expect("crash runtime");
        runtime.block_on(async {
            let task = Fixture::daemon(&fixture.paths);
            let mut readiness = task.readiness();
            tokio::time::timeout(Duration::from_secs(15), async {
                while !readiness.snapshot().ready {
                    assert!(readiness.changed().await.is_some(), "startup failed");
                }
            })
            .await
            .expect("crash generation Ready");
            task.crash().await;
        });
        drop(runtime);
        assert_eq!(
            descriptors(),
            baseline.expect("warm baseline"),
            "crash leaked"
        );
    }

    // A real startup failure after store open must close its partial generation.
    // The obstruction belongs to the caller and must survive daemon cleanup.
    std::fs::create_dir_all(&fixture.paths.runtime_dir).expect("runtime obstruction parent");
    let obstruction = fixture.paths.runtime_dir.join("h.sock");
    std::fs::write(&obstruction, b"caller-owned obstruction").expect("runtime obstruction");
    fixture.initialize();
    fixture
        .host
        .start(|owner, stop, paths| {
            run_daemon(owner, stop, async { Ok(Fixture::daemon(paths)) }, paths)
        })
        .expect("accepted obstructed startup");
    let until = Instant::now() + Duration::from_secs(15);
    while fixture.host.observe().phase != "Failed" {
        assert!(Instant::now() < until, "obstructed startup did not fail");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        fixture.host.shutdown(false, Duration::from_secs(15)),
        Status::Internal
    );
    assert!(fixture.host.release(Duration::ZERO));
    assert_eq!(
        descriptors(),
        baseline.expect("warm baseline"),
        "startup failure leaked"
    );
    assert_eq!(
        std::fs::read(&obstruction).expect("preserved obstruction"),
        b"caller-owned obstruction"
    );

    // The failure/crash paths must also preserve this unrelated caller's handle.
    assert_eq!(
        caller_file
            .metadata()
            .expect("caller file stayed open")
            .ino(),
        caller_inode
    );
    drop(caller_file);
    assert_eq!(descriptors(), baseline.expect("warm baseline") - 1);
}

#[test]
fn shared_owner_idempotent_init_start_stop_release_and_path_identity() {
    let fixture = Fixture::new();
    assert_eq!(fixture.host.observe().phase, "Stopped");
    assert_eq!(
        fixture.host.start(|_, _, _| panic!("not initialized")),
        Err(Status::NotInitialized)
    );
    fixture.initialize();
    fixture
        .host
        .initialize(fixture.paths.clone(), |previous| {
            assert!(Arc::ptr_eq(
                &previous.expect("same context").0,
                &fixture.drops
            ));
            Ok(None)
        })
        .expect("idempotent initialize");
    for expected in [1, 2] {
        fixture
            .host
            .start(|owner, stop, paths| {
                run_daemon(owner, stop, async { Ok(Fixture::daemon(paths)) }, paths)
            })
            .expect("accepted");
        assert!(fixture.host.is_running());
        assert_eq!(
            fixture.host.start(|_, _, _| panic!("double start")),
            Err(Status::AlreadyRunning)
        );
        assert_eq!(fixture.ready(), expected);
        assert_eq!(
            fixture.host.shutdown(false, Duration::from_secs(15)),
            Status::Ok
        );
        assert_eq!(fixture.host.shutdown(false, Duration::ZERO), Status::Ok);
        assert!(fixture.host.release(Duration::ZERO));
        assert!(fixture.host.release(Duration::ZERO));
        assert_eq!(fixture.host.observe().phase, "Stopped");
        assert_eq!(fixture.host.observe().daemon_generation, expected);
        assert_eq!(fixture.drops.load(Ordering::SeqCst), expected as usize);
        if expected == 1 {
            fixture.initialize();
        }
    }
    let mut other = fixture.paths.clone();
    other.profile_id = "other-profile".into();
    assert_eq!(
        fixture
            .host
            .initialize(other, |_| panic!("profile cannot switch")),
        Err(Status::BadPaths)
    );
}

#[test]
fn shared_owner_timeout_refuses_restart_and_release_until_real_runtime_quiesces() {
    let fixture = Fixture::new();
    fixture.initialize();
    let (release_reader, blocked_reader) = mpsc::channel::<()>();
    let (reader_entered, entered) = mpsc::channel();
    fixture
        .host
        .start(move |owner, stop, paths| {
            run_daemon(
                owner,
                stop,
                async {
                    tokio::task::spawn_blocking(move || {
                        reader_entered.send(()).expect("reader entered");
                        // Dropping the sender on an assertion failure also releases it.
                        let _ = blocked_reader.recv();
                    });
                    Ok(Fixture::daemon(paths))
                },
                paths,
            )
        })
        .expect("accepted");
    entered
        .recv_timeout(Duration::from_secs(15))
        .expect("actual blocking pool reader");
    let generation = fixture.ready();
    assert_eq!(
        fixture.host.shutdown(true, Duration::ZERO),
        Status::ShutdownTimeout
    );
    assert_eq!(fixture.host.observe().error_code, Some("SHUTDOWN_TIMEOUT"));
    assert!(!fixture.host.release(Duration::ZERO));
    assert_eq!(
        fixture
            .host
            .start(|_, _, _| panic!("reader still owns runtime")),
        Err(Status::AlreadyRunning)
    );
    fixture
        .host
        .with_context(|context| {
            assert!(Arc::ptr_eq(&context.0, &fixture.drops));
            Ok(())
        })
        .expect("application context retained");
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 0);
    release_reader.send(()).expect("allow quiescence");
    // A later graceful request cannot downgrade the existing forced request.
    assert_eq!(
        fixture.host.shutdown(false, Duration::from_secs(15)),
        Status::ShutdownForced
    );
    assert_eq!(fixture.host.observe().phase, "Stopped");
    assert_eq!(fixture.host.observe().daemon_generation, generation);
    assert!(fixture.host.release(Duration::ZERO));
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 1);
    assert!(!fixture.paths.runtime_dir.join("h.sock").exists());
    fixture.initialize();
    fixture
        .host
        .start(|owner, stop, paths| {
            run_daemon(owner, stop, async { Ok(Fixture::daemon(paths)) }, paths)
        })
        .expect("restart only after quiescence");
    assert!(fixture.ready() > generation);
    assert_eq!(
        fixture.host.shutdown(false, Duration::from_secs(15)),
        Status::Ok
    );
    assert!(fixture.host.release(Duration::ZERO));
}

#[test]
fn shared_owner_catches_panic_after_unwinding_blocking_readers_without_publishing_payload() {
    let fixture = Fixture::new();
    fixture.initialize();
    let (release_reader, blocked_reader) = mpsc::channel::<()>();
    let (reader_entered, entered) = mpsc::channel();
    fixture
        .host
        .start(move |owner, stop, paths| {
            run_daemon(
                owner,
                stop,
                async {
                    tokio::task::spawn_blocking(move || {
                        reader_entered.send(()).expect("reader entered");
                        let _ = blocked_reader.recv();
                    });
                    panic!("synthetic-private-panic-payload");
                },
                paths,
            )
        })
        .expect("accepted");
    entered
        .recv_timeout(Duration::from_secs(15))
        .expect("actual blocking reader");
    fixture.host.fail_boundary();
    assert_eq!(fixture.host.observe().error_code, Some("INTERNAL"));
    assert!(!fixture.host.release(Duration::ZERO));
    assert_eq!(
        fixture
            .host
            .start(|_, _, _| panic!("unwinding runtime is still live")),
        Err(Status::AlreadyRunning)
    );
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 0);
    release_reader.send(()).expect("allow unwind teardown");
    assert_eq!(
        fixture.host.shutdown(true, Duration::from_secs(15)),
        Status::Internal
    );
    let observation = fixture.host.observe();
    assert_eq!(observation.error_code, Some("INTERNAL"));
    assert!(
        !serde_json::to_string(&observation)
            .expect("snapshot")
            .contains("synthetic-private")
    );
    assert!(fixture.host.release(Duration::ZERO));
    assert_eq!(fixture.drops.load(Ordering::SeqCst), 1);
}
