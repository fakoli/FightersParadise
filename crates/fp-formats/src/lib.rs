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
//! - [`fnt`] — FNT font format — v1 bitmap (embedded PCX glyph strip + glyph map)
//!   decoded fully; v2 (MUGEN 1.0+ SFF sprite-font) detected + glyph-table parsed
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

/// Deterministic serialization helper for `HashMap` fields (F034 T086).
///
/// `HashMap` iteration order is unspecified, so two serializations of the same
/// map can differ byte-for-byte — which would defeat the content-addressed IR
/// cache the parsed formats feed into. This module serializes a map through a
/// sorted intermediate (`BTreeMap`) so the encoding is **deterministic**:
/// identical maps always produce identical bytes, with keys emitted in sorted
/// order. Deserialization uses serde's stock `HashMap` path (input order does
/// not matter), so only [`serialize`](sorted_map::serialize) is provided.
pub(crate) mod sorted_map {
    use std::collections::{BTreeMap, HashMap};

    use serde::{Serialize, Serializer};

    /// Serializes a `HashMap` as a sorted (`BTreeMap`) sequence so the byte
    /// output is deterministic across runs.
    pub(crate) fn serialize<S, K, V>(map: &HashMap<K, V>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        K: Serialize + Ord,
        V: Serialize,
    {
        let sorted: BTreeMap<&K, &V> = map.iter().collect();
        sorted.serialize(serializer)
    }
}
