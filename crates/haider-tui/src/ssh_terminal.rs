//! Human SSH terminal bridge used by the CLI. The daemon owns the russh PTY;
//! this module only translates local terminal events into typed RPC controls
//! and writes transient output bytes back to the attached terminal.

use std::fmt;
use std::io::{IsTerminal, Write, stdin, stdout};
use std::ops::ControlFlow;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use haider_client::{RpcClient, SshProfilesClientError, shell_registry, ssh_profiles};
use haider_rpc::{ShellStatusWire, SshPtySizeWire, WireFrame};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};

const EVENT_POLL: Duration = Duration::from_millis(20);
const READY_FRAME_BUDGET: usize = 16;

#[derive(Debug)]
pub enum SshTerminalError {
    NotTerminal,
    FeatureAbsent,
    EventsAlreadyTaken,
    StreamClosed,
    Io(std::io::Error),
    Client(SshProfilesClientError),
    InvalidOutput,
}

impl fmt::Display for SshTerminalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotTerminal => formatter.write_str("interactive SSH requires a terminal"),
            Self::FeatureAbsent => formatter.write_str("daemon does not advertise ssh_profiles_v1"),
            Self::EventsAlreadyTaken => {
                formatter.write_str("the daemon event stream is already in use")
            }
            Self::StreamClosed => formatter.write_str("the daemon event stream closed"),
            Self::Io(error) => error.fmt(formatter),
            Self::Client(error) => error.fmt(formatter),
            Self::InvalidOutput => formatter.write_str("daemon sent invalid SSH terminal output"),
        }
    }
}

impl std::error::Error for SshTerminalError {}

impl From<std::io::Error> for SshTerminalError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<SshProfilesClientError> for SshTerminalError {
    fn from(error: SshProfilesClientError) -> Self {
        Self::Client(error)
    }
}

struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> Result<Self, std::io::Error> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

/// Runs an unbounded human-driven PTY. The remote channel has no model/run
/// deadline; it ends only at remote exit, explicit registry close, EOF, or a
/// transport error. The authenticated profile session stays reusable.
pub async fn run_ssh_terminal(
    client: &RpcClient,
    profile: &str,
) -> Result<Option<i32>, SshTerminalError> {
    if !stdin().is_terminal() || !stdout().is_terminal() {
        return Err(SshTerminalError::NotTerminal);
    }
    let mut events = client
        .take_events()
        .ok_or(SshTerminalError::EventsAlreadyTaken)?;
    let profiles = ssh_profiles(client).ok_or(SshTerminalError::FeatureAbsent)?;
    let (cols, rows) = ratatui::crossterm::terminal::size()?;
    let size = pty_size(cols, rows);
    let term = std::env::var("TERM")
        .ok()
        .filter(|value| {
            !value.is_empty() && value.len() <= 255 && !value.chars().any(char::is_control)
        })
        .unwrap_or_else(|| "xterm-256color".into());
    // Enter raw mode before opening the remote channel so every error path
    // after a successful open is covered by the guard and cleanup block.
    let _raw = RawModeGuard::enter()?;
    let shell = profiles.open_pty(profile, None, term, size).await?;
    let mut connected = shell.status == ShellStatusWire::Running;
    let shell_id = shell.id;
    let mut output = stdout().lock();

    let result: Result<Option<i32>, SshTerminalError> = async {
        loop {
            // Apply already-received state before interpreting Ctrl+C. Once
            // Running arrives it belongs to the remote shell, not setup.
            // Continuous remote output must still yield to local close keys.
            for _ in 0..READY_FRAME_BUDGET {
                let Ok(frame) = events.try_recv() else {
                    break;
                };
                if let ControlFlow::Break(code) =
                    terminal_frame(frame, &shell_id, &mut connected, &mut output)?
                {
                    return Ok(code);
                }
            }
            while event::poll(Duration::ZERO)? {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        if cancels_terminal(key, connected) {
                            if let Some(shells) = shell_registry(client) {
                                let _ = shells.close(&shell_id).await;
                            }
                            return Ok(None);
                        }
                        if key.code == KeyCode::Char('d')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            profiles.eof(&shell_id).await?;
                            continue;
                        }
                        if let Some(bytes) = key_bytes(key) {
                            profiles.input_b64(&shell_id, BASE64.encode(bytes)).await?;
                        }
                    }
                    Event::Paste(text) if !text.is_empty() => {
                        for chunk in text.as_bytes().chunks(haider_rpc::SSH_PTY_INPUT_MAX_BYTES) {
                            profiles.input_b64(&shell_id, BASE64.encode(chunk)).await?;
                        }
                    }
                    Event::Resize(cols, rows) => {
                        profiles.resize(&shell_id, pty_size(cols, rows)).await?;
                    }
                    _ => {}
                }
            }

            let frame = match tokio::time::timeout(EVENT_POLL, events.recv()).await {
                Ok(Some(frame)) => Some(frame),
                Ok(None) => return Err(SshTerminalError::StreamClosed),
                Err(_) => None,
            };
            if let Some(frame) = frame
                && let ControlFlow::Break(code) =
                    terminal_frame(frame, &shell_id, &mut connected, &mut output)?
            {
                return Ok(code);
            }
        }
    }
    .await;
    if result.is_err() {
        let _ = profiles.eof(&shell_id).await;
        if let Some(shells) = shell_registry(client) {
            let _ = shells.close(&shell_id).await;
        }
    }
    result
}

