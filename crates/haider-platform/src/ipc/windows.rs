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
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY};

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
    Ok(PeerCredentials {
        pid: (pid != 0).then_some(pid),
        uid: u32::MAX,
        gid: u32::MAX,
    })
}

pub fn peer_is_owner(_stream: &IpcStream, _owner_uid: u32) -> io::Result<bool> {
    // Access is enforced by the named pipe's token-derived DACL. A future
    // hardening pass can compare peer process tokens explicitly.
    Ok(true)
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
        pipe_name, retryable_bind_error,
    };
    use crate::ipc::Endpoint;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

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
}
