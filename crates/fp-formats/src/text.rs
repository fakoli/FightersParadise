//! # Legacy text-encoding decoding for MUGEN text files
//!
//! MUGEN content predates the UTF-8 era, and a large fraction of community
//! characters ship their text files (`.cns`, `.cmd`, `.def`, ...) in legacy
//! single-/double-byte encodings — most commonly **Shift-JIS** (Japanese) but
//! also other Windows code pages. Reading such a file with
//! [`std::fs::read_to_string`] fails the moment a non-UTF-8 byte is hit, which
//! historically caused the entire state/command file to be skipped: the
//! character then loaded **zero states** and rendered as a test pattern.
//!
//! This module centralizes a tolerant byte-to-[`String`] decode used by the
//! text parsers ([`crate::cns`], [`crate::cmd`]) so those files parse instead of
//! being dropped:
//!
//! 1. A leading UTF-8 BOM (`EF BB BF`) is stripped, then a **strict UTF-8**
//!    decode is attempted. The overwhelming majority of files (and every
//!    file the project authors itself) are valid UTF-8 and take this fast path
//!    with no transcoding.
//! 2. If the bytes are not valid UTF-8, a **Shift-JIS** decode is attempted.
//!    Shift-JIS is ASCII-compatible, so the CNS/CMD keywords still parse and any
//!    Japanese comments/labels transcode to their UTF-8 equivalents.
//! 3. The decode **never fails**: even malformed/unknown byte sequences are
//!    decoded with U+FFFD replacement characters rather than erroring. A parser
//!    operating on the (mostly-ASCII) keywords still recovers the structure, in
//!    keeping with the engine's "never crash on bad content" philosophy. Any
//!    substitution is `tracing::warn!`-logged once per file (not per line), so it
//!    is safe to call off the 60 Hz hot path during loading.

use std::path::Path;

use encoding_rs::SHIFT_JIS;
use fp_core::FpResult;

/// Strips a leading UTF-8 byte-order mark (`EF BB BF`) from `bytes`, if present.
fn strip_utf8_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}

/// Decodes a slice of file bytes into a UTF-8 [`String`], tolerating legacy
/// encodings.
///
/// `label` is a short human description of the source (e.g. a path) used only in
/// the warning emitted when a non-UTF-8 fallback or lossy substitution happens.
///
/// Decoding **never fails**: valid UTF-8 is returned as-is (after stripping a
/// leading BOM); otherwise the bytes are transcoded from Shift-JIS, substituting
/// U+FFFD for anything undecodable. See the [module docs](self) for the full
/// policy.
pub fn decode_text_bytes(bytes: &[u8], label: &str) -> String {
    let bytes = strip_utf8_bom(bytes);

    // Fast path: already valid UTF-8 (the common case, and every file the
    // project itself authors). No transcoding, no allocation beyond the String.
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => {
            // Not UTF-8 — fall back to Shift-JIS, the dominant legacy encoding
            // for MUGEN content. encoding_rs decodes ASCII-compatibly and only
            // substitutes U+FFFD for byte sequences it cannot map, so the
            // (ASCII) CNS/CMD keywords still parse.
            let (decoded, _enc, had_errors) = SHIFT_JIS.decode(bytes);
            if had_errors {
                tracing::warn!(
                    "text: {label} is not valid UTF-8 and contains bytes Shift-JIS \
                     could not decode; substituted replacement characters"
                );
            } else {
                tracing::warn!("text: {label} decoded as Shift-JIS (not UTF-8)");
            }
            decoded.into_owned()
        }
    }
}

/// Reads a text file from `path` and decodes it into a UTF-8 [`String`],
/// tolerating legacy (e.g. Shift-JIS) encodings.
///
/// This is the encoding-aware replacement for [`std::fs::read_to_string`] used
/// by the MUGEN text parsers. Unlike `read_to_string`, it does **not** error on
/// non-UTF-8 bytes; it transcodes them (see [`decode_text_bytes`] and the
/// [module docs](self)).
///
/// # Errors
///
/// Returns [`fp_core::FpError::Io`](fp_core::FpError) only if the file cannot be
/// read at all (missing, permissions, ...). A successfully-read file always
/// decodes to a `String` — invalid encodings degrade gracefully rather than
/// failing.
pub fn read_text_file(path: &Path) -> FpResult<String> {
    let bytes = std::fs::read(path)?;
    Ok(decode_text_bytes(&bytes, &path.display().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encodes a `&str` into Shift-JIS bytes for synthesizing fixtures, never
    /// touching any external file. The clean-room contract forbids reading real
    /// content, so tests author their own Shift-JIS bytes here.
    fn to_shift_jis(s: &str) -> Vec<u8> {
        let (bytes, _enc, had_errors) = SHIFT_JIS.encode(s);
        assert!(!had_errors, "fixture must be Shift-JIS-encodable");
        bytes.into_owned()
    }

    #[test]
    fn plain_utf8_passes_through() {
        let bytes = "type = S\n".as_bytes();
        assert_eq!(decode_text_bytes(bytes, "x"), "type = S\n");
    }

    #[test]
    fn utf8_bom_is_stripped() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"type = S");
        // The BOM must not appear in the decoded string.
        assert_eq!(decode_text_bytes(&bytes, "x"), "type = S");
    }

    #[test]
    fn utf8_with_non_ascii_is_preserved() {
        // A valid UTF-8 file containing Japanese must round-trip unchanged
        // (it must NOT be misinterpreted as Shift-JIS).
        let original = "; 波動拳\ntype = S\n";
        let decoded = decode_text_bytes(original.as_bytes(), "x");
        assert_eq!(decoded, original);
    }

    #[test]
    fn shift_jis_japanese_is_transcoded() {
        // Bytes that are NOT valid UTF-8 but ARE valid Shift-JIS must transcode
        // to the correct UTF-8 text, keeping the ASCII keywords intact.
        let original = "; 波動拳コメント\ntype = S\nanim = 1000\n";
        let sjis = to_shift_jis(original);
        // Sanity: the fixture really is non-UTF-8 (so the fallback path runs).
        assert!(
            std::str::from_utf8(&sjis).is_err(),
            "fixture should be invalid UTF-8 to exercise the fallback"
        );
        let decoded = decode_text_bytes(&sjis, "x");
        assert_eq!(decoded, original);
    }

    #[test]
    fn ascii_only_shift_jis_is_identical() {
        // Shift-JIS is ASCII-compatible: a pure-ASCII payload decodes to the
        // same text via either path.
        let original = "trigger1 = AnimElem = 3\n";
        let sjis = to_shift_jis(original);
        assert_eq!(decode_text_bytes(&sjis, "x"), original);
    }

    #[test]
    fn undecodable_bytes_never_panic() {
        // A byte sequence that is neither valid UTF-8 nor cleanly Shift-JIS must
        // still decode (lossily) rather than panicking or erroring.
        let garbage: &[u8] = &[0x80, 0xFD, 0xFF, b'=', 0xFE, b'\n'];
        let decoded = decode_text_bytes(garbage, "garbage");
        // The ASCII `=` and newline survive; undecodable bytes become U+FFFD.
        assert!(decoded.contains('='));
        assert!(decoded.contains('\n'));
    }

    #[test]
    fn read_missing_file_errors() {
        let result = read_text_file(Path::new("/nonexistent/definitely/not/here.cns"));
        assert!(result.is_err(), "missing file should surface an IO error");
    }
}
