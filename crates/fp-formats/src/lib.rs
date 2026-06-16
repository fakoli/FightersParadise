//! # fp-formats
//!
//! Parsers for all MUGEN file formats used by the Fighters Paradise engine.
//!
//! This crate handles reading and decoding the binary and text-based file formats
//! that define MUGEN characters, stages, and system configuration. Each format
//! has its own submodule:
//!
//! - [`sff`] — Sprite File Format (SFF v1/v2) — indexed-color sprite containers
//! - [`act`] — ACT external palette files — 256-color VGA palettes (reverse-ordered)
//! - [`air`] — Animation file format — frame sequences with timing and collision boxes
//! - [`cmd`] — CMD command file format — input command sequences and timing
//! - [`cns`] — CNS state files — statedefs and state controllers with raw triggers
//! - [`def`] — DEF configuration files — simple INI-like key/value config
//! - [`snd`] — Sound container format — WAV/PCM blobs addressed by (group, sample)
//! - [`fnt`] — FNT bitmap font format (v1) — embedded PCX glyph strip + glyph map
//! - [`text`] — legacy-encoding-tolerant text decoding (Shift-JIS, ...) shared by
//!   the text parsers above so non-UTF-8 community files parse instead of being
//!   skipped

#![warn(missing_docs)]

pub mod act;
pub mod air;
pub mod cmd;
pub mod cns;
pub mod def;
pub mod fnt;
pub mod sff;
pub mod snd;
pub mod text;
