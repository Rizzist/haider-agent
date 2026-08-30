use std::process::ExitStatus;

#[cfg(test)]
#[path = "process_tests.rs"]
mod tests;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ProcessSignal {
    Hangup,
    Interrupt,
    Terminate,
    Kill,
    User1,
    User2,
}

impl std::fmt::Debug for ProcessSignal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Hangup => "HUP",
            Self::Interrupt => "INT",
            Self::Terminate => "TERM",
            Self::Kill => "KILL",
            Self::User1 => "USR1",
            Self::User2 => "USR2",
        })
    }
}

impl ProcessSignal {
    pub const HUP: Self = Self::Hangup;
    pub const INT: Self = Self::Interrupt;
    pub const TERM: Self = Self::Terminate;
    pub const KILL: Self = Self::Kill;
    pub const USR1: Self = Self::User1;
    pub const USR2: Self = Self::User2;
}

/// Waits for a retained child through the operating system's process-exit
/// notification without blocking an async executor worker.
///
/// The waiter is an ordinary detached thread rather than a Tokio blocking
/// task. Dropping this future at a deadline therefore cannot make Tokio's
/// runtime shutdown wait for a child that missed that deadline; while the
/// launcher remains alive, the thread still retains and eventually reaps the
/// child.
pub async fn wait_for_child_exit(mut child: std::process::Child) -> std::io::Result<ExitStatus> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("haider-child-wait".into())
        .spawn(move || {
            let _ = sender.send(child.wait());
        })
        .map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("could not start child wait thread: {error}"),
            )
        })?;
    receiver
        .await
        .map_err(|error| std::io::Error::other(format!("child wait thread failed: {error}")))?
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessId(u32);

impl ProcessId {
    #[must_use]
    pub fn from_raw(raw: i32) -> Option<Self> {
        u32::try_from(raw).ok().filter(|pid| *pid != 0).map(Self)
    }

    #[must_use]
    pub fn id(self) -> u32 {
        self.0
    }

    /// The id was validated non-zero and i32-representable at construction
    /// ([`Self::from_raw`]), so these conversions cannot fail.
    #[must_use]
    #[allow(clippy::expect_used)]
    pub fn as_raw_nonzero(self) -> std::num::NonZeroI32 {
        std::num::NonZeroI32::new(i32::try_from(self.0).expect("validated process id"))
            .expect("non-zero process id")
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessGroup(u32);

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessGroup {
    pid: u32,
    token: u64,
}

/// Failure-only state for a Windows process observed by integration tests and
/// boundary diagnostics. `None` from [`windows_process_state`] means the PID
/// no longer names an openable process; an exited process retains its code
/// only while Windows still exposes the process object.
#[cfg(windows)]
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowsProcessState {
    pub alive: bool,
    pub exit_code: Option<u32>,
    pub in_any_job: bool,
}

impl ProcessGroup {
    #[must_use]
    pub fn id(self) -> u32 {
        #[cfg(unix)]
        return self.0;
        #[cfg(windows)]
        return self.pid;
    }
}

#[cfg(unix)]
#[must_use]
pub fn process_group(pid: Option<u32>) -> Option<ProcessGroup> {
    pid.filter(|pid| *pid != 0).map(ProcessGroup)
}

/// Returns non-blocking process state for failure diagnostics.
#[cfg(windows)]
#[doc(hidden)]
#[allow(unsafe_code)]
pub fn windows_process_state(pid: u32) -> std::io::Result<Option<WindowsProcessState>> {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::JobObjects::IsProcessInJob;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject,
    };

    // SYNCHRONIZE is a frozen Win32 access-right bit; windows-sys moves its
    // module home between releases, so pin the ABI value directly.
    const SYNCHRONIZE: u32 = 0x0010_0000;

    if pid == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid process PID",
        ));
    }
    // SAFETY: the access mask and PID are values, and no borrowed pointer is
    // passed. A non-null return is one newly owned process handle.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) };
    if raw.is_null() {
        let error = std::io::Error::last_os_error();
        return if process_error_is_missing(&error) {
            Ok(None)
        } else {
            Err(error)
        };
    }
    // SAFETY: `raw` is the non-null owned handle returned immediately above;
    // transferring it prevents every error path below from leaking it.
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    // SAFETY: `handle` remains live for the call and a zero timeout never
    // blocks this failure-only diagnostic.
    let wait = unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) };
    let alive = match wait {
        WAIT_OBJECT_0 => false,
        WAIT_TIMEOUT => true,
        _ => return Err(std::io::Error::last_os_error()),
    };
    let mut raw_exit_code = 0_u32;
    // SAFETY: `handle` remains live and `raw_exit_code` is writable for the
    // duration of the call.
    if unsafe { GetExitCodeProcess(handle.as_raw_handle(), &raw mut raw_exit_code) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut in_any_job = 0;
    // SAFETY: a null Job handle asks about membership in any Job Object;
    // `handle` and the writable result remain live for the call.
    if unsafe {
        IsProcessInJob(
            handle.as_raw_handle(),
            std::ptr::null_mut(),
            &raw mut in_any_job,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Some(WindowsProcessState {
        alive,
        exit_code: (!alive).then_some(raw_exit_code),
        in_any_job: in_any_job != 0,
    }))
}

/// Reports whether `pid` belongs to the exact registered Job Object.
#[cfg(windows)]
#[doc(hidden)]
#[allow(unsafe_code)]
pub fn windows_process_in_group(group: ProcessGroup, pid: u32) -> std::io::Result<bool> {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use windows_sys::Win32::System::JobObjects::IsProcessInJob;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    if pid == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid process PID",
        ));
    }
    // SAFETY: the access mask and PID are values, and no borrowed pointer is
    // passed. A non-null return is one newly owned process handle.
    let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `raw` is the non-null owned handle returned immediately above.
    let process = unsafe { OwnedHandle::from_raw_handle(raw) };
    let registry = windows_job_registry()
        .lock()
        .map_err(|_| std::io::Error::other("Windows job registry is poisoned"))?;
    let job = registry.by_token.get(&group.token).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "Windows process group {} is no longer registered",
                group.pid
            ),
        )
    })?;
    let mut in_group = 0;
    // SAFETY: both handles remain live under this registry guard and the
    // result storage is writable for the duration of the call.
    if unsafe { IsProcessInJob(process.as_raw_handle(), job.raw(), &raw mut in_group) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(in_group != 0)
}

