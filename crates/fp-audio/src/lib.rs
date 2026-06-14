//! # fp-audio
//!
//! Audio playback core for the Fighters Paradise engine. Decodes sound effects
//! into replayable in-memory PCM, manages per-channel playback with MUGEN
//! cut-off semantics, and hides the real audio device behind a backend seam so
//! the whole engine can run headless (in tests or on machines with no audio
//! output) without panicking.
//!
//! ## Decoupling
//!
//! This crate is intentionally **decoupled from `fp-formats`**. The SND parser
//! in `fp-formats` returns each sound as raw WAV/RIFF bytes; `fp-audio` operates
//! purely on those raw bytes (and the decoded PCM) and never depends on
//! `fp-formats`. The engine wires the two together at a higher layer.
//!
//! ## Overview
//!
//! - [`Sound`] — decoded PCM held in memory, replayable any number of times.
//!   Create it with [`Sound::decode`] from raw WAV bytes.
//! - [`AudioBackend`] — object-safe trait that is the seam between channel logic
//!   and the actual audio device. Implementations: [`RodioBackend`] (real
//!   device, degrades gracefully when none exists) and [`NullBackend`] (silent).
//! - [`AudioSystem`] — owns a `Box<dyn AudioBackend>` and applies MUGEN channel
//!   cut-off bookkeeping on top of it. Its [`AudioSystem::default`] never panics:
//!   it tries the real device and falls back to silence.
//!
//! ## MUGEN channel semantics
//!
//! MUGEN's `PlaySnd` controller plays a sound on a numbered channel. Starting a
//! sound on an already-occupied channel cuts off (stops) whatever was playing
//! there. The special channel `-1` (any negative channel) is "always new": it
//! never cuts anything off and is never cut off, so overlapping hits can stack.
//! See `docs/knowledge-base/03-engine-architecture.md` for the full spec.
//!
//! ## Example
//!
//! ```no_run
//! use fp_audio::{AudioSystem, Sound};
//!
//! # fn demo(wav_bytes: &[u8]) -> fp_core::FpResult<()> {
//! let sound = Sound::decode(wav_bytes)?;
//! let mut audio = AudioSystem::default(); // never panics; silent if no device
//! audio.play_sound(&sound, 0, 1.0);       // play on channel 0 at full volume
//! audio.play_sound(&sound, 0, 0.5);       // cuts off the previous channel-0 sound
//! audio.stop(0);
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]

mod sound;
mod system;

pub use sound::Sound;
pub use system::{AudioBackend, AudioSystem, NullBackend, RodioBackend};
