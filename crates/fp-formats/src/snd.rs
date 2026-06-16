//! # SND — Sound container format parser
//!
//! Parses MUGEN `.snd` sound-container files. An SND file bundles every sound
//! effect and voice clip for a character (or for the shared `common.snd`) into a
//! single archive. Each sound is addressed by a `(group, sample)` pair — the
//! same coordinates used by the `PlaySnd` state controller — and stores a raw
//! WAV/PCM payload.
//!
//! This parser walks the container structure and exposes each sound as a raw
//! byte slice. It deliberately does **not** decode the audio; PCM decoding and
//! playback live in `fp-audio` (Phase 8).
//!
//! # Format
//!
//! The on-disk layout (little-endian throughout) is:
//!
//! | Offset | Size | Field |
//! |--------|------|-------|
//! | 0      | 12   | signature `ElecbyteSnd\0` |
//! | 12     | 4    | version (`u32`; KFM uses `4` for v4.0.0.0) |
//! | 16     | 4    | number of sounds (`u32`) |
//! | 20     | 4    | absolute byte offset of the first sound sub-header |
//!
//! Each sound sub-header is 16 bytes, immediately followed by its payload:
//!
//! | Offset | Size | Field |
//! |--------|------|-------|
//! | +0     | 4    | absolute byte offset of the **next** sub-header |
//! | +4     | 4    | payload length in bytes |
//! | +8     | 4    | group number |
//! | +12    | 4    | sample / sound number within the group |
//! | +16    | len  | raw payload (a RIFF/WAVE blob in practice) |
//!
//! The sub-headers form a singly linked list keyed by the `next` offset, but the
//! sound *count* from the header is the authoritative terminator: real files
//! (including KFM) leave the final entry's `next` pointing at end-of-file rather
//! than `0`, so we stop after `num_sounds` entries.
//!
//! # Usage
//!
//! ```no_run
//! use std::path::Path;
//! use fp_formats::snd::SndFile;
//!
//! let snd = SndFile::load(Path::new("kfm.snd")).unwrap();
//! // Look up sound (group=0, sample=0)
//! if let Some(payload) = snd.sound(0, 0) {
//!     println!("{} bytes of WAV/PCM", payload.len());
//! }
//! ```

use std::path::Path;

use fp_core::{FpError, FpResult};
use nom::bytes::complete::tag;
use nom::number::complete::le_u32;

/// The expected file signature at the start of every SND file.
const SND_SIGNATURE: &[u8; 12] = b"ElecbyteSnd\0";

/// Byte size of the fixed file header (signature + version + count + first offset).
const HEADER_SIZE: usize = 24;

/// Byte size of a single sound sub-header (next, length, group, sample).
const SUBHEADER_SIZE: usize = 16;

/// The codec of a sound payload, sniffed from its leading magic bytes.
///
/// An SND container is codec-agnostic — it just bundles opaque blobs — so a
/// single `.snd` can in principle hold any audio format. MUGEN/Ikemen content
/// is overwhelmingly RIFF/WAVE, which the sibling crate `fp-audio` decodes.
/// Classic console-MUGEN ports occasionally embed **ADX** (the CRI ADX codec,
/// sync byte `0x80`); that is **not** decodable today, so flagging it here lets
/// a consumer warn-and-skip it instead of handing garbage to the WAV decoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundFormat {
    /// RIFF/WAVE (`"RIFF" ...`) — the supported, decodable case.
    Wav,
    /// CRI ADX (sync byte `0x80`, optionally confirmed by the `(c)CRI`
    /// copyright marker) — recognised but **not** supported; a consumer should
    /// warn and skip rather than attempt to decode it.
    Adx,
    /// Anything else (or a payload too short to sniff) — format unknown.
    Unknown,
}

impl SoundFormat {
    /// Returns `true` if this codec can be decoded to PCM by `fp-audio`.
    ///
    /// Only [`SoundFormat::Wav`] is decodable today; [`SoundFormat::Adx`] is
    /// recognised but unsupported, and [`SoundFormat::Unknown`] cannot be
    /// trusted. A consumer should warn-and-skip any non-decodable entry rather
    /// than hand its bytes to the WAV decoder.
    pub fn is_decodable(self) -> bool {
        matches!(self, SoundFormat::Wav)
    }
}

/// The ASCII copyright marker every CRI ADX stream carries in its header.
///
/// It follows the leading `0x80` sync byte at the offset declared by the
/// copyright-offset field, so a payload containing it (within a short prefix)
/// is ADX even if a consumer cannot trust the sync byte alone.
const ADX_COPYRIGHT_MARKER: &[u8] = b"(c)CRI";

