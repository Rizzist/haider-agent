//! Pure discovery of image files created by foreground tool calls.
//!
//! Discovery is deliberately conservative: transcript text only nominates a
//! path, while filesystem metadata and the canonical workspace fence decide
//! whether that path is safe to publish as a durable event.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use haider_protocol::image::ImageCreatedV1;

const IMAGE_EVENT_LIMIT: usize = 4;
const MTIME_SLACK: Duration = Duration::from_secs(2);

/// Finds fresh, non-empty image files named by a command or its output.
///
/// Relative candidates are resolved against `cwd`. Every returned path is
/// canonical, refers to a regular file inside the canonical `cwd`, and has a
/// modification time no earlier than two seconds before `started`.
pub fn detect_created_images(
    command: &str,
    output_preview: &str,
    cwd: &Path,
    started: SystemTime,
) -> Vec<PathBuf> {
    let Ok(workspace) = cwd.canonicalize() else {
        return Vec::new();
    };
    let earliest_mtime = started
        .checked_sub(MTIME_SLACK)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut detected = Vec::new();
    let mut seen = HashSet::new();

    for token in command
        .split_whitespace()
        .chain(output_preview.split_whitespace())
    {
        if detected.len() == IMAGE_EVENT_LIMIT {
            break;
        }
        let candidate = trim_token(token);
        if candidate.is_empty() || !has_image_extension(Path::new(candidate)) {
            continue;
        }
        let candidate = Path::new(candidate);
        let unresolved = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            workspace.join(candidate)
        };
        let Ok(path) = unresolved.canonicalize() else {
            continue;
        };
        if !path.starts_with(&workspace) || !seen.insert(path.clone()) {
            continue;
        }
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        if !metadata.is_file() || metadata.len() == 0 {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified < earliest_mtime {
            continue;
        }
        detected.push(path);
    }

    detected
}

/// Builds the self-contained durable payload for a detected workspace image.
///
/// The extension and a matching file signature jointly determine the media
/// type. PNG and JPEG dimensions are read directly from their container
/// headers; other admitted formats deliberately leave dimensions unknown.
pub fn image_created_payload(
    path: &Path,
    workspace: &Path,
    call_id: &str,
    tool: &str,
) -> std::io::Result<Option<ImageCreatedV1>> {
    let path = path.canonicalize()?;
    let workspace = workspace.canonicalize()?;
    if !path.starts_with(&workspace) {
        return Ok(None);
    }
    let metadata = fs::metadata(&path)?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Ok(None);
    }
    // Header-bounded read: magic + PNG IHDR + JPEG SOFn all live in the
    // first bytes; slurping a multi-gigabyte render for its dimensions
    // would stall the dispatcher.
    let bytes = {
        use std::io::Read;
        const HEADER_READ_LIMIT: u64 = 256 * 1024;
        let mut head = Vec::new();
        fs::File::open(&path)?
            .take(HEADER_READ_LIMIT)
            .read_to_end(&mut head)?;
        head
    };
    let Some(media_type) = media_type_with_magic(&path, &bytes) else {
        return Ok(None);
    };
    let dimensions = match media_type {
        "image/png" => png_dimensions(&bytes),
        "image/jpeg" => jpeg_dimensions(&bytes),
        _ => None,
    };
    let display_path = path.strip_prefix(&workspace).map_or_else(
        |_| path.display().to_string(),
        |relative| relative.display().to_string(),
    );
    Ok(Some(ImageCreatedV1 {
        path: path.display().to_string(),
        display_path,
        media_type: media_type.into(),
        byte_len: metadata.len(),
        width: dimensions.map(|(width, _)| width),
        height: dimensions.map(|(_, height)| height),
        call_id: call_id.into(),
        tool: tool.into(),
    }))
}

fn media_type_with_magic<'a>(path: &Path, bytes: &'a [u8]) -> Option<&'a str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    match extension.as_str() {
        "png" if bytes.starts_with(b"\x89PNG\r\n\x1a\n") => Some("image/png"),
        "jpg" | "jpeg" if bytes.starts_with(&[0xff, 0xd8, 0xff]) => Some("image/jpeg"),
        "gif" if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") => Some("image/gif"),
        "webp" if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP".as_slice()) => {
            Some("image/webp")
        }
        "bmp" if bytes.starts_with(b"BM") => Some("image/bmp"),
        "svg" if looks_like_svg(bytes) => Some("image/svg+xml"),
        _ => None,
    }
}

fn looks_like_svg(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let text = text.trim_start_matches('\u{feff}').trim_start();
    text.starts_with("<svg") || (text.starts_with("<?xml") && text.contains("<svg"))
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") || bytes.get(12..16)? != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
    let height = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return None;
    }
    let mut cursor = 2_usize;
    while cursor + 3 < bytes.len() {
        while bytes.get(cursor) == Some(&0xff) {
            cursor += 1;
        }
        let marker = *bytes.get(cursor)?;
        cursor += 1;
        if matches!(marker, 0xd8 | 0xd9) || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        let segment_len = usize::from(u16::from_be_bytes(
            bytes.get(cursor..cursor + 2)?.try_into().ok()?,
        ));
        if segment_len < 2 || cursor.checked_add(segment_len)? > bytes.len() {
            return None;
        }
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            let height = u32::from(u16::from_be_bytes(
                bytes.get(cursor + 3..cursor + 5)?.try_into().ok()?,
            ));
            let width = u32::from(u16::from_be_bytes(
                bytes.get(cursor + 5..cursor + 7)?.try_into().ok()?,
            ));
            return (width > 0 && height > 0).then_some((width, height));
        }
        cursor += segment_len;
    }
    None
}

