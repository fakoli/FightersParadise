//! # Frame-data computation — startup / active / recovery (+ frame advantage)
//!
//! A player-facing *legibility* helper (task T065, feature F026): given an
//! animation action (an [`AnimAction`] from the character's `.air` table), it
//! decomposes the action into the three numbers a fighting-game player counts by
//! hand — **startup**, **active**, and **recovery** — all in 60Hz ticks.
//!
//! The decomposition is purely static (it reads only AIR element durations and
//! whether each element carries an attack box, [`AnimFrame::clsn1`]):
//!
//! ```text
//! first_active = first element that has a Clsn1 (attack) box
//! startup  = sum of element durations before first_active
//! active   = sum of the contiguous run of Clsn1-bearing element durations
//!            starting at first_active
//! recovery = total - startup - active   (the tail after the active run)
//! ```
//!
//! The dynamic half — **on-block / on-hit frame advantage** — is computed at the
//! moment a move connects from the defender's induced stun and the attacker's
//! remaining recovery; see [`frame_advantage`]. It is surfaced to the player via
//! a [`TickReport`](crate::executor::TickReport) field rather than computed here,
//! because it depends on live combat state, not just the AIR data.
//!
//! ## Error philosophy
//!
//! Frame data is a *readout*, never a control input, so it follows the project's
//! never-crash rule strictly: anything that cannot be counted to a single honest
//! number yields [`None`] (the UI shows `—`), never a wrong number and never a
//! panic. The two cases that cannot be counted are:
//!
//! - an action with **no attack frame at all** (no element has a Clsn1) — there is
//!   no startup/active/recovery to report;
//! - an action that **holds forever** (`time = -1`, MUGEN's infinite-hold) inside
//!   the span being summed — the duration is unbounded, so the count is undefined.
//!
//! Looping / variable-cancel states are inherently dynamic and likewise out of
//! scope for a static count; the caller is expected to only request frame data
//! for a discrete attack action.

use fp_formats::air::{AnimAction, AnimFrame};

/// Static frame data for one attack action, in 60Hz ticks.
///
/// Produced by [`MoveFrameData::compute`] from an [`AnimAction`]. All four fields
/// are in game ticks (1/60s). The invariant `startup + active + recovery == total`
/// always holds.
///
/// See the [module docs](self) for the exact decomposition and the cases that
/// make computation return [`None`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoveFrameData {
    /// Frames before the first attack box (Clsn1) appears.
    pub startup: i32,
    /// Frames the (contiguous) attack box is present — the active window.
    pub active: i32,
    /// Frames after the active window until the action ends (return to neutral).
    pub recovery: i32,
    /// Total duration of the action (`startup + active + recovery`).
    pub total: i32,
}

impl MoveFrameData {
    /// Computes startup / active / recovery / total for an attack action.
    ///
    /// Returns [`None`] (display `—`) when the action carries no attack frame at
    /// all, or when any element inside the summed span holds forever (`ticks == -1`),
    /// because neither case has a single honest frame count. Never panics.
    ///
    /// # Examples
    ///
    /// ```
    /// use fp_character::framedata::MoveFrameData;
    /// use fp_formats::air::{AnimAction, AnimFrame};
    /// use fp_core::Rect;
    ///
    /// // 3 startup frames, 2 active frames (one element holding 2 ticks with a
    /// // Clsn1), 4 recovery frames.
    /// let action = AnimAction {
    ///     action_number: 200,
    ///     loopstart: 0,
    ///     frames: vec![
    ///         AnimFrame { ticks: 3, ..Default::default() },
    ///         AnimFrame { ticks: 2, clsn1: vec![Rect::default()], ..Default::default() },
    ///         AnimFrame { ticks: 4, ..Default::default() },
    ///     ],
    /// };
    /// let fd = MoveFrameData::compute(&action).unwrap();
    /// assert_eq!((fd.startup, fd.active, fd.recovery, fd.total), (3, 2, 4, 9));
    /// ```
    #[must_use]
    pub fn compute(action: &AnimAction) -> Option<MoveFrameData> {
        let frames = &action.frames;

        // First element that carries an attack box. No Clsn1 anywhere => this is
        // not a countable attack action.
        let first_active = frames.iter().position(AnimFrame::is_attack)?;

        // Startup: the sum of durations strictly before the first attack frame.
        let startup = sum_ticks(&frames[..first_active])?;

        // Active: the contiguous run of attack-box-bearing frames starting at
        // `first_active`. MUGEN active frames are typically contiguous; once the
        // attack box drops away the move is in recovery even if a later frame
        // re-arms a box (a separate hit, not part of this active window).
        let active_end = frames[first_active..]
            .iter()
            .position(|f| !f.is_attack())
            .map_or(frames.len(), |off| first_active + off);
        let active = sum_ticks(&frames[first_active..active_end])?;

        // Recovery: everything after the active run.
        let recovery = sum_ticks(&frames[active_end..])?;

        Some(MoveFrameData {
            startup,
            active,
            recovery,
            total: startup + active + recovery,
        })
    }
}

