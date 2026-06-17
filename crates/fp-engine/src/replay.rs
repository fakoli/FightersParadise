//! Deterministic input record / replay for a [`Match`] (#38).
//!
//! The fixed-60Hz tick is deterministic by construction, and a [`Match`]'s entire
//! per-frame nondeterminism budget is its two [`MatchInput`]s plus the per-player
//! RNG seeds. So a match can be reproduced *exactly* from just three things:
//!
//! 1. the **match seed** (which seeds the two fighters' `random` streams; see
//!    [`Match::seed_players`](crate::Match::seed_players)),
//! 2. the **frame-by-frame input pairs** `(p1_input, p2_input)`, and
//! 3. the **same two characters** (same `.def`s) the match was played with.
//!
//! [`ReplayLog`] records (1) and (2) (it derives serde, so it persists to disk);
//! the loaded characters (3) are supplied at replay time. [`MatchRecorder`] wraps
//! a live [`Match`] and logs each tick's inputs as they are applied, and
//! [`replay_match`] drives a freshly-built match through a log to reproduce it.
//!
//! # Determinism contract
//!
//! Replaying a log into a fresh [`Match`] built from the same characters and the
//! log's seed reproduces the match **bit-for-bit**: the final
//! [`Match::snapshot`](crate::Match::snapshot) bytes are identical, frame for
//! frame. This is verified by the crate's record→replay and two-run determinism
//! integration tests.
//!
//! # Never panics
//!
//! Encoding / decoding a [`ReplayLog`] goes through [`bincode`] and returns a
//! recoverable [`ReplayError`] on malformed bytes. Replaying a log is infallible
//! (it only feeds inputs and ticks).

use serde::{Deserialize, Serialize};

use fp_character::CharacterFingerprint;

use crate::snapshot::MatchSnapshot;
use crate::{Match, MatchInput, StageBounds, DEFAULT_MATCH_SEED};

/// The format version stamped into every [`ReplayLog`], so a future on-disk
/// schema change can be detected rather than silently misread.
pub const REPLAY_FORMAT_VERSION: u32 = 1;

/// The default keyframe interval used by [`ReplayPlayer`]: a [`MatchSnapshot`] is
/// cached every this-many frames so a seek restores the nearest earlier keyframe
/// and re-simulates only the remainder, rather than re-running from frame 0.
///
/// 30 frames (half a second at 60Hz) keeps the worst-case re-sim from a keyframe
/// bounded to under one in-game second while keeping the cache small — a sensible
/// default for the interactive replay viewer (T076).
pub const DEFAULT_KEYFRAME_INTERVAL: u32 = 30;

/// An error from encoding, decoding, or replaying a [`ReplayLog`].
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    /// The replay log could not be (de)serialized.
    #[error("replay log codec error: {0}")]
    Codec(String),
    /// The decoded log's [`format_version`](ReplayLog::format_version) is not one
    /// this build understands.
    #[error("unsupported replay format version {found} (expected {expected})")]
    Version {
        /// The version found in the decoded log.
        found: u32,
        /// The version this build supports.
        expected: u32,
    },
    /// The log was recorded with a different character than the match it is being
    /// replayed into (#38).
    ///
    /// Replaying recorded inputs into a match built from different `.def`s would
    /// diverge immediately or corrupt state, so [`replay_match`] validates the
    /// per-player [`CharacterFingerprint`]s the log carries against the target
    /// match before feeding any input and returns this error on a mismatch
    /// (changing nothing) instead of producing a meaningless replay.
    #[error(
        "replay character mismatch on {side}: log {logged:#018x}, match {actual:#018x}; \
         the log was recorded with a different character"
    )]
    CharacterMismatch {
        /// Which player's fingerprint differed (`"P1"` or `"P2"`).
        side: &'static str,
        /// The fingerprint recorded in the log.
        logged: u64,
        /// The fingerprint of the match's loaded character.
        actual: u64,
    },
}

