//! Audio backend seam and the channel-managing [`AudioSystem`].
//!
//! The [`AudioBackend`] trait isolates channel/cut-off logic from the actual
//! audio device. [`RodioBackend`] drives a real device but degrades gracefully
//! when none exists; [`NullBackend`] is a silent no-op. [`AudioSystem`] layers
//! MUGEN channel cut-off bookkeeping on top of whichever backend it owns.

use std::collections::HashMap;

use fp_core::{FpError, FpResult};
use rodio::{OutputStream, OutputStreamHandle, Sink};

use crate::sound::Sound;

/// The seam between channel logic and the audio device.
///
/// This trait is **object-safe** so [`AudioSystem`] can own a
/// `Box<dyn AudioBackend>` and swap a real device for silence (or a test
/// recorder) without any other code changing. The backend is responsible only
/// for the mechanics of "start this sound on this channel" and "stop this
/// channel"; the MUGEN cut-off *policy* lives in [`AudioSystem`].
///
/// Implementations must never panic.
pub trait AudioBackend {
    /// Starts `sound` on `channel` at `volume` (a linear multiplier, where
    /// `1.0` is unattenuated). A backend that manages real sinks should ensure
    /// any sound already playing on a non-negative `channel` is replaced.
    ///
    /// `channel` values are passed through unchanged from the caller; negative
    /// channels are MUGEN's "always new" channels (see [`AudioSystem`]).
    fn play(&mut self, sound: &Sound, channel: i32, volume: f32);

    /// Stops whatever is currently playing on `channel`, if anything.
    fn stop_channel(&mut self, channel: i32);

    /// Stops all currently-playing sounds on every channel.
    ///
    /// The default implementation is a no-op; backends that track channels
    /// should override it.
    fn stop_all(&mut self) {}
}

/// A silent backend that does nothing.
///
/// Used as the fallback when no audio device is available, and any time audio
/// should be disabled. Every method is a safe no-op.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullBackend;

impl AudioBackend for NullBackend {
    fn play(&mut self, _sound: &Sound, _channel: i32, _volume: f32) {}
    fn stop_channel(&mut self, _channel: i32) {}
    fn stop_all(&mut self) {}
}

/// A real audio backend backed by a `rodio` output stream.
///
/// Each non-negative channel maps to a single [`Sink`]; appending a new sound to
/// an occupied channel clears that sink first (per-channel cut-off). Negative
/// "always new" channels each get their own freshly-created, detached sink so
/// overlapping sounds stack and are never cut off.
///
/// Construct with [`RodioBackend::try_new`], which **degrades gracefully**: when
/// no output device exists it logs a warning and returns an error the caller can
/// turn into a [`NullBackend`] fallback. Building and running tests never
/// requires an audio device.
pub struct RodioBackend {
    // Held to keep the output stream (and thus playback) alive; never read after
    // construction, hence the leading underscore.
    _stream: OutputStream,
    handle: OutputStreamHandle,
    /// Sinks for non-negative channels, keyed by channel number.
    channels: HashMap<i32, Sink>,
}

impl std::fmt::Debug for RodioBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RodioBackend")
            .field("active_channels", &self.channels.len())
            .finish()
    }
}

impl RodioBackend {
    /// Attempts to open the default audio output device.
    ///
    /// # Errors
    ///
    /// Returns an [`FpError`] when no output device is available (for example in
    /// CI or on a headless machine). The error is logged at `warn` level so the
    /// caller can fall back to a [`NullBackend`] and continue silently. This
    /// never panics.
    pub fn try_new() -> FpResult<Self> {
        let (stream, handle) = OutputStream::try_default().map_err(|e| {
            tracing::warn!("no audio output device available: {e}; audio will be silent");
            FpError::Other(format!("no audio output device: {e}"))
        })?;
        Ok(Self {
            _stream: stream,
            handle,
            channels: HashMap::new(),
        })
    }

