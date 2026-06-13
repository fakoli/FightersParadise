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
//! - [`cmd`] — CMD command file format — input command sequences and timing
//! - [`cns`] — CNS state files — statedefs and state controllers with raw triggers
//! - [`def`] — DEF configuration files — simple INI-like key/value config
//! - [`snd`] — Sound container format — WAV/PCM blobs addressed by (group, sample)
//!
//! Future modules (not yet implemented):
//! - `fnt` — Font format

#![warn(missing_docs)]

pub mod air;
pub mod cmd;
pub mod cns;
pub mod def;
pub mod sff;
pub mod snd;