/// A recorded match: the match seed, the stage bounds, the best-of-N target, the
/// round length, and the per-frame input pairs (#38).
///
/// Everything needed to reproduce a match *except* the two characters (supplied
/// at replay time, since the characters' loaded assets are not — and need not be
/// — serialized). Derives serde so it persists to disk as a compact bincode blob
/// (see [`ReplayLog::encode`] / [`ReplayLog::decode`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayLog {
    /// The replay schema version (see [`REPLAY_FORMAT_VERSION`]).
    pub format_version: u32,
    /// The match seed both players' RNG streams were derived from.
    pub match_seed: i32,
    /// The stage bounds the match was played on.
    pub bounds: StageBounds,
    /// The best-of-N round-win target (`rounds_to_win`).
    pub rounds_to_win: i32,
    /// The round length in seconds the match was played with.
    pub round_seconds: i32,
    /// Player 1's character identity fingerprint at record time, or the
    /// [`UNSTAMPED`](CharacterFingerprint) sentinel for a hand-built log (#38).
    ///
    /// A [`MatchRecorder`] stamps the recorded match's real fingerprints here;
    /// [`replay_match`] validates them against the target match (skipping the guard
    /// only when the log is unstamped). See [`ReplayLog::with_fingerprints`].
    pub p1_fingerprint: CharacterFingerprint,
    /// Player 2's character identity fingerprint at record time, or the unstamped
    /// sentinel. See [`p1_fingerprint`](ReplayLog::p1_fingerprint).
    pub p2_fingerprint: CharacterFingerprint,
    /// The per-frame `(p1_input, p2_input)` pairs, in tick order.
    pub inputs: Vec<(MatchInput, MatchInput)>,
}

/// The "unstamped" fingerprint sentinel a hand-built [`ReplayLog::new`] carries
/// when no character identity is known; [`replay_match`] skips the identity guard
/// for a side whose recorded fingerprint is this value.
const UNSTAMPED: CharacterFingerprint = CharacterFingerprint(0);

impl ReplayLog {
    /// Creates an empty log with the given match configuration and **unstamped**
    /// character fingerprints (the identity guard is skipped on replay).
    ///
    /// Use [`ReplayLog::with_fingerprints`] — or, more usually, a [`MatchRecorder`],
    /// which stamps them automatically — to record the character identities so a
    /// later [`replay_match`] can reject a replay into the wrong characters.
    #[must_use]
    pub fn new(
        match_seed: i32,
        bounds: StageBounds,
        rounds_to_win: i32,
        round_seconds: i32,
    ) -> Self {
        Self {
            format_version: REPLAY_FORMAT_VERSION,
            match_seed,
            bounds,
            rounds_to_win,
            round_seconds,
            p1_fingerprint: UNSTAMPED,
            p2_fingerprint: UNSTAMPED,
            inputs: Vec::new(),
        }
    }

    /// Creates an empty log carrying the two players' identity fingerprints, so a
    /// later [`replay_match`] can verify it is replayed into the same characters
    /// (#38).
    #[must_use]
    pub fn with_fingerprints(
        match_seed: i32,
        bounds: StageBounds,
        rounds_to_win: i32,
        round_seconds: i32,
        p1_fingerprint: CharacterFingerprint,
        p2_fingerprint: CharacterFingerprint,
    ) -> Self {
        Self {
            p1_fingerprint,
            p2_fingerprint,
            ..Self::new(match_seed, bounds, rounds_to_win, round_seconds)
        }
    }

    /// The number of recorded frames.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inputs.len()
    }

    /// Whether no frames have been recorded yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inputs.is_empty()
    }

    /// Appends one frame's input pair to the log.
    pub fn push(&mut self, p1: MatchInput, p2: MatchInput) {
        self.inputs.push((p1, p2));
    }

    /// Encodes the log to a compact, deterministic bincode blob.
    pub fn encode(&self) -> Result<Vec<u8>, ReplayError> {
        bincode::serialize(self).map_err(|e| ReplayError::Codec(e.to_string()))
    }

    /// Decodes a log from a bincode blob, rejecting an unknown format version.
    ///
    /// Returns [`ReplayError::Codec`] on malformed bytes (never panics) and
    /// [`ReplayError::Version`] when the blob's schema version is not this build's.
    pub fn decode(bytes: &[u8]) -> Result<Self, ReplayError> {
        let log: ReplayLog =
            bincode::deserialize(bytes).map_err(|e| ReplayError::Codec(e.to_string()))?;
        if log.format_version != REPLAY_FORMAT_VERSION {
            return Err(ReplayError::Version {
                found: log.format_version,
                expected: REPLAY_FORMAT_VERSION,
            });
        }
        Ok(log)
    }
}

