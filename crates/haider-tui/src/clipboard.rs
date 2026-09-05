//! Auto-copy for the in-app selection (owner item 9).
//!
//! ORDER, documented: (1) the platform writer — `pbcopy` on macOS, arboard's
//! CF_UNICODETEXT writer on Windows, and the existing `pbcopy` attempt on
//! other hosts; (2) OSC 52 — best-effort, ALWAYS emitted after, so a remote
//! or embedded terminal viewing this TUI (ssh, a web terminal) can mirror
//! the copy into its own host clipboard. `pbcopy` success is confirmed by
//! its exit status; Windows success is confirmed by `set_text`, which owns
//! the clipboard while calling SetClipboardData. arboard 3.6.1 retries an
//! occupied Windows clipboard five times at 5 ms (25 ms of retry sleeps).
//! OSC 52 is a single buffered write the caller flushes with the frame.
//!
//! Failure is a FLASH, never a crash: an unconfirmed local copy reports
//! `false` and the caller words the flash honestly (OSC 52 already went
//! out, so the copy may still land via the terminal). A missing platform
//! writer or busy clipboard degrades the same way.

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine as _;

/// How long [`copy_local`] will poll for `pbcopy`'s exit before declaring
/// the local copy unconfirmed. pbcopy exits within a few ms of stdin
/// closing; the bound only exists so a wedged child cannot stall the
/// event loop.
const CONFIRM_BOUND: Duration = Duration::from_millis(300);

/// The authoritative local writer; OSC 52 remains a separate mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalClipboardWriter {
    Pbcopy,
    WindowsArboard,
}

/// Keep the existing Unix behavior while selecting a real Windows writer.
#[must_use]
pub fn local_writer_for_os(os: &str) -> LocalClipboardWriter {
    match os {
        "windows" => LocalClipboardWriter::WindowsArboard,
        _ => LocalClipboardWriter::Pbcopy,
    }
}

/// Hand `text` to the host clipboard. Success means the platform writer
/// confirmed ownership, never merely that the OSC 52 bytes were emitted.
#[must_use]
pub fn copy_local(text: &str) -> bool {
    match local_writer_for_os(std::env::consts::OS) {
        LocalClipboardWriter::Pbcopy => copy_pbcopy(text),
        LocalClipboardWriter::WindowsArboard => arboard::Clipboard::new()
            .and_then(|mut board| board.set_text(text))
            .is_ok(),
    }
}

/// Wording shared by the live copy effect and its tests. An OSC 52 mirror
/// cannot establish whether the receiving terminal accepted the copy.
#[must_use]
pub const fn copy_confirmation(local_confirmed: bool, osc52_sent: bool) -> &'static str {
    if local_confirmed {
        "· copied"
    } else if osc52_sent {
        "· copy unconfirmed — sent via OSC 52 only"
    } else {
        "· copy failed — local clipboard and OSC 52 unavailable"
    }
}

/// Hand `text` to the local clipboard via `pbcopy`. Returns `true` ONLY
/// once the child's EXIT STATUS confirms success (review TUI4.1 P3-5 —
/// success used to be claimed after spawn + stdin write, so a failing
/// `pbcopy` still flashed `· copied`). The wait is bounded: a child that
/// has not exited within [`CONFIRM_BOUND`] is reaped on a detached thread
/// and the copy reports UNCONFIRMED (`false`) — a bounded process-exit
/// poll, not a synchronization sleep.
#[must_use]
fn copy_pbcopy(text: &str) -> bool {
    let mut child = match Command::new("pbcopy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };
    let wrote = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    // stdin is closed (dropped) either way — pbcopy sees EOF and exits.
    let deadline = Instant::now() + CONFIRM_BOUND;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return wrote && status.success(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            // Timed out or errored: reap off-loop, report unconfirmed.
            _ => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return false;
            }
        }
    }
}

/// The OSC 52 clipboard-set sequence for `text` (`c` = the system
/// clipboard selection). Terminals that support it (iTerm2, kitty, wezterm,
/// tmux with `set-clipboard`) copy on sight; the rest ignore it silently.
#[must_use]
pub fn osc52(text: &str) -> String {
    let payload = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{payload}\x07")
}

