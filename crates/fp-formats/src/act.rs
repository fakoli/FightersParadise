//! # ACT — MUGEN external palette parser
//!
//! Parses MUGEN `.act` palette files (referenced by `pal1`..`pal12` in a
//! character `.def`) into a typed [`ActPalette`] that mirrors the project's
//! existing RGBA palette representation (a 1024-byte, 256-colour RGBA buffer —
//! identical in shape to what [`crate::sff::SffFile::palette`] returns and what
//! [`crate::sff::rgb_to_rgba`] produces).
//!
//! # Binary layout
//!
//! A MUGEN `.act` file is a raw VGA palette: **256 RGB triples = 768 bytes**,
//! with **no header or signature**. The colours are stored in **reverse index
//! order** — the *first* triple in the file is palette index **255** and the
//! *last* triple is palette index **0**:
//!
//! ```text
//! offset  index   bytes
//! 0       255     R G B
//! 3       254     R G B
//! ...
//! 762     1       R G B
//! 765     0       R G B
//! ```
//!
//! Some `.act` files are **772 bytes** — the 768-byte palette followed by a
//! 4-byte trailer (an unused tail some editors append). Both lengths are
//! accepted; only the first 768 bytes are interpreted as palette data.
//!
//! # Conventions mirrored from SFF palettes
//!
//! - Output is 1024 bytes (256 × RGBA), exactly like the SFF palette table.
//! - **Palette index 0 is transparent** (alpha = 0); every other index is
//!   opaque (alpha = 255). This matches the MUGEN/SFF index-0-transparent rule
//!   that [`crate::sff::rgb_to_rgba`] applies.
//!
//! # Never crash on bad content
//!
//! Malformed or short input never panics. A file shorter than 768 bytes is
//! `tracing::warn!`-logged and the missing tail is treated as black; every read
//! is bounds-checked before indexing.

use std::path::Path;

use fp_core::FpResult;

/// Size of the raw VGA palette inside an `.act` file (256 colours × RGB).
pub const ACT_PALETTE_SIZE: usize = 768;

/// Size of an `.act` file that carries a 4-byte trailer after the palette.
pub const ACT_PALETTE_SIZE_WITH_TRAILER: usize = ACT_PALETTE_SIZE + 4;

/// Size of the decoded RGBA palette (256 colours × RGBA), matching SFF palettes.
pub const ACT_RGBA_SIZE: usize = 1024;

/// A parsed MUGEN `.act` palette in the project's RGBA representation.
///
/// Holds 256 colours as a flat 1024-byte RGBA buffer (the same shape returned
/// by [`crate::sff::SffFile::palette`]), already de-reversed into natural index
/// order (`rgba[0..4]` is palette index 0) with the index-0-transparent
/// convention applied.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ActPalette {
    /// 256 colours × 4 bytes (RGBA), index 0 first. Index 0 has alpha = 0.
    ///
    /// Serialized through [`rgba_bytes`] because serde's stock array support
    /// stops at 32 elements; the helper round-trips the fixed
    /// [`ACT_RGBA_SIZE`]-byte buffer losslessly via a byte sequence.
    #[serde(with = "rgba_bytes")]
    pub rgba: [u8; ACT_RGBA_SIZE],
}

/// (De)serializes the fixed [`ACT_RGBA_SIZE`]-byte palette buffer.
///
/// serde derives `Serialize`/`Deserialize` for `[T; N]` only up to `N == 32`,
/// so the 1024-byte palette needs a manual seam. It is encoded as a byte
/// sequence and decoded back into the fixed-size array, returning a recoverable
/// error (never a panic) if the byte count does not match.
mod rgba_bytes {
    use super::ACT_RGBA_SIZE;
    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer, Serializer};

    /// Serializes the array as a borrowed byte slice.
    pub(super) fn serialize<S>(rgba: &[u8; ACT_RGBA_SIZE], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(rgba)
    }

    /// Deserializes a byte buffer back into the fixed-size array.
    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<[u8; ACT_RGBA_SIZE], D::Error>
    where
        D: Deserializer<'de>,
    {
        let bytes = Vec::<u8>::deserialize(deserializer)?;
        bytes.try_into().map_err(|v: Vec<u8>| {
            D::Error::custom(format!(
                "ActPalette.rgba expected {ACT_RGBA_SIZE} bytes, got {}",
                v.len()
            ))
        })
    }
}

impl ActPalette {
    /// Loads and parses an `.act` palette from the given path.
    ///
    /// Reads the whole file into memory and delegates to [`Self::from_bytes`].
    pub fn load(path: &Path) -> FpResult<Self> {
        let data = std::fs::read(path)?;
        Self::from_bytes(&data)
    }

