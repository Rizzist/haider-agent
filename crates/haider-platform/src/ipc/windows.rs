use super::{Endpoint, EndpointError, PeerCredentials};
use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use tokio::sync::Mutex;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER, ERROR_PIPE_BUSY, HANDLE,
};
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, IsValidSid, PSID, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

const BIND_RETRY_WINDOW: Duration = Duration::from_secs(3);
const BIND_RETRY_INTERVAL: Duration = Duration::from_millis(50);

pub type IpcReadHalf = tokio::io::ReadHalf<IpcStream>;
pub type IpcWriteHalf = tokio::io::WriteHalf<IpcStream>;
pub type EndpointAddress = ();

/// A connected Windows named-pipe instance. Tokio uses distinct client and
/// server types, so this small enum presents one byte-stream type to callers.
#[derive(Debug)]
pub enum IpcStream {
    Client(NamedPipeClient),
    Server(NamedPipeServer),
}

impl AsyncRead for IpcStream {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Client(stream) => Pin::new(stream).poll_read(context, buffer),
            Self::Server(stream) => Pin::new(stream).poll_read(context, buffer),
        }
    }
}

impl AsyncWrite for IpcStream {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Client(stream) => Pin::new(stream).poll_write(context, bytes),
            Self::Server(stream) => Pin::new(stream).poll_write(context, bytes),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Client(stream) => Pin::new(stream).poll_flush(context),
            Self::Server(stream) => Pin::new(stream).poll_flush(context),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Client(stream) => Pin::new(stream).poll_shutdown(context),
            Self::Server(stream) => Pin::new(stream).poll_shutdown(context),
        }
    }
}

pub struct BoundEndpoint {
    endpoint: Endpoint,
    listener: Mutex<Option<NamedPipeServer>>,
}

impl BoundEndpoint {
    pub async fn bind(endpoint: &Endpoint, _runtime_dir: &Path) -> Result<Self, EndpointError> {
        let name = pipe_name(endpoint).map_err(|error| {
            EndpointError::io("resolve Windows named-pipe name", endpoint.address(), error)
        })?;
        let server = bind_first_instance(name, BIND_RETRY_WINDOW)
            .await
            .map_err(|error| {
                EndpointError::io("bind Windows named pipe", endpoint.address(), error)
            })?;
        Ok(Self {
            endpoint: endpoint.clone(),
            listener: Mutex::new(Some(server)),
        })
    }

    #[must_use]
    pub fn endpoint(&self) -> Endpoint {
        self.endpoint.clone()
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        self.endpoint.address()
    }

    /// Windows named pipes are created with the process token's default DACL;
    /// there is no Unix UID value to compare.
    #[must_use]
    pub fn owner_uid(&self) -> u32 {
        u32::MAX
    }

    pub fn cleanup(&mut self) -> Result<(), EndpointError> {
        Ok(())
    }

    pub async fn accept(&self) -> io::Result<(IpcStream, EndpointAddress)> {
        // Borrow the pending instance across `connect` instead of taking it
        // out. The daemon awaits `accept` inside `tokio::select!`; cancellation
        // must leave the instance installed for the next loop iteration.
        let mut listener = self.listener.lock().await;
        let server = listener.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "daemon listener is closed")
        })?;
        server.connect().await?;

        // Queue the next instance before handing this connected instance to
        // the daemon. This is the named-pipe equivalent of a reusable listener.
        let next = ServerOptions::new().create(pipe_name(&self.endpoint)?)?;
        let connected = listener.replace(next).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "daemon listener is closed")
        })?;
        Ok((IpcStream::Server(connected), ()))
    }

    pub fn close_listener(&mut self) {
        self.listener.get_mut().take();
    }
}

async fn bind_first_instance(name: &str, retry_window: Duration) -> io::Result<NamedPipeServer> {
    let deadline = tokio::time::Instant::now() + retry_window;
    loop {
        // Keep FILE_FLAG_FIRST_PIPE_INSTANCE on every attempt: a live daemon
        // remains exclusive. Windows can retain the prior generation's name
        // briefly after its last server handle closes, so only the two pipe
        // rollover errors receive this bounded Unix-rebind analogue.
        match ServerOptions::new().first_pipe_instance(true).create(name) {
            Ok(server) => return Ok(server),
            Err(error)
                if retryable_bind_error(&error) && tokio::time::Instant::now() < deadline =>
            {
                let wake = (tokio::time::Instant::now() + BIND_RETRY_INTERVAL).min(deadline);
                tokio::time::sleep_until(wake).await;
            }
            Err(error) => return Err(error),
        }
    }
}

