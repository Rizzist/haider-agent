//! Release discovery and the monotonic SemVer gate.

use super::UpdateError;
use serde_json::Value;
use std::cmp::Ordering;
use std::fmt;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, mpsc};
use std::time::Duration;

const RELEASE_PAGE_SIZE: usize = 100;
const MAX_RELEASE_PAGES: usize = 20;
const MAX_RELEASE_RESPONSE: usize = 8 * 1024 * 1024;
const CURL: &str = "/usr/bin/curl";

pub(crate) type DiscoveryCancellation = Arc<dyn Fn() -> bool + Send + Sync>;
// Registry #94: the existing QA TUI_EXIT budget is 2.5s
// (scripts/qa-gate/gate/tui_probe.py:42 and scripts/tui-probes/probelib.py reap).
// Observe closure within one tenth of that budget, reserving the remainder
// for kill, reap, the joined watcher, and Tokio runtime teardown.
pub(crate) const UPDATE_CHECK_EXIT_BUDGET: Duration = Duration::from_millis(2_500);

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CurlRequestObservation {
    Spawned(u32),
    Reaped { pid: u32, status: ExitStatus },
    WatcherJoined,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReleaseSelection {
    pub version: SemVersion,
    pub archive_name: String,
    pub archive_url: String,
    pub checksum_name: String,
    pub checksum_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DiscoveryOutcome {
    Current(SemVersion),
    Update(ReleaseSelection),
}

#[derive(Debug, Clone)]
pub(crate) struct ReleaseSource {
    pub api_base: String,
    pub repository: String,
    /// Tests may point the same strict client at a loopback HTTP fixture.
    /// Production always leaves this false.
    pub allow_http: bool,
}

impl ReleaseSource {
    pub fn production() -> Result<Self, UpdateError> {
        let repository = repository_slug(env!("CARGO_PKG_REPOSITORY"))?;
        Ok(Self {
            api_base: "https://api.github.com".to_owned(),
            repository,
            allow_http: false,
        })
    }
}

pub(crate) trait UpdateTransport {
    fn get_bytes(&mut self, url: &str, limit: usize) -> Result<Vec<u8>, UpdateError>;
    fn download(&mut self, url: &str, path: &Path, limit: u64) -> Result<(), UpdateError>;
}

pub(crate) struct CurlTransport {
    token: Option<String>,
    cancellation: Option<DiscoveryCancellation>,
    #[cfg(test)]
    request_observer: Option<Arc<dyn Fn(CurlRequestObservation) + Send + Sync>>,
}

impl CurlTransport {
    pub fn from_environment() -> Self {
        let token = std::env::var("HAIDER_GITHUB_TOKEN")
            .ok()
            .filter(|token| !token.is_empty())
            .or_else(|| {
                std::env::var("GITHUB_TOKEN")
                    .ok()
                    .filter(|token| !token.is_empty())
            });
        Self {
            token,
            cancellation: None,
            #[cfg(test)]
            request_observer: None,
        }
    }

    pub fn with_cancellation(mut self, cancellation: DiscoveryCancellation) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn without_token() -> Self {
        Self {
            token: None,
            cancellation: None,
            request_observer: None,
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn with_token_for_test(token: &str) -> Self {
        Self {
            token: Some(token.to_owned()),
            cancellation: None,
            request_observer: None,
        }
    }

    #[cfg(test)]
    pub fn with_request_observer_for_test(
        mut self,
        observer: Arc<dyn Fn(CurlRequestObservation) + Send + Sync>,
    ) -> Self {
        self.request_observer = Some(observer);
        self
    }

    fn command(
        &self,
        url: &str,
        accept: &str,
        authenticated: bool,
        follow_redirects: bool,
    ) -> Command {
        let mut command = Command::new(CURL);
        // The parent reads the documented token variables once; curl receives
        // authentication only through its private stdin header stream.
        command
            .env_remove("HAIDER_GITHUB_TOKEN")
            .env_remove("GITHUB_TOKEN");
        command.args([
            "--fail",
            "--silent",
            "--show-error",
            "--connect-timeout",
            "15",
            "--max-time",
            "120",
            "--proto",
            "=http,https",
            "--user-agent",
            concat!("haider/", env!("CARGO_PKG_VERSION")),
            "--header",
            "X-GitHub-Api-Version: 2022-11-28",
        ]);
        if follow_redirects {
            command.args(["--location", "--max-redirs", "5", "--proto-redir", "=https"]);
        }
        let accept_header = format!("Accept: {accept}");
        command.args(["--header", accept_header.as_str()]);
        if authenticated && self.token.is_some() {
            // `@-` keeps the secret out of argv, the environment, files, and
            // diagnostics. Authenticated commands never auto-follow redirects.
            command.args(["--header", "@-"]);
            command.stdin(Stdio::piped());
        } else {
            command.stdin(Stdio::null());
        }
        command.arg(url);
        command
    }

    fn write_auth(&self, child: &mut std::process::Child) -> Result<(), UpdateError> {
        let Some(token) = &self.token else {
            return Ok(());
        };
        if token.bytes().any(|byte| byte.is_ascii_control()) {
            return Err(UpdateError::Refused(
                "GitHub token contains control characters".into(),
            ));
        }
        let mut stdin = child.stdin.take().ok_or_else(|| {
            UpdateError::Internal("curl authentication pipe was not created".into())
        })?;
        stdin
            .write_all(format!("Authorization: Bearer {token}\n").as_bytes())
            .map_err(|error| UpdateError::io("write curl authentication header", error))?;
        drop(stdin);
        Ok(())
    }

    fn download_unauthenticated(
        &self,
        url: &str,
        path: &Path,
        limit: u64,
    ) -> Result<(), UpdateError> {
        let mut file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path)
            .map_err(|error| UpdateError::io("open staged download", error))?;
        let mut command = self.command(url, "application/octet-stream", false, true);
        command
            .args(["--max-filesize", &limit.to_string()])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command
            .spawn()
            .map_err(|error| UpdateError::network(format!("cannot start download: {error}")))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| UpdateError::Internal("curl download pipe was not created".into()))?;
        let mut copied = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = match stdout.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(UpdateError::network(format!(
                        "release download failed: {error}"
                    )));
                }
            };
            copied = copied.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
            if copied > limit {
                let _ = child.kill();
                let _ = child.wait();
                return Err(UpdateError::Network(
                    "release download exceeded its configured bound".into(),
                ));
            }
            if let Err(error) = file.write_all(&buffer[..read]) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(UpdateError::io("write staged download", error));
            }
        }
        let status = child
            .wait()
            .map_err(|error| UpdateError::network(format!("download failed: {error}")))?;
        if !status.success() {
            return Err(UpdateError::Network(
                "release download is unavailable".into(),
            ));
        }
        file.sync_all()
            .map_err(|error| UpdateError::io("fsync staged download", error))?;
        validate_download(path, limit)
    }

    fn download_api_asset(&self, url: &str, path: &Path, limit: u64) -> Result<(), UpdateError> {
        let authenticated = self.token.is_some();
        let header_name = format!(
            ".{}.headers",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("download")
        );
        let header_path = path.with_file_name(header_name);
        let mut header_options = OpenOptions::new();
        header_options.write(true).create_new(true);
        haider_platform::configure_file_mode(&mut header_options, 0o600);
        header_options
            .open(&header_path)
            .map_err(|error| UpdateError::io("create private response headers", error))?;
        let mut file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path)
            .map_err(|error| UpdateError::io("open staged API download", error))?;
        let mut command = self.command(url, "application/octet-stream", authenticated, false);
        command
            .args(["--max-filesize", &limit.to_string(), "--dump-header"])
            .arg(&header_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command
            .spawn()
            .map_err(|error| UpdateError::network(format!("cannot start download: {error}")))?;
        if authenticated {
            self.write_auth(&mut child)?;
        }
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| UpdateError::Internal("curl API body pipe was not created".into()))?;
        let mut copied = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = match stdout.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(UpdateError::network(format!(
                        "release API body failed: {error}"
                    )));
                }
            };
            copied = copied.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
            if copied > limit {
                let _ = child.kill();
                let _ = child.wait();
                return Err(UpdateError::Network(
                    "release API body exceeded its configured bound".into(),
                ));
            }
            if let Err(error) = file.write_all(&buffer[..read]) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(UpdateError::io("write staged API download", error));
            }
        }
        let status = child
            .wait()
            .map_err(|error| UpdateError::network(format!("download failed: {error}")))?;
        if !status.success() {
            return Err(UpdateError::Network(
                "release download is unavailable".into(),
            ));
        }
        file.sync_all()
            .map_err(|error| UpdateError::io("fsync staged API download", error))?;
        let header_bytes = std::fs::read(&header_path)
            .map_err(|error| UpdateError::io("read release response headers", error))?;
        std::fs::remove_file(&header_path)
            .map_err(|error| UpdateError::io("remove release response headers", error))?;
        if header_bytes.len() > 8192 {
            return Err(UpdateError::Network(
                "release response headers exceeded their bound".into(),
            ));
        }
        let (http_status, redirect) = parse_response_headers(&header_bytes)?;
        if (200..300).contains(&http_status) {
            return validate_download(path, limit);
        }
        if (300..400).contains(&http_status) {
            let redirect = redirect.ok_or_else(|| {
                UpdateError::Network("release asset redirect had no target".into())
            })?;
            validate_asset_redirect(&redirect)?;
            // Authentication is deliberately absent from this second process.
            return self.download_unauthenticated(&redirect, path, limit);
        }
        Err(UpdateError::Network(
            "release asset API returned an unexpected status".into(),
        ))
    }

    fn request_bytes(
        &self,
        url: &str,
        limit: usize,
        authenticated: bool,
    ) -> Result<Vec<u8>, UpdateError> {
        if let Some(cancellation) = &self.cancellation {
            return self.request_bytes_cancellable(url, limit, authenticated, cancellation.clone());
        }
        validate_transport_url(url)?;
        // Authenticated release-list requests never auto-follow redirects.
        let mut command = self.command(url, "application/vnd.github+json", authenticated, false);
        command.stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = command.spawn().map_err(|error| {
            UpdateError::network(format!("cannot start release request: {error}"))
        })?;
        if authenticated {
            self.write_auth(&mut child)?;
        }
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| UpdateError::Internal("curl response pipe was not created".into()))?;
        let take_limit = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
        let mut body = Vec::new();
        stdout
            .by_ref()
            .take(take_limit)
            .read_to_end(&mut body)
            .map_err(|error| UpdateError::network(format!("release response failed: {error}")))?;
        if body.len() > limit {
            let _ = child.kill();
            let _ = child.wait();
            return Err(UpdateError::Network(
                "release response exceeded the configured bound".into(),
            ));
        }
        let status = child
            .wait()
            .map_err(|error| UpdateError::network(format!("release request failed: {error}")))?;
        if !status.success() {
            return Err(UpdateError::Network(
                "release API or download is unavailable".into(),
            ));
        }
        Ok(body)
    }

    fn request_bytes_cancellable(
        &self,
        url: &str,
        limit: usize,
        authenticated: bool,
        cancellation: DiscoveryCancellation,
    ) -> Result<Vec<u8>, UpdateError> {
        // This preflight runs for every release page, so a closed TUI cannot
        // start another curl after a preceding response completes.
        if cancellation() {
            return Err(discovery_cancelled());
        }
        validate_transport_url(url)?;
        if authenticated
            && self
                .token
                .as_ref()
                .is_some_and(|token| token.bytes().any(|byte| byte.is_ascii_control()))
        {
            return Err(UpdateError::Refused(
                "GitHub token contains control characters".into(),
            ));
        }
        let mut command = self.command(url, "application/vnd.github+json", authenticated, false);
        command.stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = RequestChild {
            process: command.spawn().map_err(|error| {
                UpdateError::network(format!("cannot start release request: {error}"))
            })?,
            #[cfg(test)]
            observer: self.request_observer.clone(),
            #[cfg(test)]
            reaped_observed: false,
        };
        #[cfg(test)]
        if let Some(observer) = &self.request_observer {
            observer(CurlRequestObservation::Spawned(child.process.id()));
        }
        let mut stdout =
            child.process.stdout.take().ok_or_else(|| {
                UpdateError::Internal("curl response pipe was not created".into())
            })?;
        let stdin = child.process.stdin.take();
        let watcher = RequestWatcher::spawn(
            child,
            cancellation.clone(),
            #[cfg(test)]
            self.request_observer.clone(),
        )?;
        let response = (|| {
            if authenticated && let Some(token) = &self.token {
                let mut stdin = stdin.ok_or_else(|| {
                    UpdateError::Internal("curl authentication pipe was not created".into())
                })?;
                stdin
                    .write_all(format!("Authorization: Bearer {token}\n").as_bytes())
                    .map_err(|error| UpdateError::io("write curl authentication header", error))?;
            }
            let take_limit = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
            let mut body = Vec::new();
            stdout
                .by_ref()
                .take(take_limit)
                .read_to_end(&mut body)
                .map_err(|error| {
                    UpdateError::network(format!("release response failed: {error}"))
                })?;
            if body.len() > limit {
                return Err(UpdateError::Network(
                    "release response exceeded the configured bound".into(),
                ));
            }
            Ok(body)
        })();
        // Even authentication/read/bound errors stop and JOIN the owner. The
        // join completes only after curl has been waited, never detached.
        let status = watcher.finish(response.is_err());
        let body = response?;
        let status = status?;
        if cancellation() {
            return Err(discovery_cancelled());
        }
        if !status.success() {
            return Err(UpdateError::Network(
                "release API or download is unavailable".into(),
            ));
        }
        Ok(body)
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn authenticated_get_for_test(
        &self,
        url: &str,
        limit: usize,
    ) -> Result<Vec<u8>, UpdateError> {
        self.request_bytes(url, limit, true)
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn authenticated_asset_for_test(
        &self,
        url: &str,
        path: &Path,
        limit: u64,
    ) -> Result<(), UpdateError> {
        self.download_api_asset(url, path, limit)
    }
}

fn discovery_cancelled() -> UpdateError {
    UpdateError::Refused("update check cancelled because its TUI closed".into())
}

/// Owns the actual process before and during watcher construction. Every
/// unwinding/error path kills and waits; a successfully waited child is a no-op.
struct RequestChild {
    process: Child,
    #[cfg(test)]
    observer: Option<Arc<dyn Fn(CurlRequestObservation) + Send + Sync>>,
    #[cfg(test)]
    reaped_observed: bool,
}

impl RequestChild {
    fn terminate(&mut self) -> Result<(), UpdateError> {
        let _ = self.process.kill();
        let status = self.process.wait().map_err(|error| {
            UpdateError::network(format!("cannot reap release request: {error}"))
        })?;
        self.observe_reaped(status);
        Ok(())
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        let status = self.process.try_wait()?;
        if let Some(status) = status {
            self.observe_reaped(status);
        }
        Ok(status)
    }

    fn observe_reaped(&mut self, _status: ExitStatus) {
        #[cfg(test)]
        if !self.reaped_observed {
            self.reaped_observed = true;
            if let Some(observer) = &self.observer {
                // This is the actual owning Child's successful wait receipt,
                // never a post-reap PID query or inferred permission failure.
                observer(CurlRequestObservation::Reaped {
                    pid: self.process.id(),
                    status: _status,
                });
            }
        }
    }
}

impl Drop for RequestChild {
    fn drop(&mut self) {
        if !matches!(self.try_wait(), Ok(Some(_))) {
            let _ = self.terminate();
        }
    }
}

/// The watcher exclusively owns curl; the blocking caller owns stdout. This
/// avoids holding a child mutex across wait(), which would block cancellation.
struct RequestWatcher {
    stop: mpsc::Sender<()>,
    worker: Option<std::thread::JoinHandle<Result<ExitStatus, UpdateError>>>,
    #[cfg(test)]
    observer: Option<Arc<dyn Fn(CurlRequestObservation) + Send + Sync>>,
}

impl RequestWatcher {
    fn spawn(
        mut child: RequestChild,
        cancellation: DiscoveryCancellation,
        #[cfg(test)] observer: Option<Arc<dyn Fn(CurlRequestObservation) + Send + Sync>>,
    ) -> Result<Self, UpdateError> {
        let (stop, stopped) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("haider-update-curl-owner".into())
            .spawn(move || {
                loop {
                    if cancellation() {
                        child.terminate()?;
                        return Err(discovery_cancelled());
                    }
                    if let Some(status) = child.try_wait().map_err(|error| {
                        UpdateError::network(format!("release request failed: {error}"))
                    })? {
                        return Ok(status);
                    }
                    match stopped.recv_timeout(UPDATE_CHECK_EXIT_BUDGET / 10) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                            child.terminate()?;
                            return Err(UpdateError::Network("release request aborted".into()));
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
            })
            .map_err(|error| {
                // On spawn failure the dropped closure retains RequestChild's
                // kill/wait guard; no ownerless process escapes this return.
                UpdateError::io("start release request cancellation watcher", error)
            })?;
        Ok(Self {
            stop,
            worker: Some(worker),
            #[cfg(test)]
            observer,
        })
    }

    fn finish(mut self, abort: bool) -> Result<ExitStatus, UpdateError> {
        if abort {
            let _ = self.stop.send(());
        }
        self.join()
    }

    fn join(&mut self) -> Result<ExitStatus, UpdateError> {
        let worker = self.worker.take().ok_or_else(|| {
            UpdateError::Internal("release request watcher was already joined".into())
        })?;
        let outcome = worker.join().map_err(|_| {
            UpdateError::Internal("release request cancellation watcher failed".into())
        });
        #[cfg(test)]
        if let Some(observer) = &self.observer {
            observer(CurlRequestObservation::WatcherJoined);
        }
        outcome?
    }
}

impl Drop for RequestWatcher {
    fn drop(&mut self) {
        if self.worker.is_some() {
            let _ = self.stop.send(());
            let _ = self.join();
        }
    }
}

impl UpdateTransport for CurlTransport {
    fn get_bytes(&mut self, url: &str, limit: usize) -> Result<Vec<u8>, UpdateError> {
        let authenticated = self.token.is_some() && is_github_api_url(url);
        self.request_bytes(url, limit, authenticated)
    }

    fn download(&mut self, url: &str, path: &Path, limit: u64) -> Result<(), UpdateError> {
        validate_transport_url(url)?;
        if is_github_asset_api_url(url) {
            self.download_api_asset(url, path, limit)
        } else {
            // Browser-download URLs and loopback fixtures never receive the
            // GitHub token. Redirects are HTTPS-only.
            self.download_unauthenticated(url, path, limit)
        }
    }
}

fn validate_download(path: &Path, limit: u64) -> Result<(), UpdateError> {
    let length = std::fs::metadata(path)
        .map_err(|error| UpdateError::io("inspect completed download", error))?
        .len();
    if length == 0 || length > limit {
        return Err(UpdateError::Network(
            "release download was empty or exceeded its bound".into(),
        ));
    }
    Ok(())
}

fn parse_response_headers(output: &[u8]) -> Result<(u16, Option<String>), UpdateError> {
    let output = std::str::from_utf8(output)
        .map_err(|_| UpdateError::Network("release response headers were not UTF-8".into()))?;
    let mut status = None;
    let mut redirect = None;
    for line in output.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with("HTTP/") {
            status = line
                .split_whitespace()
                .nth(1)
                .and_then(|value| value.parse::<u16>().ok());
            redirect = None;
        } else if line
            .get(..9)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("location:"))
        {
            redirect = Some(line[9..].trim().to_owned());
        }
    }
    status
        .map(|status| (status, redirect.filter(|value| !value.is_empty())))
        .ok_or_else(|| UpdateError::Network("release response had no HTTP status".into()))
}

