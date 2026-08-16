use super::{Endpoint, EndpointError, PeerCredentials};
use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use tokio::sync::Mutex;

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
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(name)
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

    use super::{BoundEndpoint, connect};
    use crate::ipc::Endpoint;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static NEXT_ENDPOINT: AtomicU64 = AtomicU64::new(1);

    #[tokio::test]
    async fn cancelled_accept_retains_the_pending_pipe_instance() {
        let unique = NEXT_ENDPOINT.fetch_add(1, Ordering::Relaxed);
        let endpoint = Endpoint::new(
            "ignored",
            &format!("cancelled-accept-{}-{unique}", std::process::id()),
        );
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