/// Sums the durations of a slice of frames, in ticks.
///
/// Returns [`None`] if any frame holds forever (`ticks == -1`): an infinite hold
/// inside the span makes the count undefined. Negative durations other than the
/// sentinel are treated as `0` (defensive — AIR durations are non-negative apart
/// from the `-1` hold sentinel).
fn sum_ticks(frames: &[AnimFrame]) -> Option<i32> {
    let mut total = 0i32;
    for f in frames {
        if f.ticks < 0 {
            // -1 (or any negative) = infinite hold: span duration is undefined.
            return None;
        }
        total = total.saturating_add(f.ticks);
    }
    Some(total)
}

/// On-block / on-hit frame advantage at the moment a move connects, in 60Hz ticks.
///
/// Advantage is the defender's induced stun (block-stun on a guarded hit,
/// hit-stun on a clean hit) minus the attacker's frames-until-actionable (the
/// frames the attacker still owes before it can act — typically the move's
/// remaining recovery). A positive number means the attacker recovers first
/// (advantage); a negative number means the defender recovers first
/// (disadvantage).
///
/// ```text
/// advantage = defender_stun - attacker_frames_until_actionable
/// ```
///
/// This is intentionally a tiny pure function so the engine can call it at
/// contact time with whatever stun/recovery numbers it already tracks and stash
/// the result on the [`TickReport`](crate::executor::TickReport) for the readout.
/// Never panics; saturating arithmetic guards against overflow.
#[must_use]
pub fn frame_advantage(defender_stun: i32, attacker_frames_until_actionable: i32) -> i32 {
    defender_stun.saturating_sub(attacker_frames_until_actionable)
}

#[cfg(test)]
#[allow(non_snake_case)]
mod MoveFrameData_compute {
    use super::*;
    use fp_core::Rect;

    /// Helper: a plain non-attack frame of `ticks` duration.
    fn frame(ticks: i32) -> AnimFrame {
        AnimFrame {
            ticks,
            ..Default::default()
        }
    }

    /// Helper: an attack frame (carries a Clsn1) of `ticks` duration.
    fn atk(ticks: i32) -> AnimFrame {
        AnimFrame {
            ticks,
            clsn1: vec![Rect::default()],
            ..Default::default()
        }
    }

    fn action(frames: Vec<AnimFrame>) -> AnimAction {
        AnimAction {
            action_number: 200,
            loopstart: 0,
            frames,
        }
    }

    #[test]
    fn move_frame_data_compute_startup_active_recovery() {
        // 3 + 5 startup, 4 active, 6 + 2 recovery.
        let a = action(vec![frame(3), frame(5), atk(4), frame(6), frame(2)]);
        let fd = MoveFrameData::compute(&a).expect("countable");
        assert_eq!(fd.startup, 8);
        assert_eq!(fd.active, 4);
        assert_eq!(fd.recovery, 8);
        assert_eq!(fd.total, 20);
        assert_eq!(fd.startup + fd.active + fd.recovery, fd.total);
    }

    #[test]
    fn move_frame_data_compute_startup_only_no_clsn1_is_none() {
        // No attack box anywhere => not a countable attack => None ("—").
        let a = action(vec![frame(2), frame(2), frame(2)]);
        assert_eq!(MoveFrameData::compute(&a), None);
    }

    #[test]
    fn move_frame_data_compute_startup_plus_active_no_recovery() {
        // Active run runs to the end of the action: recovery is 0.
        let a = action(vec![frame(7), atk(3), atk(2)]);
        let fd = MoveFrameData::compute(&a).expect("countable");
        assert_eq!(fd.startup, 7);
        assert_eq!(fd.active, 5);
        assert_eq!(fd.recovery, 0);
        assert_eq!(fd.total, 12);
    }

    #[test]
    fn move_frame_data_compute_active_plus_recovery_no_startup() {
        // Attack box on the very first frame: zero startup.
        let a = action(vec![atk(4), frame(5)]);
        let fd = MoveFrameData::compute(&a).expect("countable");
        assert_eq!(fd.startup, 0);
        assert_eq!(fd.active, 4);
        assert_eq!(fd.recovery, 5);
        assert_eq!(fd.total, 9);
    }

