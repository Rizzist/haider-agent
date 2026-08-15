use std::process::ExitStatus;

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

    #[must_use]
    pub fn as_raw_nonzero(self) -> std::num::NonZeroI32 {
        std::num::NonZeroI32::new(i32::try_from(self.0).expect("validated process id"))
            .expect("non-zero process id")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessGroup(u32);

impl ProcessGroup {
    #[must_use]
    pub fn id(self) -> u32 {
        self.0
    }
}

#[must_use]
pub fn process_group(pid: Option<u32>) -> Option<ProcessGroup> {
    pid.filter(|pid| *pid != 0).map(ProcessGroup)
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
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};
    command
        .as_std_mut()
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
}

/// Adds the close-sweep required for a child that outlives its launcher.
#[cfg(unix)]
pub fn configure_background_process(command: &mut tokio::process::Command) {
    use std::os::unix::process::CommandExt as _;
    #[allow(unsafe_code)]
    unsafe {
        command.as_std_mut().pre_exec(|| {
            for fd in 3..65_536_i32 {
                rustix::io::close(fd);
            }
            Ok(())
        });
    }
}

#[cfg(windows)]
pub fn configure_background_process(command: &mut tokio::process::Command) {
    configure_process_group(command);
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

pub fn signal_process_group_id(pid: ProcessId, signal: ProcessSignal) -> std::io::Result<()> {
    signal_process_group(ProcessGroup(pid.0), signal)
}

#[cfg(unix)]
pub fn process_group_exists(group: ProcessGroup) -> std::io::Result<bool> {
    match rustix::process::test_kill_process_group(unix_pid(group.0)?) {
        Ok(()) | Err(rustix::io::Errno::PERM) => Ok(true),
        Err(rustix::io::Errno::SRCH) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
pub fn process_group_exists(group: ProcessGroup) -> std::io::Result<bool> {
    Ok(!process_leader_exited(ProcessId(group.0))?)
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
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, SYNCHRONIZE, WaitForSingleObject,
    };

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
    matches!(error.raw_os_error(), Some(87) | Some(1168))
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
pub fn signal_process_group(group: ProcessGroup, signal: ProcessSignal) -> std::io::Result<()> {
    kill_process_tree(group.0, matches!(signal, ProcessSignal::Kill))
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

    let mut command = std::process::Command::new("taskkill.exe");
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