#[cfg(unix)]
pub fn register_process_group(pid: u32) -> std::io::Result<ProcessGroup> {
    process_group(Some(pid)).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid process-group leader PID",
        )
    })
}

/// Returns the job-backed process group registered for a spawned Windows
/// process. Unregistered PIDs are deliberately rejected: reconstructing a
/// tree authority from a recyclable PID would make late cleanup unsafe.
#[cfg(windows)]
#[must_use]
pub fn process_group(pid: Option<u32>) -> Option<ProcessGroup> {
    let pid = pid.filter(|pid| *pid != 0)?;
    let registry = windows_job_registry().lock().ok()?;
    let token = *registry.by_pid.get(&pid)?;
    registry
        .by_token
        .contains_key(&token)
        .then_some(ProcessGroup { pid, token })
}

/// Assigns a just-spawned Windows process to an owned Job Object. Descendants
/// inherit membership, the job remains authoritative after the leader exits,
/// and `KILL_ON_JOB_CLOSE` provides the daemon-crash cleanup boundary.
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn register_process_group(pid: u32) -> std::io::Result<ProcessGroup> {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    if pid == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid process-group leader PID",
        ));
    }
    // SAFETY: both optional name/security pointers are null, so the API reads
    // no caller memory and returns a newly owned handle on success.
    let raw_job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw_job.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: CreateJobObjectW returned a non-null newly owned handle, and
    // this is the unique transfer into the standard RAII owner.
    let job = WindowsJob(unsafe { OwnedHandle::from_raw_handle(raw_job.cast()) });
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: `job` owns a live Job Object and `limits` is an initialized
    // structure whose exact size is supplied for this information class.
    let configured = unsafe {
        SetInformationJobObject(
            job.raw(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            std::mem::size_of_val(&limits) as u32,
        )
    };
    if configured == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the access mask and validated nonzero PID are value arguments;
    // a non-null return is one newly owned process handle.
    let process = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SET_QUOTA | PROCESS_TERMINATE,
            0,
            pid,
        )
    };
    if process.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: OpenProcess returned a non-null newly owned handle, and this is
    // the unique transfer into the standard RAII owner.
    let process = unsafe { OwnedHandle::from_raw_handle(process.cast()) };
    // SAFETY: both the Job Object and process handles remain live and owned by
    // this function for the full call; the API borrows rather than consumes.
    let assigned = unsafe { AssignProcessToJobObject(job.raw(), process.as_raw_handle().cast()) };
    let error = (assigned == 0).then(std::io::Error::last_os_error);
    drop(process);
    if let Some(error) = error {
        return Err(error);
    }

    // KILL_ON_JOB_CLOSE makes this a fail-closed spawn: no command byte ran
    // and closing the only job handle terminates the suspended process.
    resume_suspended_process(pid)?;

    let token = NEXT_WINDOWS_JOB.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let group = ProcessGroup { pid, token };
    let mut registry = windows_job_registry()
        .lock()
        .map_err(|_| std::io::Error::other("Windows job registry is poisoned"))?;
    registry.by_pid.insert(pid, token);
    registry.by_token.insert(token, job);
    Ok(group)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn resume_suspended_process(pid: u32) -> std::io::Result<()> {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    // SAFETY: the flags and process-id value require no caller pointers; a
    // non-sentinel return is one newly owned snapshot handle.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the non-sentinel snapshot is a newly owned handle and is
    // transferred exactly once into the standard RAII owner.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot.cast()) };
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..THREADENTRY32::default()
    };
    // SAFETY: `snapshot` is live and `entry` is writable with dwSize set to
    // the structure size required by ToolHelp iteration.
    let mut found = unsafe { Thread32First(snapshot.as_raw_handle().cast(), &raw mut entry) } != 0;
    let mut thread_ids = Vec::new();
    while found {
        if entry.th32OwnerProcessID == pid {
            thread_ids.push(entry.th32ThreadID);
        }
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        // SAFETY: the snapshot remains live and dwSize is reset before every
        // writable Thread32Next output operation.
        found = unsafe { Thread32Next(snapshot.as_raw_handle().cast(), &raw mut entry) } != 0;
    }
    drop(snapshot);
    if thread_ids.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("threads for suspended process {pid} were not found"),
        ));
    }

    // ToolHelp does not identify the primary thread and does not promise an
    // ordering. Snapshot every thread before resuming any: a security product
    // may have injected an auxiliary thread into the newly created process,
    // and resuming only the first matching entry can leave the real primary
    // thread suspended forever while falsely reporting a successful spawn.
    let mut resumed = 0_usize;
    let mut already_running = 0_usize;
    let mut first_error = None;
    let mut observations = Vec::with_capacity(thread_ids.len());
    for thread_id in thread_ids {
        // SAFETY: `thread_id` came from the live snapshot and the access mask
        // and inherit flag are value arguments; success returns an owned handle.
        let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, thread_id) };
        if thread.is_null() {
            let error = std::io::Error::last_os_error();
            observations.push((thread_id, None));
            first_error.get_or_insert_with(|| {
                std::io::Error::new(
                    error.kind(),
                    format!("open thread {thread_id} for suspended process {pid}: {error}"),
                )
            });
            continue;
        }
        // SAFETY: OpenThread returned a non-null newly owned handle and this
        // is its unique transfer into the standard RAII owner.
        let thread = unsafe { OwnedHandle::from_raw_handle(thread.cast()) };
        // SAFETY: `thread` is the non-null owned handle from OpenThread and
        // remains live while ResumeThread borrows it.
        let previous_count = unsafe { ResumeThread(thread.as_raw_handle().cast()) };
        let resume_error = (previous_count == u32::MAX).then(std::io::Error::last_os_error);
        drop(thread);
        observations.push((thread_id, Some(previous_count)));
        if let Some(error) = resume_error {
            first_error.get_or_insert_with(|| {
                std::io::Error::new(
                    error.kind(),
                    format!("resume thread {thread_id} for suspended process {pid}: {error}"),
                )
            });
        } else if previous_count == 0 {
            already_running = already_running.saturating_add(1);
        } else if previous_count == 1 {
            resumed = resumed.saturating_add(1);
        } else {
            first_error.get_or_insert_with(|| {
                std::io::Error::other(format!(
                    "suspended process {pid} thread {thread_id} had unexpected suspend count {previous_count}"
                ))
            });
        }
    }
    if std::env::var("HAIDER_TEST_PROCESS_TRACE").is_ok_and(|value| value == "1") {
        eprintln!(
            "haider-daemon windows-process phase=job-assigned-and-threads-resumed pid={pid} resumed={resumed} already_running={already_running} observations={observations:?}"
        );
    }
    if let Some(error) = first_error {
        return Err(error);
    }
    if resumed == 0 {
        return Err(std::io::Error::other(format!(
            "suspended process {pid} had no suspended thread to resume"
        )));
    }
    Ok(())
}

