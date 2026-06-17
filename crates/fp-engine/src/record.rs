//! Training-mode setup record / playback (T068).
//!
//! The Lab's killer drill: capture the *dummy*'s per-frame inputs from a chosen
//! start position, then replay them on a loop so you can rehearse punishing a
//! setup (a wake-up reversal, a meaty, a blockstring) against a perfectly
//! reproducible foil. This is the [`crate::dummy`] sibling for *scripted* dummies:
//! instead of a fixed stance, the dummy reproduces an exact recorded motion.
//!
//! ## How it reuses the determinism core
//!
//! A recording is nothing more than a **start [`MatchSnapshot`]** plus a
//! **`Vec<MatchInput>` of the dummy side's per-frame inputs** — the same
//! ingredients the shipped record/replay core ([`crate::replay`] /
//! [`crate::snapshot`]) already proves reproduce a match bit-for-bit. Playback is:
//!
//! 1. [`Match::restore_snapshot`](crate::Match::restore_snapshot) the start state
//!    (this re-seats *both* fighters, including their input buffers + command
//!    matchers, so a multi-frame motion never replays half-recognized), then
//! 2. each tick, feed the *live* player input to the player side and the recorded
//!    `inputs[frame]` to the dummy side through the ordinary
//!    [`Match::tick`](crate::Match::tick) path, advancing the frame cursor, and
//! 3. on reaching the end, restore the start snapshot again and loop from frame 0.
//!
//! Because the dummy's input is *raw* [`MatchInput`] (not a recognized command), a
//! recorded fireball replays as a fireball: the same buttons go through the same
//! facing-relative matcher and produce the same special. The only state that
//! varies loop-to-loop is the live player side; if the player feeds the *same*
//! inputs each loop (e.g. neutral), the dummy reproduces an **identical** motion
//! every loop — verified byte-for-byte in this crate's tests.
//!
//! ## Gotcha: only the dummy is replayed
//!
//! A [`TrainingRecorder`] records **only the dummy side's** inputs — never the
//! player side's. The player is live during both record and playback; capturing
//! their future inputs would defeat the purpose (you rehearse *your* response, the
//! dummy is the fixture). The start snapshot captures both fighters' positions so
//! "Reset" re-seats the pair, but the input log is one-sided.

use serde::{Deserialize, Serialize};

use fp_core::{FpError, FpResult};

use crate::{Match, MatchInput, Side};

/// A recorded training setup: the start state plus the dummy side's per-frame
/// inputs (T068).
///
/// Build one with a [`TrainingRecorder`] (which captures the start snapshot and
/// logs each frame), then drive a [`TrainingPlayback`] from it to rehearse against
/// the looping dummy. Derives serde so a setup can be saved to disk and reloaded
/// (a compact bincode blob via [`encode`](TrainingRecording::encode) /
/// [`decode`](TrainingRecording::decode)).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrainingRecording {
    /// The schema version stamped into the recording (see
    /// [`RECORDING_FORMAT_VERSION`]), so a future on-disk change is detected
    /// rather than silently misread.
    pub format_version: u32,
    /// Which side is the *dummy* (the side whose inputs are recorded and replayed).
    /// The other side is the live, player-controlled fighter.
    pub dummy_side: Side,
    /// The serialized [`MatchSnapshot`](crate::MatchSnapshot) bytes of the start
    /// state both fighters are re-seated to at the top of every loop.
    pub start: Vec<u8>,
    /// The dummy side's per-frame [`MatchInput`], in tick order.
    pub dummy_inputs: Vec<MatchInput>,
}

/// The format version stamped into every [`TrainingRecording`].
pub const RECORDING_FORMAT_VERSION: u32 = 1;

impl TrainingRecording {
    /// The number of recorded frames.
    #[must_use]
    pub fn len(&self) -> usize {
        self.dummy_inputs.len()
    }

