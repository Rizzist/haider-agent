//! Streaming download with sha256-before-atomic-rename (ADE parity).
//!
//! The install discipline is the ADE's `download_whisper_model`
//! (rust-diffforge `src-tauri/src/audio.rs:9710-9915`): stream into
//! `<file>.download` in the SAME directory, verify the pinned sha256 over
//! the temp file, then atomically rename onto the final name. Concurrent
//! ADE/Haider downloads race benignly — whichever rename lands last wins
//! with identical bytes, and a failed verification removes the temp file and
//! installs nothing. An already-present final file short-circuits to success
//! without touching the network (models are shared, never re-downloaded).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::Digest as _;

use crate::SttError;
use crate::catalog::WhisperModel;

/// ADE download budget: 900 s end-to-end per artifact.
pub const DOWNLOAD_TIMEOUT_SECS: u64 = 900;
/// Temp-file suffix appended to the final filename during download.
pub const DOWNLOAD_TEMP_SUFFIX: &str = ".download";

/// Progress phases (ADE `forge-audio-model-download-progress` states).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadState {
    Starting,
    Downloading,
    Done,
}

/// One progress emission during an install.
#[derive(Debug, Clone, PartialEq)]
pub struct DownloadProgress {
    pub state: DownloadState,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    /// `Some` only when the server advertised a non-zero total.
    pub percent: Option<f64>,
}

/// What to fetch and how to verify it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadSpec<'a> {
    pub url: &'a str,
    pub sha256: &'a str,
    /// Final filename inside the target directory.
    pub file_name: &'a str,
}

impl WhisperModel {
    /// This model's catalog-pinned download coordinates.
    #[must_use]
    pub fn download_spec(&self) -> DownloadSpec<'_> {
        DownloadSpec {
            url: self.url,
            sha256: self.sha256,
            file_name: self.file,
        }
    }
}

/// A reqwest client with the ADE download budget.
pub fn download_client() -> Result<reqwest::Client, SttError> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
        .build()
        .map_err(|error| SttError::Download(format!("could not build HTTP client: {error}")))
}

fn sha256_file(path: &Path) -> Result<String, SttError> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| SttError::Io(format!("could not open downloaded file: {error}")))?;
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut file, &mut hasher)
        .map_err(|error| SttError::Io(format!("could not hash downloaded file: {error}")))?;
    Ok(hex::encode(hasher.finalize()))
}

/// Streams `spec.url` into `dir/<file_name>` with verify-before-rename.
///
/// Laws:
/// - An existing final file short-circuits: no network I/O, immediate `Done`.
/// - Body bytes stream into `<file_name>.download`; every chunk emits a
///   `Downloading` progress with honest byte counts.
/// - The sha256 is computed over the CLOSED temp file BEFORE rename; a
///   mismatch removes the temp file and returns
///   [`SttError::ChecksumMismatch`] with both digests — the final path never
///   comes into existence.
/// - Install is `rename(2)` in the same directory: readers observe either
///   nothing or the complete verified file, never a torn write.
pub async fn install(
    client: &reqwest::Client,
    dir: &Path,
    spec: DownloadSpec<'_>,
    mut progress: impl FnMut(DownloadProgress),
) -> Result<PathBuf, SttError> {
    let target = dir.join(spec.file_name);
    if target.is_file() {
        progress(DownloadProgress {
            state: DownloadState::Done,
            downloaded_bytes: 0,
            total_bytes: None,
            percent: Some(100.0),
        });
        return Ok(target);
    }
    std::fs::create_dir_all(dir)
        .map_err(|error| SttError::Io(format!("could not create model directory: {error}")))?;
    progress(DownloadProgress {
        state: DownloadState::Starting,
        downloaded_bytes: 0,
        total_bytes: None,
        percent: None,
    });
    let mut response = client
        .get(spec.url)
        .send()
        .await
        .map_err(|error| SttError::Download(format!("request failed: {error}")))?;
    if !response.status().is_success() {
        return Err(SttError::Download(format!(
            "download returned HTTP {}",
            response.status()
        )));
    }
    let total_bytes = response.content_length();
    let temp = dir.join(format!("{}{DOWNLOAD_TEMP_SUFFIX}", spec.file_name));
    let result: Result<(), SttError> = async {
        let mut file = std::fs::File::create(&temp)
            .map_err(|error| SttError::Io(format!("could not create download file: {error}")))?;
        let mut downloaded_bytes = 0u64;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| SttError::Download(format!("interrupted body: {error}")))?
        {
            file.write_all(&chunk)
                .map_err(|error| SttError::Io(format!("could not write download: {error}")))?;
            downloaded_bytes += chunk.len() as u64;
            let percent = total_bytes
                .filter(|total| *total > 0)
                .map(|total| (downloaded_bytes as f64 / total as f64) * 100.0);
            progress(DownloadProgress {
                state: DownloadState::Downloading,
                downloaded_bytes,
                total_bytes,
                percent,
            });
        }
        file.flush()
            .map_err(|error| SttError::Io(format!("could not finish download write: {error}")))?;
        drop(file);
        let actual = sha256_file(&temp)?;
        if !actual.eq_ignore_ascii_case(spec.sha256) {
            return Err(SttError::ChecksumMismatch {
                expected: spec.sha256.to_owned(),
                actual,
            });
        }
        std::fs::rename(&temp, &target)
            .map_err(|error| SttError::Io(format!("could not install downloaded file: {error}")))
    }
    .await;
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    progress(DownloadProgress {
        state: DownloadState::Done,
        downloaded_bytes: 0,
        total_bytes,
        percent: Some(100.0),
    });
    Ok(target)
}

/// Installs one catalog model into the shared whisper dir.
pub async fn install_model(
    client: &reqwest::Client,
    whisper_dir: &Path,
    model: &WhisperModel,
    progress: impl FnMut(DownloadProgress),
) -> Result<PathBuf, SttError> {
    install(client, whisper_dir, model.download_spec(), progress).await
}
