//! Human SSH terminal bridge used by the CLI. The daemon owns the russh PTY;
//! this module only translates local terminal events into typed RPC controls
//! and writes transient output bytes back to the attached terminal.

use std::fmt;
use std::io::{IsTerminal, Write, stdin, stdout};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use haider_client::{RpcClient, SshProfilesClientError, shell_registry, ssh_profiles};
use haider_rpc::{ShellStatusWire, SshPtySizeWire, WireFrame};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};

const EVENT_POLL: Duration = Duration::from_millis(20);

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
    let shell_id = shell.id;
    let mut output = stdout().lock();

    let result: Result<Option<i32>, SshTerminalError> = async {
        loop {
            while event::poll(Duration::ZERO)? {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        if key.code == KeyCode::Char('d')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            profiles.eof(&shell_id).await?;
                            continue;
                        }
                        if key.code == KeyCode::Char(']')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            if let Some(shells) = shell_registry(client) {
                                let _ = shells.close(&shell_id).await;
                            }
                            return Ok(None);
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
            match frame {
                Some(WireFrame::ShellOutput { id, chunk_b64, .. }) if id == shell_id => {
                    let bytes = zeroize::Zeroizing::new(
                        BASE64
                            .decode(chunk_b64.expose())
                            .map_err(|_| SshTerminalError::InvalidOutput)?,
                    );
                    output.write_all(bytes.as_slice())?;
                    output.flush()?;
                }
                Some(WireFrame::ShellState {
                    shell:
                        haider_rpc::ShellWire {
                            id,
                            status: ShellStatusWire::Exited { code },
                            ..
                        },
                }) if id == shell_id => return Ok(code),
                Some(WireFrame::ShellClosed { shell }) if shell.id == shell_id => return Ok(None),
                _ => {}
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