    /// Whether no dummy frames were recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.dummy_inputs.is_empty()
    }

    /// Encodes the recording to a compact, deterministic bincode blob.
    ///
    /// # Errors
    ///
    /// [`FpError::Other`] if serialization fails (it does not for this plain-data
    /// type, but the contract never panics).
    pub fn encode(&self) -> FpResult<Vec<u8>> {
        bincode::serialize(self)
            .map_err(|e| FpError::Other(format!("training recording serialize failed: {e}")))
    }

    /// Decodes a recording from a bincode blob, rejecting an unknown format
    /// version.
    ///
    /// # Errors
    ///
    /// - [`FpError::Other`] on malformed bytes (never panics).
    /// - [`FpError::Mismatch`] if the blob's schema version is not this build's.
    pub fn decode(bytes: &[u8]) -> FpResult<Self> {
        let rec: TrainingRecording = bincode::deserialize(bytes)
            .map_err(|e| FpError::Other(format!("training recording deserialize failed: {e}")))?;
        if rec.format_version != RECORDING_FORMAT_VERSION {
            return Err(FpError::Mismatch(format!(
                "unsupported training recording format version {} (expected {})",
                rec.format_version, RECORDING_FORMAT_VERSION
            )));
        }
        Ok(rec)
    }
}

/// Records a training setup: captures the start snapshot of a live [`Match`] and
/// logs the **dummy side's** per-frame inputs until stopped (T068).
///
/// Construct it with the match positioned where the drill should begin and the
/// side that is the dummy; then call [`tick`](Self::tick) once per frame with
/// *both* sides' inputs (the player drives the dummy to author the setup, and is
/// of course free to move their own fighter too). Only the dummy side's input is
/// logged. When the setup is captured, take the [`TrainingRecording`] with
/// [`into_recording`](Self::into_recording).
///
/// The recorder borrows the match mutably for its lifetime; read access to the
/// match is reachable through [`match_ref`](Self::match_ref).
pub struct TrainingRecorder<'m> {
    game: &'m mut Match,
    dummy_side: Side,
    start: Vec<u8>,
    dummy_inputs: Vec<MatchInput>,
}

impl<'m> TrainingRecorder<'m> {
    /// Begins recording, snapshotting the match's current state as the loop's
    /// start position.
    ///
    /// # Errors
    ///
    /// Propagates a [`Match::snapshot`](crate::Match::snapshot) failure (it does
    /// not fail for a well-formed match, but the contract never panics).
    pub fn new(game: &'m mut Match, dummy_side: Side) -> FpResult<Self> {
        let start = game.snapshot()?;
        Ok(Self {
            game,
            dummy_side,
            start,
            dummy_inputs: Vec::new(),
        })
    }

    /// Applies one frame to the recorded match **and** logs the dummy side's input.
    ///
    /// `p1`/`p2` are this frame's absolute-direction inputs for the two sides; both
    /// are applied to the live match (so the player sees the dummy move as they
    /// author it), but only the [`dummy_side`](TrainingRecorder) input is recorded.
    pub fn tick(&mut self, p1: MatchInput, p2: MatchInput) {
        let dummy = match self.dummy_side {
            Side::P1 => p1,
            Side::P2 => p2,
        };
        self.dummy_inputs.push(dummy);
        self.game.tick(p1, p2);
    }

    /// Read access to the match being recorded.
    #[must_use]
    pub fn match_ref(&self) -> &Match {
        self.game
    }

    /// The number of frames recorded so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.dummy_inputs.len()
    }

    /// Whether no frames have been recorded yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.dummy_inputs.is_empty()
    }

    /// Stops recording and returns the captured [`TrainingRecording`].
    #[must_use]
    pub fn into_recording(self) -> TrainingRecording {
        TrainingRecording {
            format_version: RECORDING_FORMAT_VERSION,
            dummy_side: self.dummy_side,
            start: self.start,
            dummy_inputs: self.dummy_inputs,
        }
    }
}