fn is_github_api_url(url: &str) -> bool {
    url.starts_with("https://api.github.com/")
}

fn is_github_asset_api_url(url: &str) -> bool {
    is_github_api_url(url) && url.contains("/releases/assets/")
}

fn validate_asset_redirect(url: &str) -> Result<(), UpdateError> {
    validate_transport_url(url)?;
    if [
        "https://release-assets.githubusercontent.com/",
        "https://objects.githubusercontent.com/",
        "https://github-releases.githubusercontent.com/",
    ]
    .iter()
    .any(|prefix| url.starts_with(prefix))
    {
        Ok(())
    } else {
        Err(UpdateError::Refused(format!(
            "release asset redirect is outside trusted GitHub storage: {}",
            redact_url(url)
        )))
    }
}

fn validate_transport_url(url: &str) -> Result<(), UpdateError> {
    if url.is_empty()
        || url
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b' ')
    {
        Err(UpdateError::Refused("release URL is malformed".into()))
    } else {
        Ok(())
    }
}

pub(crate) fn compiled_target() -> Result<&'static str, UpdateError> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Ok("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Ok("x86_64-apple-darwin")
    } else {
        Err(UpdateError::Refused(
            "self-update is supported only by packaged macOS binaries".into(),
        ))
    }
}

pub(crate) fn discover<T: UpdateTransport>(
    transport: &mut T,
    source: &ReleaseSource,
    current: &str,
    target: &str,
) -> Result<DiscoveryOutcome, UpdateError> {
    validate_source(source)?;
    let current = SemVersion::parse(current).map_err(|message| {
        UpdateError::Internal(format!("running binary has an invalid version: {message}"))
    })?;
    let mut releases = Vec::new();
    for page in 1..=MAX_RELEASE_PAGES {
        let url = format!(
            "{}/repos/{}/releases?per_page={RELEASE_PAGE_SIZE}&page={page}",
            source.api_base.trim_end_matches('/'),
            source.repository
        );
        let body = transport.get_bytes(&url, MAX_RELEASE_RESPONSE)?;
        let page_value: Value = serde_json::from_slice(&body).map_err(|error| {
            UpdateError::network(format!("invalid release API response: {error}"))
        })?;
        let page_releases = page_value.as_array().ok_or_else(|| {
            UpdateError::Network("release API returned a non-array response".into())
        })?;
        for release in page_releases {
            if release.get("draft").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            releases.push(parse_release(release)?);
        }
        if page_releases.len() < RELEASE_PAGE_SIZE {
            break;
        }
        if page == MAX_RELEASE_PAGES {
            return Err(UpdateError::Refused(
                "release listing exceeded the pagination bound".into(),
            ));
        }
    }
    releases.sort_by(|left, right| left.version.cmp(&right.version));
    if releases
        .windows(2)
        .any(|pair| pair[0].version == pair[1].version)
    {
        return Err(UpdateError::Refused(
            "release listing contains duplicate SemVer precedence".into(),
        ));
    }
    let latest = releases
        .into_iter()
        .next_back()
        .ok_or_else(|| UpdateError::Refused("no published releases were found".into()))?;

    let archive_name = format!("haider-v{}-{target}.tar.xz", latest.version);
    let checksum_name = format!("{archive_name}.sha256");
    let archive_urls = latest.asset_urls(&archive_name);
    let checksum_urls = latest.asset_urls(&checksum_name);
    if archive_urls.len() != 1 || checksum_urls.len() != 1 {
        return Err(UpdateError::Refused(format!(
            "release v{} does not contain exactly one `{archive_name}` and one `{checksum_name}`",
            latest.version
        )));
    }
    validate_asset_url(source, &archive_urls[0])?;
    validate_asset_url(source, &checksum_urls[0])?;

    match latest.version.cmp(&current) {
        Ordering::Less => Err(UpdateError::Refused(format!(
            "latest published release v{} is older than running version v{current}",
            latest.version
        ))),
        Ordering::Equal => Ok(DiscoveryOutcome::Current(current)),
        Ordering::Greater => Ok(DiscoveryOutcome::Update(ReleaseSelection {
            version: latest.version,
            archive_name,
            archive_url: archive_urls[0].clone(),
            checksum_name,
            checksum_url: checksum_urls[0].clone(),
        })),
    }
}

