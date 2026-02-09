//! # fp-input
//!
//! Input handling for the Fighters Paradise engine. Manages keyboard/gamepad
//! polling, input buffering (60-frame ring buffer), and MUGEN command sequence
//! matching for special move detection.

#![warn(missing_docs)]

pub mod buffer;
pub mod command;
pub mod state;

pub use buffer::InputBuffer;
pub use command::{compile_command, CommandDef, CommandElement, CommandMatcher, InputModifier};
pub use state::{Button, DirToken, Direction, InputState, LogicalDirection, BUTTON_COUNT};