fn retryable_bind_error(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code)
            if code == ERROR_ACCESS_DENIED.cast_signed()
                || code == ERROR_PIPE_BUSY.cast_signed()
    )
}

fn pipe_name(endpoint: &Endpoint) -> io::Result<&str> {
    endpoint.address().to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows named-pipe endpoint is not valid UTF-8",
        )
    })
}

/// Windows named pipes leave no filesystem node behind when their owner
/// exits, so there is nothing to sweep: the kernel reclaims the instance with
/// the process. Present so callers stay platform-agnostic.
pub async fn sweep_stale_endpoints(_runtime_dir: &Path, _keep: Option<&Path>) -> usize {
    0
}

pub async fn connect(endpoint: impl AsRef<Path>) -> io::Result<IpcStream> {
    let name = endpoint.as_ref().to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows named-pipe endpoint is not valid UTF-8",
        )
    })?;
    ClientOptions::new().open(name).map(IpcStream::Client)
}

pub fn split(stream: IpcStream) -> (IpcReadHalf, IpcWriteHalf) {
    tokio::io::split(stream)
}

#[allow(unsafe_code)]
pub fn peer_credentials(stream: &IpcStream) -> io::Result<PeerCredentials> {
    use windows_sys::Win32::System::Pipes::{
        GetNamedPipeClientProcessId, GetNamedPipeServerProcessId,
    };

    let mut pid = 0_u32;
    let ok = unsafe {
        match stream {
            IpcStream::Client(pipe) => GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid),
            IpcStream::Server(pipe) => GetNamedPipeClientProcessId(pipe.as_raw_handle(), &mut pid),
        }
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Windows named-pipe peer reported process id zero",
        ));
    }
    let same_user = peer_process_has_current_user_sid(pid)?;
    Ok(PeerCredentials {
        pid: Some(pid),
        uid: u32::MAX,
        gid: u32::MAX,
        same_user,
    })
}

pub fn peer_is_owner(stream: &IpcStream, _owner_uid: u32) -> io::Result<bool> {
    peer_credentials(stream).map(|credentials| credentials.same_user)
}

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn new(handle: HANDLE, operation: &'static str) -> io::Result<Self> {
        if handle.is_null() {
            let error = io::Error::last_os_error();
            Err(io::Error::new(
                error.kind(),
                format!("{operation}: {error}"),
            ))
        } else {
            Ok(Self(handle))
        }
    }
}

#[allow(unsafe_code)]
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: OwnedHandle is constructed only for real owned process or
        // token handles and never wraps the GetCurrentProcess pseudo-handle.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct TokenUserBuffer {
    // TOKEN_USER contains pointer-aligned fields. usize storage preserves the
    // alignment required before casting the returned bytes.
    words: Vec<usize>,
}

impl TokenUserBuffer {
    #[allow(unsafe_code)]
    fn sid(&self) -> io::Result<PSID> {
        if self.words.len().saturating_mul(size_of::<usize>()) < size_of::<TOKEN_USER>() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Windows token returned a truncated TokenUser record",
            ));
        }
        // SAFETY: storage is usize-aligned, GetTokenInformation initialized at
        // least TOKEN_USER bytes, and storage remains alive for the SID use.
        let sid = unsafe { (*(self.words.as_ptr().cast::<TOKEN_USER>())).User.Sid };
        if sid.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Windows token returned a null user SID",
            ));
        }
        // SAFETY: the pointer is owned by the live TokenUser buffer.
        if unsafe { IsValidSid(sid) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Windows token returned an invalid user SID",
            ));
        }
        Ok(sid)
    }
}

#[allow(unsafe_code)]
fn open_process_token(process: HANDLE) -> io::Result<OwnedHandle> {
    let mut token = std::ptr::null_mut();
    // SAFETY: process is a live process handle or the documented current
    // process pseudo-handle, and token is a writable out pointer.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    OwnedHandle::new(token, "open Windows process token")
}

#[allow(unsafe_code)]
fn token_user(token: HANDLE) -> io::Result<TokenUserBuffer> {
    let mut required = 0_u32;
    // SAFETY: the null/zero first call is the documented size query.
    let first =
        unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut required) };
    if first != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows TokenUser size query unexpectedly succeeded without a buffer",
        ));
    }
    let size_error = io::Error::last_os_error();
    if size_error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER.cast_signed())
        || usize::try_from(required).unwrap_or(0) < size_of::<TOKEN_USER>()
    {
        return Err(size_error);
    }
    let required = usize::try_from(required).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows TokenUser size does not fit memory",
        )
    })?;
    let word_count = required.div_ceil(size_of::<usize>());
    let mut buffer = TokenUserBuffer {
        words: vec![0; word_count],
    };
    let byte_len = u32::try_from(buffer.words.len().saturating_mul(size_of::<usize>()))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "TokenUser buffer is too large"))?;
    let mut returned = 0_u32;
    // SAFETY: the aligned buffer is writable for byte_len bytes; the token is
    // live and queried only for TOKEN_QUERY information.
    if unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.words.as_mut_ptr().cast(),
            byte_len,
            &mut returned,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if returned < u32::try_from(size_of::<TOKEN_USER>()).unwrap_or(u32::MAX) || returned > byte_len
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows token returned an inconsistent TokenUser size",
        ));
    }
    buffer.sid()?;
    Ok(buffer)
}