#[must_use]
pub fn process_id(pid: Option<u32>) -> Option<ProcessId> {
    pid.and_then(|pid| i32::try_from(pid).ok())
        .and_then(ProcessId::from_raw)
}

#[cfg(unix)]
pub fn configure_process_group(command: &mut tokio::process::Command) {
    use std::os::unix::process::CommandExt as _;
    command.as_std_mut().process_group(0);
}

#[cfg(windows)]
pub fn configure_process_group(command: &mut tokio::process::Command) {
    use std::os::windows::process::CommandExt as _;
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    };
    command
        .as_std_mut()
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_SUSPENDED);
}

/// Restores the non-secret OS variables required by Windows command
/// interpreters after a caller deliberately clears the child environment.
#[cfg(unix)]
pub fn configure_process_environment(_command: &mut tokio::process::Command) {}

#[cfg(windows)]
pub fn configure_process_environment(command: &mut tokio::process::Command) {
    for name in [
        "PATH",
        "SystemRoot",
        "WINDIR",
        "COMSPEC",
        "PATHEXT",
        "TEMP",
        "TMP",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
}

#[cfg(windows)]
#[must_use]
pub fn windows_command_interpreter() -> std::path::PathBuf {
    std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .map(std::path::PathBuf::from)
        .map(|root| root.join("System32").join("cmd.exe"))
        .filter(|path| path.is_absolute() && path.is_file())
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe"))
}

/// Absolute inbox PowerShell used by the shared shell-execution engine.
/// Resolving through `SystemRoot` happens before child `env_clear`; the fixed
/// fallback preserves an absolute trusted-system coordinate when the
/// inherited environment is incomplete.
#[cfg(windows)]
#[must_use]
pub fn windows_powershell() -> std::path::PathBuf {
    std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .map(std::path::PathBuf::from)
        .map(|root| {
            root.join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe")
        })
        .filter(|path| path.is_absolute() && path.is_file())
        .unwrap_or_else(|| {
            std::path::PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe")
        })
}

/// Adds the close-sweep required for a child that outlives its launcher.
#[cfg(unix)]
pub fn configure_background_process(command: &mut tokio::process::Command) {
    use std::os::unix::process::CommandExt as _;
    let upper_bound = inherited_descriptor_upper_bound();
    // SAFETY: std requires unsafe because arbitrary pre_exec work can violate
    // post-fork rules; this closure calls only the async-signal-safe fd sweep.
    #[allow(unsafe_code)]
    unsafe {
        command.as_std_mut().pre_exec(move || {
            close_inherited_descriptors_from(3, upper_bound);
            Ok(())
        });
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
pub(crate) fn close_inherited_descriptors(upper_bound: std::os::raw::c_int) {
    close_inherited_descriptors_from(3, upper_bound);
}

#[cfg(unix)]
#[allow(unsafe_code)]
pub(crate) fn close_inherited_descriptors_from(
    first: std::os::raw::c_int,
    upper_bound: std::os::raw::c_int,
) {
    if close_inherited_descriptor_range(first) {
        return;
    }

    // Fallback for an older Linux kernel or a Unix without a bulk CLOEXEC API.
    // Mark rather than close: std::process creates a private CLOEXEC pipe
    // before fork so the child can report pre-exec/exec errors to its parent.
    // Closing that unknown descriptor here would turn ENOENT/EACCES into a
    // false successful spawn. The parent computes and captures `upper_bound`;
    // the child only calls async-signal-safe `fcntl(2)`. EBADF is expected for
    // unused slots.
    for fd in first..upper_bound {
        // SAFETY: fcntl takes the numeric descriptor by value; open slots are
        // owned by the child and closed slots may return EBADF, which is ignored.
        let _ = unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
}

/// Returns a conservative descriptor ceiling while still in the parent.
///
/// macOS commonly reports an `RLIMIT_NOFILE` of 1,048,576. Sweeping that
/// entire range in every forked child adds roughly one million `fcntl` calls
/// before `exec`. `/dev/fd` exposes the parent's actual open set; 64 spare
/// slots cover the small fixed set `Command` may allocate before the hook.
/// A failed enumeration retains the historical rlimit-based ceiling.
#[cfg(unix)]
pub(crate) fn inherited_descriptor_upper_bound() -> std::os::raw::c_int {
    const COMMAND_DESCRIPTOR_HEADROOM: std::os::raw::c_int = 64;

    let maximum = std::fs::read_dir("/dev/fd").ok().and_then(|entries| {
        entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name();
                name.to_str()
                    .and_then(|name| name.parse::<std::os::raw::c_int>().ok())
            })
            .max()
    });
    maximum.map_or_else(inherited_descriptor_limit, |maximum| {
        maximum
            .saturating_add(1)
            .saturating_add(COMMAND_DESCRIPTOR_HEADROOM)
    })
}

#[cfg(all(
    unix,
    not(any(target_os = "espidf", target_os = "horizon", target_os = "vita"))
))]
#[allow(unsafe_code)]
fn inherited_descriptor_limit() -> std::os::raw::c_int {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is writable for exactly one rlimit structure and the
    // fixed RLIMIT_NOFILE selector requires no other caller-owned memory.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) } == 0 {
        let capped = limit.rlim_cur.min(std::os::raw::c_int::MAX as libc::rlim_t);
        return capped as std::os::raw::c_int;
    }
    65_536
}