impl Default for ReplayLog {
    /// A default-configured, empty log (default match seed, default bounds,
    /// default best-of-three, default round length).
    fn default() -> Self {
        Self::new(
            DEFAULT_MATCH_SEED,
            StageBounds::default(),
            crate::DEFAULT_ROUNDS_TO_WIN,
            crate::DEFAULT_ROUND_SECONDS,
        )
    }
}

/// Records a live [`Match`] frame-by-frame into a [`ReplayLog`] (#38).
///
/// Wrap a freshly-seeded match, then drive the *recorder's* [`tick`](Self::tick)
/// instead of the match's directly: each call applies the inputs to the match and
/// appends them to the log. When the match ends, take the [`ReplayLog`] with
/// [`into_log`](Self::into_log) (or borrow it via [`log`](Self::log)) to persist
/// or replay it.
///
/// The recorder borrows the match mutably for its lifetime; read accessors on the
/// match are reachable through [`match_ref`](Self::match_ref).
pub struct MatchRecorder<'m> {
    game: &'m mut Match,
    log: ReplayLog,
}

impl<'m> MatchRecorder<'m> {
    /// Begins recording the given match, stamping the log with the supplied
    /// configuration.
    ///
    /// `match_seed` must be the seed the match's players were actually seeded with
    /// (via [`Match::seed_players`](crate::Match::seed_players) /
    /// [`Match::with_seed`](crate::Match::with_seed)) for the replay to reproduce
    /// it; the recorder records inputs but does not re-seed.
    #[must_use]
    pub fn new(game: &'m mut Match, match_seed: i32, round_seconds: i32) -> Self {
        let bounds = game.bounds();
        let rounds_to_win = game.rounds_to_win();
        // Stamp the recorded characters' identity fingerprints so a later
        // `replay_match` can reject a replay into different characters (#38).
        let (p1_fp, p2_fp) = game.character_fingerprints();
        Self {
            game,
            log: ReplayLog::with_fingerprints(
                match_seed,
                bounds,
                rounds_to_win,
                round_seconds,
                p1_fp,
                p2_fp,
            ),
        }
    }

    /// Applies one frame's input pair to the recorded match **and** logs it.
    pub fn tick(&mut self, p1: MatchInput, p2: MatchInput) {
        self.log.push(p1, p2);
        self.game.tick(p1, p2);
    }

    /// Read access to the match being recorded.
    #[must_use]
    pub fn match_ref(&self) -> &Match {
        self.game
    }

    /// Borrows the log recorded so far.
    #[must_use]
    pub fn log(&self) -> &ReplayLog {
        &self.log
    }

    /// Consumes the recorder, returning the completed [`ReplayLog`].
    #[must_use]
    pub fn into_log(self) -> ReplayLog {
        self.log
    }
}

/// Replays a [`ReplayLog`] into the given freshly-built match, reproducing it.
///
/// The `game` must be a match built from the **same two characters** the log was
/// recorded with (same `.def`s). Before feeding any input this validates the log's
/// recorded per-player [`CharacterFingerprint`]s against `game`'s loaded
/// characters: on a mismatch it returns [`ReplayError::CharacterMismatch`] and
/// **does nothing** (no seeding, no ticking), rather than producing a corrupt /
/// meaningless replay. A log with [`UNSTAMPED`] fingerprints (a hand-built
/// [`ReplayLog::new`]) skips the guard for that side.
///
/// On a match it seeds the players from the log's match seed and feeds every
/// recorded input pair in order. After it returns `Ok(())`, `game` is in the exact
/// final runtime state the recorded match ended in — a
/// [`Match::snapshot`](crate::Match::snapshot) of it is byte-equal to a snapshot of
/// the original at the same frame.
///
/// The caller is responsible for having built `game` with the log's
/// [`bounds`](ReplayLog::bounds) / [`rounds_to_win`](ReplayLog::rounds_to_win) /
/// [`round_seconds`](ReplayLog::round_seconds) for a faithful reproduction (a
/// match built with different config diverges, as it would live).
///
/// # Errors
///
/// [`ReplayError::CharacterMismatch`] if a stamped fingerprint in the log does not
/// match the corresponding loaded character in `game`.
pub fn replay_match(game: &mut Match, log: &ReplayLog) -> Result<(), ReplayError> {
    check_replay_fingerprints(game, log)?;
    game.seed_players(log.match_seed);
    for &(p1, p2) in &log.inputs {
        game.tick(p1, p2);
    }
    Ok(())
}

