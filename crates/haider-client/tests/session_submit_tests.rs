#![cfg(unix)]
#![allow(clippy::expect_used)]
//! Ordinary continuation against the actual candidate daemon and durable journal.

use std::collections::BTreeSet;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use haider_client::{
    ClientConfig, DaemonLifetime, EnsureOptions, HeadlessEventMode, HeadlessOutcome,
    HeadlessRunRequest, ProfileEnv, connect, resolve_profile, run_headless,
    submit_headless_with_event_mode_and_interrupts,
};
use haider_rpc::{RequestBody, ResponseBody, SeqRange};
use tokio::sync::mpsc;

struct OwnedDaemon(Child);

impl Drop for OwnedDaemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn three_turns_keep_native_session_and_journal_with_distinct_runs() {
    assert_eq!(
        std::env::var("HAIDER_TEST_SIBLINGS_PREBUILT").as_deref(),
        Ok("1"),
        "prebuild haider-daemond before running this real-daemon integration test"
    );
    let executable = std::env::current_exe().expect("test executable");
    let daemon_path = executable
        .parent()
        .expect("deps")
        .parent()
        .expect("target profile")
        .join("haiderd");
    assert!(
        daemon_path.is_file(),
        "missing candidate daemon: {}",
        daemon_path.display()
    );
    let root = tempfile::Builder::new()
        .prefix("hsubmit")
        .tempdir_in("/tmp")
        .expect("short isolated root");
    let home = root.path().join("home");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&home).expect("isolated home");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let profile = resolve_profile(&ProfileEnv {
        profile_dir: Some(root.path().join("profile")),
        home: Some(home.clone()),
        user_profile: None,
        model: None,
        runtime_dir: None,
        xdg_runtime_dir: None,
    })
    .expect("profile");
    let stderr_path = root.path().join("daemon.stderr");
    let script = serde_json::json!([
        {"step":"emit_text","text":"first reply"}, {"step":"finish","reason":"end_turn"},
        {"step":"emit_text","text":"second reply"}, {"step":"finish","reason":"end_turn"},
        {"step":"emit_text","text":"third reply"}, {"step":"finish","reason":"end_turn"}
    ]);
    let mut daemon = OwnedDaemon(
        Command::new(&daemon_path)
            .current_dir(&workspace)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("HAIDER_PROFILE_DIR", &profile.store_dir)
            .env("HAIDER_DISCOVERY_DISABLED", "1")
            .env("HAIDER_TEST_FAKE_PROVIDER", script.to_string())
            .env_remove("HAIDER_MODEL")
            .env_remove("HAIDER_RUNTIME_DIR")
            .env_remove("XDG_RUNTIME_DIR")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&stderr_path).expect("daemon log"))
            .spawn()
            .expect("spawn candidate daemon"),
    );
    // Startup owns the production readiness allowance. Each connect attempt is
    // bounded by that same deadline, so handshake retries cannot multiply it.
    let deadline = tokio::time::Instant::now() + haider_client::spawn::STARTUP_DEADLINE;
    loop {
        let status = daemon.0.try_wait().expect("owned daemon status");
        assert!(
            status.is_none(),
            "daemon exited {status:?}: {}",
            std::fs::read_to_string(&stderr_path).expect("log")
        );
        let attempt = tokio::time::timeout_at(
            deadline,
            connect(&profile.endpoint_path, ClientConfig::default()),
        )
        .await
        .map(|result| result.map(|_| ()));
        if matches!(attempt, Ok(Ok(_))) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "daemon readiness: {attempt:?}; {}",
            std::fs::read_to_string(&stderr_path).expect("log")
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let ensure = EnsureOptions {
        daemon_lifetime: DaemonLifetime::Persistent,
        ..EnsureOptions::default()
    };
    let mut session_id = None;
    let mut run_ids = BTreeSet::new();
    let mut terminal_seq = 0;
    for ordinal in 1..=3 {
        let request = HeadlessRunRequest {
            cwd: workspace.to_str().expect("workspace utf8").into(),
            prompt: format!("continuation prompt {ordinal}"),
            attachments: vec![],
            durable_attachments: vec![],
            provider: (ordinal == 1).then(|| "fake".into()),
            model: (ordinal == 1).then(|| "fake-model".into()),
            max_tokens: 4096,
            budget: Default::default(),
            seed: None,
            replay_of: None,
            journal_pin: false,
            detached: false,
            permission_overrides: Default::default(),
            trust_hooks: false,
            // This is the caller's product deadline, not a performance bound.
            timeout: Some(Duration::from_secs(30)),
            terminal_grace: haider_client::DEFAULT_TERMINAL_GRACE,
        };
        let (sender, mut receiver) = mpsc::channel(32);
        let drain = tokio::spawn(async move {
            let mut envelopes = Vec::new();
            while let Some(event) = receiver.recv().await {
                if let haider_client::HeadlessEvent::Envelope(envelope) = event {
                    envelopes.push(envelope);
                }
            }
            envelopes
        });
        let result = if let Some(id) = session_id.clone() {
            submit_headless_with_event_mode_and_interrupts(
                &profile,
                ensure.clone(),
                request,
                id,
                sender,
                HeadlessEventMode::Stream,
                None,
            )
            .await
        } else {
            run_headless(&profile, ensure.clone(), request, sender).await
        }
        .expect("accepted ordinary turn");
        let streamed = drain.await.expect("event drain");
        if ordinal > 1 {
            assert!(!streamed.is_empty(), "new run envelopes are streamed");
            assert!(
                streamed
                    .iter()
                    .all(|event| event.run_id.as_ref() == Some(&result.run_id)),
                "ordinary continuation never streams historical terminal records"
            );
        }
        assert_eq!(result.outcome, HeadlessOutcome::Done, "{result:?}");
        assert!(
            run_ids.insert(result.run_id.as_str().to_owned()),
            "fresh run per turn"
        );
        assert_eq!(
            session_id.get_or_insert(result.session_id.clone()),
            &result.session_id
        );
        let terminal = result.terminal_seq.expect("durable terminal");
        assert!(terminal > terminal_seq, "journal advances across turns");
        for event in result.events.iter().expect("result ledger") {
            let event = event.expect("ledger event");
            assert_eq!(
                event.run_id.as_ref(),
                Some(&result.run_id),
                "no old run result leakage"
            );
        }
        println!(
            "turn={ordinal} session={} run={} terminal_seq={terminal}",
            result.session_id, result.run_id
        );
        terminal_seq = terminal;
    }
    let connected = connect(&profile.endpoint_path, ClientConfig::default())
        .await
        .expect("journal connection");
    let response = connected
        .client
        .request(RequestBody::SessionRead {
            session_id: session_id.expect("native session"),
            range: SeqRange {
                start_seq: 1,
                end_seq: terminal_seq,
            },
        })
        .await
        .expect("journal read");
    let ResponseBody::SessionRead { result } = response else {
        panic!("journal response: {response:?}");
    };
    assert_eq!(result.envelopes.len() as u64, terminal_seq);
    for (index, event) in result.envelopes.iter().enumerate() {
        assert_eq!(event.seq, index as u64 + 1, "no missing journal sequence");
        assert_eq!(event.session_id, result.session_id);
    }
    let journal_runs: BTreeSet<_> = result
        .envelopes
        .iter()
        .filter_map(|event| event.run_id.as_ref().map(ToString::to_string))
        .collect();
    assert_eq!(
        journal_runs, run_ids,
        "all three runs survive in the one journal"
    );
    let journal = serde_json::to_string(&result.envelopes).expect("journal json");
    for ordinal in 1..=3 {
        assert!(
            journal.contains(&format!("continuation prompt {ordinal}")),
            "all prompts survive"
        );
    }
}