// Newlib libc exposes `getrlimit` but no `RLIMIT_NOFILE` selector on these
// non-daemon targets. Retain the historical conservative ceiling there.
#[cfg(all(
    unix,
    any(target_os = "espidf", target_os = "horizon", target_os = "vita")
))]
fn inherited_descriptor_limit() -> std::os::raw::c_int {
    65_536
}

/// Runtime probe for Linux's 5.11+ `CLOSE_RANGE_CLOEXEC`. A kernel without the
/// flag returns an error; seccomp and other runtime refusals also fall back to
/// the conservative individual-fcntl loop.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[allow(unsafe_code)]
fn close_inherited_descriptor_range(first: std::os::raw::c_int) -> bool {
    // Linux UAPI value shared by Android kernels. libc does not expose the
    // named constant on every Android target supported by this workspace.
    const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;

    // SAFETY: this invokes Linux close_range with value-only arguments; the
    // CLOEXEC flag preserves descriptors while preventing inheritance.
    unsafe {
        libc::syscall(
            libc::SYS_close_range,
            first as libc::c_uint,
            libc::c_uint::MAX,
            CLOSE_RANGE_CLOEXEC,
        ) == 0
    }
}

/// BSD `closefrom(2)` cannot preserve std::process's private exec-error pipe,
/// so use the bounded CLOEXEC loop on these targets as well.
#[cfg(any(
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
fn close_inherited_descriptor_range(_first: std::os::raw::c_int) -> bool {
    false
}