// ---------------------------------------------------------------------------
// READ side (970 owner bug 2): pasting an IMAGE out of the OS clipboard.
// ---------------------------------------------------------------------------
//
// WHY THIS EXISTS AT ALL. Bracketed paste (`Event::Paste`, runtime.rs) carries
// TEXT and only text — no terminal on any platform delivers image bytes
// through it. So a clipboard image has to be read from the OS clipboard
// directly, on the keystroke, and turned into an attachment by hand.
//
// WHY `arboard`. The alternative was shelling out per platform, and it does
// not survive contact with macOS: `pbpaste` cannot emit an image at all (the
// only shell route is an `osascript` "«class PNGf»" hex dump), and this
// workspace denies `unsafe_code`, so hand-rolled NSPasteboard bindings are
// not an option either. `arboard` is pure Rust, has no GUI-runtime or
// event-loop requirement, and covers macOS (NSPasteboard), Windows
// (clipboard-win) and Linux/X11 (x11rb, already a workspace dependency).
//
// WHY NOT its Wayland backend. `arboard`'s `wayland-data-control` feature
// pulls `wl-clipboard-rs` -> `wayland-client`, which links libwayland — a
// build dependency none of the four CI targets is provisioned for. It is
// switched OFF, and Wayland is served instead by [`wl_paste_png`], a
// zero-dependency shell-out to the `wl-paste` tool that every Wayland
// desktop's clipboard package ships. Under XWayland the X11 backend already
// answers, so the fallback only runs when it is genuinely needed.

/// Largest clipboard image accepted, matching the daemon's per-attachment
/// limit (`session_hub::rpc::MAX_ATTACHMENT_BYTES`) so the composer refuses
/// with a readable notice instead of issuing an upload the daemon rejects.
pub const MAX_CLIPBOARD_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// A PNG the clipboard handed us, ready for the `/attach` pipeline.
#[derive(Clone, PartialEq, Eq)]
pub struct ClipboardImage {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl std::fmt::Debug for ClipboardImage {
    /// Never dump the pixels into a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipboardImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("png_bytes", &self.png.len())
            .finish()
    }
}

/// What one clipboard read found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardContent {
    /// A raster image, already re-encoded as PNG.
    Image(ClipboardImage),
    /// Text from a forwarded paste chord, protected at the same ingress
    /// boundary as terminal paste. A terminal-handled chord delivers paste
    /// content instead of requesting this read.
    Text(crate::app::Pasted),
    /// Nothing on the clipboard, or nothing in a shape we can use.
    Empty,
}

/// A clipboard read that could not happen (no clipboard server, a locked
/// Windows clipboard, an unreadable image). Always a NOTICE, never a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardError(pub String);

impl std::fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The read side of the OS clipboard, behind one seam so the paste reducer
/// is testable without a desktop session ([`FakeClipboard`]).
pub trait ClipboardSource {
    /// Read the clipboard ONCE. Implementations must not block the event
    /// loop for longer than a keystroke's worth of time.
    fn read(&self) -> Result<ClipboardContent, ClipboardError>;
}

/// The real OS clipboard.
#[derive(Debug, Clone, Copy, Default)]
pub struct OsClipboard;

impl ClipboardSource for OsClipboard {
    fn read(&self) -> Result<ClipboardContent, ClipboardError> {
        match arboard::Clipboard::new() {
            Ok(mut board) => match board.get_image() {
                Ok(image) => encode_rgba_png(image.width, image.height, &image.bytes)
                    .map(ClipboardContent::Image),
                // A forwarded chord has not delivered text. Keep the bytes
                // so the runtime can feed the ordinary atomic paste path.
                Err(arboard::Error::ContentNotAvailable) => Ok(match board.get_text() {
                    Ok(text) if !text.is_empty() => ClipboardContent::Text(text.into()),
                    Ok(_) | Err(arboard::Error::ContentNotAvailable) => {
                        wayland_fallback().unwrap_or(ClipboardContent::Empty)
                    }
                    Err(error) => match wayland_fallback() {
                        Some(content) => content,
                        None => return Err(ClipboardError(clipboard_note(&error))),
                    },
                }),
                Err(error) => match wayland_fallback() {
                    Some(content) => Ok(content),
                    None => Err(ClipboardError(clipboard_note(&error))),
                },
            },
            // No clipboard connection at all — on Wayland this is the
            // expected answer, because the X11 backend has nothing to talk
            // to. Try the `wl-paste` route before calling it a failure.
            Err(error) => match wayland_fallback() {
                Some(content) => Ok(content),
                None => Err(ClipboardError(clipboard_note(&error))),
            },
        }
    }
}

/// Short, honest wording for an arboard failure — never the raw Debug.
fn clipboard_note(error: &arboard::Error) -> String {
    match error {
        arboard::Error::ContentNotAvailable => "the clipboard is empty".to_owned(),
        arboard::Error::ClipboardNotSupported => "no clipboard on this display server".to_owned(),
        arboard::Error::ClipboardOccupied => "the clipboard is busy — try again".to_owned(),
        other => format!("clipboard unreadable — {other}"),
    }
}