#[derive(Debug)]
struct ParsedRelease {
    version: SemVersion,
    assets: Vec<(String, String)>,
}

impl ParsedRelease {
    fn asset_urls(&self, name: &str) -> Vec<String> {
        self.assets
            .iter()
            .filter(|(asset, _)| asset == name)
            .map(|(_, url)| url.clone())
            .collect()
    }
}

fn parse_release(value: &Value) -> Result<ParsedRelease, UpdateError> {
    if value.get("draft").and_then(Value::as_bool).is_none() {
        return Err(UpdateError::Network(
            "release API item has no boolean draft field".into(),
        ));
    }
    let tag = value
        .get("tag_name")
        .and_then(Value::as_str)
        .ok_or_else(|| UpdateError::Refused("published release has no tag_name".into()))?;
    let raw = tag.strip_prefix('v').ok_or_else(|| {
        UpdateError::Refused(format!("published release tag `{tag}` is not v<semver>"))
    })?;
    let version = SemVersion::parse(raw).map_err(|message| {
        UpdateError::Refused(format!(
            "published release tag `{tag}` is malformed: {message}"
        ))
    })?;
    let assets = value
        .get("assets")
        .and_then(Value::as_array)
        .ok_or_else(|| UpdateError::Refused(format!("release `{tag}` has no asset list")))?
        .iter()
        .map(|asset| {
            let name = asset.get("name").and_then(Value::as_str).ok_or_else(|| {
                UpdateError::Refused(format!("release `{tag}` has an unnamed asset"))
            })?;
            // The API asset endpoint works for both public and token-backed
            // private repositories when requested as octet-stream. Retain a
            // browser URL fallback for fixture/forward compatibility.
            let url = asset
                .get("url")
                .and_then(Value::as_str)
                .or_else(|| asset.get("browser_download_url").and_then(Value::as_str))
                .ok_or_else(|| {
                    UpdateError::Refused(format!("release `{tag}` asset `{name}` has no URL"))
                })?;
            Ok((name.to_owned(), url.to_owned()))
        })
        .collect::<Result<Vec<_>, UpdateError>>()?;
    Ok(ParsedRelease { version, assets })
}

