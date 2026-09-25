//! Linux presence fallback (`haiderd --cu-presence-overlay`): a resident
//! desktop notification with a **Stop** action over the freedesktop
//! `org.freedesktop.Notifications` D-Bus interface.
//!
//! Why not a pointer overlay here (see `docs/cu-presence.md`):
//!
//! * Wayland: a client cannot place a global always-on-top surface or learn
//!   global coordinates; `wlr-layer-shell` exists only on wlroots/KDE, not
//!   GNOME, and the portal ScreenCast stream the backend captures from has
//!   no per-surface exclusion.
//! * X11: an override-redirect window is trivially drawable, but X11 has no
//!   capture-exclusion primitive — `GetImage` on the root window (the
//!   backend's capture) would include the overlay in the model's view.
//!
//! The notification works on both. It renders the badge text, is updated
//! with the latest action, and its Stop action reaches the daemon exactly
//! like the macOS/Windows badge. Limitation: a notification *popup* is an
//! ordinary window and can appear in a screenshot taken while it is on
//! screen; it is posted with low urgency so the desktop retires the popup
//! into its notification list quickly, where Stop stays available.

use super::{PresenceCommand, PresenceEvent, PresenceMark, encode_event, parse_command_line};
use futures_util::StreamExt as _;
use std::collections::HashMap;
use std::io::BufRead as _;
use std::io::Write as _;
use std::time::{Duration, Instant};
use zbus::zvariant::Value;

const DESTINATION: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const INTERFACE: &str = "org.freedesktop.Notifications";
const STOP_ACTION: &str = "stop";
/// Body updates are rate-limited so a burst of actions does not spam the bus.
const UPDATE_INTERVAL: Duration = Duration::from_millis(800);

pub(super) fn run() -> i32 {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            emit(&PresenceEvent::Error {
                message: format!("presence helper runtime failed: {error}"),
            });
            return 70;
        }
    };
    runtime.block_on(run_async())
}

fn emit(event: &PresenceEvent) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{}", encode_event(event));
    let _ = stdout.flush();
}

/// Where to stop when the notification server offers no action buttons.
const FALLBACK_STOP_HINT: &str = "To stop: press Esc in the Haider TUI session, or for a headless run use `haider run --stop <run-id>`.";
/// Time for the desktop to retire a closed popup before a capture starts.
const CONCEAL_SETTLE: Duration = Duration::from_millis(200);

struct Notifier {
    connection: zbus::Connection,
    id: u32,
    label: String,
    last_update: Option<Instant>,
    last_body: String,
    /// `GetCapabilities` advertised "actions": the Stop button can exist.
    actions_supported: bool,
}

impl Notifier {
    async fn notify(&mut self, body: &str) {
        self.last_body = body.to_owned();
        let body = if self.actions_supported {
            body.to_owned()
        } else {
            format!("{body}\n{FALLBACK_STOP_HINT}")
        };
        let body = body.as_str();
        let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
        hints.insert("resident", Value::from(true));
        hints.insert("urgency", Value::from(0u8));
        hints.insert("category", Value::from("device"));
        let actions = if self.actions_supported {
            vec![STOP_ACTION, "Stop"]
        } else {
            Vec::new()
        };
        let reply = self
            .connection
            .call_method(
                Some(DESTINATION),
                PATH,
                Some(INTERFACE),
                "Notify",
                &(
                    "Haider",
                    self.id,
                    "dialog-warning",
                    self.label.as_str(),
                    body,
                    actions,
                    hints,
                    0i32,
                ),
            )
            .await;
        match reply.and_then(|reply| reply.body().deserialize::<u32>()) {
            Ok(id) => self.id = id,
            Err(error) => emit(&PresenceEvent::Error {
                message: format!("desktop notification failed: {error}"),
            }),
        }
        self.last_update = Some(Instant::now());
    }

    async fn close(&mut self) {
        if self.id == 0 {
            return;
        }
        let _ = self
            .connection
            .call_method(
                Some(DESTINATION),
                PATH,
                Some(INTERFACE),
                "CloseNotification",
                &(self.id,),
            )
            .await;
        self.id = 0;
    }
}

/// Blocking stdin lines forwarded into the async loop (`None` = EOF).
fn stdin_lines() -> tokio::sync::mpsc::UnboundedReceiver<String> {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    let _ = std::thread::Builder::new()
        .name("presence-stdin".into())
        .spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
    receiver
}

