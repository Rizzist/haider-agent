use super::{
    Endpoint, EndpointError, IpcShutdownOutcome, OwnedDirectoryInspection, OwnedDirectoryPathState,
    OwnedDirectoryReceipt, OwnedDirectoryRemoval, PeerCredentials, PeerExitReason,
    PreparedRuntimeDirectory,
};
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle as _, OwnedHandle as ProcessHandle};
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard};
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
const PEER_LIVENESS_POLL: Duration = Duration::from_millis(250);
const NAMED_PIPE_PATH_CODE_UNITS: usize = 256;

pub type IpcReadHalf = tokio::io::ReadHalf<IpcStream>;
pub type IpcWriteHalf = tokio::io::WriteHalf<IpcStream>;
pub type EndpointAddress = ();

#[derive(Debug)]
enum PipeStream {
    Client(NamedPipeClient),
    Server(NamedPipeServer),
}

/// A connected Windows named-pipe instance. Tokio uses distinct client and
/// server types, so the shared slot presents one byte-stream type to callers
/// and gives synchronous close authority access to the owning handle.
#[derive(Debug)]
pub struct IpcStream {
    stream: Arc<StdMutex<Option<PipeStream>>>,
}

impl IpcStream {
    fn client(stream: NamedPipeClient) -> Self {
        Self::new(PipeStream::Client(stream))
    }

    fn server(stream: NamedPipeServer) -> Self {
        Self::new(PipeStream::Server(stream))
    }

    fn new(stream: PipeStream) -> Self {
        Self {
            stream: Arc::new(StdMutex::new(Some(stream))),
        }
    }

    fn lock_stream(&self) -> io::Result<StdMutexGuard<'_, Option<PipeStream>>> {
        self.stream
            .lock()
            .map_err(|_| io::Error::other("Windows named-pipe state lock poisoned"))
    }
}

impl Drop for IpcStream {
    fn drop(&mut self) {
        if take_pipe(&self.stream) == Some(PipeRole::Server) {
            eprintln!(
                "haiderd: ephemeral-lifecycle event=windows_pipe_instance_close trigger=stream_drop outcome=local_slot_emptied kernel_close=deferred_until_iocp_poll close_order=connection_task_then_pipe_instance"
            );
        }
    }
}

/// Shared ownership of the pipe slot retained outside the async tasks.
///
/// Taking this slot drops Tokio's named-pipe object and cancels its overlapped
/// operations, but mio retains handle-owning references until the IOCP driver
/// dequeues their completion packets. Therefore [`Self::request`] does not by
/// itself notify the peer or close the kernel handle: after a
/// [`IpcShutdownOutcome::LocalSlotEmptiedOnly`] result, the owning Tokio runtime
/// must be polled before peer-visible closure is guaranteed.
///
/// Follow-up for a reactor-independent protocol goodbye: the immediate writer
/// must first share a whole-frame exclusion gate with the async writer. The
/// current `try_write` loop can be partial or `WouldBlock`, and serializes only
/// each syscall, so injecting a frame here could interleave with an async frame.
pub struct IpcShutdown {
    stream: Arc<StdMutex<Option<PipeStream>>>,
}

/// Retains both the authenticated peer process and the accepted pipe instance.
/// Process signaling covers abrupt termination; polling the pipe covers an
/// explicit close by a client process that remains alive.
pub struct PeerExitWatcher {
    process: ProcessHandle,
    stream: Arc<StdMutex<Option<PipeStream>>>,
}