fn validate_source(source: &ReleaseSource) -> Result<(), UpdateError> {
    validate_transport_url(&source.api_base)?;
    let prefix = if source.allow_http {
        ["http://", "https://"].as_slice()
    } else {
        ["https://"].as_slice()
    };
    if !prefix
        .iter()
        .any(|prefix| source.api_base.starts_with(prefix))
    {
        return Err(UpdateError::Refused(
            "release API URL is not allowed".into(),
        ));
    }
    if source.repository.split('/').count() != 2
        || source.repository.split('/').any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
    {
        return Err(UpdateError::Internal(
            "package repository metadata is not owner/repository".into(),
        ));
    }
    Ok(())
}

fn validate_asset_url(source: &ReleaseSource, url: &str) -> Result<(), UpdateError> {
    validate_transport_url(url)?;
    if source.allow_http {
        if url.starts_with("http://") || url.starts_with("https://") {
            return Ok(());
        }
    } else {
        let browser_prefix = format!(
            "https://github.com/{}/releases/download/",
            source.repository
        );
        let api_prefix = format!(
            "https://api.github.com/repos/{}/releases/assets/",
            source.repository
        );
        if url.starts_with(&browser_prefix) || url.starts_with(&api_prefix) {
            return Ok(());
        }
    }
    Err(UpdateError::Refused(format!(
        "release asset URL is outside the trusted repository: {}",
        redact_url(url)
    )))
}