#[allow(unsafe_code)]
fn token_user_sids_equal(left: &TokenUserBuffer, right: &TokenUserBuffer) -> io::Result<bool> {
    sids_equal(left.sid()?, right.sid()?)
}

#[allow(unsafe_code)]
fn sids_equal(left: PSID, right: PSID) -> io::Result<bool> {
    if left.is_null() || right.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cannot compare a null Windows SID",
        ));
    }
    if unsafe { IsValidSid(left) } == 0 || unsafe { IsValidSid(right) } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "cannot compare an invalid Windows SID",
        ));
    }
    // SAFETY: both pointers name valid SIDs kept alive by their callers.
    Ok(unsafe { EqualSid(left, right) } != 0)
}

#[allow(unsafe_code)]
fn peer_process_has_current_user_sid(pid: u32) -> io::Result<bool> {
    // There is a narrow PID-exit/reuse window before OpenProcess succeeds.
    // Once acquired, this handle pins the process identity for token lookup.
    // SAFETY: OpenProcess receives a concrete kernel-reported peer PID and no
    // inherited handle is requested.
    let peer_process = OwnedHandle::new(
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) },
        "open Windows named-pipe peer process",
    )?;
    let peer_token = open_process_token(peer_process.0)?;
    // SAFETY: GetCurrentProcess returns the documented pseudo-handle. It is
    // passed through but never wrapped or closed.
    let current_token = open_process_token(unsafe { GetCurrentProcess() })?;
    let peer_user = token_user(peer_token.0)?;
    let current_user = token_user(current_token.0)?;
    token_user_sids_equal(&peer_user, &current_user)
}