    /// Parses an `.act` palette from raw bytes already in memory.
    ///
    /// Accepts the canonical 768-byte palette and the 772-byte (palette + 4-byte
    /// trailer) variant; any longer input is tolerated and the trailing bytes are
    /// ignored. The on-disk colours are stored in reverse index order, so this
    /// de-reverses them into natural order (index 0 first). Input shorter than
    /// 768 bytes never panics: the missing colours are filled with black and a
    /// warning is logged.
    pub fn from_bytes(data: &[u8]) -> FpResult<Self> {
        if data.len() < ACT_PALETTE_SIZE {
            tracing::warn!(
                len = data.len(),
                "ACT: palette shorter than {ACT_PALETTE_SIZE} bytes; padding missing colours with black"
            );
        } else if data.len() != ACT_PALETTE_SIZE && data.len() != ACT_PALETTE_SIZE_WITH_TRAILER {
            tracing::warn!(
                len = data.len(),
                "ACT: unexpected palette length (expected {ACT_PALETTE_SIZE} or {ACT_PALETTE_SIZE_WITH_TRAILER}); using first {ACT_PALETTE_SIZE} bytes"
            );
        }

        let mut rgba = [0u8; ACT_RGBA_SIZE];
        for idx in 0..256usize {
            // The file stores index 255 first, so the on-disk triple for palette
            // index `idx` lives at reversed position `255 - idx`.
            let src = (255 - idx) * 3;
            let dst = idx * 4;
            // Bounds-check every read so a short file is a safe default, not a panic.
            let (r, g, b) = if src + 3 <= data.len() {
                (data[src], data[src + 1], data[src + 2])
            } else {
                (0, 0, 0)
            };
            rgba[dst] = r;
            rgba[dst + 1] = g;
            rgba[dst + 2] = b;
            // Index 0 is transparent in MUGEN palettes; all others opaque.
            rgba[dst + 3] = if idx == 0 { 0 } else { 255 };
        }

        Ok(Self { rgba })
    }

    /// Returns the RGBA bytes for palette `index` (0..=255) as `(r, g, b, a)`.
    ///
    /// Out-of-range indices return transparent black `(0, 0, 0, 0)`.
    pub fn color(&self, index: usize) -> (u8, u8, u8, u8) {
        let base = index * 4;
        if base + 4 <= self.rgba.len() {
            (
                self.rgba[base],
                self.rgba[base + 1],
                self.rgba[base + 2],
                self.rgba[base + 3],
            )
        } else {
            (0, 0, 0, 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a synthetic 768-byte `.act`: on-disk position `p` (0..256) gets a
    /// distinct RGB so the reversal can be asserted. On-disk position 0 is the
    /// highest palette index (255); position 255 is palette index 0.
    fn make_reversed_act() -> Vec<u8> {
        let mut buf = vec![0u8; ACT_PALETTE_SIZE];
        for p in 0..256usize {
            // Encode the on-disk position into the colour so we can detect where
            // it lands after de-reversal.
            buf[p * 3] = p as u8; // R = on-disk position
            buf[p * 3 + 1] = 10; // G marker
            buf[p * 3 + 2] = 20; // B marker
        }
        buf
    }

    #[test]
    fn parses_768_byte_reversed_palette() {
        let act = ActPalette::from_bytes(&make_reversed_act()).unwrap();

        // On-disk position 0 (R=0) is palette index 255.
        let (r, g, b, a) = act.color(255);
        assert_eq!((r, g, b), (0, 10, 20));
        assert_eq!(a, 255);

        // On-disk position 255 (R=255) is palette index 0.
        let (r, g, b, a) = act.color(0);
        assert_eq!((r, g, b), (255, 10, 20));
        // Index 0 must be transparent regardless of its colour.
        assert_eq!(a, 0);

        // On-disk position 1 (R=1) is palette index 254.
        let (r, _, _, _) = act.color(254);
        assert_eq!(r, 1);

        // On-disk position 200 (R=200) is palette index 55.
        let (r, _, _, _) = act.color(55);
        assert_eq!(r, 200);
    }

    #[test]
    fn index_zero_is_transparent_all_others_opaque() {
        let act = ActPalette::from_bytes(&make_reversed_act()).unwrap();
        assert_eq!(act.color(0).3, 0);
        for idx in 1..256 {
            assert_eq!(act.color(idx).3, 255, "index {idx} must be opaque");
        }
    }

    #[test]
    fn parses_772_byte_variant_identically() {
        let base = make_reversed_act();
        let mut with_trailer = base.clone();
        // Append a 4-byte trailer with arbitrary content that must be ignored.
        with_trailer.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(with_trailer.len(), ACT_PALETTE_SIZE_WITH_TRAILER);

        let a = ActPalette::from_bytes(&base).unwrap();
        let b = ActPalette::from_bytes(&with_trailer).unwrap();
        // The trailer must not affect any palette entry.
        assert_eq!(a.rgba, b.rgba);
    }

    #[test]
    fn short_input_recovers_without_panic() {
        // Far too short — only one colour's worth of data.
        let act = ActPalette::from_bytes(&[1, 2, 3]).unwrap();
        // On-disk position 0 (palette index 255) gets the only present triple.
        assert_eq!(act.color(255), (1, 2, 3, 255));
        // Everything else falls back to black; index 0 still transparent.
        assert_eq!(act.color(0), (0, 0, 0, 0));
        assert_eq!(act.color(100), (0, 0, 0, 255));
    }

    #[test]
    fn empty_input_recovers_without_panic() {
        let act = ActPalette::from_bytes(&[]).unwrap();
        assert_eq!(act.color(0), (0, 0, 0, 0));
        assert_eq!(act.color(255), (0, 0, 0, 255));
    }

    #[test]
    fn out_of_range_color_is_transparent_black() {
        let act = ActPalette::from_bytes(&make_reversed_act()).unwrap();
        assert_eq!(act.color(256), (0, 0, 0, 0));
        assert_eq!(act.color(9999), (0, 0, 0, 0));
    }

    #[test]
    fn rgba_buffer_matches_sff_shape() {
        let act = ActPalette::from_bytes(&make_reversed_act()).unwrap();
        // Same 1024-byte (256 × RGBA) shape SFF palettes use.
        assert_eq!(act.rgba.len(), ACT_RGBA_SIZE);
        assert_eq!(act.rgba.len(), crate::sff::PALETTE_RGBA_SIZE);
    }
}