/// macOS and other Unix targets use the loop when their deployed libc exposes
/// no async-signal-safe bulk-close primitive.
#[cfg(all(
    unix,
    not(any(
        target_os = "android",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "linux",
        target_os = "netbsd",
        target_os = "openbsd"
    ))
))]
fn close_inherited_descriptor_range(_first: std::os::raw::c_int) -> bool {
    false
}

/// Moves the daemon's inherited startup descriptors to fixed coordinates.
///
/// This runs after `fork` and before `exec`, so it uses only async-signal-safe
/// libc calls. Descriptors 3 and 4 are then excluded from the background close
/// sweep; the daemon restores `FD_CLOEXEC` as soon as it adopts each endpoint.
#[cfg(unix)]
#[allow(unsafe_code)]
pub(crate) fn install_daemon_spawn_descriptors(
    readiness: Option<std::os::raw::c_int>,
    liveness: Option<std::os::raw::c_int>,
    upper_bound: std::os::raw::c_int,
) -> std::io::Result<()> {
    const DAEMON_READINESS_FD: std::os::raw::c_int = 3;
    const DAEMON_LIVENESS_FD: std::os::raw::c_int = 4;
    const F_SETFD: std::os::raw::c_int = 2;

    unsafe extern "C" {
        fn dup2(oldfd: std::os::raw::c_int, newfd: std::os::raw::c_int) -> std::os::raw::c_int;
        fn fcntl(fd: std::os::raw::c_int, command: std::os::raw::c_int, ...)
        -> std::os::raw::c_int;
    }

    for (source, target) in [
        (readiness, DAEMON_READINESS_FD),
        (liveness, DAEMON_LIVENESS_FD),
    ] {
        let Some(source) = source else {
            continue;
        };
        // SAFETY: `source` is an inherited owned descriptor and `target` is a
        // reserved child coordinate; dup2 atomically replaces only `target`.
        if source != target && unsafe { dup2(source, target) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `target` is live after the identity/dup2 path and fcntl only
        // changes its descriptor flags without borrowing Rust-managed memory.
        if unsafe { fcntl(target, F_SETFD, 0) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    // A liveness-only caller still reserves fd 4. Mark the unused fd 3 for
    // exec closure without consuming std::process's private error reporter.
    if readiness.is_none() && liveness.is_some() {
        // SAFETY: fd 3 is deliberately unused in this child; fcntl receives
        // only its numeric value and EBADF is an acceptable no-op result.
        let _ = unsafe { libc::fcntl(DAEMON_READINESS_FD, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    let last = if liveness.is_some() {
        DAEMON_LIVENESS_FD
    } else {
        DAEMON_READINESS_FD
    };
    close_inherited_descriptors_from(last + 1, upper_bound);
    Ok(())
}

#[cfg(windows)]
pub fn configure_background_process(command: &mut tokio::process::Command) {
    configure_process_group(command);
}

#[cfg(windows)]
struct WindowsJob(std::os::windows::io::OwnedHandle);

#[cfg(windows)]
impl WindowsJob {
    fn raw(&self) -> windows_sys::Win32::Foundation::HANDLE {
        use std::os::windows::io::AsRawHandle as _;

        self.0.as_raw_handle().cast()
    }
}

#[cfg(windows)]
#[derive(Default)]
struct WindowsJobRegistry {
    by_pid: std::collections::HashMap<u32, u64>,
    by_token: std::collections::HashMap<u64, WindowsJob>,
}

#[cfg(windows)]
static NEXT_WINDOWS_JOB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[cfg(windows)]
fn windows_job_registry() -> &'static std::sync::Mutex<WindowsJobRegistry> {
    static REGISTRY: std::sync::OnceLock<std::sync::Mutex<WindowsJobRegistry>> =
        std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(WindowsJobRegistry::default()))
}

/// Releases this process-tree authority. On Windows, closing the owned Job
/// Object is itself the final fail-safe kill, so no PID-based cleanup follows.
#[cfg(unix)]
pub fn release_process_group(_group: ProcessGroup) {}

#[cfg(windows)]
pub fn release_process_group(group: ProcessGroup) {
    let Ok(mut registry) = windows_job_registry().lock() else {
        return;
    };
    registry.by_token.remove(&group.token);
    if registry.by_pid.get(&group.pid) == Some(&group.token) {
        registry.by_pid.remove(&group.pid);
    }
}

#[cfg(unix)]
fn unix_pid(pid: u32) -> std::io::Result<rustix::process::Pid> {
    i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid peer PID"))
}

#[cfg(unix)]
fn unix_signal(signal: ProcessSignal) -> rustix::process::Signal {
    match signal {
        ProcessSignal::Hangup => rustix::process::Signal::HUP,
        ProcessSignal::Interrupt => rustix::process::Signal::INT,
        ProcessSignal::Terminate => rustix::process::Signal::TERM,
        ProcessSignal::Kill => rustix::process::Signal::KILL,
        ProcessSignal::User1 => rustix::process::Signal::USR1,
        ProcessSignal::User2 => rustix::process::Signal::USR2,
    }
}

#[cfg(unix)]
pub fn signal_process(pid: u32, signal: ProcessSignal) -> std::io::Result<()> {
    rustix::process::kill_process(unix_pid(pid)?, unix_signal(signal)).map_err(std::io::Error::from)
}

#[cfg(windows)]
#[allow(unsafe_code)]
pub fn signal_process(pid: u32, _signal: ProcessSignal) -> std::io::Result<()> {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    if pid == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid peer PID",
        ));
    }
    // SAFETY: access rights and validated PID are value arguments; a non-null
    // result is a newly owned process handle.
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: OpenProcess returned a non-null newly owned handle and this is
    // its unique transfer into the standard RAII owner.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle.cast()) };
    // SAFETY: `handle` is the live owned OpenProcess result and is borrowed
    // only for this termination request.
    let terminated = unsafe { TerminateProcess(handle.as_raw_handle().cast(), 1) };
    let error = (terminated == 0).then(std::io::Error::last_os_error);
    drop(handle);
    error.map_or(Ok(()), Err)
}

#[cfg(unix)]
pub fn signal_process_group(group: ProcessGroup, signal: ProcessSignal) -> std::io::Result<()> {
    rustix::process::kill_process_group(unix_pid(group.0)?, unix_signal(signal))
        .map_err(std::io::Error::from)
}

#[cfg(unix)]
pub fn signal_process_group_id(pid: ProcessId, signal: ProcessSignal) -> std::io::Result<()> {
    signal_process_group(ProcessGroup(pid.0), signal)
}

#[cfg(windows)]
pub fn signal_process_group_id(pid: ProcessId, signal: ProcessSignal) -> std::io::Result<()> {
    let group = process_group(Some(pid.0)).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no registered Windows Job Object for process {}", pid.0),
        )
    })?;
    signal_process_group(group, signal)
}