    /// Plays on a negative ("always new") channel: a fresh detached sink so the
    /// sound is never cut off and never cuts anything off.
    fn play_always_new(&mut self, sound: &Sound, volume: f32) {
        match Sink::try_new(&self.handle) {
            Ok(sink) => {
                sink.set_volume(volume.max(0.0));
                sink.append(sound.to_source());
                // Detach so the sink keeps playing to completion on its own.
                sink.detach();
            }
            Err(e) => tracing::warn!("failed to create sink for always-new channel: {e}"),
        }
    }
}

impl AudioBackend for RodioBackend {
    fn play(&mut self, sound: &Sound, channel: i32, volume: f32) {
        if channel < 0 {
            self.play_always_new(sound, volume);
            return;
        }

        // Reuse an existing sink for this channel, clearing whatever was queued
        // so the previous sound is cut off; otherwise create one.
        let sink = match self.channels.entry(channel) {
            std::collections::hash_map::Entry::Occupied(slot) => {
                let sink = slot.into_mut();
                sink.clear();
                sink
            }
            std::collections::hash_map::Entry::Vacant(slot) => match Sink::try_new(&self.handle) {
                Ok(sink) => slot.insert(sink),
                Err(e) => {
                    tracing::warn!("failed to create sink for channel {channel}: {e}");
                    return;
                }
            },
        };

        sink.set_volume(volume.max(0.0));
        sink.append(sound.to_source());
        // `clear()` pauses the sink; ensure playback is running.
        sink.play();
    }

    fn stop_channel(&mut self, channel: i32) {
        if let Some(sink) = self.channels.remove(&channel) {
            sink.stop();
        }
    }

    fn stop_all(&mut self) {
        for (_, sink) in self.channels.drain() {
            sink.stop();
        }
    }
}

/// Owns an [`AudioBackend`] and applies MUGEN channel cut-off policy.
///
/// The system tracks which non-negative channels are "occupied" and enforces the
/// MUGEN rule that starting a sound on an occupied channel first stops the sound
/// already there. Channel `-1` (any negative channel) is "always new": it never
/// cuts anything off and is never tracked, so overlapping sounds stack.
///
/// [`AudioSystem::default`] never panics: it tries [`RodioBackend`] and falls
/// back to [`NullBackend`] when no device is present, so callers can always
/// construct a working (possibly silent) system.
pub struct AudioSystem {
    backend: Box<dyn AudioBackend>,
    /// Non-negative channels currently believed to be playing.
    occupied: std::collections::HashSet<i32>,
}

impl std::fmt::Debug for AudioSystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioSystem")
            .field("occupied_channels", &self.occupied.len())
            .finish()
    }
}

impl Default for AudioSystem {
    /// Builds an [`AudioSystem`] over a real device when one exists, falling
    /// back to a silent [`NullBackend`] otherwise. Never panics.
    fn default() -> Self {
        match RodioBackend::try_new() {
            Ok(backend) => {
                tracing::info!("audio system initialized with rodio backend");
                Self::with_backend(Box::new(backend))
            }
            Err(_) => {
                tracing::info!("audio system falling back to silent null backend");
                Self::with_backend(Box::new(NullBackend))
            }
        }
    }
}

impl AudioSystem {
    /// Creates an audio system over an explicit backend.
    ///
    /// Useful for forcing silence with [`NullBackend`] or, in tests, for
    /// injecting a recording backend to assert channel behavior headlessly.
    pub fn with_backend(backend: Box<dyn AudioBackend>) -> Self {
        Self {
            backend,
            occupied: std::collections::HashSet::new(),
        }
    }