/// Re-encode a clipboard image's RGBA8 pixels as PNG.
///
/// Every platform hands back a different native format (macOS TIFF, Windows
/// DIB, X11 whatever the source app offered); arboard normalizes them to
/// RGBA8, and PNG is what the attachment pipeline and every vision model
/// accept. Bounded by [`MAX_CLIPBOARD_IMAGE_BYTES`].
fn encode_rgba_png(
    width: usize,
    height: usize,
    rgba: &[u8],
) -> Result<ClipboardImage, ClipboardError> {
    let (Ok(w), Ok(h)) = (u32::try_from(width), u32::try_from(height)) else {
        return Err(ClipboardError("clipboard image is too large".to_owned()));
    };
    if w == 0 || h == 0 {
        return Err(ClipboardError("clipboard image is empty".to_owned()));
    }
    let expected = (width)
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4));
    if expected != Some(rgba.len()) {
        return Err(ClipboardError(
            "clipboard image pixels are malformed".to_owned(),
        ));
    }
    let mut png = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut png);
    image::ImageEncoder::write_image(encoder, rgba, w, h, image::ExtendedColorType::Rgba8)
        .map_err(|error| {
            ClipboardError(format!("clipboard image could not be encoded: {error}"))
        })?;
    if png.len() > MAX_CLIPBOARD_IMAGE_BYTES {
        return Err(ClipboardError(format!(
            "clipboard image is {} MB — the limit is {} MB",
            png.len() / (1024 * 1024),
            MAX_CLIPBOARD_IMAGE_BYTES / (1024 * 1024)
        )));
    }
    Ok(ClipboardImage {
        png,
        width: w,
        height: h,
    })
}

/// Wayland's `wl-paste` route, used only when the X11 backend answered
/// nothing (see the module note). `None` on every other OS, and on Linux
/// whenever the tool is missing or holds no image — the caller then reports
/// the original clipboard failure rather than inventing one.
#[cfg(target_os = "linux")]
fn wayland_fallback() -> Option<ClipboardContent> {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return None;
    }
    let png = wl_paste_png()?;
    encode_check_png(&png)
}

#[cfg(not(target_os = "linux"))]
const fn wayland_fallback() -> Option<ClipboardContent> {
    None
}

/// `wl-paste --type image/png`, bounded. Returns the raw PNG bytes.
#[cfg(target_os = "linux")]
fn wl_paste_png() -> Option<Vec<u8>> {
    use std::process::{Command, Stdio};
    let output = Command::new("wl-paste")
        .args(["--no-newline", "--type", "image/png"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    (output.status.success() && !output.stdout.is_empty()).then_some(output.stdout)
}

/// Accept already-PNG bytes from the shell route, verifying the header and
/// the size bound rather than trusting the tool.
#[cfg(target_os = "linux")]
fn encode_check_png(png: &[u8]) -> Option<ClipboardContent> {
    const MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if !png.starts_with(&MAGIC) || png.len() > MAX_CLIPBOARD_IMAGE_BYTES {
        return None;
    }
    let (width, height) = png_dimensions(png)?;
    Some(ClipboardContent::Image(ClipboardImage {
        png: png.to_vec(),
        width,
        height,
    }))
}

/// A PNG's IHDR dimensions (bytes 16..24), so the chip can label the paste
/// without decoding the whole image.
#[cfg(target_os = "linux")]
fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    let width = u32::from_be_bytes(png.get(16..20)?.try_into().ok()?);
    let height = u32::from_be_bytes(png.get(20..24)?.try_into().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

/// A scripted clipboard for tests — the whole point of [`ClipboardSource`].
#[derive(Debug, Clone)]
pub struct FakeClipboard(pub Result<ClipboardContent, ClipboardError>);

impl FakeClipboard {
    /// A clipboard holding one image, built from real RGBA pixels so the
    /// PNG encoding is exercised too.
    #[must_use]
    pub fn image(width: u32, height: u32) -> Self {
        let pixels = vec![0x40_u8; (width as usize) * (height as usize) * 4];
        Self(encode_rgba_png(width as usize, height as usize, &pixels).map(ClipboardContent::Image))
    }

    #[must_use]
    pub fn text() -> Self {
        Self::text_with("clipboard text")
    }

    #[must_use]
    pub fn text_with(text: &str) -> Self {
        Self(Ok(ClipboardContent::Text(text.to_owned().into())))
    }

    #[must_use]
    pub const fn empty() -> Self {
        Self(Ok(ClipboardContent::Empty))
    }

    #[must_use]
    pub fn broken(note: &str) -> Self {
        Self(Err(ClipboardError(note.to_owned())))
    }
}

impl ClipboardSource for FakeClipboard {
    fn read(&self) -> Result<ClipboardContent, ClipboardError> {
        self.0.clone()
    }
}