impl PeerExitWatcher {
    /// Waits until the peer process exits or its named-pipe instance disconnects.
    #[allow(unsafe_code)]
    pub async fn wait(self) -> io::Result<PeerExitReason> {
        use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::WaitForSingleObject;

        loop {
            // SAFETY: `self.process` owns a live process handle with
            // SYNCHRONIZE access for the full duration of this poll.
            let status = unsafe { WaitForSingleObject(self.process.as_raw_handle().cast(), 0) };
            match status {
                WAIT_OBJECT_0 => {
                    eprintln!(
                        "haiderd: ephemeral-lifecycle event=windows_peer_wait result=process_exit close_order=wait_then_pipe_instance"
                    );
                    let closed = take_pipe(&self.stream).is_some();
                    eprintln!(
                        "haiderd: ephemeral-lifecycle event=windows_pipe_instance_close trigger=process_exit outcome={} kernel_close=not_proven_until_iocp_poll",
                        if closed {
                            "local_slot_emptied"
                        } else {
                            "local_slot_already_empty"
                        }
                    );
                    return Ok(PeerExitReason::ProcessExited);
                }
                WAIT_TIMEOUT => {}
                WAIT_FAILED => {
                    let error = io::Error::last_os_error();
                    eprintln!(
                        "haiderd: ephemeral-lifecycle event=windows_peer_wait result=failed raw_os_error={:?}",
                        error.raw_os_error()
                    );
                    return Err(error);
                }
                other => {
                    eprintln!(
                        "haiderd: ephemeral-lifecycle event=windows_peer_wait result=unexpected wait_status={other}"
                    );
                    return Err(io::Error::other(format!(
                        "IPC peer process wait returned unexpected status {other}"
                    )));
                }
            }
            let connected = match pipe_peer_is_connected(&self.stream) {
                Ok(connected) => connected,
                Err(error) => {
                    eprintln!(
                        "haiderd: ephemeral-lifecycle event=windows_peer_wait result=pipe_probe_failed raw_os_error={:?}",
                        error.raw_os_error()
                    );
                    return Err(error);
                }
            };
            if !connected {
                // Empty the accepted server slot before the connection task
                // returns. This cancels its split read/write operations; mio
                // may retain kernel-handle references until IOCP is polled.
                eprintln!(
                    "haiderd: ephemeral-lifecycle event=windows_peer_wait result=connection_close close_order=probe_then_pipe_instance"
                );
                let closed = take_pipe(&self.stream).is_some();
                eprintln!(
                    "haiderd: ephemeral-lifecycle event=windows_pipe_instance_close trigger=connection_close outcome={} kernel_close=not_proven_until_iocp_poll",
                    if closed {
                        "local_slot_emptied"
                    } else {
                        "local_slot_already_empty"
                    }
                );
                return Ok(PeerExitReason::ConnectionClosed);
            }
            tokio::time::sleep(PEER_LIVENESS_POLL).await;
        }
    }
}

