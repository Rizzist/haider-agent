//! Local AppKit gate: `cargo test -p haider-tools --features native-macos-presence-gate
//! --test overlay_macos_native_gate`. The executable's main is the AppKit main
//! thread; libtest runs test functions on worker threads.

#[cfg(target_os = "macos")]
pub use haider_tools::presence::{
    PRESENCE_EVIDENCE_CAPTURABLE_ENV, PresenceCommand, PresenceEvent, PresenceMark, PresencePoint,
    SYNTHETIC_INPUT_TAG, art, encode_event, parse_command_line,
};

#[cfg(target_os = "macos")]
#[allow(dead_code, unused_imports)] // The harness includes the full production module.
#[path = "../src/presence/overlay_macos_logic.rs"]
mod overlay_macos_logic;

#[cfg(target_os = "macos")]
#[allow(dead_code, unused_imports, unsafe_code)] // Only the gate entry runs here.
#[path = "../src/presence/overlay_macos.rs"]
mod overlay_macos;

#[cfg(target_os = "macos")]
fn main() {
    overlay_macos::run_native_gate();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("NATIVE_GATE: SKIP: AppKit is available only on macOS");
}