/// How far into a payload [`sniff_sound_format`] scans for [`ADX_COPYRIGHT_MARKER`].
///
/// ADX places the marker a few bytes past a tiny fixed header; a small bounded
/// window keeps the sniff allocation-free and cheap while covering real files.
const ADX_MARKER_SCAN_LEN: usize = 32;

/// A single sound entry parsed from an SND container.
///
/// The payload is the raw, undecoded blob stored in the file — in practice a
/// RIFF/WAVE chunk. Decoding to PCM samples is the responsibility of `fp-audio`.
#[derive(Debug, Clone)]
pub struct SndEntry {
    /// Group number this sound belongs to (the first half of the `PlaySnd` key).
    pub group: u32,
    /// Sample / sound number within the group (the second half of the key).
    pub sample: u32,
    /// Raw sound payload bytes (typically a RIFF/WAVE blob).
    pub data: Vec<u8>,
}

impl SndEntry {
    /// Sniffs the payload's codec from its leading magic bytes.
    ///
    /// This is a pure, allocation-free header check — it inspects only a short
    /// bounded prefix and never decodes the audio. Use it to skip an undecodable
    /// payload (e.g. [`SoundFormat::Adx`]) before handing the blob to the WAV
    /// decoder. A too-short or unrecognised payload returns
    /// [`SoundFormat::Unknown`].
    pub fn format(&self) -> SoundFormat {
        sniff_sound_format(&self.data)
    }
}

/// Sniffs the audio codec of a raw sound payload from its leading magic bytes.
///
/// - **WAV**: starts with the `"RIFF"` chunk magic (the supported case).
/// - **ADX (CRI)**: starts with the `0x80` sync byte (the high bit of the
///   copyright-offset field), **or** carries the `(c)CRI` copyright marker
///   within the first [`ADX_MARKER_SCAN_LEN`] bytes. The marker is the
///   stronger signal; either match flags ADX. This codec is not decodable
///   today, so a consumer should warn-and-skip it (see [`SoundFormat::Adx`]).
/// - Everything else (including an empty/short payload): [`SoundFormat::Unknown`].
///
/// This is a pure, bounded, allocation-free header check and never decodes audio.
pub fn sniff_sound_format(data: &[u8]) -> SoundFormat {
    // The `(c)CRI` marker is unambiguous; check it first within a short window.
    let scan = &data[..data.len().min(ADX_MARKER_SCAN_LEN)];
    if scan
        .windows(ADX_COPYRIGHT_MARKER.len())
        .any(|w| w == ADX_COPYRIGHT_MARKER)
    {
        return SoundFormat::Adx;
    }
    if data.first() == Some(&0x80) {
        return SoundFormat::Adx;
    }
    if data.len() >= 4 && &data[0..4] == b"RIFF" {
        return SoundFormat::Wav;
    }
    SoundFormat::Unknown
}

/// A fully loaded SND sound container.
///
/// Holds the parsed file version and every successfully recovered sound entry.
/// Truncated or garbled entries are warn-logged and skipped, so a partially
/// damaged file still yields a usable [`SndFile`] with whatever sounds parsed.
#[derive(Debug, Clone)]
pub struct SndFile {
    /// The container format version (`4` for the v4.0.0.0 files MUGEN 1.0 ships).
    pub version: u32,
    /// All sounds recovered from the container, in file order.
    pub sounds: Vec<SndEntry>,
}

impl SndFile {
    /// Loads and parses an SND file from the given path.
    ///
    /// Reads the entire file into memory and delegates to [`SndFile::from_bytes`].
    /// Returns an [`FpError::Io`] if the file cannot be read.
    pub fn load(path: &Path) -> FpResult<Self> {
        let data = std::fs::read(path)?;
        Self::from_bytes(&data)
    }

