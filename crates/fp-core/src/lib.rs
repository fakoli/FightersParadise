//! # fp-core
//!
//! Core types, math primitives, and error handling for the Fighters Paradise engine.
//!
//! This crate provides the foundational types shared across all other crates in the
//! workspace. It intentionally has minimal dependencies (only `thiserror` and `tracing`)
//! so it can be depended on by every other crate without pulling in heavy libraries.
//!
//! ## Key Types
//!
//! - [`Vec2`] — Generic 2D vector used for positions, velocities, and sizes
//! - [`Rect`] — Axis-aligned bounding box for collision detection and sprite regions
//! - [`SpriteId`] — Group/image pair identifying a sprite in an SFF file
//! - [`AnimId`] — Animation action number
//! - [`SoundId`] — Group/sample pair identifying a sound in an SND file
//! - [`FpError`] — Unified error type for the entire engine
//! - [`FpResult`] — Convenience alias for `Result<T, FpError>`

#![warn(missing_docs)]

mod error;
mod math;
mod types;

pub use error::{FpError, FpResult};
pub use math::{Rect, Vec2};
pub use types::{AnimId, SoundId, SpriteId};