#[allow(unsafe_code)]
fn pipe_peer_is_connected(stream: &StdMutex<Option<PipeStream>>) -> io::Result<bool> {
    use windows_sys::Win32::Foundation::{
        ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED,
    };
    use windows_sys::Win32::System::Pipes::PeekNamedPipe;

    let stream = stream
        .lock()
        .map_err(|_| io::Error::other("Windows named-pipe state lock poisoned"))?;
    let Some(pipe) = stream.as_ref() else {
        return Ok(false);
    };
    let handle = match pipe {
        PipeStream::Client(pipe) => pipe.as_raw_handle(),
        PipeStream::Server(pipe) => pipe.as_raw_handle(),
    };
    // SAFETY: the locked stream owns this overlapped, read-capable pipe handle;
    // null output pointers request a non-consuming connection-state probe.
    if unsafe {
        PeekNamedPipe(
            handle.cast(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } != 0
    {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(code)
            if code == ERROR_BROKEN_PIPE.cast_signed()
                || code == ERROR_NO_DATA.cast_signed()
                || code == ERROR_PIPE_NOT_CONNECTED.cast_signed() =>
        {
            Ok(false)
        }
        _ => Err(error),
    }
}

impl IpcShutdown {
    pub fn request(&self) -> io::Result<IpcShutdownOutcome> {
        Ok(match take_pipe(&self.stream) {
            Some(_) => IpcShutdownOutcome::LocalSlotEmptiedOnly,
            None => IpcShutdownOutcome::AlreadyRequested,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PipeRole {
    Client,
    Server,
}

fn take_pipe(stream: &StdMutex<Option<PipeStream>>) -> Option<PipeRole> {
    let mut stream = match stream.lock() {
        Ok(stream) => stream,
        Err(poisoned) => poisoned.into_inner(),
    };
    stream.take().map(|pipe| match pipe {
        PipeStream::Client(_) => PipeRole::Client,
        PipeStream::Server(_) => PipeRole::Server,
    })
}

impl AsyncRead for IpcStream {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut stream = match self.get_mut().lock_stream() {
            Ok(stream) => stream,
            Err(error) => return Poll::Ready(Err(error)),
        };
        match stream.as_mut() {
            Some(PipeStream::Client(stream)) => Pin::new(stream).poll_read(context, buffer),
            Some(PipeStream::Server(stream)) => Pin::new(stream).poll_read(context, buffer),
            None => Poll::Ready(Err(closed_pipe_error())),
        }
    }
}

impl AsyncWrite for IpcStream {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut stream = match self.get_mut().lock_stream() {
            Ok(stream) => stream,
            Err(error) => return Poll::Ready(Err(error)),
        };
        match stream.as_mut() {
            Some(PipeStream::Client(stream)) => Pin::new(stream).poll_write(context, bytes),
            Some(PipeStream::Server(stream)) => Pin::new(stream).poll_write(context, bytes),
            None => Poll::Ready(Err(closed_pipe_error())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut stream = match self.get_mut().lock_stream() {
            Ok(stream) => stream,
            Err(error) => return Poll::Ready(Err(error)),
        };
        match stream.as_mut() {
            Some(PipeStream::Client(stream)) => Pin::new(stream).poll_flush(context),
            Some(PipeStream::Server(stream)) => Pin::new(stream).poll_flush(context),
            None => Poll::Ready(Err(closed_pipe_error())),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut stream = match self.get_mut().lock_stream() {
            Ok(stream) => stream,
            Err(error) => return Poll::Ready(Err(error)),
        };
        match stream.as_mut() {
            Some(PipeStream::Client(stream)) => Pin::new(stream).poll_shutdown(context),
            Some(PipeStream::Server(stream)) => Pin::new(stream).poll_shutdown(context),
            None => Poll::Ready(Err(closed_pipe_error())),
        }
    }
}

fn closed_pipe_error() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "Windows named pipe is closed")
}

pub struct BoundEndpoint {
    endpoint: Endpoint,
    listener: Mutex<Option<NamedPipeServer>>,
}

impl BoundEndpoint {
    pub async fn bind(endpoint: &Endpoint, runtime_dir: &Path) -> Result<Self, EndpointError> {
        validate_endpoint_budget(endpoint, runtime_dir)?;
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
        // Named pipes have no filesystem node to unlink. Their names remain
        // kernel-visible while any listener or connected instance is held,
        // so this step cannot claim that the pipe name was released.
        Ok(())
    }

    /// Windows named-pipe rendezvous names have no filesystem entry inside
    /// the runtime directory.
    pub fn owned_runtime_paths(&self) -> Result<Vec<std::path::PathBuf>, EndpointError> {
        Ok(Vec::new())
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
        Ok((IpcStream::server(connected), ()))
    }

    pub fn close_listener(&mut self) {
        self.listener.get_mut().take();
    }
}

pub(super) fn prepare_runtime_directory(
    runtime_dir: &Path,
) -> Result<PreparedRuntimeDirectory, EndpointError> {
    prepare_runtime_directory_inner(runtime_dir, None)
}

pub(super) fn prepare_runtime_directory_with_temp(
    runtime_dir: &Path,
    daemon_temp_dir: &Path,
) -> Result<PreparedRuntimeDirectory, EndpointError> {
    prepare_runtime_directory_inner(runtime_dir, Some(daemon_temp_dir))
}

fn prepare_runtime_directory_inner(
    runtime_dir: &Path,
    daemon_temp_dir: Option<&Path>,
) -> Result<PreparedRuntimeDirectory, EndpointError> {
    let parent = runtime_dir
        .parent()
        .ok_or_else(|| EndpointError::Endpoint {
            message: format!(
                "profile runtime directory {} has no containing root",
                runtime_dir.display()
            ),
        })?;
    std::fs::create_dir_all(parent).map_err(|error| {
        EndpointError::io("create profile runtime root ancestors", parent, error)
    })?;
    let runtime_receipt = prepare_windows_directory(runtime_dir, "profile runtime directory")?;
    let temp = runtime_dir.join("tmp");
    let temp_receipt = match prepare_windows_directory(&temp, "daemon temporary directory") {
        Ok(receipt) => receipt,
        Err(error) => {
            if let Some(mut receipt) = runtime_receipt {
                return match remove_owned_empty_directory(&mut receipt) {
                    Ok(
                        OwnedDirectoryRemoval::Removed
                        | OwnedDirectoryRemoval::AlreadyAbsent
                        | OwnedDirectoryRemoval::ReplacementPreserved,
                    ) => Err(error),
                    Ok(
                        OwnedDirectoryRemoval::RemovalRequested
                        | OwnedDirectoryRemoval::CoordinateLost
                        | OwnedDirectoryRemoval::NotEmpty,
                    )
                    | Err(_) => Err(EndpointError::OwnedResidual {
                        path: receipt.path().to_path_buf(),
                        source: Box::new(error),
                    }),
                };
            }
            return Err(error);
        }
    };
    let mut receipts = Vec::with_capacity(2);
    if let Some(receipt) = runtime_receipt {
        receipts.push(receipt);
    }
    if let Some(receipt) = temp_receipt {
        receipts.push(receipt);
    }
    if let Some(daemon_temp_dir) = daemon_temp_dir {
        if daemon_temp_dir.parent() != Some(temp.as_path()) {
            return rollback_windows_directories(
                receipts,
                EndpointError::Endpoint {
                    message: format!(
                        "daemon temp directory {} is not a direct child of {}",
                        daemon_temp_dir.display(),
                        temp.display()
                    ),
                },
            );
        }
        match prepare_windows_directory(daemon_temp_dir, "daemon-private temporary directory") {
            Ok(Some(receipt)) => receipts.push(receipt),
            Ok(None) => {
                return rollback_windows_directories(
                    receipts,
                    EndpointError::Endpoint {
                        message: format!(
                            "daemon-private temp path unexpectedly pre-existed: {}",
                            daemon_temp_dir.display()
                        ),
                    },
                );
            }
            Err(error) => return rollback_windows_directories(receipts, error),
        }
    }
    Ok(PreparedRuntimeDirectory::new(receipts))
}

fn rollback_windows_directories(
    mut receipts: Vec<OwnedDirectoryReceipt>,
    error: EndpointError,
) -> Result<PreparedRuntimeDirectory, EndpointError> {
    while let Some(mut receipt) = receipts.pop() {
        match remove_owned_empty_directory(&mut receipt) {
            Ok(
                OwnedDirectoryRemoval::Removed
                | OwnedDirectoryRemoval::AlreadyAbsent
                | OwnedDirectoryRemoval::ReplacementPreserved,
            ) => {}
            Ok(OwnedDirectoryRemoval::RemovalRequested) => {
                return Err(EndpointError::OwnedResidual {
                    path: receipt.path().to_path_buf(),
                    source: Box::new(error),
                });
            }
            Ok(OwnedDirectoryRemoval::CoordinateLost | OwnedDirectoryRemoval::NotEmpty)
            | Err(_) => {
                return Err(EndpointError::OwnedResidual {
                    path: receipt.path().to_path_buf(),
                    source: Box::new(error),
                });
            }
        }
    }
    Err(error)
}

fn prepare_windows_directory(
    path: &Path,
    description: &'static str,
) -> Result<Option<OwnedDirectoryReceipt>, EndpointError> {
    match open_removable_directory(path) {
        Ok(anchor) => {
            crate::set_mode(path, 0o700)
                .map_err(|error| EndpointError::io("secure private directory", path, error))?;
            drop(anchor);
            return Ok(None);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(EndpointError::io("open private directory", path, error));
        }
    }
    loop {
        let staging = owned_directory_claim_path(path)?;
        match std::fs::create_dir(&staging) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(EndpointError::io(
                    "create staged private directory",
                    &staging,
                    error,
                ));
            }
        }
        let anchor = match open_removable_directory(&staging) {
            Ok(anchor) => anchor,
            Err(error) => {
                return Err(EndpointError::OwnedResidual {
                    path: staging.clone(),
                    source: Box::new(EndpointError::io(
                        "open staged private directory",
                        &staging,
                        error,
                    )),
                });
            }
        };
        if let Err(error) = crate::set_mode(&staging, 0o700) {
            let mut receipt = OwnedDirectoryReceipt::new(staging.clone(), anchor);
            return match remove_owned_empty_directory(&mut receipt) {
                Ok(OwnedDirectoryRemoval::Removed | OwnedDirectoryRemoval::AlreadyAbsent) => Err(
                    EndpointError::io("secure staged private directory", &staging, error),
                ),
                Ok(
                    OwnedDirectoryRemoval::RemovalRequested
                    | OwnedDirectoryRemoval::ReplacementPreserved
                    | OwnedDirectoryRemoval::CoordinateLost
                    | OwnedDirectoryRemoval::NotEmpty,
                )
                | Err(_) => Err(EndpointError::OwnedResidual {
                    path: receipt.path().to_path_buf(),
                    source: Box::new(EndpointError::io(
                        "secure staged private directory",
                        &staging,
                        error,
                    )),
                }),
            };
        }
        let mut receipt = OwnedDirectoryReceipt::new(staging, anchor);
        match rename_directory_handle(&receipt, path) {
            Ok(()) => {
                receipt.path = path.to_path_buf();
                return Ok(Some(receipt));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                match remove_owned_empty_directory(&mut receipt) {
                    Ok(OwnedDirectoryRemoval::Removed | OwnedDirectoryRemoval::AlreadyAbsent) => {}
                    _ => {
                        return Err(EndpointError::OwnedResidual {
                            path: receipt.path().to_path_buf(),
                            source: Box::new(EndpointError::Endpoint {
                                message: format!(
                                    "{description} appeared while its staging directory could not be retired: {}",
                                    path.display()
                                ),
                            }),
                        });
                    }
                }
                let anchor = open_removable_directory(path).map_err(|open_error| {
                    EndpointError::io(
                        "open concurrently created private directory",
                        path,
                        open_error,
                    )
                })?;
                crate::set_mode(path, 0o700).map_err(|mode_error| {
                    EndpointError::io(
                        "secure concurrently created private directory",
                        path,
                        mode_error,
                    )
                })?;
                drop(anchor);
                return Ok(None);
            }
            Err(error) => {
                let remove_result = remove_owned_empty_directory(&mut receipt);
                return match remove_result {
                    Ok(OwnedDirectoryRemoval::Removed | OwnedDirectoryRemoval::AlreadyAbsent) => {
                        Err(EndpointError::io("publish private directory", path, error))
                    }
                    Ok(
                        OwnedDirectoryRemoval::RemovalRequested
                        | OwnedDirectoryRemoval::ReplacementPreserved
                        | OwnedDirectoryRemoval::CoordinateLost
                        | OwnedDirectoryRemoval::NotEmpty,
                    )
                    | Err(_) => Err(EndpointError::OwnedResidual {
                        path: receipt.path().to_path_buf(),
                        source: Box::new(EndpointError::io(
                            "publish private directory",
                            path,
                            error,
                        )),
                    }),
                };
            }
        }
    }
}

fn open_removable_directory(path: &Path) -> io::Result<std::fs::File> {
    use std::os::windows::fs::MetadataExt as _;
    use std::os::windows::fs::OpenOptionsExt as _;

    const DELETE: u32 = 0x0001_0000;
    const FILE_LIST_DIRECTORY: u32 = 0x0000_0001;
    const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

    let file = std::fs::OpenOptions::new()
        .access_mode(DELETE | FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::other(format!(
            "{} is not a real directory",
            path.display()
        )));
    }
    Ok(file)
}

pub(super) fn remove_owned_empty_directory(
    receipt: &mut OwnedDirectoryReceipt,
) -> io::Result<OwnedDirectoryRemoval> {
    let Some(_) = receipt.anchor.as_ref() else {
        return match std::fs::symlink_metadata(&receipt.path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Ok(OwnedDirectoryRemoval::Removed)
            }
            Ok(_) => Err(io::Error::from_raw_os_error(303)),
            Err(error) => Err(error),
        };
    };
    let original = receipt.path.clone();
    let claim = owned_directory_claim_path(&original).map_err(io::Error::other)?;
    match rename_directory_handle(receipt, &claim) {
        Ok(()) => receipt.path = claim,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return match owned_directory_path_state(receipt)? {
                OwnedDirectoryPathState::OwnedObjectUnlinked => {
                    Ok(OwnedDirectoryRemoval::AlreadyAbsent)
                }
                _ => Ok(OwnedDirectoryRemoval::CoordinateLost),
            };
        }
        Err(error) => return Err(error),
    }
    match request_directory_delete(receipt) {
        Ok(()) => {
            drop(receipt.anchor.take());
            Ok(OwnedDirectoryRemoval::RemovalRequested)
        }
        Err(error) if error.raw_os_error() == Some(145) => {
            rename_directory_handle(receipt, &original)?;
            receipt.path = original;
            Ok(OwnedDirectoryRemoval::NotEmpty)
        }
        Err(error) => match rename_directory_handle(receipt, &original) {
            Ok(()) => {
                receipt.path = original;
                Err(error)
            }
            Err(restore_error) => Err(io::Error::other(format!(
                "remove owned directory failed: {error}; restore failed: {}",
                restore_error
            ))),
        },
    }
}

