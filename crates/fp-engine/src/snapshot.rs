//! Whole-[`Match`](crate::Match) runtime-state snapshot / restore (#38).
//!
//! [`MatchSnapshot`] is a plain-data, serializable mirror of the *mutable runtime
//! state* a [`Match`](crate::Match) carries: the round/match flow (phase, timer,
//! round wins, freeze, effects, game clock) plus each player's runtime via
//! [`PlayerSnapshot`] (the [`fp_character::CharacterSnapshot`] and the transient
//! input-buffer / command-recognition state).
//!
//! # Runtime vs. static split
//!
//! A snapshot deliberately captures **only** the runtime state. It does *not*
//! carry the loaded static data, which is reloaded from each character's `.def`:
//!
//! - per-player [`fp_character::LoadedCharacter`] (compiled state graph, sprites,
//!   AIR, CMD) and [`fp_character::CharacterConstants`],
//! - the compiled [`fp_input::CommandDef`]s inside each player's matcher,
//! - the round-reset templates and stage bounds, which are fixed at construction
//!   and are *also* re-established when a match is freshly built from the same
//!   characters.
//!
//! [`Match::restore_snapshot`](crate::Match::restore_snapshot) is therefore
//! applied to an **already-loaded** match built from the same two characters: it
//! overwrites the mutable runtime and leaves the static handles untouched. This
//! is the rollback / save-state primitive for netplay and replay.
//!
//! # Encoding & never-panic
//!
//! [`Match::snapshot`](crate::Match::snapshot) encodes the [`MatchSnapshot`] with
//! [`bincode`] into a compact, deterministic little-endian byte blob;
//! [`Match::restore_snapshot`](crate::Match::restore_snapshot) decodes it. A
//! truncated or malformed blob yields a recoverable [`fp_core::FpError`] (never a
//! panic), per the engine's "never crash on bad content" rule.

use serde::{Deserialize, Serialize};

use fp_character::{CharacterFingerprint, CharacterSnapshot};
use fp_core::{FpError, FpResult};
use fp_input::{CommandMatcherSnapshot, InputBufferSnapshot};

use crate::{
    Effect, Freeze, FreezeExempt, Match, MatchState, Player, RoundResetState, RoundState, StageBounds,
    Winner,
};

/// A serializable snapshot of one [`Player`]'s mutable runtime state (#38).
///
/// Bundles the character runtime ([`CharacterSnapshot`]) with the transient input
/// pipeline state — the raw input ring buffer and the command-recognition timers
/// — so a restore reproduces command recognition exactly across the cut. The
/// compiled command definitions and loaded assets are static and not carried.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlayerSnapshot {
    /// The character entity's runtime state.
    pub character: CharacterSnapshot,
    /// The raw input ring-buffer contents (command-history).
    pub input_buffer: InputBufferSnapshot,
    /// The command matcher's active-command timers.
    pub matcher: CommandMatcherSnapshot,
}

impl PlayerSnapshot {
    /// Captures a player's runtime state.
    #[must_use]
    pub fn capture(player: &Player) -> Self {
        Self {
            character: player.character.snapshot(),
            input_buffer: player.input_buffer.snapshot(),
            matcher: player.matcher.snapshot(),
        }
    }

    /// Restores this snapshot onto an already-loaded player (same `.def`).
    pub fn apply_to(&self, player: &mut Player) {
        player.character.restore_from_snapshot(&self.character);
        player.input_buffer.restore_snapshot(&self.input_buffer);
        player.matcher.restore_snapshot(&self.matcher);
    }
}