fn redact_url(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or("<invalid-url>")
}

fn repository_slug(repository: &str) -> Result<String, UpdateError> {
    let slug = repository
        .strip_prefix("https://github.com/")
        .and_then(|rest| rest.strip_suffix(".git").or(Some(rest)))
        .map(|rest| rest.trim_end_matches('/'))
        .ok_or_else(|| UpdateError::Internal("CARGO_PKG_REPOSITORY is not a GitHub URL".into()))?;
    if slug.split('/').count() != 2 || slug.split('/').any(|part| part.is_empty()) {
        return Err(UpdateError::Internal(
            "CARGO_PKG_REPOSITORY is not a GitHub owner/repository URL".into(),
        ));
    }
    Ok(slug.to_owned())
}

#[derive(Debug, Clone)]
pub(crate) struct SemVersion {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Vec<Identifier>,
    build: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Identifier {
    Numeric(u64),
    Text(String),
}

impl SemVersion {
    pub fn parse(input: &str) -> Result<Self, String> {
        if input.is_empty() || input.trim() != input {
            return Err("empty or whitespace-padded version".into());
        }
        let (without_build, build) = split_once(input, '+');
        let (core, pre) = split_once(without_build, '-');
        let mut numbers = core.split('.');
        let major = parse_core_number(numbers.next(), "major")?;
        let minor = parse_core_number(numbers.next(), "minor")?;
        let patch = parse_core_number(numbers.next(), "patch")?;
        if numbers.next().is_some() {
            return Err("core has more than three components".into());
        }
        let pre = match pre {
            Some(value) => parse_identifiers(value, true)?,
            None => Vec::new(),
        };
        let build = match build {
            Some(value) => parse_build_identifiers(value)?,
            None => Vec::new(),
        };
        Ok(Self {
            major,
            minor,
            patch,
            pre,
            build,
        })
    }
}

fn split_once(input: &str, separator: char) -> (&str, Option<&str>) {
    input
        .split_once(separator)
        .map_or((input, None), |(left, right)| (left, Some(right)))
}

fn parse_core_number(value: Option<&str>, name: &str) -> Result<u64, String> {
    let value = value.ok_or_else(|| format!("missing {name} component"))?;
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err(format!("invalid {name} component"));
    }
    value
        .parse()
        .map_err(|_| format!("{name} component is too large"))
}