pub(super) fn owned_directory_path_state(
    receipt: &OwnedDirectoryReceipt,
) -> io::Result<OwnedDirectoryPathState> {
    let Some(anchor) = receipt.anchor.as_ref() else {
        return match std::fs::symlink_metadata(&receipt.path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Ok(OwnedDirectoryPathState::OwnedObjectUnlinked)
            }
            Ok(_) => Ok(OwnedDirectoryPathState::ReplacementPreserved),
            Err(error) => Err(error),
        };
    };
    let expected = crate::windows_file_identity(anchor)?;
    let linked = windows_handle_link_count(anchor)? > 0;
    match open_removable_directory(&receipt.path) {
        Ok(found) if crate::windows_file_identity(&found)? == expected => {
            Ok(OwnedDirectoryPathState::Owned)
        }
        Ok(_) if linked => Ok(OwnedDirectoryPathState::CoordinateLost),
        Ok(_) => Ok(OwnedDirectoryPathState::ReplacementPreserved),
        Err(error) if error.kind() == io::ErrorKind::NotFound && linked => {
            Ok(OwnedDirectoryPathState::CoordinateLost)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Ok(OwnedDirectoryPathState::OwnedObjectUnlinked)
        }
        Err(error) => Err(error),
    }
}

#[allow(unsafe_code)]
pub(super) fn inspect_owned_directory(
    receipt: &OwnedDirectoryReceipt,
) -> io::Result<OwnedDirectoryInspection> {
    use std::os::windows::ffi::OsStringExt as _;
    use windows_sys::Win32::Foundation::ERROR_NO_MORE_FILES;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ID_BOTH_DIR_INFO, FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo,
        GetFileInformationByHandleEx,
    };

    let Some(anchor) = receipt.anchor.as_ref() else {
        return Ok(OwnedDirectoryInspection::OwnedObjectUnlinked);
    };
    if windows_handle_link_count(anchor)? == 0 {
        return Ok(OwnedDirectoryInspection::OwnedObjectUnlinked);
    }

    const BUFFER_BYTES: usize = 64 * 1024;
    let mut buffer = vec![0_usize; BUFFER_BYTES.div_ceil(std::mem::size_of::<usize>())];
    let mut information_class = FileIdBothDirectoryRestartInfo;
    let mut entries = Vec::new();
    loop {
        // SAFETY: `anchor` is a live directory handle opened with
        // FILE_LIST_DIRECTORY. `buffer` is suitably aligned, writable for
        // BUFFER_BYTES, and lives through this call.
        let succeeded = unsafe {
            GetFileInformationByHandleEx(
                anchor.as_raw_handle().cast(),
                information_class,
                buffer.as_mut_ptr().cast(),
                BUFFER_BYTES as u32,
            )
        };
        if succeeded == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                break;
            }
            return Err(error);
        }

        let mut offset = 0_usize;
        loop {
            let header_bytes = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
            if offset % std::mem::align_of::<FILE_ID_BOTH_DIR_INFO>() != 0
                || offset
                    .checked_add(header_bytes)
                    .is_none_or(|end| end > BUFFER_BYTES)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid Windows directory enumeration record offset",
                ));
            }
            // SAFETY: the offset and fixed header were bounds- and
            // alignment-checked above; the kernel initialized this record.
            let information = unsafe {
                &*buffer
                    .as_ptr()
                    .cast::<u8>()
                    .add(offset)
                    .cast::<FILE_ID_BOTH_DIR_INFO>()
            };
            let name_bytes = information.FileNameLength as usize;
            let name_start = offset + header_bytes;
            if name_bytes % std::mem::size_of::<u16>() != 0
                || name_start
                    .checked_add(name_bytes)
                    .is_none_or(|end| end > BUFFER_BYTES)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid Windows directory enumeration filename length",
                ));
            }
            // SAFETY: FileName is UTF-16 aligned within the checked record and
            // the byte length was validated against the output buffer.
            let name = unsafe {
                std::slice::from_raw_parts(
                    buffer.as_ptr().cast::<u8>().add(name_start).cast::<u16>(),
                    name_bytes / std::mem::size_of::<u16>(),
                )
            };
            let name = std::ffi::OsString::from_wide(name);
            if name != "." && name != ".." {
                entries.push(receipt.path.join(name));
            }
            let next = information.NextEntryOffset as usize;
            if next == 0 {
                break;
            }
            if next < header_bytes || offset.checked_add(next).is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid Windows directory enumeration next offset",
                ));
            }
            offset += next;
        }
        information_class = FileIdBothDirectoryInfo;
    }
    entries.sort();
    Ok(OwnedDirectoryInspection::Entries(entries))
}

