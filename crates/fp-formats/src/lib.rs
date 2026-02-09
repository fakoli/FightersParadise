//! # fp-formats
//!
//! Parsers for all MUGEN file formats used by the Fighters Paradise engine.
//!
//! This crate handles reading and decoding the binary and text-based file formats
//! that define MUGEN characters, stages, and system configuration. Each format
//! has its own submodule:
//!
//! - [`sff`] — Sprite File Format (SFF v1/v2) — indexed-color sprite containers
//!
//! Future modules (not yet implemented):
//! - `air` — Animation file format
//! - `cns` — Character state definitions
//! - `cmd` — Input command definitions
//! - `def` — INI-like configuration files
//! - `snd` — Sound container format
//! - `fnt` — Font format

#![warn(missing_docs)]

pub mod sff;
