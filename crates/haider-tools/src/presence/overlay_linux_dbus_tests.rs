#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use std::io::{BufRead, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use zbus::object_server::SignalEmitter;

struct ChildGuard(Child);

impl std::ops::Deref for ChildGuard {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}

impl std::ops::DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[derive(Default)]
struct FakeNotifications {
    next: AtomicU32,
    posted: Arc<Mutex<Vec<(u32, String, String)>>>,
    stop_on_close: Arc<AtomicBool>,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl FakeNotifications {
    fn get_capabilities(&self) -> Vec<&str> {
        vec!["actions", "body"]
    }

    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        _app_name: &str,
        _replaces_id: u32,
        _app_icon: &str,
        summary: &str,
        body: &str,
        actions: Vec<String>,
        _hints: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
        _expire_timeout: i32,
    ) -> u32 {
        assert!(actions.iter().any(|action| action == "stop"));
        let id = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        self.posted
            .lock()
            .unwrap()
            .push((id, summary.into(), body.into()));
        id
    }

    async fn close_notification(
        &self,
        id: u32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        if self.stop_on_close.load(Ordering::SeqCst) {
            // The signal is on the bus before the method reply, which is the
            // exact queued-click interleaving the helper must preserve.
            Self::action_invoked(&emitter, id, "stop").await.unwrap();
        }
    }

    #[zbus(signal)]
    async fn action_invoked(emitter: &SignalEmitter<'_>, id: u32, key: &str) -> zbus::Result<()>;
}

fn command(child: &mut Child, value: &PresenceCommand) {
    writeln!(
        child.stdin.as_mut().unwrap(),
        "{}",
        serde_json::to_string(value).unwrap()
    )
    .unwrap();
    child.stdin.as_mut().unwrap().flush().unwrap();
}

async fn until<F: Fn() -> bool>(condition: F) {
    for _ in 0..200 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for fake notification server/helper");
}

/// Runs only in the child process spawned by the real-loop test.
#[test]
#[ignore = "launched by linux_real_helper_queues_stop_before_close_reply"]
fn linux_helper_child_entry() {
    if std::env::var_os("HAIDER_LINUX_HELPER_TEST_CHILD").is_some() {
        std::process::exit(run());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires dbus-daemon; run explicitly in the Linux validation container"]
async fn linux_real_helper_queues_stop_before_close_reply() {
    let mut bus = ChildGuard(
        Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("dbus-daemon"),
    );
    let mut address = String::new();
    std::io::BufReader::new(bus.stdout.take().unwrap())
        .read_line(&mut address)
        .unwrap();
    let address = address.trim().to_owned();
    assert!(!address.is_empty());
    let posted = Arc::new(Mutex::new(Vec::new()));
    let stop_on_close = Arc::new(AtomicBool::new(false));
    let server = FakeNotifications {
        next: AtomicU32::new(0),
        posted: Arc::clone(&posted),
        stop_on_close: Arc::clone(&stop_on_close),
    };
    let connection = zbus::connection::Builder::address(address.as_str())
        .unwrap()
        .name(DESTINATION)
        .unwrap()
        .serve_at(PATH, server)
        .unwrap()
        .build()
        .await
        .unwrap();
    let exe = std::env::current_exe().unwrap();
    let mut helper = ChildGuard(
        Command::new(exe)
            .args([
                "--ignored",
                "--exact",
                "presence::overlay_linux::dbus_tests::linux_helper_child_entry",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("DBUS_SESSION_BUS_ADDRESS", &address)
            .env("HAIDER_LINUX_HELPER_TEST_CHILD", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let stdout = helper.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    // The helper must have connected and subscribed to ActionInvoked before
    // commands are sent. `ready` is its real wire event.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if rx.recv().await.unwrap().contains("\"event\":\"ready\"") {
                break;
            }
        }
    })
    .await
    .expect("helper ready");
    let screen = super::super::PresenceSurface::Screen;
    let show = |label: &str| PresenceCommand::Show {
        surface: screen,
        label: label.into(),
    };
    command(&mut helper, &show("generation one"));
    until(|| posted.lock().unwrap().len() == 1).await;
    let stale_id = posted.lock().unwrap()[0].0;
    command(
        &mut helper,
        &PresenceCommand::Hide {
            surface: screen,
            reason: super::super::PresenceEndReason::RunEnded,
        },
    );
    command(&mut helper, &show("generation two"));
    until(|| posted.lock().unwrap().len() == 2).await;
    let emitter = SignalEmitter::new(&connection, PATH).unwrap();
    FakeNotifications::action_invoked(&emitter, stale_id, "stop")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    command(
        &mut helper,
        &PresenceCommand::Conceal {
            surface: screen,
            seq: 700,
        },
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let line = rx.recv().await.unwrap();
            assert!(
                !line.contains("\"event\":\"stop\""),
                "stale id stopped a later generation"
            );
            if line.contains("\"event\":\"ack\",\"seq\":700") {
                break;
            }
        }
    })
    .await
    .expect("conceal ack");
    command(&mut helper, &show("updated label"));
    command(
        &mut helper,
        &PresenceCommand::Pointer {
            surface: screen,
            seq: 701,
            mark: PresenceMark::Type,
            point: None,
        },
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let line = rx.recv().await.unwrap();
            if line.contains("\"event\":\"ack\",\"seq\":701") {
                break;
            }
        }
    })
    .await
    .expect("pointer ack while concealed");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(posted.lock().unwrap().len(), 2, "no Notify while concealed");
    command(&mut helper, &PresenceCommand::Reveal { surface: screen });
    until(|| posted.lock().unwrap().len() == 3).await;
    let latest = posted.lock().unwrap()[2].clone();
    assert_eq!(latest.1, "updated label");
    assert!(latest.2.contains("Last action: Haider · typing"));
    stop_on_close.store(true, Ordering::SeqCst);
    command(
        &mut helper,
        &PresenceCommand::Conceal {
            surface: screen,
            seq: 702,
        },
    );
    let mut stops = 0;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let line = rx.recv().await.unwrap();
            if line.contains("\"event\":\"stop\"") {
                stops += 1;
            }
            if line.contains("\"event\":\"ack\",\"seq\":702") {
                break;
            }
        }
    })
    .await
    .expect("queued Stop and ack");
    if stops == 0 {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if rx.recv().await.unwrap().contains("\"event\":\"stop\"") {
                    stops += 1;
                    break;
                }
            }
        })
        .await
        .expect("Stop queued before CloseNotification reply");
    }
    FakeNotifications::action_invoked(&emitter, latest.0, "stop")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Ok(line) = rx.try_recv() {
        if line.contains("\"event\":\"stop\"") {
            stops += 1;
        }
    }
    assert_eq!(stops, 1, "one Stop despite duplicate ActionInvoked");
    assert_eq!(
        posted.lock().unwrap().len(),
        3,
        "concealed Stop cannot re-post"
    );
    drop(helper.stdin.take());
    let status = helper.wait().unwrap();
    assert!(status.success(), "helper process: {status}");
    drop(connection);
    bus.kill().unwrap();
    bus.wait().unwrap();
}