pub fn write_immediate(stream: &IpcStream, bytes: &[u8]) {
    let mut written = 0;
    while written < bytes.len() {
        let result = match stream {
            IpcStream::Client(pipe) => pipe.try_write(&bytes[written..]),
            IpcStream::Server(pipe) => pipe.try_write(&bytes[written..]),
        };
        match result {
            Ok(0) | Err(_) => break,
            Ok(count) => written = written.saturating_add(count),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{
        BIND_RETRY_INTERVAL, BIND_RETRY_WINDOW, BoundEndpoint, bind_first_instance, connect,
        peer_credentials, peer_is_owner, pipe_name, retryable_bind_error, sids_equal,
    };
    use crate::ipc::{Endpoint, peer_credentials_are_owner};
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;
    use windows_sys::Win32::Security::{
        CreateWellKnownSid, PSID, SECURITY_MAX_SID_SIZE, WELL_KNOWN_SID_TYPE, WinLocalServiceSid,
        WinLocalSystemSid,
    };

    static NEXT_ENDPOINT: AtomicU64 = AtomicU64::new(1);

    fn unique_endpoint(label: &str) -> Endpoint {
        let unique = NEXT_ENDPOINT.fetch_add(1, Ordering::Relaxed);
        Endpoint::new(
            "ignored",
            &format!("{label}-{}-{unique}", std::process::id()),
        )
    }

    #[tokio::test]
    async fn dropped_listener_can_be_rebound_immediately() {
        let endpoint = unique_endpoint("drop-rebind");
        let bound = BoundEndpoint::bind(&endpoint, Path::new("ignored"))
            .await
            .expect("bind first named-pipe generation");

        drop(bound);

        let rebound = tokio::time::timeout(
            BIND_RETRY_WINDOW + Duration::from_secs(1),
            BoundEndpoint::bind(&endpoint, Path::new("ignored")),
        )
        .await
        .expect("immediate rebind finishes within the retry window")
        .expect("immediate rebind succeeds after listener drop");
        drop(rebound);
    }

    #[tokio::test]
    async fn bind_retries_until_held_name_is_released_within_window() {
        let endpoint = unique_endpoint("retry-window");
        let held = BoundEndpoint::bind(&endpoint, Path::new("ignored"))
            .await
            .expect("bind held named-pipe generation");
        let mut retrying = Box::pin(BoundEndpoint::bind(&endpoint, Path::new("ignored")));

        assert!(
            tokio::time::timeout(BIND_RETRY_INTERVAL / 2, &mut retrying)
                .await
                .is_err(),
            "the polled competing bind must wait while the first instance is held"
        );
        drop(held);

        let rebound = tokio::time::timeout(BIND_RETRY_WINDOW, retrying)
            .await
            .expect("retrying bind finishes within its bounded window")
            .expect("retrying bind succeeds after the held name is released");
        drop(rebound);
    }

    #[tokio::test]
    async fn bind_retry_window_expires_while_name_remains_held() {
        let endpoint = unique_endpoint("retry-deadline");
        let held = BoundEndpoint::bind(&endpoint, Path::new("ignored"))
            .await
            .expect("bind held named-pipe generation");
        let retry_window = Duration::from_millis(125);
        let started = tokio::time::Instant::now();

        let result = tokio::time::timeout(
            retry_window + Duration::from_secs(1),
            bind_first_instance(
                pipe_name(&endpoint).expect("resolve retry test pipe name"),
                retry_window,
            ),
        )
        .await
        .expect("competing bind returns after its bounded retry window");
        let error = match result {
            Ok(server) => {
                drop(server);
                panic!("a held first pipe instance must exclude a competing bind");
            }
            Err(error) => error,
        };
        assert!(
            retryable_bind_error(&error),
            "unexpected bind error: {error}"
        );
        assert!(
            started.elapsed() >= retry_window,
            "a retryable bind error must not escape before the retry window"
        );
        drop(held);
    }

    #[tokio::test]
    async fn cancelled_accept_retains_the_pending_pipe_instance() {
        let endpoint = unique_endpoint("cancelled-accept");
        let mut bound = BoundEndpoint::bind(&endpoint, std::path::Path::new("ignored"))
            .await
            .expect("bind named pipe");

        assert!(
            tokio::time::timeout(Duration::from_millis(10), bound.accept())
                .await
                .is_err(),
            "the first accept must be cancelled while no client exists"
        );

        let client = connect(endpoint.address()).await.expect("connect client");
        let (server, ()) = tokio::time::timeout(Duration::from_secs(1), bound.accept())
            .await
            .expect("retained listener accepts before deadline")
            .expect("retained listener accepts");
        drop((client, server));
        bound.close_listener();
    }

    #[allow(unsafe_code)]
    fn well_known_sid(kind: WELL_KNOWN_SID_TYPE) -> Vec<usize> {
        let words = usize::try_from(SECURITY_MAX_SID_SIZE)
            .expect("SID ceiling fits usize")
            .div_ceil(size_of::<usize>());
        let mut storage = vec![0_usize; words];
        let mut bytes = SECURITY_MAX_SID_SIZE;
        // SAFETY: storage is aligned and writable for SECURITY_MAX_SID_SIZE;
        // a null domain SID is documented for well-known absolute SIDs.
        assert_ne!(
            unsafe {
                CreateWellKnownSid(
                    kind,
                    std::ptr::null_mut(),
                    storage.as_mut_ptr().cast(),
                    &mut bytes,
                )
            },
            0,
            "create well-known SID"
        );
        storage
    }

    #[test]
    fn sid_compare_accepts_same_sid_and_rejects_different_sid() {
        let system = well_known_sid(WinLocalSystemSid);
        let service = well_known_sid(WinLocalServiceSid);
        let system_sid: PSID = system.as_ptr().cast_mut().cast();
        let service_sid: PSID = service.as_ptr().cast_mut().cast();
        assert!(sids_equal(system_sid, system_sid).expect("same SID comparison"));
        assert!(!sids_equal(system_sid, service_sid).expect("different SID comparison"));
    }

    #[tokio::test]
    async fn same_process_pipe_peers_pass_token_owner_check_in_both_directions() {
        let endpoint = unique_endpoint("same-token-owner");
        let bound = BoundEndpoint::bind(&endpoint, Path::new("ignored"))
            .await
            .expect("bind named pipe");
        let (client, accepted) = tokio::join!(connect(endpoint.address()), bound.accept());
        let client = client.expect("connect same-process client");
        let (server, ()) = accepted.expect("accept same-process client");

        for stream in [&client, &server] {
            let credentials = peer_credentials(stream).expect("query peer token");
            assert_eq!(credentials.pid, Some(std::process::id()));
            assert!(peer_credentials_are_owner(&credentials, u32::MAX));
            assert!(peer_is_owner(stream, u32::MAX).expect("compare peer owner"));
        }
    }
}
