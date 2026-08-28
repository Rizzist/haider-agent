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
    use windows_sys::Win32::Foundation::CloseHandle;
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
    let raw_job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw_job.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let job = WindowsJob(raw_job);
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let configured = unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            std::mem::size_of_val(&limits) as u32,
        )
    };
    if configured == 0 {
        return Err(std::io::Error::last_os_error());
    }
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
    let assigned = unsafe { AssignProcessToJobObject(job.0, process) };
    let error = (assigned == 0).then(std::io::Error::last_os_error);
    unsafe { CloseHandle(process) };
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
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..THREADENTRY32::default()
    };
    let mut found = unsafe { Thread32First(snapshot, &raw mut entry) } != 0;
    let mut thread_ids = Vec::new();
    while found {
        if entry.th32OwnerProcessID == pid {
            thread_ids.push(entry.th32ThreadID);
        }
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        found = unsafe { Thread32Next(snapshot, &raw mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
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
        let previous_count = unsafe { ResumeThread(thread) };
        let resume_error = (previous_count == u32::MAX).then(std::io::Error::last_os_error);
        unsafe { CloseHandle(thread) };
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
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
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
        if source != target && unsafe { dup2(source, target) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { fcntl(target, F_SETFD, 0) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    // A liveness-only caller still reserves fd 4. Mark the unused fd 3 for
    // exec closure without consuming std::process's private error reporter.
    if readiness.is_none() && liveness.is_some() {
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
struct WindowsJob(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
#[allow(unsafe_code)]
unsafe impl Send for WindowsJob {}

#[cfg(windows)]
#[allow(unsafe_code)]
unsafe impl Sync for WindowsJob {}

#[cfg(windows)]
#[allow(unsafe_code)]
impl Drop for WindowsJob {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
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
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

    if pid == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid peer PID",
        ));
    }
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let terminated = unsafe { TerminateProcess(handle, 1) };
    let error = (terminated == 0).then(std::io::Error::last_os_error);
    unsafe { CloseHandle(handle) };
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
    let queried = unsafe {
        QueryInformationJobObject(
            job.0,
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

#[cfg(windows)]
#[allow(unsafe_code)]
pub fn process_leader_exited(pid: ProcessId) -> std::io::Result<bool> {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject,
    };

    // SYNCHRONIZE is a frozen Win32 access-right bit; windows-sys moves its
    // module home between releases, so pin the ABI value directly.
    const SYNCHRONIZE: u32 = 0x0010_0000;

    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid.0) };
    if handle.is_null() {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(87) {
            Ok(true)
        } else {
            Err(error)
        };
    }
    let wait = unsafe { WaitForSingleObject(handle, 0) };
    unsafe { CloseHandle(handle) };
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
    if unsafe { TerminateJobObject(job.0, exit_code) } == 0 {
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