    /// Parses an SND container from raw bytes already in memory.
    ///
    /// Validates the `ElecbyteSnd` signature and reads the version, then walks
    /// the sound directory capturing each `(group, sample)` payload. Per the
    /// engine's never-crash rule, individual entries that are truncated or point
    /// out of bounds are warn-logged and skipped rather than aborting the parse,
    /// so the result is always a usable (possibly partial) [`SndFile`].
    ///
    /// Returns an [`FpError::Parse`] only when the fixed header itself is
    /// missing or invalid (too small, or wrong signature) — i.e. when there is
    /// nothing meaningful to recover.
    pub fn from_bytes(data: &[u8]) -> FpResult<Self> {
        if data.len() < HEADER_SIZE {
            return Err(FpError::parse(
                "SND",
                format!(
                    "file too small for header: {} bytes (need {})",
                    data.len(),
                    HEADER_SIZE
                ),
            ));
        }

        // Validate the signature.
        let (rest, _) = tag(SND_SIGNATURE.as_slice())(data).map_err(
            |_: nom::Err<nom::error::Error<&[u8]>>| {
                FpError::parse("SND", "invalid file signature (expected 'ElecbyteSnd\\0')")
            },
        )?;

        // Read version, sound count, and first sub-header offset. These reads are
        // infallible because we already checked `data.len() >= HEADER_SIZE`.
        let (rest, version) = read_u32(rest, "version")?;
        let (rest, num_sounds) = read_u32(rest, "sound count")?;
        let (_rest, first_offset) = read_u32(rest, "first sub-header offset")?;

        tracing::info!(version, num_sounds, "SND: parsing container");

        let sounds = walk_directory(data, num_sounds, first_offset as usize);

        if (sounds.len() as u32) < num_sounds {
            tracing::warn!(
                recovered = sounds.len(),
                declared = num_sounds,
                "SND: recovered fewer sounds than the header declared"
            );
        }

        Ok(Self { version, sounds })
    }

    /// Looks up a sound's raw payload by `(group, sample)`.
    ///
    /// Returns the first matching entry's bytes, or `None` if no sound with that
    /// `(group, sample)` pair exists. The bytes are the undecoded WAV/PCM blob.
    pub fn sound(&self, group: u32, sample: u32) -> Option<&[u8]> {
        self.sounds
            .iter()
            .find(|s| s.group == group && s.sample == sample)
            .map(|s| s.data.as_slice())
    }

    /// Returns the number of sounds recovered from the container.
    pub fn len(&self) -> usize {
        self.sounds.len()
    }

    /// Returns `true` if no sounds were recovered from the container.
    pub fn is_empty(&self) -> bool {
        self.sounds.is_empty()
    }
}

/// Reads a little-endian `u32` from `input`, mapping nom failures to a parse error.
fn read_u32<'a>(input: &'a [u8], field: &'static str) -> FpResult<(&'a [u8], u32)> {
    le_u32::<_, nom::error::Error<&[u8]>>(input)
        .map_err(|_| FpError::parse("SND", format!("failed to read {field}")))
}