/// Validates that a `log`'s recorded per-player [`CharacterFingerprint`]s match the
/// characters loaded into `game`, skipping a side whose recorded fingerprint is the
/// [`UNSTAMPED`] sentinel.
///
/// Shared by [`replay_match`] (one-shot reproduce) and [`ReplayPlayer::new`] (the
/// interactive viewer), so both reject a replay into the wrong characters by the
/// same rule rather than corrupting state.
///
/// # Errors
///
/// [`ReplayError::CharacterMismatch`] if a stamped fingerprint differs.
fn check_replay_fingerprints(game: &Match, log: &ReplayLog) -> Result<(), ReplayError> {
    let (actual_p1, actual_p2) = game.character_fingerprints();
    if log.p1_fingerprint != UNSTAMPED && log.p1_fingerprint != actual_p1 {
        return Err(ReplayError::CharacterMismatch {
            side: "P1",
            logged: log.p1_fingerprint.0,
            actual: actual_p1.0,
        });
    }
    if log.p2_fingerprint != UNSTAMPED && log.p2_fingerprint != actual_p2 {
        return Err(ReplayError::CharacterMismatch {
            side: "P2",
            logged: log.p2_fingerprint.0,
            actual: actual_p2.0,
        });
    }
    Ok(())
}

/// An interactive replay-study transport over a [`ReplayLog`] (T076).
///
/// Wraps an already-loaded [`Match`] (built from the **same two characters** the
/// log was recorded with) and drives it through the log's recorded inputs, exposing
/// VCR-style transport controls — **play / pause**, **step ±1 frame**, and **seek
/// (scrub)** to an arbitrary frame — for a replay-study UI. The F026 overlays
/// (hitbox view, input display, frame data) are pure draw layers over the live
/// match reachable via [`match_ref`](ReplayPlayer::match_ref), so they apply to the
/// replay unchanged.
///
/// # Forward-only re-simulation
///
/// The engine tick is deterministic but forward-only — there is no reverse
/// integration. A seek therefore *restores* the nearest earlier cached
/// [`MatchSnapshot`] ("keyframe") and *re-simulates* forward, feeding the recorded
/// inputs:
///
/// ```text
/// seek(f): kf = nearest_keyframe <= f; restore(kf.snapshot); for i in kf.frame..f: tick(inputs[i])
/// ```
///
/// Frame 0 (the freshly-seeded, pre-first-tick state) is always a keyframe, so a
/// seek can always re-sim from a valid base. Keyframes are cached lazily every
/// [`keyframe_interval`](ReplayPlayer::keyframe_interval) frames as playback /
/// seeking visits them, bounding the worst-case re-sim distance.
///
/// **Step-back is seek-to-(current − 1)** — a re-sim, never reverse integration.
///
/// # Determinism
///
/// Because re-sim is deterministic, seeking to a frame `f` always yields the
/// **identical** match state regardless of the path taken to get there:
/// `seek(f)` then `seek(f)` again (or `seek(g)` then `seek(f)`) leaves the match
/// byte-equal. This is the viewer's correctness invariant and is unit-tested.
///
/// # Never panics
///
/// [`new`](ReplayPlayer::new) validates the log's character fingerprints (returning
/// [`ReplayError::CharacterMismatch`] and changing nothing on a mismatch) before
/// seeding. All transport methods clamp their target frame to `0..=len` and never
/// block on I/O.
pub struct ReplayPlayer<'m> {
    game: &'m mut Match,
    log: ReplayLog,
    /// The current frame: the number of recorded input pairs applied so far, in
    /// `0..=log.len()`. `0` is the freshly-seeded pre-tick state.
    frame: u32,
    /// Whether the transport is in "play" (advancing) vs. "pause" — a flag the
    /// caller's run loop reads to decide whether to [`advance`](ReplayPlayer::advance).
    playing: bool,
    /// How often (in frames) a keyframe snapshot is cached.
    keyframe_interval: u32,
    /// Cached keyframes as `(frame, snapshot)`, kept sorted ascending by frame and
    /// always containing frame 0. Used as seek bases for restore + re-sim.
    keyframes: Vec<(u32, MatchSnapshot)>,
}