pub(crate) fn cancels_terminal(key: KeyEvent, connected: bool) -> bool {
    key.kind != KeyEventKind::Release
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && (matches!(key.code, KeyCode::Char(']' | '5' | '\u{1d}'))
            || (!connected && matches!(key.code, KeyCode::Char('c' | 'C'))))
}

fn terminal_frame(
    frame: WireFrame,
    shell_id: &str,
    connected: &mut bool,
    output: &mut impl Write,
) -> Result<ControlFlow<Option<i32>>, SshTerminalError> {
    match frame {
        WireFrame::ShellOutput { id, chunk_b64, .. } if id == shell_id => {
            let bytes = zeroize::Zeroizing::new(
                BASE64
                    .decode(chunk_b64.expose())
                    .map_err(|_| SshTerminalError::InvalidOutput)?,
            );
            // Output is published only by a running PTY. This also recovers
            // the connection state if a full event queue dropped Running.
            *connected = true;
            output.write_all(bytes.as_slice())?;
            output.flush()?;
        }
        WireFrame::ShellState { shell } | WireFrame::ShellClosed { shell }
            if shell.id == shell_id =>
        {
            match shell.status {
                ShellStatusWire::Running => *connected = true,
                ShellStatusWire::Exited { .. } | ShellStatusWire::Closed => {
                    return shell_exit_result(shell).map(ControlFlow::Break);
                }
                _ => {}
            }
        }
        _ => {}
    }
    Ok(ControlFlow::Continue(()))
}

fn shell_exit_result(shell: haider_rpc::ShellWire) -> Result<Option<i32>, SshTerminalError> {
    if let Some(error) = shell.setup_error {
        return Err(SshTerminalError::Client(SshProfilesClientError::Refused {
            code: error.code,
            message: error.message,
            retryable: error.retryable,
            data: None,
        }));
    }
    Ok(match shell.status {
        ShellStatusWire::Exited { code } => code,
        _ => None,
    })
}