/// Walks the linked list of sound sub-headers, collecting recoverable entries.
///
/// Iteration is bounded by `num_sounds` (the authoritative terminator) and by a
/// visited-offset guard, so malformed `next` pointers — including cycles or a
/// final entry that points at end-of-file — can never cause an infinite loop.
/// Any entry whose sub-header or payload falls out of bounds is warn-logged and
/// skipped; the walk then stops because the linked list can no longer be trusted.
fn walk_directory(data: &[u8], num_sounds: u32, first_offset: usize) -> Vec<SndEntry> {
    let mut sounds = Vec::new();
    let mut offset = first_offset;

    for index in 0..num_sounds {
        // Stop cleanly if the chain runs off the end of the file. A final entry
        // whose `next` equals the file length is the normal end-of-list marker.
        if offset == 0 || offset >= data.len() {
            break;
        }

        // Bounds-check the fixed sub-header.
        let Some(header) = data.get(offset..offset + SUBHEADER_SIZE) else {
            tracing::warn!(
                index,
                offset,
                file_len = data.len(),
                "SND: sub-header truncated; skipping remaining sounds"
            );
            break;
        };

        // Infallible: `header` is exactly SUBHEADER_SIZE bytes.
        let next_offset = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let length = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        let group = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
        let sample = u32::from_le_bytes([header[12], header[13], header[14], header[15]]);

        let payload_start = offset + SUBHEADER_SIZE;
        let payload_end = payload_start.saturating_add(length);

        if payload_end > data.len() {
            tracing::warn!(
                index,
                group,
                sample,
                length,
                file_len = data.len(),
                "SND: sound payload extends past end of file; skipping"
            );
            break;
        }

        sounds.push(SndEntry {
            group,
            sample,
            data: data[payload_start..payload_end].to_vec(),
        });

        // Guard against a `next` pointer that doesn't advance (would loop forever).
        if next_offset != 0 && next_offset <= offset {
            tracing::warn!(
                index,
                offset,
                next_offset,
                "SND: non-advancing next pointer; stopping directory walk"
            );
            break;
        }

        offset = next_offset;
    }

    sounds
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Appends a single sound sub-header + payload to `buf`, returning the
    /// absolute offset at which it was written. `next` is the caller-supplied
    /// absolute offset of the following sub-header (or any sentinel value).
    fn push_sound(buf: &mut Vec<u8>, next: u32, group: u32, sample: u32, payload: &[u8]) -> u32 {
        let at = buf.len() as u32;
        buf.extend_from_slice(&next.to_le_bytes());
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(&group.to_le_bytes());
        buf.extend_from_slice(&sample.to_le_bytes());
        buf.extend_from_slice(payload);
        at
    }

    /// Builds a synthetic SND file from a list of `(group, sample, payload)`
    /// tuples, wiring each sub-header's `next` to the following one. The final
    /// entry's `next` is set to the file length, matching real MUGEN files.
    fn make_test_snd(version: u32, sounds: &[(u32, u32, &[u8])]) -> Vec<u8> {
        // Header is 24 bytes; first sub-header starts right after it.
        let first_offset: u32 = HEADER_SIZE as u32;

        let mut body: Vec<u8> = Vec::new();
        // First, lay out the sounds into `body` (offsets relative to body start),
        // then fix up the `next` pointers to absolute file offsets.
        let mut offsets = Vec::new();
        for (group, sample, payload) in sounds {
            let rel = push_sound(&mut body, 0, *group, *sample, payload);
            offsets.push(rel);
        }

        // Fix up `next` pointers: each points at the following sub-header's
        // absolute offset; the last points at the file length.
        let total_len = HEADER_SIZE + body.len();
        for (i, rel) in offsets.iter().enumerate() {
            let abs = HEADER_SIZE + *rel as usize;
            let next_abs = if i + 1 < offsets.len() {
                HEADER_SIZE + offsets[i + 1] as usize
            } else {
                total_len
            } as u32;
            body[abs - HEADER_SIZE..abs - HEADER_SIZE + 4].copy_from_slice(&next_abs.to_le_bytes());
        }

        let mut buf = Vec::with_capacity(total_len);
        buf.extend_from_slice(SND_SIGNATURE.as_slice());
        buf.extend_from_slice(&version.to_le_bytes());
        buf.extend_from_slice(&(sounds.len() as u32).to_le_bytes());
        buf.extend_from_slice(&first_offset.to_le_bytes());
        buf.extend_from_slice(&body);
        buf
    }

    #[test]
    fn rejects_bad_signature() {
        let mut data = make_test_snd(4, &[(0, 0, b"RIFFblah")]);
        data[0] = b'X';
        let err = SndFile::from_bytes(&data).unwrap_err();
        assert!(err.to_string().contains("signature"), "{err}");
    }

    #[test]
    fn rejects_too_small() {
        let data = vec![0u8; 8];
        let err = SndFile::from_bytes(&data).unwrap_err();
        assert!(err.to_string().contains("too small"), "{err}");
    }

    #[test]
    fn reads_version_and_walks_multiple_sounds() {
        let data = make_test_snd(
            4,
            &[
                (0, 0, b"RIFF....WAVE-one"),
                (0, 1, b"RIFF....WAVE-two"),
                (5, 2, b"RIFF....WAVE-three"),
            ],
        );
        let snd = SndFile::from_bytes(&data).unwrap();

        assert_eq!(snd.version, 4);
        assert_eq!(snd.len(), 3);
        assert!(!snd.is_empty());

        // File-order preserved with correct (group, sample) keys.
        assert_eq!(snd.sounds[0].group, 0);
        assert_eq!(snd.sounds[0].sample, 0);
        assert_eq!(snd.sounds[2].group, 5);
        assert_eq!(snd.sounds[2].sample, 2);
    }

    #[test]
    fn lookup_hit_and_miss() {
        let data = make_test_snd(
            4,
            &[(0, 0, b"RIFF-a"), (1, 1, b"RIFF-b"), (5, 0, b"RIFF-c")],
        );
        let snd = SndFile::from_bytes(&data).unwrap();

        assert_eq!(snd.sound(1, 1), Some(b"RIFF-b".as_slice()));
        assert_eq!(snd.sound(5, 0), Some(b"RIFF-c".as_slice()));
        assert!(snd.sound(99, 99).is_none());
    }

    #[test]
    fn empty_payload_is_recovered() {
        let data = make_test_snd(4, &[(7, 3, b"")]);
        let snd = SndFile::from_bytes(&data).unwrap();
        assert_eq!(snd.len(), 1);
        assert_eq!(snd.sound(7, 3), Some(b"".as_slice()));
    }

    /// Builds an SND header (24 bytes) with explicit version/count/first-offset.
    /// Used by tests that need to construct malformed directories by hand.
    fn make_header(version: u32, count: u32, first_offset: u32) -> Vec<u8> {
        let mut buf = Vec::with_capacity(HEADER_SIZE);
        buf.extend_from_slice(SND_SIGNATURE.as_slice());
        buf.extend_from_slice(&version.to_le_bytes());
        buf.extend_from_slice(&count.to_le_bytes());
        buf.extend_from_slice(&first_offset.to_le_bytes());
        buf
    }

    #[test]
    fn accepts_exact_header_size_with_zero_sounds() {
        // A 24-byte file declaring zero sounds is well-formed: valid header,
        // empty directory. Must parse cleanly to an empty container.
        let data = make_header(4, 0, HEADER_SIZE as u32);
        assert_eq!(data.len(), HEADER_SIZE);
        let snd = SndFile::from_bytes(&data).expect("zero-sound file is valid");
        assert_eq!(snd.version, 4);
        assert_eq!(snd.len(), 0);
        assert!(snd.is_empty());
        assert!(snd.sound(0, 0).is_none());
    }

    #[test]
    fn one_byte_short_of_header_is_rejected() {
        // Boundary: HEADER_SIZE - 1 bytes must still be rejected as too small.
        let data = vec![0u8; HEADER_SIZE - 1];
        let err = SndFile::from_bytes(&data).unwrap_err();
        assert!(err.to_string().contains("too small"), "{err}");
    }

    #[test]
    fn empty_input_is_rejected() {
        let err = SndFile::from_bytes(&[]).unwrap_err();
        assert!(err.to_string().contains("too small"), "{err}");
    }

    #[test]
    fn version_is_preserved_verbatim_for_non_v4() {
        // The parser must not assume v4; whatever version the header carries is
        // surfaced unchanged so callers can branch on it.
        let data = make_test_snd(7, &[(0, 0, b"RIFF-x")]);
        let snd = SndFile::from_bytes(&data).unwrap();
        assert_eq!(snd.version, 7);
        assert_eq!(snd.len(), 1);
    }

    #[test]
    fn first_offset_can_follow_a_padding_gap() {
        // Real MUGEN files (incl. KFM) leave a large zero gap between the 24-byte
        // header and the first sub-header (KFM's first_offset is 512). The walk
        // must honor first_offset rather than assuming the directory is
        // contiguous with the header.
        let payload: &[u8] = b"RIFF....WAVE-gap";
        let first_offset: u32 = 512;
        let mut data = make_header(4, 1, first_offset);
        // Zero-pad up to the first sub-header offset.
        data.resize(first_offset as usize, 0);
        // Single sub-header whose `next` points at EOF (the normal terminator).
        let next_eof = (first_offset as usize + SUBHEADER_SIZE + payload.len()) as u32;
        data.extend_from_slice(&next_eof.to_le_bytes());
        data.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        data.extend_from_slice(&3u32.to_le_bytes()); // group
        data.extend_from_slice(&9u32.to_le_bytes()); // sample
        data.extend_from_slice(payload);

        let snd = SndFile::from_bytes(&data).expect("padded directory must parse");
        assert_eq!(snd.len(), 1);
        assert_eq!(snd.sound(3, 9), Some(payload));
    }

    #[test]
    fn first_offset_zero_yields_no_sounds() {
        // A header claiming sounds but with first_offset == 0 has nowhere to
        // walk; the parser must recover an empty (not panicking) container.
        let data = make_header(4, 2, 0);
        let snd = SndFile::from_bytes(&data).expect("must not panic on zero first offset");
        assert_eq!(snd.version, 4);
        assert_eq!(snd.len(), 0);
    }

    #[test]
    fn first_offset_past_eof_yields_no_sounds() {
        // first_offset beyond the file must be skipped without panic.
        let data = make_header(4, 1, 9999);
        let snd = SndFile::from_bytes(&data).expect("must not panic on OOB first offset");
        assert_eq!(snd.len(), 0);
    }

    #[test]
    fn first_offset_inside_header_with_truncated_subheader_recovers() {
        // first_offset points so close to EOF that a full 16-byte sub-header
        // cannot be read. Must skip cleanly, returning an empty container.
        let mut data = make_header(4, 1, HEADER_SIZE as u32);
        data.extend_from_slice(&[0u8; 4]); // only 4 of the 16 sub-header bytes
        let snd = SndFile::from_bytes(&data).expect("truncated sub-header must not panic");
        assert_eq!(snd.len(), 0);
    }

    #[test]
    fn header_count_is_authoritative_terminator() {
        // Two sounds physically present, but the header declares only 1. The walk
        // must stop after the declared count even though `next` would continue.
        let mut data = make_test_snd(4, &[(0, 0, b"RIFF-keep"), (0, 1, b"RIFF-drop")]);
        data[16..20].copy_from_slice(&1u32.to_le_bytes());
        let snd = SndFile::from_bytes(&data).expect("over-declared body must parse");
        assert_eq!(snd.len(), 1);
        assert_eq!(snd.sound(0, 0), Some(b"RIFF-keep".as_slice()));
        assert!(snd.sound(0, 1).is_none());
    }

    #[test]
    fn cyclic_next_pointer_does_not_loop_forever() {
        // A sub-header whose `next` points back at itself (non-advancing) must
        // be caught by the guard: the one entry is recovered, then the walk stops
        // instead of spinning. We declare a high count to prove the guard, not
        // the count, is what halts iteration.
        let payload: &[u8] = b"RIFF-self";
        let first_offset = HEADER_SIZE as u32;
        let mut data = make_header(4, 1000, first_offset);
        data.extend_from_slice(&first_offset.to_le_bytes()); // next == self
        data.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(payload);

        let snd = SndFile::from_bytes(&data).expect("self-cycle must not hang");
        assert_eq!(snd.len(), 1);
        assert_eq!(snd.sound(0, 0), Some(payload));
    }

    #[test]
    fn backward_next_pointer_is_rejected() {
        // Lay out two sounds, then rewrite the second sound's `next` to point
        // *backward* at the first. The visited/advance guard must stop the walk
        // after the second entry rather than re-reading the first forever.
        let mut data = make_test_snd(4, &[(0, 0, b"RIFF-one"), (0, 1, b"RIFF-two")]);
        // Bump declared count so only the guard can terminate the loop.
        data[16..20].copy_from_slice(&1000u32.to_le_bytes());
        // The second sub-header begins right after the first sub-header+payload.
        let second_off = HEADER_SIZE + SUBHEADER_SIZE + b"RIFF-one".len();
        // Point its `next` back at the first sub-header (offset HEADER_SIZE).
        data[second_off..second_off + 4].copy_from_slice(&(HEADER_SIZE as u32).to_le_bytes());
        let snd = SndFile::from_bytes(&data).expect("backward pointer must not hang");
        // Both legit entries recovered, then the backward jump halts the walk.
        assert_eq!(snd.len(), 2);
        assert_eq!(snd.sound(0, 0), Some(b"RIFF-one".as_slice()));
        assert_eq!(snd.sound(0, 1), Some(b"RIFF-two".as_slice()));
    }

    #[test]
    fn duplicate_group_sample_returns_first_match() {
        // MUGEN allows (and community files contain) duplicate (group, sample)
        // keys; the documented lookup contract returns the *first* match.
        let data = make_test_snd(4, &[(2, 2, b"RIFF-first"), (2, 2, b"RIFF-second")]);
        let snd = SndFile::from_bytes(&data).unwrap();
        assert_eq!(snd.len(), 2);
        assert_eq!(snd.sound(2, 2), Some(b"RIFF-first".as_slice()));
    }

    #[test]
    fn large_group_and_sample_numbers_round_trip() {
        // Groups/samples are u32; large values (negative-looking when read as i32)
        // must round-trip through parse and lookup unchanged.
        let big_group = 0xFFFF_FFF0u32;
        let big_sample = 0x8000_0001u32;
        let data = make_test_snd(4, &[(big_group, big_sample, b"RIFF-big")]);
        let snd = SndFile::from_bytes(&data).unwrap();
        assert_eq!(snd.sounds[0].group, big_group);
        assert_eq!(snd.sounds[0].sample, big_sample);
        assert_eq!(
            snd.sound(big_group, big_sample),
            Some(b"RIFF-big".as_slice())
        );
        assert!(snd.sound(0, 0).is_none());
    }

    #[test]
    fn next_points_past_eof_stops_cleanly() {
        // A single valid entry whose `next` points beyond EOF. The entry is
        // recovered and the walk stops at the OOB pointer without panic.
        let payload: &[u8] = b"RIFF-eofnext";
        let first_offset = HEADER_SIZE as u32;
        let mut data = make_header(4, 5, first_offset);
        data.extend_from_slice(&999_999u32.to_le_bytes()); // next way past EOF
        data.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend_from_slice(payload);
        let snd = SndFile::from_bytes(&data).expect("OOB next must not panic");
        assert_eq!(snd.len(), 1);
        assert_eq!(snd.sound(1, 1), Some(payload));
    }

    #[test]
    fn load_from_path_round_trips() {
        // Exercise the file-reading path (not just from_bytes) end-to-end via a
        // temp file written to disk.
        let data = make_test_snd(4, &[(0, 0, b"RIFF-disk"), (3, 4, b"RIFF-disk2")]);
        let dir = std::env::temp_dir();
        let path = dir.join(format!("fp_snd_test_{}.snd", std::process::id()));
        std::fs::write(&path, &data).expect("write temp snd");
        let snd = SndFile::load(&path).expect("load from disk");
        let _ = std::fs::remove_file(&path);
        assert_eq!(snd.len(), 2);
        assert_eq!(snd.sound(3, 4), Some(b"RIFF-disk2".as_slice()));
    }

    #[test]
    fn load_missing_path_is_io_error() {
        let path = std::path::Path::new("/nonexistent/fp-formats/does-not-exist.snd");
        let err = SndFile::load(path).unwrap_err();
        // Reading a missing file surfaces as an Io error, not a parse error.
        assert!(
            matches!(err, FpError::Io(_)),
            "expected Io error for missing file, got {err:?}"
        );
    }

    #[test]
    fn signature_must_include_trailing_nul() {
        // 'ElecbyteSnd' with a non-NUL 12th byte must be rejected — the NUL
        // terminator is part of the 12-byte signature.
        let mut data = make_test_snd(4, &[(0, 0, b"RIFF-z")]);
        data[11] = b'X'; // clobber the trailing NUL
        let err = SndFile::from_bytes(&data).unwrap_err();
        assert!(err.to_string().contains("signature"), "{err}");
    }

    #[test]
    fn recovers_partial_on_truncated_entry() {
        // Two valid sounds, then a header claiming a count of 3 but with the
        // third entry's payload running past EOF. We must recover the first two
        // and skip the truncated tail without panicking or erroring out.
        let mut data = make_test_snd(4, &[(0, 0, b"RIFF-one"), (0, 1, b"RIFF-two")]);

        // Bump the declared count to 3 and append a third sub-header whose
        // payload length lies about its size (claims 9999 bytes, has none).
        data[16..20].copy_from_slice(&3u32.to_le_bytes());

        // Point the (previously last) second sound's `next` at the new entry.
        // Locate the second sub-header: it's the one whose group/sample is (0,1).
        // Simplest: append the bogus entry and rewire the final `next`.
        let bogus_offset = data.len() as u32;
        // Find the last `next` field that currently equals the old file length
        // and repoint it. The second sound's sub-header `next` was set to the
        // old total length during construction.
        let old_len = data.len() as u32;
        for i in (HEADER_SIZE..data.len() - 3).step_by(1) {
            if data[i..i + 4] == old_len.to_le_bytes() {
                data[i..i + 4].copy_from_slice(&bogus_offset.to_le_bytes());
                break;
            }
        }
        // Append a sub-header that claims a huge payload but provides none.
        data.extend_from_slice(&0u32.to_le_bytes()); // next
        data.extend_from_slice(&9999u32.to_le_bytes()); // length (lies)
        data.extend_from_slice(&2u32.to_le_bytes()); // group
        data.extend_from_slice(&2u32.to_le_bytes()); // sample

        let snd = SndFile::from_bytes(&data).expect("partial parse must succeed");
        // Recovered the two intact sounds; the truncated third is skipped.
        assert_eq!(snd.len(), 2);
        assert_eq!(snd.sound(0, 0), Some(b"RIFF-one".as_slice()));
        assert_eq!(snd.sound(0, 1), Some(b"RIFF-two".as_slice()));
        assert!(snd.sound(2, 2).is_none());
    }

    // --- Payload codec sniffing (compat-matrix gap: WAV supported, ADX not) ---

    #[test]
    fn sniffs_wav_payload() {
        // A real RIFF/WAVE blob is the supported, decodable case.
        assert_eq!(
            sniff_sound_format(b"RIFF\x24\x00\x00\x00WAVE"),
            SoundFormat::Wav
        );
        // A bare RIFF magic (no WAVE form type yet) still reads as WAV.
        assert_eq!(sniff_sound_format(b"RIFF...."), SoundFormat::Wav);
    }

    #[test]
    fn sniffs_adx_payload_as_unsupported() {
        // CRI ADX begins with the 0x80 sync byte. We must recognise it as ADX so
        // a consumer can warn-and-skip rather than feed it to the WAV decoder.
        let adx: &[u8] = &[0x80, 0x00, 0x00, 0x20, 0x03, 0x12, 0x04, 0x00];
        assert_eq!(sniff_sound_format(adx), SoundFormat::Adx);
    }

    #[test]
    fn sniffs_unknown_and_short_payloads() {
        // Neither RIFF nor ADX.
        assert_eq!(sniff_sound_format(b"OggS...."), SoundFormat::Unknown);
        // Too short to carry a 4-byte RIFF magic, and not the ADX sync byte.
        assert_eq!(sniff_sound_format(b"RIF"), SoundFormat::Unknown);
        assert_eq!(sniff_sound_format(&[]), SoundFormat::Unknown);
    }

    #[test]
    fn entry_format_reflects_payload() {
        // A container mixing a WAV and an ADX payload: the WAV entry is decodable,
        // the ADX entry is flagged so a consumer can skip it.
        let adx: &[u8] = &[0x80, 0x00, 0x00, 0x20, 0xAA, 0xBB];
        let data = make_test_snd(4, &[(0, 0, b"RIFF....WAVEpcm"), (0, 1, adx)]);
        let snd = SndFile::from_bytes(&data).unwrap();

        assert_eq!(snd.len(), 2);
        let wav = snd.sounds.iter().find(|s| s.sample == 0).unwrap();
        let bad = snd.sounds.iter().find(|s| s.sample == 1).unwrap();
        assert_eq!(wav.format(), SoundFormat::Wav);
        assert_eq!(bad.format(), SoundFormat::Adx);
    }

    /// Builds a synthetic CRI-ADX header: the `0x80` sync byte, a big-endian
    /// copyright-offset field, the `(c)CRI` copyright marker at the offset that
    /// field points at, then a little trailing body. This mirrors the shape of a
    /// real ADX stream (clean-room: authored from the public format description,
    /// not copied from any tool/asset) so the sniffer's marker path is exercised.
    fn make_adx_header() -> Vec<u8> {
        // copyright_offset counts bytes from offset 2 to the marker; we place the
        // marker at byte 4, so the field value is 4 - 2 = 2.
        let mut v = vec![0x80, 0x00, 0x00, 0x02];
        v.extend_from_slice(ADX_COPYRIGHT_MARKER); // marker at offset 4
        v.extend_from_slice(&[0x00, 0x03, 0x12, 0x04]); // arbitrary trailing header bytes
        v
    }

    #[test]
    fn sniffs_adx_via_copyright_marker_even_without_sync_byte() {
        // The `(c)CRI` marker is the unambiguous ADX signal: a payload carrying it
        // is ADX even if its first byte is not the 0x80 sync byte (e.g. a wrapper).
        let mut data = make_adx_header();
        data[0] = 0x00; // clobber the sync byte; the marker must still flag ADX
        assert_eq!(sniff_sound_format(&data), SoundFormat::Adx);
    }

    #[test]
    fn adx_marker_beyond_scan_window_is_not_misclassified() {
        // A `(c)CRI` marker buried far past the bounded scan window must not flag
        // ADX off the marker alone — the sniff stays cheap and bounded.
        let mut data = vec![b'O', b'g', b'g', b'S']; // not RIFF, not 0x80
        data.extend(std::iter::repeat_n(0u8, ADX_MARKER_SCAN_LEN));
        data.extend_from_slice(ADX_COPYRIGHT_MARKER);
        assert_eq!(sniff_sound_format(&data), SoundFormat::Unknown);
    }

    #[test]
    fn sound_format_decodable_predicate() {
        // Only WAV is decodable today; ADX and Unknown are warn-and-skip.
        assert!(SoundFormat::Wav.is_decodable());
        assert!(!SoundFormat::Adx.is_decodable());
        assert!(!SoundFormat::Unknown.is_decodable());
    }

    #[test]
    fn adx_entry_round_trips_in_container_for_warn_skip() {
        // AC#1/#3: a realistic ADX-encoded entry living inside a full SND container
        // is recovered intact (never panics) and flagged ADX, so a consumer can
        // warn-and-skip it. The container itself parses fine alongside a WAV entry.
        let adx = make_adx_header();
        let data = make_test_snd(
            4,
            &[
                (0, 0, b"RIFF....WAVE-pcm"),
                (1, 0, adx.as_slice()),
                (2, 0, b"RIFF....WAVE-tail"),
            ],
        );
        let snd = SndFile::from_bytes(&data).expect("mixed-codec container must parse");

        assert_eq!(snd.len(), 3);

        // The ADX payload is recovered byte-for-byte (the container is codec-blind).
        let recovered = snd.sound(1, 0).expect("adx entry present");
        assert_eq!(recovered, adx.as_slice());

        // Codec classification lets a consumer route each entry correctly.
        let by_key = |g, s| {
            snd.sounds
                .iter()
                .find(|e| e.group == g && e.sample == s)
                .unwrap()
        };
        assert!(by_key(0, 0).format().is_decodable(), "WAV decodes");
        assert_eq!(by_key(1, 0).format(), SoundFormat::Adx);
        assert!(
            !by_key(1, 0).format().is_decodable(),
            "ADX is recognised-but-unsupported, so a consumer skips it"
        );
        assert!(
            by_key(2, 0).format().is_decodable(),
            "trailing WAV still decodes"
        );
    }
}