impl<'m> ReplayPlayer<'m> {
    /// Opens a replay-study transport over `log`, driving `game`.
    ///
    /// `game` must be a freshly-built match from the **same two characters** the log
    /// was recorded with. This validates the log's recorded character fingerprints
    /// (skipping the [`UNSTAMPED`] sentinel), seeds the players from the log's match
    /// seed, and captures frame 0 as the first keyframe. On a fingerprint mismatch
    /// it returns [`ReplayError::CharacterMismatch`] and **changes nothing** (no
    /// seeding), exactly like [`replay_match`].
    ///
    /// Uses [`DEFAULT_KEYFRAME_INTERVAL`]; for a custom cadence use
    /// [`with_keyframe_interval`](ReplayPlayer::with_keyframe_interval).
    ///
    /// # Errors
    ///
    /// [`ReplayError::CharacterMismatch`] if a stamped fingerprint in the log does
    /// not match `game`'s loaded characters.
    pub fn new(game: &'m mut Match, log: ReplayLog) -> Result<Self, ReplayError> {
        Self::with_keyframe_interval(game, log, DEFAULT_KEYFRAME_INTERVAL)
    }

    /// Like [`new`](ReplayPlayer::new) but with an explicit keyframe interval.
    ///
    /// An `interval` of `0` is treated as `1` (cache every frame). A larger interval
    /// caches fewer keyframes (less memory) at the cost of a longer worst-case
    /// re-sim per seek.
    ///
    /// # Errors
    ///
    /// [`ReplayError::CharacterMismatch`] if a stamped fingerprint in the log does
    /// not match `game`'s loaded characters.
    pub fn with_keyframe_interval(
        game: &'m mut Match,
        log: ReplayLog,
        interval: u32,
    ) -> Result<Self, ReplayError> {
        check_replay_fingerprints(game, &log)?;
        game.seed_players(log.match_seed);
        let frame0 = game.snapshot_state();
        Ok(Self {
            game,
            log,
            frame: 0,
            playing: false,
            keyframe_interval: interval.max(1),
            keyframes: vec![(0, frame0)],
        })
    }

    /// The total number of recorded frames (the seekable range is `0..=len`).
    #[must_use]
    pub fn len(&self) -> u32 {
        self.log.inputs.len() as u32
    }

