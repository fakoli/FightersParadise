//! # fp-input
//!
//! Input handling for the Fighters Paradise engine. Manages keyboard/gamepad
//! polling, input buffering (60-frame ring buffer), and MUGEN command sequence
//! matching for special move detection.

#![warn(missing_docs)]

pub mod buffer;
pub mod command;
pub mod controller;
pub mod state;

pub use buffer::{InputBuffer, InputBufferSnapshot};
pub use controller::{map_controller, ControllerInput, RawController, DEADZONE_DEFAULT};
pub use command::{
    compile_command, CommandDef, CommandElement, CommandMatcher, CommandMatcherSnapshot,
    InputModifier,
};
pub use state::{
    dir_matches, dir_matches_detect, logical_direction, Button, DirToken, Direction, InputState,
    LogicalDirection, BUTTON_COUNT,
};