#[cfg(target_os = "linux")]
pub fn process_group_exists(group: ProcessGroup) -> std::io::Result<bool> {
    match rustix::process::test_kill_process_group(unix_pid(group.0)?) {
        Ok(()) => linux_process_group_has_live_member(group),
        Err(rustix::io::Errno::PERM) => Ok(true),
        Err(rustix::io::Errno::SRCH) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(target_os = "linux")]
fn linux_process_group_has_live_member(group: ProcessGroup) -> std::io::Result<bool> {
    for entry in std::fs::read_dir("/proc")? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => return Ok(true),
        };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let stat = match std::fs::read(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            // If procfs withholds a visible process, retain kill(2)'s
            // conservative live verdict rather than overlooking a member.
            Err(_) => return Ok(true),
        };
        let Some((state, process_group)) = linux_proc_state_and_group(&stat) else {
            return Ok(true);
        };
        if process_group == group.0 && !matches!(state, b'Z' | b'X') {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(target_os = "linux")]
fn linux_proc_state_and_group(stat: &[u8]) -> Option<(u8, u32)> {
    // The comm field is parenthesized and may itself contain `)`, so split at
    // the last close-paren. The next fields are state, parent pid, and pgid.
    let fields = stat.get(stat.iter().rposition(|byte| *byte == b')')? + 2..)?;
    let mut fields = fields.split(|byte| *byte == b' ');
    let state = *fields.next()?.first()?;
    fields.next()?;
    let process_group = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
    Some((state, process_group))
}

#[cfg(all(unix, not(target_os = "linux")))]
pub fn process_group_exists(group: ProcessGroup) -> std::io::Result<bool> {
    match rustix::process::test_kill_process_group(unix_pid(group.0)?) {
        Ok(()) | Err(rustix::io::Errno::PERM) => Ok(true),
        Err(rustix::io::Errno::SRCH) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
pub fn process_group_exists(group: ProcessGroup) -> std::io::Result<bool> {
    use windows_sys::Win32::System::JobObjects::{
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation,
        QueryInformationJobObject,
    };

    let mut registry = windows_job_registry()
        .lock()
        .map_err(|_| std::io::Error::other("Windows job registry is poisoned"))?;
    let Some(job) = registry.by_token.get(&group.token) else {
        return Ok(false);
    };
    let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    // SAFETY: the registry guard keeps the Job Object live, `accounting` is writable,
    // and the buffer size matches the selected Job information class.
    let queried = unsafe {
        QueryInformationJobObject(
            job.raw(),
            JobObjectBasicAccountingInformation,
            (&raw mut accounting).cast(),
            std::mem::size_of_val(&accounting) as u32,
            std::ptr::null_mut(),
        )
    };
    if queried == 0 {
        return Err(std::io::Error::last_os_error());
    }
    if accounting.ActiveProcesses != 0 {
        return Ok(true);
    }
    registry.by_token.remove(&group.token);
    if registry.by_pid.get(&group.pid) == Some(&group.token) {
        registry.by_pid.remove(&group.pid);
    }
    Ok(false)
}

#[cfg(unix)]
pub fn process_leader_exited(pid: ProcessId) -> std::io::Result<bool> {
    use rustix::process::{WaitId, WaitIdOptions};

    let options = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG;
    match rustix::process::waitid(WaitId::Pid(unix_pid(pid.0)?), options) {
        Ok(Some(status)) => Ok(status.exited() || status.killed() || status.dumped()),
        Ok(None) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Waits for a process leader's exit without reaping it.
///
/// Linux and macOS use kernel exit notifications so short commands do not
/// inherit a coarse polling interval. Other targets use a cancel-safe poll
/// fallback capped at one millisecond; callers may drop the future to cancel
/// observation without changing process ownership.
pub async fn observe_process_leader_exit(pid: ProcessId) -> std::io::Result<()> {
    if process_leader_exited(pid)? {
        return Ok(());
    }

    #[cfg(target_os = "linux")]
    if observe_process_leader_exit_with_pidfd(pid).await? {
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    if observe_process_leader_exit_with_kqueue(pid).await? {
        return Ok(());
    }

    observe_process_leader_exit_by_polling(pid).await
}

#[cfg(target_os = "linux")]
async fn observe_process_leader_exit_with_pidfd(pid: ProcessId) -> std::io::Result<bool> {
    use rustix::process::{Pid, PidfdFlags, pidfd_open};
    use tokio::io::unix::AsyncFd;

    let Some(pid) = Pid::from_raw(pid.as_raw_nonzero().get()) else {
        return Err(std::io::Error::other("process leader PID is zero"));
    };
    let descriptor = match pidfd_open(pid, PidfdFlags::NONBLOCK) {
        Ok(descriptor) => descriptor,
        Err(_) => return Ok(false),
    };
    let descriptor = AsyncFd::new(descriptor)?;
    let _ready = descriptor.readable().await?;
    Ok(true)
}

#[cfg(target_os = "macos")]
async fn observe_process_leader_exit_with_kqueue(pid: ProcessId) -> std::io::Result<bool> {
    use nix::libc::timespec;
    use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};

    const NOTIFICATION_REPAIR_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

    // A leader can become waitable between the caller's probe and kqueue
    // registration. Probe immediately before arming so an already-reaped
    // registration failure cannot strand this wait.
    if process_leader_exited(pid)? {
        return Ok(true);
    }

    let queue = Kqueue::new().map_err(std::io::Error::other)?;
    let event = KEvent::new(
        pid.id() as usize,
        EventFilter::EVFILT_PROC,
        EvFlags::EV_ADD | EvFlags::EV_ONESHOT,
        FilterFlag::NOTE_EXIT,
        0,
        0,
    );
    let registered = queue.kevent(
        &[event],
        &mut [],
        Some(timespec {
            tv_sec: 0,
            tv_nsec: 0,
        }),
    );
    if registered.is_err() {
        return process_leader_exited(pid).map(|exited| exited.then_some(()).is_some());
    }

    // Registration precedes this second probe. An exit racing the arm is
    // therefore either visible here or retained by kqueue for the event wait.
    if process_leader_exited(pid)? {
        return Ok(true);
    }

    let (sender, mut receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("haider-process-exit".into())
        .spawn(move || {
            let timeout = timespec {
                tv_sec: NOTIFICATION_REPAIR_INTERVAL.as_secs() as _,
                tv_nsec: 0,
            };
            let result = loop {
                let mut events = [event];
                match queue.kevent(&[], &mut events, Some(timeout)) {
                    Ok(count) if count != 0 => break Ok(true),
                    Ok(_) => match process_leader_exited(pid) {
                        Ok(true) => break Ok(true),
                        Ok(false) if sender.is_closed() => return,
                        Ok(false) => {}
                        Err(error) => break Err(error),
                    },
                    Err(error) => break Err(std::io::Error::other(error)),
                }
            };
            let _ = sender.send(result);
        })
        .map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("could not start process-exit observer: {error}"),
            )
        })?;
    loop {
        match crate::bounded_wait(
            "macOS process-exit notification",
            NOTIFICATION_REPAIR_INTERVAL,
            &mut receiver,
        )
        .await
        {
            crate::BoundedWait::Completed(result) => {
                return result.map_err(|error| {
                    std::io::Error::other(format!("process-exit observer stopped: {error}"))
                })?;
            }
            crate::BoundedWait::TimedOut(_timeout) => {
                // A dropped/coalesced kernel notification cannot hang the
                // observer: both the kernel wait and this typed deadline audit
                // waitable state every 30 s. Callers' existing execution
                // deadlines remain the faster cancellation bound, and a
                // dropped caller closes the sender so the kernel thread stops
                // no later than its next audit.
                if process_leader_exited(pid)? {
                    return Ok(true);
                }
            }
        }
    }
}

async fn observe_process_leader_exit_by_polling(pid: ProcessId) -> std::io::Result<()> {
    const POLL_MIN: std::time::Duration = std::time::Duration::from_micros(50);
    const POLL_MAX: std::time::Duration = std::time::Duration::from_millis(1);

    let mut backoff = POLL_MIN;
    loop {
        if process_leader_exited(pid)? {
            return Ok(());
        }
        tokio::time::sleep(backoff).await;
        backoff = backoff.saturating_mul(2).min(POLL_MAX);
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
pub fn process_leader_exited(pid: ProcessId) -> std::io::Result<bool> {
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject,
    };

    // SYNCHRONIZE is a frozen Win32 access-right bit; windows-sys moves its
    // module home between releases, so pin the ABI value directly.
    const SYNCHRONIZE: u32 = 0x0010_0000;

    // SAFETY: the access mask and PID are value arguments; a non-null result
    // is one newly owned process handle.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid.0) };
    if handle.is_null() {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(87) {
            Ok(true)
        } else {
            Err(error)
        };
    }
    // SAFETY: OpenProcess returned a non-null newly owned handle and this is
    // its unique transfer into the standard RAII owner.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle.cast()) };
    // SAFETY: `handle` remains live and a zero timeout makes this a nonblocking
    // state query that does not transfer ownership.
    let wait = unsafe { WaitForSingleObject(handle.as_raw_handle().cast(), 0) };
    drop(handle);
    match wait {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(std::io::Error::last_os_error()),
    }
}

#[cfg(unix)]
#[must_use]
pub fn process_error_is_missing(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error())
}

#[cfg(windows)]
#[must_use]
pub fn process_error_is_missing(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound
        || matches!(error.raw_os_error(), Some(87) | Some(1168))
}

#[cfg(unix)]
#[must_use]
pub fn process_error_is_permission(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(rustix::io::Errno::PERM.raw_os_error())
}

#[cfg(windows)]
#[must_use]
pub fn process_error_is_permission(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::PermissionDenied
}

#[cfg(windows)]
#[allow(unsafe_code)]
pub fn signal_process_group(group: ProcessGroup, signal: ProcessSignal) -> std::io::Result<()> {
    use windows_sys::Win32::System::JobObjects::TerminateJobObject;

    let registry = windows_job_registry()
        .lock()
        .map_err(|_| std::io::Error::other("Windows job registry is poisoned"))?;
    let Some(job) = registry.by_token.get(&group.token) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "Windows process group {} is no longer registered",
                group.pid
            ),
        ));
    };
    let exit_code = if matches!(signal, ProcessSignal::Kill) {
        1
    } else {
        0xC000_013Au32
    };
    // SAFETY: the registry guard keeps the owned Job Object live throughout
    // the call; exit_code is a value and the handle is only borrowed.
    if unsafe { TerminateJobObject(job.raw(), exit_code) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
pub fn kill_process_tree(pid: u32, force: bool) -> std::io::Result<()> {
    signal_process_group(
        ProcessGroup(pid),
        if force {
            ProcessSignal::Kill
        } else {
            ProcessSignal::Terminate
        },
    )
}

#[cfg(windows)]
pub fn kill_process_tree(pid: u32, force: bool) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt as _;
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

    let system_root = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "SystemRoot is unavailable while resolving taskkill.exe",
            )
        })?;
    let taskkill = std::path::PathBuf::from(system_root)
        .join("System32")
        .join("taskkill.exe");
    if !taskkill.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("taskkill.exe is unavailable at {}", taskkill.display()),
        ));
    }
    let mut command = std::process::Command::new(taskkill);
    command.arg("/PID").arg(pid.to_string()).arg("/T");
    if force {
        command.arg("/F");
    }
    let status = command
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "taskkill exited with status {status}"
        )))
    }
}