fn trim_token(token: &str) -> &str {
    token
        .trim_start_matches(['\'', '"', '`', '(', '[', '{', '<'])
        .trim_end_matches([
            '\'', '"', '`', ')', ']', '}', '>', ',', ';', ':', '!', '?', '.',
        ])
}

fn has_image_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg"
            )
        })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{detect_created_images, image_created_payload};
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, SystemTime};

    fn write_image(root: &Path, name: &str) -> std::path::PathBuf {
        let path = root.join(name);
        fs::write(&path, b"image bytes").expect("write nominated image");
        path.canonicalize().expect("canonical image path")
    }

    /// MUTATION CHECK: dropping either input stream loses one of these two
    /// files; failing relative resolution loses both.
    #[test]
    fn detects_command_and_stdout_tokens_relative_to_cwd() {
        let workspace = tempfile::tempdir().expect("workspace");
        let started = SystemTime::now();
        let command_image = write_image(workspace.path(), "command.PNG");
        let stdout_image = write_image(workspace.path(), "stdout.jpeg");

        let detected = detect_created_images(
            "encoder command.PNG",
            "saved stdout.jpeg",
            workspace.path(),
            started,
        );

        assert_eq!(detected, vec![command_image, stdout_image]);
    }

    /// MUTATION CHECK: removing the canonical workspace fence publishes the
    /// sibling file nominated by its absolute path.
    #[test]
    fn never_emits_a_path_outside_cwd() {
        let parent = tempfile::tempdir().expect("parent");
        let workspace = parent.path().join("workspace");
        fs::create_dir(&workspace).expect("workspace directory");
        let outside = write_image(parent.path(), "outside.png");
        let started = SystemTime::now()
            .checked_sub(Duration::from_secs(1))
            .expect("recent start");

        let detected = detect_created_images(&outside.to_string_lossy(), "", &workspace, started);

        assert!(detected.is_empty());
    }

    /// MUTATION CHECK: removing the two-second mtime gate admits this file,
    /// whose timestamp predates the simulated tool start by more than slack.
    #[test]
    fn ignores_files_older_than_the_tool_start() {
        let workspace = tempfile::tempdir().expect("workspace");
        write_image(workspace.path(), "old.webp");
        let future_start = SystemTime::now()
            .checked_add(Duration::from_secs(5))
            .expect("future start");

        let detected = detect_created_images("old.webp", "", workspace.path(), future_start);

        assert!(detected.is_empty());
    }

    /// MUTATION CHECK: removing either deduplication or the four-item cap
    /// changes the exact cardinality and ordering pinned here.
    #[test]
    fn deduplicates_candidates_and_caps_each_call_at_four() {
        let workspace = tempfile::tempdir().expect("workspace");
        let started = SystemTime::now();
        let expected: Vec<_> = (0..5)
            .map(|index| write_image(workspace.path(), &format!("{index}.gif")))
            .take(4)
            .collect();

        let detected = detect_created_images(
            "0.gif 0.gif 1.gif 2.gif",
            "3.gif 4.gif",
            workspace.path(),
            started,
        );

        assert_eq!(detected, expected);
    }

    /// MUTATION CHECK: removing wrapper trimming prevents both quoted and
    /// parenthesized transcript tokens from resolving to their files.
    #[test]
    fn strips_quoted_parenthesized_and_trailing_wrappers() {
        let workspace = tempfile::tempdir().expect("workspace");
        let started = SystemTime::now();
        let quoted = write_image(workspace.path(), "quoted.svg");
        let parenthesized = write_image(workspace.path(), "wrapped.bmp");

        let detected = detect_created_images(
            "tool 'quoted.svg',",
            "created (wrapped.bmp).",
            workspace.path(),
            started,
        );

        assert_eq!(detected, vec![quoted, parenthesized]);
    }

    /// MUTATION CHECK: dropping signature validation, workspace-relative
    /// display, dimensions, byte length, or producer coordinates changes the
    /// self-contained durable payload pinned here.
    #[test]
    fn builds_complete_png_payload_with_magic_and_dimensions() {
        let workspace = tempfile::tempdir().expect("workspace");
        let path = workspace.path().join("chart.png");
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&640_u32.to_be_bytes());
        png.extend_from_slice(&480_u32.to_be_bytes());
        fs::write(&path, &png).expect("write png header");

        let payload = image_created_payload(&path, workspace.path(), "call-image", "process_exec")
            .expect("inspect image")
            .expect("valid image payload");

        assert_eq!(
            payload.path,
            path.canonicalize()
                .expect("canonical")
                .display()
                .to_string()
        );
        assert_eq!(payload.display_path, "chart.png");
        assert_eq!(payload.media_type, "image/png");
        assert_eq!(payload.byte_len, u64::try_from(png.len()).expect("length"));
        assert_eq!((payload.width, payload.height), (Some(640), Some(480)));
        assert_eq!(payload.call_id, "call-image");
        assert_eq!(payload.tool, "process_exec");
    }

    /// MUTATION CHECK: trusting only the file extension would emit this
    /// non-image as an image-created event.
    #[test]
    fn rejects_extension_without_matching_magic() {
        let workspace = tempfile::tempdir().expect("workspace");
        let path = workspace.path().join("not-really.png");
        fs::write(&path, b"plain text").expect("write false image");

        let payload = image_created_payload(&path, workspace.path(), "call", "fs_write")
            .expect("inspect candidate");

        assert!(payload.is_none());
    }
}
