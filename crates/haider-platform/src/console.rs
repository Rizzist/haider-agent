//! Windows console ownership and input used by short-lived front-door errors.

use std::io;

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetConsoleProcessList, GetStdHandle, INPUT_RECORD, KEY_EVENT,
    ReadConsoleInputW, STD_INPUT_HANDLE,
};

/// A console input handle proven to have exactly one attached process.
///
/// The handle is borrowed from the process standard handles and must not be
/// closed. Construction also proves stdin is still a console, rather than a
/// pipe or redirected file, before any blocking read can occur.
#[derive(Debug)]
pub struct SoleProcessConsole {
    input: HANDLE,
}

/// Typed failure while classifying or reading the current Windows console.
#[derive(Debug)]
pub enum ConsoleHoldError {
    /// Windows could not enumerate the processes attached to this console.
    ProcessList(io::Error),
    /// Standard input is absent, redirected, or no longer a console handle.
    ConsoleInput(io::Error),
    /// Reading the next console input event failed.
    ReadInput(io::Error),
}

impl std::fmt::Display for ConsoleHoldError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProcessList(error) => write!(formatter, "query console process list: {error}"),
            Self::ConsoleInput(error) => write!(formatter, "open console input: {error}"),
            Self::ReadInput(error) => write!(formatter, "read console input: {error}"),
        }
    }
}

impl std::error::Error for ConsoleHoldError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ProcessList(error) | Self::ConsoleInput(error) | Self::ReadInput(error) => {
                Some(error)
            }
        }
    }
}

/// Returns a console token only when this process is its sole attached owner.
///
/// `GetConsoleProcessList == 1` distinguishes an Explorer double-click from a
/// launch inside an existing shell. The console-mode check additionally makes
/// redirected and piped stdin ineligible for the blocking key read.
#[allow(unsafe_code)]
pub fn sole_process_console() -> Result<Option<SoleProcessConsole>, ConsoleHoldError> {
    let mut process_id = 0_u32;
    // SAFETY: `process_id` is writable storage for the one entry requested.
    let attached = unsafe { GetConsoleProcessList(&raw mut process_id, 1) };
    if attached == 0 {
        return Err(ConsoleHoldError::ProcessList(io::Error::last_os_error()));
    }
    if attached != 1 {
        return Ok(None);
    }

    // SAFETY: the documented standard-input selector requires no caller-owned pointer.
    let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if input.is_null() || input == INVALID_HANDLE_VALUE {
        return Err(ConsoleHoldError::ConsoleInput(io::Error::new(
            io::ErrorKind::NotConnected,
            "standard input handle is unavailable",
        )));
    }
    let mut mode = 0_u32;
    // SAFETY: `input` is the process standard-input handle and `mode` is writable storage.
    if unsafe { GetConsoleMode(input, &raw mut mode) } == 0 {
        return Err(ConsoleHoldError::ConsoleInput(io::Error::last_os_error()));
    }
    Ok(Some(SoleProcessConsole { input }))
}

impl SoleProcessConsole {
    /// Blocks until the console reports a key-down event.
    #[allow(unsafe_code)]
    pub fn wait_for_keypress(self) -> Result<(), ConsoleHoldError> {
        loop {
            let mut record = INPUT_RECORD::default();
            let mut records_read = 0_u32;
            // SAFETY: `self.input` was verified as a console input handle;
            // `record` and `records_read` are writable for the requested item.
            if unsafe { ReadConsoleInputW(self.input, &raw mut record, 1, &raw mut records_read) }
                == 0
            {
                return Err(ConsoleHoldError::ReadInput(io::Error::last_os_error()));
            }
            if records_read == 0 || record.EventType != KEY_EVENT as u16 {
                continue;
            }
            // SAFETY: `EventType == KEY_EVENT` selects the active union field.
            let key = unsafe { record.Event.KeyEvent };
            if key.bKeyDown != 0 {
                return Ok(());
            }
        }
    }
}