#[cfg(unix)]
#[must_use]
pub fn exit_signal(status: &ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt as _;
    status.signal()
}

#[cfg(windows)]
#[must_use]
pub fn exit_signal(_status: &ExitStatus) -> Option<i32> {
    None
}

/// W-flow (owner 2026-08-22) — is `program` runnable on this device?
///
/// A Loom agent type DECLARES the CLIs it may touch, and that declaration is
/// a capability grant enforced at spawn. It is not a promise the program is
/// installed: a type can register naming `yt-dlp` and only discover the gap
/// at its first failing turn. This answers the question before the bind.
///
/// Resolution mirrors what an exec would actually do, deliberately WITHOUT
/// running a shell — no `which`, no subprocess, nothing the probe itself
/// could execute. A name containing a separator is a path and is checked
/// where it points; a bare name is searched along `PATH`.
///
/// A directory never counts, and on unix neither does a non-executable
/// file — `PATH` hits that cannot be run are not presence.
pub fn program_on_path(program: &str) -> bool {
    if program.is_empty() {
        return false;
    }
    let looks_like_path = program.contains('/') || (cfg!(windows) && program.contains('\\'));
    if looks_like_path {
        return is_executable_file(std::path::Path::new(program));
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        if dir.as_os_str().is_empty() {
            return false;
        }
        let candidate = dir.join(program);
        if is_executable_file(&candidate) {
            return true;
        }
        // Windows runs `tool` as `tool.exe`/`tool.cmd`/...; PATHEXT is the
        // authority, with the documented default when it is unset.
        if cfg!(windows) {
            let extensions =
                std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned());
            return extensions
                .split(';')
                .filter(|ext| !ext.is_empty())
                .any(|ext| is_executable_file(&dir.join(format!("{program}{ext}"))));
        }
        false
    })
}

fn is_executable_file(path: &std::path::Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}