fn pty_size(cols: u16, rows: u16) -> SshPtySizeWire {
    SshPtySizeWire {
        cols: u32::from(cols.max(1)),
        rows: u32::from(rows.max(1)),
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn key_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    if key.modifiers.contains(KeyModifiers::ALT) {
        bytes.push(0x1b);
    }
    match key.code {
        KeyCode::Char(character) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let lower = character.to_ascii_lowercase();
            if lower.is_ascii_lowercase() {
                bytes.push((lower as u8) & 0x1f);
            } else {
                return None;
            }
        }
        KeyCode::Char(character) => {
            let mut encoded = [0_u8; 4];
            bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
        }
        KeyCode::Enter => bytes.push(b'\r'),
        KeyCode::Tab => bytes.push(b'\t'),
        KeyCode::BackTab => bytes.extend_from_slice(b"\x1b[Z"),
        KeyCode::Backspace => bytes.push(0x7f),
        KeyCode::Esc => bytes.push(0x1b),
        KeyCode::Up => bytes.extend_from_slice(b"\x1b[A"),
        KeyCode::Down => bytes.extend_from_slice(b"\x1b[B"),
        KeyCode::Right => bytes.extend_from_slice(b"\x1b[C"),
        KeyCode::Left => bytes.extend_from_slice(b"\x1b[D"),
        KeyCode::Home => bytes.extend_from_slice(b"\x1b[H"),
        KeyCode::End => bytes.extend_from_slice(b"\x1b[F"),
        KeyCode::Delete => bytes.extend_from_slice(b"\x1b[3~"),
        KeyCode::Insert => bytes.extend_from_slice(b"\x1b[2~"),
        KeyCode::PageUp => bytes.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => bytes.extend_from_slice(b"\x1b[6~"),
        _ => return None,
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    fn shell(status: ShellStatusWire) -> haider_rpc::ShellWire {
        haider_rpc::ShellWire {
            id: "owned-shell".into(),
            kind: haider_rpc::ShellKindWire::Ssh {
                profile: "fixture".into(),
            },
            status,
            setup_error: None,
            title: "fixture".into(),
            cwd_or_host: "127.0.0.1".into(),
            created_at_ms: 0,
            last_activity_ms: 0,
            bytes_out: 0,
        }
    }

    #[test]
    fn setup_refusals_preserve_codes_messages_and_retryability() {
        for code in [
            "ssh_connection_failed",
            "ssh_authentication_failed",
            "ssh_host_key_changed",
            "ssh_agent_unavailable",
            "ssh_channel_quota",
            "ssh_vault_error",
            "ssh_timeout",
        ] {
            for status in [
                ShellStatusWire::Exited { code: None },
                ShellStatusWire::Closed,
            ] {
                let mut shell = shell(status);
                shell.setup_error = Some(haider_rpc::ShellSetupErrorWire {
                    code: code.into(),
                    message: "synthetic setup message".into(),
                    retryable: true,
                });
                let error = shell_exit_result(shell).expect_err("setup is a refusal");
                assert!(
                    matches!(error, SshTerminalError::Client(SshProfilesClientError::Refused {
                    code: actual, message, retryable: true, data: None,
                }) if actual == code && message == "synthetic setup message")
                );
            }
        }
    }

    #[test]
    fn terminal_cancel_chords_cover_legacy_encodings_and_connection_states() {
        for connected in [false, true] {
            for character in [']', '5', '\u{1d}', 'c', 'C', 'd', 'x'] {
                for modifiers in [KeyModifiers::CONTROL, KeyModifiers::NONE, KeyModifiers::ALT] {
                    for kind in [
                        KeyEventKind::Press,
                        KeyEventKind::Repeat,
                        KeyEventKind::Release,
                    ] {
                        let key =
                            KeyEvent::new_with_kind(KeyCode::Char(character), modifiers, kind);
                        let expected = kind != KeyEventKind::Release
                            && modifiers == KeyModifiers::CONTROL
                            && (matches!(character, ']' | '5' | '\u{1d}')
                                || (!connected && matches!(character, 'c' | 'C')));
                        assert_eq!(
                            cancels_terminal(key, connected),
                            expected,
                            "{key:?} connected={connected}"
                        );
                        if connected
                            && matches!(character, 'c' | 'C')
                            && modifiers == KeyModifiers::CONTROL
                        {
                            assert_eq!(key_bytes(key), Some(vec![3]));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn only_the_owned_running_frame_finishes_terminal_setup() {
        let mut connected = false;
        let mut output = Vec::new();
        let mut other = shell(ShellStatusWire::Running);
        other.id = "other-shell".into();
        assert!(
            terminal_frame(
                WireFrame::ShellState { shell: other },
                "owned-shell",
                &mut connected,
                &mut output
            )
            .expect("other state")
            .is_continue()
        );
        assert!(!connected);
        assert!(
            terminal_frame(
                WireFrame::ShellState {
                    shell: shell(ShellStatusWire::Running)
                },
                "owned-shell",
                &mut connected,
                &mut output
            )
            .expect("running state")
            .is_continue()
        );
        assert!(connected);
        assert!(!cancels_terminal(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            connected
        ));
    }

    #[test]
    fn owned_output_recovers_running_state_after_event_loss() {
        let mut connected = false;
        let mut output = Vec::new();
        for id in ["other-shell", "owned-shell"] {
            let frame = WireFrame::ShellOutput {
                id: id.into(),
                stream: haider_rpc::ShellOutputStreamWire::Stdout,
                chunk_b64: haider_rpc::SecretWire::new(BASE64.encode(b"ready")),
            };
            assert!(
                terminal_frame(frame, "owned-shell", &mut connected, &mut output)
                    .expect("output frame")
                    .is_continue()
            );
            assert_eq!(connected, id == "owned-shell");
        }
        assert_eq!(output, b"ready");
        assert!(!cancels_terminal(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            connected
        ));
    }

    #[test]
    fn remote_exit_and_explicit_close_are_not_setup_refusals() {
        for code in [None, Some(0), Some(7)] {
            assert_eq!(
                shell_exit_result(shell(ShellStatusWire::Exited { code })).expect("remote result"),
                code
            );
        }
        assert_eq!(
            shell_exit_result(shell(ShellStatusWire::Closed)).expect("cancel"),
            None
        );
    }
}