/// A serializable snapshot of a whole [`Match`]'s mutable runtime state (#38).
///
/// Every field mirrors the like-named [`Match`] field; see that struct for the
/// per-field semantics. Build / apply it through
/// [`Match::snapshot`](crate::Match::snapshot) /
/// [`Match::restore_snapshot`](crate::Match::restore_snapshot), which add the
/// bincode encode/decode and the never-panic error handling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchSnapshot {
    /// Player 1's character identity fingerprint at capture time (#38).
    ///
    /// Stamped from the loaded `.def` so [`apply_to`](MatchSnapshot::apply_to) can
    /// reject a restore into a match built from a *different* player-1 character
    /// instead of silently corrupting state. See [`CharacterFingerprint`].
    pub p1_fingerprint: CharacterFingerprint,
    /// Player 2's character identity fingerprint at capture time (#38). See
    /// [`p1_fingerprint`](MatchSnapshot::p1_fingerprint).
    pub p2_fingerprint: CharacterFingerprint,
    /// Player 1 runtime.
    pub p1: PlayerSnapshot,
    /// Player 2 runtime.
    pub p2: PlayerSnapshot,
    /// The horizontal playfield bounds (also static, but cheap and self-checking
    /// on restore).
    pub bounds: StageBounds,
    /// Current round phase.
    pub round_state: RoundState,
    /// Frames remaining on the round clock.
    pub timer: i32,
    /// Frames elapsed in the current intro / KO phase.
    pub phase_timer: i32,
    /// The decided winner of the current round, if any.
    pub winner: Option<Winner>,
    /// Number of round wins required to win the match.
    pub rounds_to_win: i32,
    /// Rounds player 1 has won.
    pub p1_round_wins: i32,
    /// Rounds player 2 has won.
    pub p2_round_wins: i32,
    /// The 1-based current round number.
    pub round_number: i32,
    /// Whether the match is in progress or decided.
    pub match_state: MatchState,
    /// The match winner, if decided.
    pub match_winner: Option<Winner>,
    /// Player 1's round-reset template (internal type; crate-visible).
    pub(crate) p1_reset: RoundResetState,
    /// Player 2's round-reset template (internal type; crate-visible).
    pub(crate) p2_reset: RoundResetState,
    /// The round length in frames the timer resets to.
    pub round_frames: i32,
    /// Total game ticks elapsed (`GameTime`).
    pub game_time: i32,
    /// The active whole-match freeze (internal type; crate-visible).
    pub(crate) freeze: Freeze,
    /// Live hit-spark / effect entities.
    pub effects: Vec<Effect>,
}

impl MatchSnapshot {
    /// Captures the full runtime state of a match.
    #[must_use]
    pub fn capture(m: &Match) -> Self {
        let (p1_fingerprint, p2_fingerprint) = m.character_fingerprints();
        Self {
            p1_fingerprint,
            p2_fingerprint,
            p1: PlayerSnapshot::capture(&m.p1),
            p2: PlayerSnapshot::capture(&m.p2),
            bounds: m.bounds,
            round_state: m.round_state,
            timer: m.timer,
            phase_timer: m.phase_timer,
            winner: m.winner,
            rounds_to_win: m.rounds_to_win,
            p1_round_wins: m.p1_round_wins,
            p2_round_wins: m.p2_round_wins,
            round_number: m.round_number,
            match_state: m.match_state,
            match_winner: m.match_winner,
            p1_reset: m.p1_reset,
            p2_reset: m.p2_reset,
            round_frames: m.round_frames,
            game_time: m.game_time,
            freeze: m.freeze,
            effects: m.effects.clone(),
        }
    }

    /// Restores this snapshot onto an already-loaded match built from the **same
    /// two characters** (#38).
    ///
    /// First validates that the snapshot's recorded per-player
    /// [`CharacterFingerprint`]s match the target match's loaded characters via
    /// [`check_fingerprints`](MatchSnapshot::check_fingerprints). On a mismatch it
    /// returns a recoverable [`FpError::Mismatch`] and **changes nothing** —
    /// applying runtime state captured from a different character would silently
    /// corrupt the simulation. On a match it overwrites the runtime fields and
    /// per-player runtime; the loaded assets, constants, and compiled command
    /// definitions are left untouched. The per-player sound-request scratch vectors
    /// are cleared (they are derived fresh each tick, never read back across a
    /// snapshot boundary).
    ///
    /// # Errors
    ///
    /// [`FpError::Mismatch`] if either stored fingerprint does not match the target
    /// match's corresponding loaded character.
    pub fn apply_to(&self, m: &mut Match) -> FpResult<()> {
        self.check_fingerprints(m)?;
        self.p1.apply_to(&mut m.p1);
        self.p2.apply_to(&mut m.p2);
        m.bounds = self.bounds;
        m.round_state = self.round_state;
        m.timer = self.timer;
        m.phase_timer = self.phase_timer;
        m.winner = self.winner;
        m.rounds_to_win = self.rounds_to_win;
        m.p1_round_wins = self.p1_round_wins;
        m.p2_round_wins = self.p2_round_wins;
        m.round_number = self.round_number;
        m.match_state = self.match_state;
        m.match_winner = self.match_winner;
        m.p1_reset = self.p1_reset;
        m.p2_reset = self.p2_reset;
        m.round_frames = self.round_frames;
        m.game_time = self.game_time;
        m.freeze = self.freeze;
        m.effects = self.effects.clone();
        // Per-tick scratch: cleared so a restored match does not surface a stale
        // prior tick's PlaySnd requests before its next tick repopulates them.
        m.p1_sound_requests.clear();
        m.p2_sound_requests.clear();
        Ok(())
    }

