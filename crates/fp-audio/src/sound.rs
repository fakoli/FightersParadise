//! Decoded, replayable in-memory sound data.

use std::io::Cursor;

use fp_core::{FpError, FpResult};
use rodio::{Decoder, Source};

/// Upper bound on the number of interleaved samples [`Sound::decode`] will
/// materialize, ~128 MB of `i16`.
///
/// A crafted WAV can declare a `data` chunk far larger than its real payload; the
/// decoder then yields that many (zero) samples. This cap bounds the resulting
/// allocation. It is generously above any real sound effect (≈12 minutes of
/// 44.1 kHz mono), so legitimate content is never truncated.
const MAX_DECODED_SAMPLES: usize = 64 * 1024 * 1024;

/// Validates that `bytes` is a RIFF/WAVE stream whose `fmt ` chunk declares a
/// sample format the decoder handles **without panicking**.
///
/// `rodio` 0.20's hound-backed WAV decoder `panic!`s on its sample iterator for
/// any `(audio_format, bits_per_sample)` outside PCM 8/16/24/32-bit or IEEE
/// float 32-bit. That panic fires while draining the decoder (inside
/// [`Sound::decode`]), *past* the `Decoder::new` error seam, so unsupported specs
/// must be rejected up front to keep the "never crash on bad content" guarantee.
///
/// All indexing is bounds-checked; malformed/truncated headers return an error
/// rather than panic. Chunks advance by at least 8 bytes per step, so the scan
/// always terminates.
fn validate_wav_spec(bytes: &[u8]) -> FpResult<()> {
    // RIFF container header: "RIFF" <u32 size> "WAVE", then sub-chunks.
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(FpError::parse("WAV", "not a RIFF/WAVE stream"));
    }
    let mut pos = 12usize;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size =
            u32::from_le_bytes([bytes[pos + 4], bytes[pos + 5], bytes[pos + 6], bytes[pos + 7]])
                as usize;
        let body = pos + 8;
        if id == b"fmt " {
            // fmt body: audio_format(u16), channels(u16), sample_rate(u32),
            // byte_rate(u32), block_align(u16), bits_per_sample(u16).
            if body + 16 > bytes.len() {
                return Err(FpError::parse("WAV", "truncated fmt chunk"));
            }
            let audio_format = u16::from_le_bytes([bytes[body], bytes[body + 1]]);
            let bits = u16::from_le_bytes([bytes[body + 14], bytes[body + 15]]);
            let supported = match audio_format {
                1 => matches!(bits, 8 | 16 | 24 | 32), // PCM
                3 => bits == 32,                        // IEEE float
                _ => false,
            };
            if !supported {
                return Err(FpError::parse(
                    "WAV",
                    format!("unsupported WAV format {audio_format} at {bits}-bit"),
                ));
            }
            return Ok(());
        }
        // Advance to the next chunk; chunk bodies are word-aligned (pad to even).
        // `body = pos + 8`, so this always moves forward by >= 8 bytes.
        pos = body + size + (size & 1);
    }
    Err(FpError::parse("WAV", "no fmt chunk found"))
}

/// A sound decoded into in-memory PCM, ready to be played any number of times.
///
/// MUGEN characters reference the same sound repeatedly (e.g. a punch whiff that
/// fires on every attack), so the decoded interleaved PCM is kept resident in
/// memory rather than re-decoding from the raw bytes on each playback. Cloning a
/// [`Sound`] clones the underlying sample buffer.
///
/// Construct one with [`Sound::decode`] from raw WAV/RIFF bytes (the payload the
/// `fp-formats` SND parser hands back).
///
/// # Examples
///
/// ```
/// # use fp_audio::Sound;
/// # fn demo(wav_bytes: &[u8]) -> fp_core::FpResult<()> {
/// let sound = Sound::decode(wav_bytes)?;
/// println!(
///     "{} ch @ {} Hz, {} samples",
///     sound.channels(),
///     sound.sample_rate(),
///     sound.sample_count()
/// );
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Sound {
    channels: u16,
    sample_rate: u32,
    samples: Vec<i16>,
}