    /// Plays `sound` on `channel` at `volume`, applying MUGEN cut-off policy.
    ///
    /// For a non-negative `channel`, if that channel is already occupied the
    /// previous sound is stopped first (cut-off), then the new sound starts and
    /// the channel is marked occupied. For a negative `channel` ("always new")
    /// nothing is cut off and no occupancy is recorded, so overlapping sounds
    /// stack. Empty sounds are dropped (nothing is played, no channel touched).
    pub fn play_sound(&mut self, sound: &Sound, channel: i32, volume: f32) {
        if sound.is_empty() {
            tracing::warn!("ignoring play request for empty sound on channel {channel}");
            return;
        }

        if channel < 0 {
            // Always-new: never cuts off, never tracked.
            self.backend.play(sound, channel, volume);
            return;
        }

        if self.occupied.contains(&channel) {
            // Cut off the previous sound on this channel before starting anew.
            self.backend.stop_channel(channel);
        }
        self.backend.play(sound, channel, volume);
        self.occupied.insert(channel);
    }

    /// Stops any sound currently playing on `channel` and marks it free.
    ///
    /// A negative channel is forwarded to the backend but is not tracked here
    /// (always-new sounds are not individually addressable).
    pub fn stop(&mut self, channel: i32) {
        self.backend.stop_channel(channel);
        self.occupied.remove(&channel);
    }

    /// Stops all sounds on all channels and clears occupancy.
    pub fn stop_all(&mut self) {
        self.backend.stop_all();
        self.occupied.clear();
    }