async fn run_async() -> i32 {
    let mut lines = stdin_lines();
    let connection = match zbus::Connection::session().await {
        Ok(connection) => connection,
        Err(error) => {
            emit(&PresenceEvent::Error {
                message: format!("no D-Bus session bus for the presence notification: {error}"),
            });
            // Keep consuming commands so the daemon's writes never fail.
            while lines.recv().await.is_some() {}
            return 0;
        }
    };
    let proxy = zbus::Proxy::new(&connection, DESTINATION, PATH, INTERFACE).await;
    let mut actions = match &proxy {
        Ok(proxy) => proxy.receive_signal("ActionInvoked").await.ok(),
        Err(_) => None,
    };
    // Actions are optional in the freedesktop spec: ask before promising a
    // Stop button (GetCapabilities -> as).
    let actions_supported = match connection
        .call_method(
            Some(DESTINATION),
            PATH,
            Some(INTERFACE),
            "GetCapabilities",
            &(),
        )
        .await
        .and_then(|reply| reply.body().deserialize::<Vec<String>>())
    {
        Ok(capabilities) => capabilities
            .iter()
            .any(|capability| capability == "actions"),
        Err(error) => {
            emit(&PresenceEvent::Error {
                message: format!("notification server GetCapabilities failed: {error}"),
            });
            false
        }
    };
    if !actions_supported {
        emit(&PresenceEvent::Error {
            message: "notification server has no action buttons; Stop is available only via the Haider TUI (Esc) or `haider run --stop`".into(),
        });
    }
    emit(&PresenceEvent::Ready {
        platform: if actions_supported {
            "linux-notification".into()
        } else {
            "linux-notification-no-actions".into()
        },
        capture_excluded: false,
    });
    let mut notifier = Notifier {
        connection,
        id: 0,
        label: "Haider is controlling this screen".into(),
        last_update: None,
        last_body: String::new(),
        actions_supported,
    };
    let mut visible = false;
    let mut stopping = false;
    loop {
        tokio::select! {
            line = lines.recv() => {
                let Some(line) = line else { break };
                match parse_command_line(&line) {
                    Some(Ok(command)) => match command {
                        PresenceCommand::Show { label, .. } => {
                            notifier.label = label;
                            visible = true;
                            stopping = false;
                            let body = if notifier.actions_supported {
                                "Haider is using your mouse and keyboard. Choose Stop to cancel the run."
                            } else {
                                "Haider is using your mouse and keyboard."
                            };
                            notifier.notify(body).await;
                        }
                        PresenceCommand::Pointer { seq, mark, point, .. } => {
                            emit(&PresenceEvent::Ack { seq });
                            let due = notifier.last_update.is_none_or(|last| last.elapsed() >= UPDATE_INTERVAL);
                            if visible && due && mark != PresenceMark::Observe {
                                let body = point.map_or_else(
                                    || format!("Last action: {}", mark.caption()),
                                    |point| format!("Last action: {} at ({:.0}, {:.0})", mark.caption(), point.x, point.y),
                                );
                                notifier.notify(&body).await;
                            }
                        }
                        PresenceCommand::Stopping { .. } => stopping = true,
                        PresenceCommand::Hide { .. } => {
                            visible = false;
                            notifier.close().await;
                        }
                        // The popup is an ordinary window: close it around a
                        // model-facing capture, then post it again.
                        PresenceCommand::Conceal { seq, .. } => {
                            notifier.close().await;
                            tokio::time::sleep(CONCEAL_SETTLE).await;
                            emit(&PresenceEvent::Ack { seq });
                        }
                        PresenceCommand::Reveal { .. } => {
                            if visible && !stopping {
                                let body = notifier.last_body.clone();
                                notifier.notify(&body).await;
                            }
                        }
                    },
                    Some(Err(message)) => emit(&PresenceEvent::Error {
                        message: format!("ignored malformed presence command: {message}"),
                    }),
                    None => {}
                }
            }
            signal = async {
                match actions.as_mut() {
                    Some(stream) => stream.next().await,
                    None => std::future::pending().await,
                }
            } => {
                let Some(signal) = signal else {
                    actions = None;
                    continue;
                };
                if let Ok((id, key)) = signal.body().deserialize::<(u32, String)>()
                    && id == notifier.id
                    && key == STOP_ACTION
                    && visible
                    && !stopping
                {
                    stopping = true;
                    emit(&PresenceEvent::Stop);
                }
            }
        }
    }
    notifier.close().await;
    0
}