impl Sound {
    /// Decodes raw audio bytes (WAV/RIFF) into replayable in-memory PCM.
    ///
    /// The bytes are decoded with the `rodio` [`Decoder`], which handles WAV
    /// among other formats. The interleaved `i16` samples are collected into an
    /// in-memory buffer along with the channel count and sample rate so the
    /// sound can be replayed without re-decoding.
    ///
    /// # Errors
    ///
    /// Returns an [`FpError`] (never panics) when the bytes are empty, are not a
    /// RIFF/WAVE stream `rodio` can decode, declare an unsupported sample format
    /// (see below), claim more samples than the decode budget allows, or are
    /// otherwise malformed. Garbage and hostile input yield an error rather than a
    /// crash, in keeping with the engine's "never crash on bad content" philosophy.
    ///
    /// Two robustness guards run *before* the bytes are drained, because `rodio`'s
    /// WAV path can crash or exhaust memory on adversarial-but-structurally-valid
    /// input that its constructor accepts:
    /// - **Format pre-validation:** the `fmt ` chunk is checked and any
    ///   `(audio_format, bits_per_sample)` outside PCM 8/16/24/32-bit or IEEE
    ///   float 32-bit is rejected — `rodio` 0.20 `panic!`s on its sample iterator
    ///   for those, *past* the `Decoder::new` error seam.
    /// - **Sample budget:** a WAV declaring a huge `data` length yields that many
    ///   samples; the decode is capped at [`MAX_DECODED_SAMPLES`] to bound memory.
    ///
    /// A successfully-decoded stream that happens to contain zero samples is
    /// accepted and produces an empty [`Sound`]; callers can detect this with
    /// [`Sound::is_empty`].
    pub fn decode(bytes: &[u8]) -> FpResult<Sound> {
        if bytes.is_empty() {
            return Err(FpError::parse("WAV", "cannot decode empty sound data"));
        }

        // Reject sample formats rodio's WAV iterator would panic on, up front
        // (the panic fires while draining, not at construction, so map_err on
        // Decoder::new does not catch it).
        validate_wav_spec(bytes)?;

        // `Decoder` needs an owned, seekable reader. The raw bytes are small
        // (single SFX), so copying into a Cursor is cheap.
        let cursor = Cursor::new(bytes.to_vec());
        let decoder = Decoder::new(cursor).map_err(|e| {
            FpError::parse("WAV", format!("failed to decode sound data: {e}"))
        })?;

        // `channels()` / `sample_rate()` describe the current frame and must be
        // read before the iterator is drained.
        let channels = decoder.channels();
        let sample_rate = decoder.sample_rate();

        // Fast path: a WAV with a bogus oversized `data` length reports its
        // (inflated) remaining count here, so we can reject without allocating.
        if decoder.size_hint().1.is_some_and(|n| n > MAX_DECODED_SAMPLES) {
            return Err(FpError::parse(
                "WAV",
                "declared sample count exceeds the decode budget",
            ));
        }

        // Drain with a hard cap as the backstop (size_hint may be unreliable for
        // some streams), so a hostile data-length claim cannot exhaust memory.
        let mut samples: Vec<i16> = Vec::new();
        for s in decoder {
            if samples.len() >= MAX_DECODED_SAMPLES {
                return Err(FpError::parse(
                    "WAV",
                    "sound exceeds the maximum decoded sample budget",
                ));
            }
            samples.push(s);
        }

        // A valid stream with no audio is unusual but not fatal — warn and keep
        // an empty sound so playback is a harmless no-op.
        if samples.is_empty() {
            tracing::warn!("decoded sound contains zero samples");
        }

        Ok(Sound {
            channels,
            sample_rate,
            samples,
        })
    }

    /// Returns the number of interleaved channels (1 = mono, 2 = stereo).
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Returns the sample rate in hertz (e.g. `44_100`).
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Returns the interleaved PCM samples.
    ///
    /// The slice holds all channels interleaved (for stereo: `L, R, L, R, ...`),
    /// so its length is [`Sound::sample_count`].
    pub fn samples(&self) -> &[i16] {
        &self.samples
    }

