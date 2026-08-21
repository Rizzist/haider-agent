//! v0.0.938 stale-endpoint hygiene: a daemon killed without running its
//! cleanup (SIGKILL, panic, power loss) leaves its socket node behind, and in
//! a shared runtime directory those accumulate without bound — 1259 were
//! observed on one development machine. A stale node is indistinguishable
//! from a live one by name, mode, or mtime; ONLY a connect tells them apart,
//! which is exactly what the sweep proves before unlinking.

#![allow(clippy::expect_used)]

#[cfg(unix)]
mod unix {
    use haider_platform::{BoundEndpoint, Endpoint, sweep_stale_endpoints};

    /// A SHORT runtime root. `TMPDIR` on macOS is ~60 characters before the
    /// endpoint name, which overruns `sun_path` (~104 bytes) — the very limit
    /// the endpoint naming scheme exists to respect. Removed on drop.
    struct ShortRoot(std::path::PathBuf);

    impl ShortRoot {
        fn new(tag: &str) -> Self {
            let path = std::path::PathBuf::from(format!("/tmp/hsw-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create short runtime root");
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for ShortRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// MUTATION CHECK (executed): zero the sweep budget so nothing is ever
    /// removed — the removal assertion fails. That is the falsifiable half.
    ///
    /// The live-endpoint assertion below is deliberately NOT claimed as
    /// mutation-killed, because it is not falsifiable by a realistic
    /// mutation and saying otherwise would be a lie a future reader would
    /// trust. Live protection here is STRUCTURAL, not a runtime guard: the
    /// removal branch matches only `Err(ConnectionRefused)`, and a live
    /// endpoint probes `Ok`, so it can never reach that branch at all.
    /// Verified by isolation — disabling the refused-only gate AND the
    /// claim-time re-probe inside `remove_verified_stale` together still
    /// leaves a live endpoint untouched. The assertion is kept as a
    /// regression tripwire for a future refactor that makes the branch
    /// reachable from a successful probe.
    #[tokio::test]
    async fn the_sweep_removes_dead_nodes_and_never_a_live_one() {
        let root = ShortRoot::new("sweep");
        let runtime_dir = root.path().to_path_buf();

        // A LIVE endpoint: bound and still held for the whole test.
        let live_endpoint = Endpoint::new(&runtime_dir, "profile-live");
        let live = BoundEndpoint::bind(&live_endpoint, &runtime_dir)
            .await
            .expect("live endpoint binds");
        let live_path = live.path().to_path_buf();

        // A DEAD endpoint: bind it, then drop the listener WITHOUT cleanup so
        // the node survives its owner exactly as a SIGKILL leaves it.
        let dead_endpoint = Endpoint::new(&runtime_dir, "profile-dead");
        let mut dead = BoundEndpoint::bind(&dead_endpoint, &runtime_dir)
            .await
            .expect("dead endpoint binds");
        let dead_path = dead.path().to_path_buf();
        // Close the LISTENER so connects are refused (the daemon is gone),
        // then forget the guard so the filesystem node survives — exactly the
        // state a SIGKILL leaves. Dropping normally would clean up and there
        // would be nothing to sweep.
        dead.close_listener();
        std::mem::forget(dead);
        assert!(dead_path.exists(), "the node outlives its owner");

        // A node that is NOT an endpoint name must never be touched.
        let bystander = runtime_dir.join("unrelated-file.txt");
        std::fs::write(&bystander, b"not ours").expect("write bystander");

        let removed = sweep_stale_endpoints(&runtime_dir, Some(&live_path)).await;

        assert_eq!(removed, 1, "exactly the dead node was removed");
        assert!(!dead_path.exists(), "the dead node is gone");
        assert!(
            live_path.exists(),
            "a LIVE endpoint is never removed — only a refused connect proves death"
        );
        assert!(bystander.exists(), "non-endpoint names are left alone");

        // And the live endpoint still accepts after the sweep.
        haider_platform::connect(&live_path)
            .await
            .expect("the live endpoint still serves");
    }

    /// Sweeping a directory with nothing to do is a no-op, and passing no
    /// `keep` never removes a live node either (liveness, not the keep list,
    /// is what protects it).
    #[tokio::test]
    async fn liveness_not_the_keep_list_protects_a_live_endpoint() {
        let root = ShortRoot::new("keep");
        let runtime_dir = root.path().to_path_buf();
        assert_eq!(sweep_stale_endpoints(&runtime_dir, None).await, 0);

        let endpoint = Endpoint::new(&runtime_dir, "profile-unkept");
        let live = BoundEndpoint::bind(&endpoint, &runtime_dir)
            .await
            .expect("binds");
        let path = live.path().to_path_buf();
        assert_eq!(
            sweep_stale_endpoints(&runtime_dir, None).await,
            0,
            "no keep list, still untouched: the connect is the proof"
        );
        assert!(path.exists());
    }
}
