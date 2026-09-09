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