#[allow(unsafe_code)]
pub(super) fn windows_handle_link_count(file: &std::fs::File) -> io::Result<u32> {
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = unsafe { std::mem::zeroed::<BY_HANDLE_FILE_INFORMATION>() };
    // SAFETY: `file` owns a live handle and `information` is writable for the
    // exact structure expected by GetFileInformationByHandle.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &raw mut information) } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(information.nNumberOfLinks)
    }
}

#[allow(unsafe_code)]
fn rename_directory_handle(receipt: &OwnedDirectoryReceipt, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_RENAME_INFO, FileRenameInfo, SetFileInformationByHandle,
    };

    let anchor = receipt
        .anchor
        .as_ref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "owned directory handle closed"))?;
    let mut destination = destination.as_os_str().encode_wide().collect::<Vec<_>>();
    if destination.is_empty() || destination.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "directory rename destination is empty or contains NUL",
        ));
    }
    let name_bytes = destination
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .ok_or_else(|| io::Error::other("directory rename destination is too long"))?;
    destination.push(0);
    let buffer_bytes = std::mem::offset_of!(FILE_RENAME_INFO, FileName)
        .checked_add(name_bytes)
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<u16>()))
        .ok_or_else(|| io::Error::other("directory rename buffer size overflow"))?;
    let words = buffer_bytes.div_ceil(std::mem::size_of::<usize>());
    let mut buffer = vec![0_usize; words];
    let information = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        (*information).Anonymous.ReplaceIfExists = false;
        (*information).RootDirectory = std::ptr::null_mut();
        (*information).FileNameLength = u32::try_from(name_bytes)
            .map_err(|_| io::Error::other("directory rename destination is too long"))?;
        std::ptr::copy_nonoverlapping(
            destination.as_ptr(),
            std::ptr::addr_of_mut!((*information).FileName).cast::<u16>(),
            destination.len(),
        );
    }
    // SAFETY: `information` names a suitably aligned buffer of
    // `buffer_bytes`; the live directory handle has DELETE access and the
    // no-replace flag preserves any destination that appeared concurrently.
    if unsafe {
        SetFileInformationByHandle(
            anchor.as_raw_handle().cast(),
            FileRenameInfo,
            information.cast(),
            u32::try_from(buffer_bytes)
                .map_err(|_| io::Error::other("directory rename buffer is too large"))?,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[allow(unsafe_code)]
fn request_directory_delete(receipt: &OwnedDirectoryReceipt) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_DISPOSITION_INFO, FileDispositionInfo, SetFileInformationByHandle,
    };

    let anchor = receipt
        .anchor
        .as_ref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "owned directory handle closed"))?;
    let information = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: the structure is initialized for this information class and the
    // retained directory handle has DELETE access.
    if unsafe {
        SetFileInformationByHandle(
            anchor.as_raw_handle().cast(),
            FileDispositionInfo,
            std::ptr::from_ref(&information).cast_mut().cast(),
            u32::try_from(std::mem::size_of_val(&information))
                .map_err(|_| io::Error::other("directory disposition size overflow"))?,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn owned_directory_claim_path(path: &Path) -> Result<std::path::PathBuf, EndpointError> {
    use std::fmt::Write as _;

    let mut random = [0_u8; 12];
    getrandom::fill(&mut random).map_err(|error| EndpointError::Task {
        message: format!("cannot generate directory claim name: {error}"),
    })?;
    let mut basename = String::from(".haiderd-dir-");
    for byte in random {
        write!(&mut basename, "{byte:02x}").map_err(|error| EndpointError::Task {
            message: format!("cannot format directory claim name: {error}"),
        })?;
    }
    Ok(path.with_file_name(basename))
}

pub(super) fn validate_endpoint_budget(
    endpoint: &Endpoint,
    _runtime_dir: &Path,
) -> Result<(), EndpointError> {
    use std::os::windows::ffi::OsStrExt as _;

    let length = endpoint.address().as_os_str().encode_wide().count();
    if length > NAMED_PIPE_PATH_CODE_UNITS {
        return Err(EndpointError::AddressTooLong {
            path: endpoint.address().to_path_buf(),
            length,
            limit: NAMED_PIPE_PATH_CODE_UNITS,
            unit: "UTF-16 code units",
        });
    }
    Ok(())
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
    ClientOptions::new().open(name).map(IpcStream::client)
}

pub fn shutdown_handle(stream: &IpcStream) -> io::Result<IpcShutdown> {
    Ok(IpcShutdown {
        stream: Arc::clone(&stream.stream),
    })
}

pub fn split(stream: IpcStream) -> (IpcReadHalf, IpcWriteHalf) {
    tokio::io::split(stream)
}

pub fn peer_credentials(stream: &IpcStream) -> io::Result<PeerCredentials> {
    peer_credentials_and_process(stream).map(|(credentials, _process)| credentials)
}

pub fn peer_credentials_and_exit_watcher(
    stream: &IpcStream,
) -> io::Result<(PeerCredentials, Option<PeerExitWatcher>)> {
    peer_credentials_and_process(stream).map(|(credentials, process)| {
        (
            credentials,
            Some(PeerExitWatcher {
                process,
                stream: Arc::clone(&stream.stream),
            }),
        )
    })
}

#[allow(unsafe_code)]
fn peer_credentials_and_process(
    stream: &IpcStream,
) -> io::Result<(PeerCredentials, ProcessHandle)> {
    use windows_sys::Win32::System::Pipes::{
        GetNamedPipeClientProcessId, GetNamedPipeServerProcessId,
    };

    let stream = stream.lock_stream()?;
    let pipe = stream.as_ref().ok_or_else(closed_pipe_error)?;
    let mut pid = 0_u32;
    let ok = unsafe {
        match pipe {
            PipeStream::Client(pipe) => GetNamedPipeServerProcessId(pipe.as_raw_handle(), &mut pid),
            PipeStream::Server(pipe) => GetNamedPipeClientProcessId(pipe.as_raw_handle(), &mut pid),
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
    // SYNCHRONIZE is a frozen Win32 access-right bit; windows-sys moves its
    // module home between releases, so pin the ABI value directly.
    const SYNCHRONIZE: u32 = 0x0010_0000;
    // SAFETY: `pid` came from the connected pipe instance. Retaining the
    // returned process handle pins that exact peer identity against PID reuse.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenProcess returned one newly owned real handle.
    let process = unsafe { ProcessHandle::from_raw_handle(raw.cast()) };
    let same_user = peer_process_has_current_user_sid(process.as_raw_handle().cast())?;
    Ok((
        PeerCredentials {
            pid: Some(pid),
            uid: u32::MAX,
            gid: u32::MAX,
            same_user,
        },
        process,
    ))
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
fn peer_process_has_current_user_sid(peer_process: HANDLE) -> io::Result<bool> {
    let peer_token = open_process_token(peer_process)?;
    // SAFETY: GetCurrentProcess returns the documented pseudo-handle. It is
    // passed through but never wrapped or closed.
    let current_token = open_process_token(unsafe { GetCurrentProcess() })?;
    let peer_user = token_user(peer_token.0)?;
    let current_user = token_user(current_token.0)?;
    token_user_sids_equal(&peer_user, &current_user)
}

pub fn write_immediate(stream: &IpcStream, bytes: &[u8]) {
    let Ok(mut stream) = stream.lock_stream() else {
        return;
    };
    let Some(pipe) = stream.as_mut() else {
        return;
    };
    let mut written = 0;
    while written < bytes.len() {
        let result = match pipe {
            PipeStream::Client(pipe) => pipe.try_write(&bytes[written..]),
            PipeStream::Server(pipe) => pipe.try_write(&bytes[written..]),
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
        peer_credentials, peer_credentials_and_exit_watcher, peer_is_owner, pipe_name,
        retryable_bind_error, sids_equal, validate_endpoint_budget,
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

    #[test]
    fn named_pipe_budget_has_a_typed_early_failure() {
        let endpoint = Endpoint::from_address(format!(r"\\.\pipe\{}", "x".repeat(256)));
        match validate_endpoint_budget(&endpoint, Path::new("ignored")) {
            Err(crate::ipc::EndpointError::AddressTooLong {
                path,
                length,
                limit,
                unit,
            }) => {
                assert_eq!(path, endpoint.address());
                assert!(length > limit);
                assert_eq!(limit, super::NAMED_PIPE_PATH_CODE_UNITS);
                assert_eq!(unit, "UTF-16 code units");
            }
            other => panic!("expected typed named-pipe budget error, got {other:?}"),
        }
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

    #[tokio::test]
    async fn peer_watcher_observes_pipe_close_while_peer_process_remains_alive() {
        let endpoint = unique_endpoint("peer-close");
        let bound = BoundEndpoint::bind(&endpoint, Path::new("ignored"))
            .await
            .expect("bind named pipe");
        let (client, accepted) = tokio::join!(connect(endpoint.address()), bound.accept());
        let client = client.expect("connect same-process client");
        let (server, ()) = accepted.expect("accept same-process client");
        let (_, watcher) = peer_credentials_and_exit_watcher(&server)
            .expect("authenticate peer and retain watcher");
        let watcher = watcher.expect("Windows supplies a peer watcher");

        drop(client);

        tokio::time::timeout(Duration::from_secs(1), watcher.wait())
            .await
            .expect("peer watcher observes pipe close before deadline")
            .expect("peer watcher accepts ordinary pipe close");
        drop(server);
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