    /// Returns `true` if a non-negative `channel` is currently marked occupied.
    ///
    /// Reflects the system's bookkeeping, not real-time device state (a sound
    /// may have finished naturally without the system being told).
    pub fn is_channel_occupied(&self, channel: i32) -> bool {
        self.occupied.contains(&channel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A test-only backend that records every `play` and `stop` so channel
    /// policy can be asserted with no audio device.
    #[derive(Debug, Default)]
    struct RecordingBackend {
        /// Each play recorded as `(channel, volume, sample_count)`.
        plays: Vec<(i32, f32, usize)>,
        /// Each `stop_channel` call's channel.
        stops: Vec<i32>,
        /// Number of `stop_all` calls.
        stop_alls: usize,
    }

    impl AudioBackend for RecordingBackend {
        fn play(&mut self, sound: &Sound, channel: i32, volume: f32) {
            self.plays.push((channel, volume, sound.sample_count()));
        }

        fn stop_channel(&mut self, channel: i32) {
            self.stops.push(channel);
        }

        fn stop_all(&mut self) {
            self.stop_alls += 1;
        }
    }

    /// Builds a minimal, valid PCM WAV (16-bit) in memory for decode tests.
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
        // fmt chunk
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
        v.extend_from_slice(&1u16.to_le_bytes()); // PCM format
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&sample_rate.to_le_bytes());
        v.extend_from_slice(&byte_rate.to_le_bytes());
        v.extend_from_slice(&block_align.to_le_bytes());
        v.extend_from_slice(&bits_per_sample.to_le_bytes());
        // data chunk
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            v.extend_from_slice(&s.to_le_bytes());
        }
        v
    }

    #[test]
    fn decode_mono_wav_reports_format_and_count() {
        let samples = [0i16, 100, -100, 32_000, -32_000, 1, 2, 3];
        let bytes = make_wav(1, 22_050, &samples);
        let sound = Sound::decode(&bytes).expect("mono WAV should decode");
        assert_eq!(sound.channels(), 1);
        assert_eq!(sound.sample_rate(), 22_050);
        assert_eq!(sound.sample_count(), samples.len());
        assert!(!sound.is_empty());
        assert_eq!(sound.samples(), samples);
    }

    #[test]
    fn decode_stereo_wav_reports_two_channels() {
        // 4 frames * 2 channels = 8 interleaved samples.
        let samples = [10i16, -10, 20, -20, 30, -30, 40, -40];
        let bytes = make_wav(2, 44_100, &samples);
        let sound = Sound::decode(&bytes).expect("stereo WAV should decode");
        assert_eq!(sound.channels(), 2);
        assert_eq!(sound.sample_rate(), 44_100);
        assert_eq!(sound.sample_count(), 8);
    }

    #[test]
    fn decode_empty_bytes_errors_without_panic() {
        let err = Sound::decode(&[]).expect_err("empty bytes must error");
        assert!(err.to_string().contains("WAV"));
    }

    #[test]
    fn decode_garbage_bytes_errors_without_panic() {
        let garbage = [0xDEu8, 0xAD, 0xBE, 0xEF, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05];
        let result = Sound::decode(&garbage);
        assert!(result.is_err(), "garbage must not decode to a Sound");
    }

    /// Convenience: a non-empty Sound with `frames` mono samples.
    fn make_sound(frames: usize) -> Sound {
        let samples: Vec<i16> = (0..frames as i16).collect();
        let bytes = make_wav(1, 8_000, &samples);
        Sound::decode(&bytes).expect("synth sound should decode")
    }

    /// Wraps a [`RecordingBackend`] in shared mutability so a test can inject it
    /// into an [`AudioSystem`] and still read back the recorded events without
    /// downcasting through the `Box<dyn AudioBackend>`.
    #[derive(Debug, Default, Clone)]
    struct SharedRecorder(Rc<RefCell<RecordingBackend>>);

    impl AudioBackend for SharedRecorder {
        fn play(&mut self, sound: &Sound, channel: i32, volume: f32) {
            self.0.borrow_mut().play(sound, channel, volume);
        }
        fn stop_channel(&mut self, channel: i32) {
            self.0.borrow_mut().stop_channel(channel);
        }
        fn stop_all(&mut self) {
            self.0.borrow_mut().stop_all();
        }
    }

    #[test]
    fn cutoff_and_volume_forwarding_via_shared_recorder() {
        let rec = SharedRecorder::default();
        let inner = rec.0.clone();
        let mut sys = AudioSystem::with_backend(Box::new(rec));
        let sound = make_sound(6);

        // Fresh channel 0: play, no preceding stop.
        sys.play_sound(&sound, 0, 0.25);
        {
            let r = inner.borrow();
            assert_eq!(r.plays, vec![(0, 0.25, 6)]);
            assert!(r.stops.is_empty(), "fresh channel must not cut off");
        }

        // Occupied channel 0: stop(0) then play, volume forwarded.
        sys.play_sound(&sound, 0, 0.75);
        {
            let r = inner.borrow();
            assert_eq!(r.stops, vec![0], "occupied channel must cut off first");
            assert_eq!(r.plays, vec![(0, 0.25, 6), (0, 0.75, 6)]);
        }
    }

    #[test]
    fn negative_channel_never_cuts_off() {
        let rec = SharedRecorder::default();
        let inner = rec.0.clone();
        let mut sys = AudioSystem::with_backend(Box::new(rec));
        let sound = make_sound(3);

        sys.play_sound(&sound, -1, 1.0);
        sys.play_sound(&sound, -1, 1.0);
        sys.play_sound(&sound, -1, 0.5);

        let r = inner.borrow();
        assert!(r.stops.is_empty(), "channel -1 must never cut off");
        assert_eq!(r.plays.len(), 3, "all always-new plays go through");
        assert_eq!(r.plays[2].1, 0.5, "volume forwarded on always-new");
        assert!(!sys.is_channel_occupied(-1));
    }

    #[test]
    fn stop_and_stop_all_clear_occupancy() {
        let rec = SharedRecorder::default();
        let inner = rec.0.clone();
        let mut sys = AudioSystem::with_backend(Box::new(rec));
        let sound = make_sound(2);

        sys.play_sound(&sound, 1, 1.0);
        sys.play_sound(&sound, 2, 1.0);
        assert!(sys.is_channel_occupied(1) && sys.is_channel_occupied(2));

        sys.stop(1);
        assert!(!sys.is_channel_occupied(1));
        assert!(sys.is_channel_occupied(2));
        assert_eq!(inner.borrow().stops, vec![1]);

        sys.stop_all();
        assert!(!sys.is_channel_occupied(2));
        assert_eq!(inner.borrow().stop_alls, 1);
    }

    #[test]
    fn empty_sound_is_not_played() {
        let rec = SharedRecorder::default();
        let inner = rec.0.clone();
        let mut sys = AudioSystem::with_backend(Box::new(rec));
        // Construct an empty Sound by decoding a header-only WAV (no samples).
        let bytes = make_wav(1, 8_000, &[]);
        let sound = Sound::decode(&bytes).expect("empty WAV decodes to empty sound");
        assert!(sound.is_empty());

        sys.play_sound(&sound, 0, 1.0);
        assert!(inner.borrow().plays.is_empty(), "empty sound must not play");
        assert!(!sys.is_channel_occupied(0));
    }

    #[test]
    fn default_audio_system_builds_without_device_and_play_is_safe() {
        // Must not panic even when there is no audio device (CI/headless).
        let mut sys = AudioSystem::default();
        let sound = make_sound(4);
        // Whether the backend is rodio or null, these must be safe no-ops.
        sys.play_sound(&sound, 0, 1.0);
        sys.play_sound(&sound, 0, 0.5);
        sys.play_sound(&sound, -1, 1.0);
        sys.stop(0);
        sys.stop_all();
    }

    #[test]
    fn null_backend_methods_are_safe_noops() {
        let mut backend = NullBackend;
        let sound = make_sound(2);
        backend.play(&sound, 0, 1.0);
        backend.stop_channel(0);
        backend.stop_all();
    }

    #[test]
    fn recording_backend_records_sample_count_per_sound() {
        // AC#4: RecordingBackend records sample_count, so distinct sounds are
        // distinguishable in the log — proves cut-off swaps the actual payload.
        let rec = SharedRecorder::default();
        let inner = rec.0.clone();
        let mut sys = AudioSystem::with_backend(Box::new(rec));
        let short = make_sound(2);
        let long = make_sound(9);

        sys.play_sound(&short, 0, 1.0);
        sys.play_sound(&long, 0, 1.0); // cuts off `short`, plays `long`

        let r = inner.borrow();
        assert_eq!(r.stops, vec![0], "second play on channel 0 must cut off");
        assert_eq!(
            r.plays,
            vec![(0, 1.0, 2), (0, 1.0, 9)],
            "sample counts must distinguish the two sounds"
        );
    }

    #[test]
    fn cutoff_is_isolated_per_channel() {
        // Playing on channel 1 must not cut off channel 0, and vice versa.
        let rec = SharedRecorder::default();
        let inner = rec.0.clone();
        let mut sys = AudioSystem::with_backend(Box::new(rec));
        let sound = make_sound(4);

        sys.play_sound(&sound, 0, 1.0);
        sys.play_sound(&sound, 1, 1.0);
        sys.play_sound(&sound, 2, 1.0);
        // No channel was re-occupied, so no cut-off should have fired.
        assert!(
            inner.borrow().stops.is_empty(),
            "distinct channels must not cut each other off"
        );
        assert!(
            sys.is_channel_occupied(0) && sys.is_channel_occupied(1) && sys.is_channel_occupied(2)
        );

        // Re-playing channel 1 cuts off ONLY channel 1.
        sys.play_sound(&sound, 1, 1.0);
        assert_eq!(inner.borrow().stops, vec![1]);
    }

    #[test]
    fn channel_reusable_after_explicit_stop_without_extra_cutoff() {
        // After stop(0) frees the channel, the next play_sound must NOT issue a
        // redundant cut-off stop (the channel is already free).
        let rec = SharedRecorder::default();
        let inner = rec.0.clone();
        let mut sys = AudioSystem::with_backend(Box::new(rec));
        let sound = make_sound(3);

        sys.play_sound(&sound, 0, 1.0);
        sys.stop(0);
        assert!(!sys.is_channel_occupied(0));
        sys.play_sound(&sound, 0, 1.0); // fresh again

        let r = inner.borrow();
        // Exactly one stop (from the explicit stop call), no cut-off stop.
        assert_eq!(r.stops, vec![0], "no redundant cut-off after explicit stop");
        assert_eq!(r.plays.len(), 2);
        assert!(sys.is_channel_occupied(0));
    }

    #[test]
    fn stop_on_unoccupied_channel_is_safe_and_forwarded() {
        // stop() on a never-played channel must not panic; it forwards to the
        // backend (idempotent) and leaves occupancy empty.
        let rec = SharedRecorder::default();
        let inner = rec.0.clone();
        let mut sys = AudioSystem::with_backend(Box::new(rec));

        sys.stop(7);
        assert!(!sys.is_channel_occupied(7));
        assert_eq!(
            inner.borrow().stops,
            vec![7],
            "stop is forwarded even if free"
        );
    }

    #[test]
    fn play_sound_forwards_raw_volume_including_zero_and_out_of_range() {
        // AC#3/#4: AudioSystem forwards volume verbatim; clamping is a backend
        // concern. Verify zero, >1.0, and negative all reach the backend as-is.
        let rec = SharedRecorder::default();
        let inner = rec.0.clone();
        let mut sys = AudioSystem::with_backend(Box::new(rec));
        let sound = make_sound(1);

        sys.play_sound(&sound, -1, 0.0); // always-new, muted
        sys.play_sound(&sound, -1, 2.5); // always-new, boosted
        sys.play_sound(&sound, -1, -0.5); // always-new, negative

        let r = inner.borrow();
        let vols: Vec<f32> = r.plays.iter().map(|(_, v, _)| *v).collect();
        assert_eq!(
            vols,
            vec![0.0, 2.5, -0.5],
            "volume must pass through unchanged"
        );
    }

    #[test]
    fn multiple_negative_channels_all_stack_untracked() {
        // Different negative channel numbers are all "always new": none tracked,
        // none cut off.
        let rec = SharedRecorder::default();
        let inner = rec.0.clone();
        let mut sys = AudioSystem::with_backend(Box::new(rec));
        let sound = make_sound(2);

        sys.play_sound(&sound, -1, 1.0);
        sys.play_sound(&sound, -5, 1.0);
        sys.play_sound(&sound, -1, 1.0);

        let r = inner.borrow();
        assert!(r.stops.is_empty(), "negative channels never cut off");
        assert_eq!(r.plays.len(), 3);
        assert!(!sys.is_channel_occupied(-1) && !sys.is_channel_occupied(-5));
    }

    #[test]
    fn stop_all_with_no_active_channels_is_safe() {
        let rec = SharedRecorder::default();
        let inner = rec.0.clone();
        let mut sys = AudioSystem::with_backend(Box::new(rec));
        sys.stop_all();
        assert_eq!(inner.borrow().stop_alls, 1);
        // Occupancy stays empty; no panic.
        assert!(!sys.is_channel_occupied(0));
    }

    #[test]
    fn null_backed_system_default_path_via_explicit_null() {
        // AC#3: a system explicitly over NullBackend behaves as a safe no-op for
        // every operation while still maintaining occupancy bookkeeping.
        let mut sys = AudioSystem::with_backend(Box::new(NullBackend));
        let sound = make_sound(3);
        sys.play_sound(&sound, 0, 1.0);
        assert!(
            sys.is_channel_occupied(0),
            "occupancy tracked even when silent"
        );
        sys.play_sound(&sound, 0, 0.5); // cut-off path, still silent
        sys.play_sound(&sound, -1, 1.0); // always-new path
        sys.stop(0);
        assert!(!sys.is_channel_occupied(0));
        sys.stop_all();
    }

    #[test]
    fn debug_impls_do_not_leak_internals_and_dont_panic() {
        // Debug for AudioSystem / NullBackend must render without panicking.
        let sys = AudioSystem::with_backend(Box::new(NullBackend));
        let s = format!("{sys:?}");
        assert!(s.contains("AudioSystem"));
        assert!(format!("{NullBackend:?}").contains("NullBackend"));
    }

    #[test]
    fn audio_backend_is_object_safe() {
        // AC#2: compile-time proof the trait is object-safe (usable as dyn).
        let backends: Vec<Box<dyn AudioBackend>> =
            vec![Box::new(NullBackend), Box::new(SharedRecorder::default())];
        assert_eq!(backends.len(), 2);
    }
}