    /// Returns the total number of interleaved samples across all channels.
    ///
    /// For a 2-channel sound this is twice the number of per-channel frames.
    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }

    /// Returns `true` if the sound contains no samples.
    ///
    /// An empty sound decodes successfully but plays nothing; this lets callers
    /// skip scheduling it on a channel.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Builds a `rodio` source that replays this sound's PCM.
    ///
    /// Used internally by [`crate::RodioBackend`]; the returned source borrows a
    /// fresh copy of the samples so the [`Sound`] can be replayed independently.
    pub(crate) fn to_source(&self) -> rodio::buffer::SamplesBuffer<i16> {
        rodio::buffer::SamplesBuffer::new(
            self.channels.max(1),
            self.sample_rate.max(1),
            self.samples.clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Builds a minimal, valid 16-bit PCM WAV in memory for decode tests.
    ///
    /// Kept self-contained so `sound.rs` tests do not depend on `system.rs`.
    fn make_wav(channels: u16, sample_rate: u32, samples: &[i16]) -> Vec<u8> {
        let bits_per_sample: u16 = 16;
        let block_align: u16 = channels * (bits_per_sample / 8);
        let byte_rate: u32 = sample_rate * u32::from(block_align);
        let data_len: u32 = (samples.len() as u32) * u32::from(bits_per_sample / 8);
        let riff_len: u32 = 36 + data_len;

        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&riff_len.to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&sample_rate.to_le_bytes());
        v.extend_from_slice(&byte_rate.to_le_bytes());
        v.extend_from_slice(&block_align.to_le_bytes());
        v.extend_from_slice(&bits_per_sample.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            v.extend_from_slice(&s.to_le_bytes());
        }
        v
    }

    #[test]
    fn decode_preserves_exact_sample_values_and_order() {
        // AC#1: decoded PCM is faithfully replayable in memory. Use distinctive,
        // non-monotonic values including the i16 extremes to catch byte-order or
        // truncation bugs.
        let samples = [i16::MIN, -1, 0, 1, i16::MAX, 12_345, -12_345];
        let bytes = make_wav(1, 11_025, &samples);
        let sound = Sound::decode(&bytes).expect("WAV should decode");
        assert_eq!(sound.samples(), samples, "sample bytes must round-trip");
        assert_eq!(sound.sample_count(), samples.len());
        assert_eq!(sound.channels(), 1);
        assert_eq!(sound.sample_rate(), 11_025);
    }

    #[test]
    fn decode_is_replayable_via_repeated_to_source() {
        // AC#1: held in memory so it can be replayed. Calling to_source twice
        // must each yield an independent source carrying the same payload, and
        // must not consume/empty the Sound.
        let samples = [5i16, 6, 7, 8];
        let bytes = make_wav(2, 48_000, &samples);
        let sound = Sound::decode(&bytes).expect("decode");

        let s1 = sound.to_source();
        let s2 = sound.to_source();
        assert_eq!(s1.channels(), 2);
        assert_eq!(s2.channels(), 2);
        assert_eq!(s1.sample_rate(), 48_000);
        // Sound itself is untouched after producing sources.
        assert_eq!(sound.sample_count(), 4);
        assert_eq!(sound.samples(), samples);
    }

    #[test]
    fn decode_clone_equals_original() {
        let bytes = make_wav(1, 8_000, &[1, 2, 3, 4]);
        let sound = Sound::decode(&bytes).expect("decode");
        let cloned = sound.clone();
        assert_eq!(sound, cloned, "Clone must produce an equal Sound");
        assert_eq!(cloned.sample_count(), 4);
    }

    #[test]
    fn decode_empty_slice_errors_as_parse() {
        let err = Sound::decode(&[]).expect_err("empty bytes must error");
        match err {
            FpError::Parse { format, .. } => assert_eq!(format, "WAV"),
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn decode_garbage_errors_without_panic() {
        // Random non-audio bytes must not decode and must not panic.
        let garbage: Vec<u8> = (0u8..64).collect();
        assert!(Sound::decode(&garbage).is_err());
    }

    #[test]
    fn decode_riff_header_only_no_format_errors() {
        // "RIFF....WAVE" with nothing after it: looks like a WAV start but has no
        // fmt/data chunks. Must error rather than panic or produce junk.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        assert!(Sound::decode(&bytes).is_err());
    }

    #[test]
    fn decode_truncated_data_chunk_does_not_panic() {
        // Build a valid WAV then chop the last few sample bytes off. Whatever the
        // decoder does (error or partial), it must never panic.
        let full = make_wav(1, 8_000, &[1, 2, 3, 4, 5, 6]);
        let truncated = &full[..full.len() - 5];
        let _ = Sound::decode(truncated); // result intentionally unchecked; just no panic
    }

    #[test]
    fn decode_lying_data_length_does_not_panic() {
        // Declare a huge data chunk length but provide almost no sample bytes.
        // Exercises bounds handling in the decoder; must not panic.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&36u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&8_000u32.to_le_bytes());
        bytes.extend_from_slice(&16_000u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes()); // absurd length
        bytes.extend_from_slice(&123i16.to_le_bytes()); // only one sample present
        let _ = Sound::decode(&bytes); // must not panic
    }

    /// Builds a WAV header with an arbitrary `fmt` audio_format + bit depth and a
    /// given declared `data` length (no real sample bytes), for the robustness
    /// guards. Mirrors `make_wav` but lets the spec be hostile.
    fn make_wav_header(audio_format: u16, bits: u16, declared_data_len: u32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&36u32.to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&audio_format.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // mono
        v.extend_from_slice(&44_100u32.to_le_bytes());
        v.extend_from_slice(&176_400u32.to_le_bytes());
        v.extend_from_slice(&((bits / 8).max(1)).to_le_bytes());
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&declared_data_len.to_le_bytes());
        v
    }

    #[test]
    fn decode_unsupported_format_errors_without_panic() {
        // SHOULD_FIX #2: a structurally-valid WAV declaring IEEE float 64-bit — a
        // spec rodio's WAV iterator panics on while draining. The fmt pre-validation
        // must reject it so decode returns Err instead of crashing (AC#5: never panics).
        let bytes = make_wav_header(3, 64, 0);
        let err = Sound::decode(&bytes).expect_err("64-bit float WAV must error, not panic");
        match err {
            FpError::Parse { format, .. } => assert_eq!(format, "WAV"),
            other => panic!("expected Parse error, got {other:?}"),
        }
        // And a non-multiple-of-8 PCM depth is likewise rejected up front.
        assert!(Sound::decode(&make_wav_header(1, 12, 0)).is_err());
        // Supported specs are NOT over-rejected by the guard (validate directly so
        // the assertion does not depend on rodio's header-only decode behavior).
        for bits in [8u16, 16, 24, 32] {
            assert!(
                validate_wav_spec(&make_wav_header(1, bits, 0)).is_ok(),
                "PCM {bits}-bit should pass validation"
            );
        }
        assert!(
            validate_wav_spec(&make_wav_header(3, 32, 0)).is_ok(),
            "float 32-bit should pass validation"
        );
    }

    #[test]
    fn decode_oversized_declared_length_errors_without_oom() {
        // SHOULD_FIX #1: a WAV declaring a huge (even, channel-aligned) data length
        // but no real payload. rodio/hound accepts the header and would yield ~1e9
        // samples; the decode budget must reject it (fast, no multi-GB allocation).
        let mut bytes = make_wav_header(1, 16, 0x7FFF_FFFE);
        bytes.extend_from_slice(&7i16.to_le_bytes()); // one real sample only
        assert!(
            Sound::decode(&bytes).is_err(),
            "oversized declared data length must error, not OOM"
        );
    }

    #[test]
    fn empty_wav_decodes_to_empty_sound() {
        let bytes = make_wav(1, 8_000, &[]);
        let sound = Sound::decode(&bytes).expect("header-only WAV decodes");
        assert!(sound.is_empty());
        assert_eq!(sound.sample_count(), 0);
        assert!(sound.samples().is_empty());
        // Format metadata is still reported for an empty stream.
        assert_eq!(sound.channels(), 1);
        assert_eq!(sound.sample_rate(), 8_000);
    }

    /// Real-fixture test: decode an on-disk WAV when one is provided under
    /// `crates/fp-audio/test-assets/`. Skips cleanly when the directory or file
    /// is absent so the suite passes in a checkout without binary assets.
    #[test]
    fn decode_real_wav_fixture_when_present() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test-assets");
        if !dir.is_dir() {
            eprintln!("skipping: {} not present", dir.display());
            return;
        }
        // Pick the first *.wav under the fixtures dir, if any.
        let wav = std::fs::read_dir(&dir)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                p.extension()
                    .and_then(|x| x.to_str())
                    .is_some_and(|x| x.eq_ignore_ascii_case("wav"))
            });
        let Some(path) = wav else {
            eprintln!("skipping: no .wav fixture in {}", dir.display());
            return;
        };
        let bytes = std::fs::read(&path).expect("read fixture");
        let sound = Sound::decode(&bytes)
            .unwrap_or_else(|e| panic!("real fixture {} failed to decode: {e}", path.display()));
        // A real sound file should have a sane sample rate and at least one channel.
        assert!(sound.channels() >= 1, "fixture must have >=1 channel");
        assert!(sound.sample_rate() > 0, "fixture must have a sample rate");
    }
}