/// Plays a [`TrainingRecording`] back on a loop, driving the dummy side from the
/// recorded inputs while the player side stays live (T068).
///
/// Construct it from a recording and the match to drive (built from the **same two
/// characters** the recording was captured with). It immediately re-seats the
/// match to the recording's start snapshot; then call [`tick`](Self::tick) once
/// per frame with the *live player* input. Each tick feeds the player's input to
/// the player side and the recorded input to the dummy side; when the recorded
/// inputs run out it re-seats both fighters to the start and loops from frame 0.
///
/// # Determinism
///
/// Restoring the start snapshot resets the dummy's *entire* runtime (position,
/// state, RNG-affected character state, and input buffer / command matcher), so
/// replaying the same recorded inputs reproduces the **identical** dummy motion
/// every loop. The only variable is the live player side.
pub struct TrainingPlayback<'m> {
    game: &'m mut Match,
    recording: TrainingRecording,
    cursor: usize,
}

impl<'m> TrainingPlayback<'m> {
    /// Begins playback, immediately re-seating the match to the recording's start
    /// snapshot.
    ///
    /// `game` must be built from the same two characters the recording was captured
    /// with; the restore validates this via the snapshot's character fingerprints.
    ///
    /// # Errors
    ///
    /// - [`FpError::Other`] if the recording's start snapshot blob is malformed.
    /// - [`FpError::Mismatch`] if `game`'s characters do not match the snapshot's
    ///   (the match is left unchanged).
    pub fn new(game: &'m mut Match, recording: TrainingRecording) -> FpResult<Self> {
        game.restore_snapshot(&recording.start)?;
        Ok(Self {
            game,
            recording,
            cursor: 0,
        })
    }

    /// Which side the recorded dummy drives (the opposite side is the live player).
    #[must_use]
    pub fn dummy_side(&self) -> Side {
        self.recording.dummy_side
    }

    /// The current frame cursor into the recorded inputs (`0` right after a loop).
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Whether the next [`tick`](Self::tick) will wrap the loop (the cursor has
    /// reached the end of the recorded inputs).
    #[must_use]
    pub fn at_loop_boundary(&self) -> bool {
        self.cursor >= self.recording.dummy_inputs.len()
    }

    /// Re-seats both fighters to the recording's start snapshot and rewinds the
    /// cursor to frame 0 — the "Reset" action, also performed automatically at the
    /// end of each loop.
    ///
    /// # Errors
    ///
    /// Propagates a [`Match::restore_snapshot`](crate::Match::restore_snapshot)
    /// failure (malformed blob or character mismatch); the match is left unchanged
    /// in the mismatch case.
    pub fn reset(&mut self) -> FpResult<()> {
        self.game.restore_snapshot(&self.recording.start)?;
        self.cursor = 0;
        Ok(())
    }

    /// Advances playback one frame: feeds `player_input` to the live player side and
    /// the recorded input to the dummy side, then advances the cursor; on reaching
    /// the end of the recorded inputs it re-seats both fighters and loops.
    ///
    /// An empty recording is a no-op that simply re-seats to the start each call (a
    /// degenerate but never-panicking case).
    ///
    /// # Errors
    ///
    /// Propagates a [`reset`](Self::reset) failure when the loop wraps.
    pub fn tick(&mut self, player_input: MatchInput) -> FpResult<()> {
        // Wrap *before* this frame if the prior frame consumed the last input, so
        // the start state is in place and the dummy replays from frame 0 again.
        if self.cursor >= self.recording.dummy_inputs.len() {
            self.reset()?;
            if self.recording.dummy_inputs.is_empty() {
                // Nothing to replay; the reset above already re-seated the start.
                return Ok(());
            }
        }
        let dummy_input = self.recording.dummy_inputs[self.cursor];
        let (p1, p2) = match self.recording.dummy_side {
            Side::P1 => (dummy_input, player_input),
            Side::P2 => (player_input, dummy_input),
        };
        self.game.tick(p1, p2);
        self.cursor += 1;
        Ok(())
    }

    /// Read access to the match being driven.
    #[must_use]
    pub fn match_ref(&self) -> &Match {
        self.game
    }
}