    #[test]
    fn move_frame_data_compute_contiguous_active_only() {
        // A later re-armed Clsn1 (a second hit) is NOT folded into the first
        // active window; it counts as recovery for the first window.
        let a = action(vec![frame(2), atk(3), frame(4), atk(3)]);
        let fd = MoveFrameData::compute(&a).expect("countable");
        assert_eq!(fd.startup, 2);
        assert_eq!(fd.active, 3, "only the first contiguous active run");
        assert_eq!(fd.recovery, 7, "later re-armed box counts as recovery");
        assert_eq!(fd.total, 12);
    }

    #[test]
    fn move_frame_data_compute_infinite_hold_in_startup_is_none() {
        // time = -1 (hold forever) anywhere in the summed span => undefined => None.
        let a = action(vec![frame(-1), atk(3), frame(4)]);
        assert_eq!(MoveFrameData::compute(&a), None);
    }

    #[test]
    fn move_frame_data_compute_infinite_hold_in_active_is_none() {
        let a = action(vec![frame(3), atk(-1), frame(4)]);
        assert_eq!(MoveFrameData::compute(&a), None);
    }

    #[test]
    fn move_frame_data_compute_infinite_hold_in_recovery_is_none() {
        let a = action(vec![frame(3), atk(2), frame(-1)]);
        assert_eq!(MoveFrameData::compute(&a), None);
    }

    #[test]
    fn move_frame_data_compute_empty_action_is_none() {
        let a = action(vec![]);
        assert_eq!(MoveFrameData::compute(&a), None);
    }

    #[test]
    fn move_frame_data_compute_zero_duration_frames_count_as_zero() {
        // A 0-tick startup frame contributes nothing but does not break the count.
        let a = action(vec![frame(0), atk(2), frame(3)]);
        let fd = MoveFrameData::compute(&a).expect("countable");
        assert_eq!(fd.startup, 0);
        assert_eq!(fd.active, 2);
        assert_eq!(fd.recovery, 3);
    }

    #[test]
    fn frame_advantage_on_block_disadvantage() {
        // Defender held in 12 frames of blockstun, attacker owes 17 frames of
        // recovery => -5 (disadvantage on block).
        assert_eq!(frame_advantage(12, 17), -5);
    }

    #[test]
    fn frame_advantage_on_hit_plus() {
        // Defender held in 20 frames of hitstun, attacker owes 17 => +3.
        assert_eq!(frame_advantage(20, 17), 3);
    }

    #[test]
    fn frame_advantage_even() {
        assert_eq!(frame_advantage(15, 15), 0);
    }

    #[test]
    fn frame_advantage_saturates_no_overflow() {
        // Defensive: extreme inputs must not panic on overflow.
        assert_eq!(frame_advantage(i32::MIN, i32::MAX), i32::MIN);
        assert_eq!(frame_advantage(i32::MAX, i32::MIN), i32::MAX);
    }

    /// Hand-counted reference against the shipped clean-room `trainingdummy`.
    ///
    /// `trainingdummy.air` action 200 is the deterministic basic attack:
    /// frame 1 = 3-tick windup (no Clsn1), frame 2 = 4-tick active (one Clsn1
    /// attack box), frame 3 = 5-tick recovery. So the hand count is
    /// startup=3, active=4, recovery=5, total=12 — and frame data must match
    /// within ±0 frames (T065 acceptance criterion). The trainingdummy assets
    /// ship in-repo (not under `test-assets/`), so this test is NOT asset-gated.
    #[test]
    fn move_frame_data_compute_matches_trainingdummy_action_200_handcount() {
        use fp_formats::air::AirFile;
        use std::path::Path;

        // CARGO_MANIFEST_DIR is crates/fp-character; the asset is at the
        // workspace root under assets/trainingdummy.
        let air_path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/trainingdummy/trainingdummy.air");
        let air = AirFile::load(&air_path).expect("shipped trainingdummy.air must parse");
        let action = air
            .actions
            .get(&200)
            .expect("trainingdummy action 200 (basic attack) must exist");

        let fd = MoveFrameData::compute(action).expect("action 200 is a countable attack");
        assert_eq!(fd.startup, 3, "hand-counted startup");
        assert_eq!(fd.active, 4, "hand-counted active");
        assert_eq!(fd.recovery, 5, "hand-counted recovery");
        assert_eq!(fd.total, 12, "hand-counted total");
    }
}