    /// Validates that this snapshot's recorded identity fingerprints match the
    /// target match's loaded characters (#38).
    ///
    /// The snapshot carries only runtime state; restoring it into a match built
    /// from different `.def`s would corrupt the simulation. This compares each
    /// stored [`CharacterFingerprint`] against the live match's corresponding
    /// loaded character.
    ///
    /// # Errors
    ///
    /// [`FpError::Mismatch`] (with the offending side and both fingerprints) if
    /// either player's fingerprint differs. Returns `Ok(())` when both match.
    pub fn check_fingerprints(&self, m: &Match) -> FpResult<()> {
        let (live_p1, live_p2) = m.character_fingerprints();
        if self.p1_fingerprint != live_p1 {
            return Err(FpError::Mismatch(format!(
                "P1 character fingerprint differs (snapshot {:#018x}, match {:#018x}); \
                 the snapshot was taken from a different character",
                self.p1_fingerprint.0, live_p1.0
            )));
        }
        if self.p2_fingerprint != live_p2 {
            return Err(FpError::Mismatch(format!(
                "P2 character fingerprint differs (snapshot {:#018x}, match {:#018x}); \
                 the snapshot was taken from a different character",
                self.p2_fingerprint.0, live_p2.0
            )));
        }
        Ok(())
    }
}

impl Match {
    /// Serializes this match's mutable runtime state into a compact binary blob
    /// (the rollback / save-state primitive, #38).
    ///
    /// Captures only the runtime (round flow, freeze, effects, game clock, and
    /// each player's character + input-recognition state) — never the loaded
    /// static data, which is reloaded from the `.def`. Encoded with [`bincode`]
    /// (deterministic, little-endian). Returns a recoverable [`FpError`] if
    /// encoding fails (it does not for the plain-data snapshot, but the contract
    /// never panics).
    ///
    /// Restore the blob into a freshly-loaded match (built from the same two
    /// characters) with [`Match::restore_snapshot`].
    pub fn snapshot(&self) -> FpResult<Vec<u8>> {
        let snap = MatchSnapshot::capture(self);
        bincode::serialize(&snap).map_err(|e| {
            FpError::Other(format!("match snapshot serialize failed: {e}"))
        })
    }

    /// Captures this match's runtime state as a typed [`MatchSnapshot`]
    /// (in-memory, no encoding).
    ///
    /// Useful for rollback or equality checks that stay in-process and want to
    /// avoid the bincode round-trip. For a serializable blob use
    /// [`Match::snapshot`].
    #[must_use]
    pub fn snapshot_state(&self) -> MatchSnapshot {
        MatchSnapshot::capture(self)
    }

    /// Restores a previously-[`snapshot`](Match::snapshot)ed runtime state into
    /// this already-loaded match (#38).
    ///
    /// The match must have been built from the **same two characters** (same
    /// `.def`s) as the one the snapshot was taken from: only the mutable runtime
    /// is overwritten; the loaded assets / constants / compiled commands stay as
    /// they are. A truncated or malformed `bytes` blob returns a recoverable
    /// [`FpError`] (never a panic).
    ///
    /// # Errors
    ///
    /// - [`FpError::Other`] if `bytes` is truncated or malformed (decode failure).
    /// - [`FpError::Mismatch`] if the decoded snapshot's per-character identity
    ///   fingerprints do not match this match's loaded characters (#38) — the match
    ///   is left **unchanged** in that case.
    pub fn restore_snapshot(&mut self, bytes: &[u8]) -> FpResult<()> {
        let snap: MatchSnapshot = bincode::deserialize(bytes).map_err(|e| {
            FpError::Other(format!("match snapshot deserialize failed: {e}"))
        })?;
        snap.apply_to(self)
    }

    /// Restores a typed [`MatchSnapshot`] directly (in-memory, no decoding).
    ///
    /// The in-process counterpart of [`Match::restore_snapshot`]; pairs with
    /// [`Match::snapshot_state`].
    ///
    /// # Errors
    ///
    /// [`FpError::Mismatch`] if `snap`'s per-character identity fingerprints do not
    /// match this match's loaded characters (#38); the match is left unchanged.
    pub fn restore_snapshot_state(&mut self, snap: &MatchSnapshot) -> FpResult<()> {
        snap.apply_to(self)
    }
}

/// Documents the runtime field set covered by the snapshot. A genuine compile
/// check that `FreezeExempt` is reachable from this module (so its serde derive
/// is exercised), without adding a runtime cost.
const _: fn() = || {
    let _ = FreezeExempt::None;
};
