//! Typed input protocol for the native `computer` tool.
//!
//! Coordinates are integer pixels in the most recent screenshot delivered to
//! the provider, after the CU-1 image admission/downscale step. The platform
//! backend is responsible for mapping that coordinate space to native display
//! coordinates.

use serde::{Deserialize, Serialize};

/// Registered name of the native `computer` tool. The daemon routes exactly
/// this name to the computer backend, so it is the tool's typed identity.
pub const COMPUTER_TOOL_NAME: &str = "computer";

/// Seconds without any `computer`/`mobile` action after which the visible
/// "Haider is controlling …" presence indicator retires. Shared by the
/// daemon's overlay controller, the TUI header chip and the Android overlay
/// (mirrored as a Kotlin constant) so every surface appears and disappears
/// on the same schedule. The next action re-shows it immediately.
pub const CU_PRESENCE_IDLE_SECS: u64 = 30;

/// One point in the model-visible screenshot coordinate space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenPoint {
    pub x: u32,
    pub y: u32,
}

/// Direction for a native scroll-wheel action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

/// Provider-neutral action vocabulary exposed by Haider's `computer` tool.
///
/// The tagged shape deliberately keeps the action name at the top level so
/// native provider adapters can translate it without adding a second nested
/// command envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ComputerAction {
    Screenshot,
    CursorPosition,
    Inspect {
        x: u32,
        y: u32,
    },
    LeftClick {
        x: u32,
        y: u32,
    },
    RightClick,
    MiddleClick,
    DoubleClick,
    TripleClick,
    LeftMouseDown,
    LeftMouseUp,
    MouseMove {
        x: u32,
        y: u32,
    },
    LeftClickDrag {
        from: ScreenPoint,
        to: ScreenPoint,
    },
    Type {
        text: String,
    },
    Key {
        keys: String,
    },
    Scroll {
        x: u32,
        y: u32,
        direction: ScrollDirection,
        amount: u32,
    },
    Wait {
        ms: u64,
    },
}

impl ComputerAction {
    /// Native click-state count for repeated left clicks at the current cursor.
    #[must_use]
    pub const fn click_count(&self) -> Option<u8> {
        match self {
            Self::DoubleClick => Some(2),
            Self::TripleClick => Some(3),
            _ => None,
        }
    }

    /// The permission class for this exact dynamic action.
    #[must_use]
    pub const fn effect_class(&self) -> crate::effect::EffectClass {
        match self {
            Self::Screenshot | Self::CursorPosition | Self::Inspect { .. } => {
                crate::effect::EffectClass::ScreenObserve
            }
            // `wait` is conservatively control-gated: it is part of an
            // interactive computer-action sequence, but grants no observer a
            // new way to keep a control sequence alive.
            Self::LeftClick { .. }
            | Self::RightClick
            | Self::MiddleClick
            | Self::TripleClick
            | Self::DoubleClick
            | Self::LeftMouseDown
            | Self::LeftMouseUp
            | Self::MouseMove { .. }
            | Self::LeftClickDrag { .. }
            | Self::Type { .. }
            | Self::Key { .. }
            | Self::Scroll { .. }
            | Self::Wait { .. } => crate::effect::EffectClass::ScreenControl,
        }
    }
}
