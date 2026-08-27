#![allow(clippy::expect_used)]
//! v0.0.938 stale-endpoint hygiene: a daemon killed without running its
//! cleanup (SIGKILL, panic, power loss) leaves its socket node behind. A stale
//! node is indistinguishable
//! from a live one by name, mode, or mtime; ONLY a connect tells them apart,
//! which is exactly what the sweep proves before unlinking.

#[cfg(unix)]
mod unix {
    use haider_platform::{BoundEndpoint, Endpoint, sweep_stale_endpoints};

    /// A SHORT runtime root. `TMPDIR` on macOS is ~60 characters before the
    /// endpoint name, which overruns `sun_path` (~104 bytes) — the very limit
    /// the endpoint naming scheme exists to respect. Removed on drop.
    struct ShortRoot(std::path::PathBuf);

    impl ShortRoot {
        fn new(tag: &str) -> Self {
            use std::os::unix::fs::DirBuilderExt as _;

            let root = std::path::PathBuf::from(format!("/tmp/hsw-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(&root).expect("create short runtime root");
            Self(root.join("profile"))
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for ShortRoot {
        fn drop(&mut self) {
            if let Some(root) = self.0.parent() {
                let _ = std::fs::remove_dir_all(root);
            }
        }
    }

    /// MUTATION CHECK (executed): zero the sweep budget so nothing is ever
    /// removed — the removal assertion fails. That is the falsifiable half.
    ///
    #[tokio::test]
    async fn the_sweep_removes_the_dead_profile_endpoint_only() {
        let root = ShortRoot::new("sweep");
        let runtime_dir = root.path().to_path_buf();

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

        let removed = sweep_stale_endpoints(&runtime_dir, None).await;

        assert_eq!(removed, 1, "exactly the dead node was removed");
        assert!(!dead_path.exists(), "the dead node is gone");
        assert!(bystander.exists(), "non-endpoint names are left alone");
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
