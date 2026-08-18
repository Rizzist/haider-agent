//! Typed input protocol for the native `computer` tool.
//!
//! Coordinates are integer pixels in the most recent screenshot delivered to
//! the provider, after the CU-1 image admission/downscale step. The platform
//! backend is responsible for mapping that coordinate space to native display
//! coordinates.

use serde::{Deserialize, Serialize};

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

/// Anthropic-compatible action vocabulary exposed by the `computer` tool.
///
/// The tagged shape deliberately keeps the action name at the top level so a
/// later native provider adapter can map it without translating a nested
/// command envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ComputerAction {
    Screenshot,
    CursorPosition,
    LeftClick {
        x: u32,
        y: u32,
    },
    RightClick,
    MiddleClick,
    DoubleClick,
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
    /// The permission class for this exact dynamic action.
    #[must_use]
    pub const fn effect_class(&self) -> crate::effect::EffectClass {
        match self {
            Self::Screenshot | Self::CursorPosition => crate::effect::EffectClass::ScreenObserve,
            // `wait` is conservatively control-gated: it is part of an
            // interactive computer-action sequence, but grants no observer a
            // new way to keep a control sequence alive.
            Self::LeftClick { .. }
            | Self::RightClick
            | Self::MiddleClick
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