    /// Whether the log has no recorded frames.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.log.inputs.is_empty()
    }

    /// The current playhead frame (recorded input pairs applied so far), `0..=len`.
    #[must_use]
    pub fn current_frame(&self) -> u32 {
        self.frame
    }

    /// The keyframe interval this player caches snapshots at.
    #[must_use]
    pub fn keyframe_interval(&self) -> u32 {
        self.keyframe_interval
    }

    /// Whether the transport is in the "play" (advancing) state.
    #[must_use]
    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// Whether the playhead is at the end of the log (nothing left to advance).
    #[must_use]
    pub fn at_end(&self) -> bool {
        self.frame >= self.len()
    }

    /// Read access to the live match the replay is driving — the surface the F026
    /// overlays draw over.
    #[must_use]
    pub fn match_ref(&self) -> &Match {
        self.game
    }

    /// Sets play (advancing); a no-op transport flag the run loop reads.
    pub fn play(&mut self) {
        self.playing = true;
    }

    /// Sets pause (holding the current frame).
    pub fn pause(&mut self) {
        self.playing = false;
    }

    /// Toggles between play and pause, returning the new playing state.
    pub fn toggle_play(&mut self) -> bool {
        self.playing = !self.playing;
        self.playing
    }

    /// Advances exactly one frame if not already at the end, feeding the next
    /// recorded input pair, and returns whether a frame was consumed.
    ///
    /// This is the "play" step the run loop calls each tick while
    /// [`is_playing`](ReplayPlayer::is_playing). It also opportunistically caches a
    /// keyframe when it lands on an interval boundary. At the end it auto-pauses and
    /// returns `false`.
    pub fn advance(&mut self) -> bool {
        if self.at_end() {
            self.playing = false;
            return false;
        }
        let (p1, p2) = self.log.inputs[self.frame as usize];
        self.game.tick(p1, p2);
        self.frame += 1;
        self.cache_keyframe_if_due();
        true
    }

    /// Steps the playhead forward one frame (alias of [`advance`](ReplayPlayer::advance)).
    pub fn step_forward(&mut self) -> bool {
        self.advance()
    }

    /// Steps the playhead **back** one frame by seeking to `current − 1` (re-sim
    /// from the nearest keyframe). A no-op (returns `false`) at frame 0.
    pub fn step_back(&mut self) -> bool {
        if self.frame == 0 {
            return false;
        }
        self.seek(self.frame - 1);
        true
    }

    /// Seeks (scrubs) the playhead to `target` (clamped to `0..=len`).
    ///
    /// Restores the nearest cached keyframe at or before `target`, then re-simulates
    /// forward feeding the recorded inputs, caching new keyframes on interval
    /// boundaries along the way. Pausing is implied — seeking does not resume play.
    /// Returns the actual (clamped) frame landed on.
    ///
    /// Determinism guarantees seeking to the same frame twice yields byte-equal
    /// match state regardless of the prior playhead position.
    pub fn seek(&mut self, target: u32) -> u32 {
        let target = target.min(self.len());
        // Find the nearest keyframe at or before `target` (keyframes are sorted and
        // always include frame 0, so this never fails).
        let (kf_frame, kf_snap) = self
            .keyframes
            .iter()
            .rev()
            .find(|(f, _)| *f <= target)
            .map(|(f, s)| (*f, s.clone()))
            .expect("frame-0 keyframe always present");
        // Restore is infallible for a snapshot taken from this same match (the
        // fingerprints match by construction); on the impossible mismatch we fall
        // back to re-seeding from frame 0 rather than panicking.
        if self.game.restore_snapshot_state(&kf_snap).is_err() {
            self.game.seed_players(self.log.match_seed);
            self.frame = 0;
            return self.seek_from_zero(target);
        }
        self.frame = kf_frame;
        self.resim_to(target);
        target
    }

    /// Re-seeds from scratch then re-sims to `target` — the cold fallback path used
    /// only if a keyframe restore unexpectedly fails. Frame is assumed already 0.
    fn seek_from_zero(&mut self, target: u32) -> u32 {
        self.resim_to(target);
        target
    }

    /// Re-simulates forward from the current `frame` up to `target`, feeding the
    /// recorded inputs and caching keyframes on interval boundaries.
    fn resim_to(&mut self, target: u32) {
        while self.frame < target {
            let (p1, p2) = self.log.inputs[self.frame as usize];
            self.game.tick(p1, p2);
            self.frame += 1;
            self.cache_keyframe_if_due();
        }
    }

    /// Caches a keyframe at the current frame if it is on an interval boundary and
    /// not already cached, keeping [`keyframes`](Self::keyframes) sorted ascending.
    fn cache_keyframe_if_due(&mut self) {
        if !self.frame.is_multiple_of(self.keyframe_interval) {
            return;
        }
        // Binary-search for the insertion point; skip if already present.
        match self
            .keyframes
            .binary_search_by_key(&self.frame, |(f, _)| *f)
        {
            Ok(_) => {}
            Err(pos) => {
                let snap = self.game.snapshot_state();
                self.keyframes.insert(pos, (self.frame, snap));
            }
        }
    }

    /// Consumes the player, returning the owned [`ReplayLog`].
    #[must_use]
    pub fn into_log(self) -> ReplayLog {
        self.log
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_log_encode_decode_round_trips() {
        let mut log = ReplayLog::new(7, StageBounds::new(-100.0, 100.0), 2, 99);
        log.push(
            MatchInput {
                right: true,
                ..MatchInput::none()
            },
            MatchInput::none(),
        );
        log.push(
            MatchInput::none(),
            MatchInput {
                a: true,
                ..MatchInput::none()
            },
        );

        let bytes = log.encode().expect("encode");
        let decoded = ReplayLog::decode(&bytes).expect("decode");
        assert_eq!(log, decoded);
    }

    #[test]
    fn decode_rejects_truncated_bytes() {
        let log = ReplayLog::new(1, StageBounds::default(), 2, 99);
        let mut bytes = log.encode().expect("encode");
        bytes.truncate(bytes.len() / 2);
        // Truncated input is a recoverable error, never a panic.
        assert!(matches!(
            ReplayLog::decode(&bytes),
            Err(ReplayError::Codec(_))
        ));
    }

    #[test]
    fn decode_rejects_bad_version() {
        let mut log = ReplayLog::new(1, StageBounds::default(), 2, 99);
        log.format_version = 999;
        let bytes = log.encode().expect("encode");
        match ReplayLog::decode(&bytes) {
            Err(ReplayError::Version { found, expected }) => {
                assert_eq!(found, 999);
                assert_eq!(expected, REPLAY_FORMAT_VERSION);
            }
            other => panic!("expected version error, got {other:?}"),
        }
    }
}