fn parse_identifiers(value: &str, prerelease: bool) -> Result<Vec<Identifier>, String> {
    value
        .split('.')
        .map(|part| {
            if part.is_empty()
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            {
                return Err("invalid empty or non-ASCII identifier".into());
            }
            if part.bytes().all(|byte| byte.is_ascii_digit()) {
                if prerelease && part.len() > 1 && part.starts_with('0') {
                    return Err("numeric prerelease identifier has a leading zero".into());
                }
                let number = part
                    .parse()
                    .map_err(|_| "numeric identifier is too large".to_owned())?;
                Ok(Identifier::Numeric(number))
            } else {
                Ok(Identifier::Text(part.to_owned()))
            }
        })
        .collect()
}

fn parse_build_identifiers(value: &str) -> Result<Vec<String>, String> {
    value
        .split('.')
        .map(|part| {
            if part.is_empty()
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            {
                Err("invalid empty or non-ASCII identifier".into())
            } else {
                Ok(part.to_owned())
            }
        })
        .collect()
}

impl PartialEq for SemVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for SemVersion {}

impl Ord for SemVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => compare_pre(&self.pre, &other.pre),
            })
    }
}

impl PartialOrd for SemVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn compare_pre(left: &[Identifier], right: &[Identifier]) -> Ordering {
    for (left, right) in left.iter().zip(right) {
        let ordering = match (left, right) {
            (Identifier::Numeric(left), Identifier::Numeric(right)) => left.cmp(right),
            (Identifier::Numeric(_), Identifier::Text(_)) => Ordering::Less,
            (Identifier::Text(_), Identifier::Numeric(_)) => Ordering::Greater,
            (Identifier::Text(left), Identifier::Text(right)) => left.cmp(right),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.len().cmp(&right.len())
}

impl fmt::Display for SemVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            formatter.write_str("-")?;
            for (index, identifier) in self.pre.iter().enumerate() {
                if index != 0 {
                    formatter.write_str(".")?;
                }
                match identifier {
                    Identifier::Numeric(number) => write!(formatter, "{number}")?,
                    Identifier::Text(text) => formatter.write_str(text)?,
                }
            }
        }
        if !self.build.is_empty() {
            write!(formatter, "+{}", self.build.join("."))?;
        }
        Ok(())
    }
}
