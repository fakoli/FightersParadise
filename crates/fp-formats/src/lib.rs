//! # fp-formats
//!
//! Parsers for all MUGEN file formats used by the Fighters Paradise engine.
//!
//! This crate handles reading and decoding the binary and text-based file formats
//! that define MUGEN characters, stages, and system configuration. Each format
//! has its own submodule:
//!
//! - [`sff`] — Sprite File Format (SFF v1/v2) — indexed-color sprite containers
//! - [`air`] — Animation file format — frame sequences with timing and collision boxes
//! - [`def`] — DEF configuration files — simple INI-like key/value config
//!
//! Future modules (not yet implemented):
//! - `cns` — Character state definitions
//! - `cmd` — Input command definitions
//! - `snd` — Sound container format
//! - `fnt` — Font format

#![warn(missing_docs)]

pub mod air;
pub mod def;
pub mod sff;
