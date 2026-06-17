//! Training-mode dummy control (T067).
//!
//! In the Lab (training mode) player 2 is a *dummy* whose behaviour the player
//! sets from a quick-keys / submenu: stand still, crouch, jump on a loop, guard
//! everything, guard after the first hit lands, or hand back to the baseline CPU
//! AI. This module models that as a [`DummyMode`] plus a pure per-tick translator
//! ([`dummy_input`]) that emits a [`MatchInput`] — the *same* absolute-direction
//! input a keyboard frame produces — so the dummy drives the fighter through the
//! ordinary [`Match::tick`](crate::Match::tick) path with **no executor change**
//! (it mirrors the existing tests that drive `holdup`/`fwd`).
//!
//! ## Crossup-correct blocking
//!
//! A guarding dummy must hold *away from the opponent*, and that direction flips
//! the instant the opponent crosses to the dummy's other side (a jump-over /
//! cross-up). Because [`MatchInput`] carries **absolute** screen directions and
//! the engine resolves facing at match time (in `feed_input`, where `holding_back`
//! is derived facing-relative), the block rule here is simply "hold the absolute
//! direction *opposite* the opponent". The translator recomputes that from the
//! live opponent position every tick, so a dummy still guards correctly after the
//! attacker jumps over it.

use crate::MatchInput;

/// How a training-mode dummy (player 2 in the Lab) behaves each tick.
///
/// Set from the training quick-keys / submenu. Every mode but [`DummyMode::Cpu`]
/// is a fixed held-state the engine reproduces deterministically; `Cpu` hands the
/// slot back to the baseline [`fp_input::CpuAi`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DummyMode {
    /// Stand idle — no direction held, no button pressed (the default).
    #[default]
    Stand,
    /// Crouch — hold down every tick.
    Crouch,
    /// Jump on a loop — pulse "up" so the fighter jumps, lands, and jumps again.
    JumpLoop,
    /// Guard everything — hold away from the opponent every tick (crossup-correct).
    BlockAll,
    /// Guard only after the first hit of a combo lands — stand until hit, then
    /// hold away from the opponent (so the first hit lands, subsequent hits are
    /// blocked). The "has been hit" latch is owned by the caller and passed in.
    BlockAfterFirst,
    /// Hand the slot back to the baseline CPU AI (no dummy input; the caller
    /// substitutes the AI's frame instead).
    Cpu,
}

impl DummyMode {
    /// Whether this mode is driven by the baseline CPU AI rather than a fixed
    /// dummy held-state. When `true` the caller should feed the slot from its
    /// [`fp_input::CpuAi`] instead of calling [`dummy_input`].
    #[must_use]
    pub fn is_cpu(self) -> bool {
        matches!(self, DummyMode::Cpu)
    }

    /// A short human-readable label for a training HUD / menu.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            DummyMode::Stand => "STAND",
            DummyMode::Crouch => "CROUCH",
            DummyMode::JumpLoop => "JUMP",
            DummyMode::BlockAll => "BLOCK ALL",
            DummyMode::BlockAfterFirst => "BLOCK AFTER 1ST",
            DummyMode::Cpu => "CPU",
        }
    }

    /// The next mode in a fixed cycle, for a single quick-key that rotates through
    /// the stances (`Stand → Crouch → JumpLoop → BlockAll → BlockAfterFirst → Cpu
    /// → Stand`).
    #[must_use]
    pub fn cycle_next(self) -> Self {
        match self {
            DummyMode::Stand => DummyMode::Crouch,
            DummyMode::Crouch => DummyMode::JumpLoop,
            DummyMode::JumpLoop => DummyMode::BlockAll,
            DummyMode::BlockAll => DummyMode::BlockAfterFirst,
            DummyMode::BlockAfterFirst => DummyMode::Cpu,
            DummyMode::Cpu => DummyMode::Stand,
        }
    }
}

/// The jump cadence for [`DummyMode::JumpLoop`]: hold "up" for the first
/// [`JUMP_HOLD_TICKS`] of every [`JUMP_PERIOD_TICKS`]-tick cycle, release for the
/// rest so the fighter lands (and its `up`-press-edge re-arms) before the next
/// jump. A press-edge command (`holdup`/`U`) only fires on the frame "up" goes
/// from released → held, so a brief release between bursts is what produces a
/// *repeating* jump rather than a single one.
const JUMP_HOLD_TICKS: u64 = 4;
/// The full jump-loop period in ticks (hold + release). ~0.5s at 60Hz.
const JUMP_PERIOD_TICKS: u64 = 30;

/// Translates a [`DummyMode`] into the [`MatchInput`] the dummy should feed this
/// tick, given the live opponent side and combat/clock context.
///
/// - `opponent_on_right`: `true` when the opponent is to the dummy's screen-right
///   (so guarding means holding screen-**left**, away from it). Recompute this
///   every tick from the live positions so a cross-up still blocks.
/// - `was_hit`: whether the dummy has already taken a hit this combo — only
///   consulted by [`DummyMode::BlockAfterFirst`]. The caller owns this latch
///   (set it when the dummy takes a hit, clear it on reset / round start).
/// - `tick`: a monotonic frame counter, used only to phase [`DummyMode::JumpLoop`].
///
/// [`DummyMode::Cpu`] returns [`MatchInput::none`] — the caller substitutes the
/// CPU AI's frame instead of using this output (see [`DummyMode::is_cpu`]).
#[must_use]
pub fn dummy_input(
    mode: DummyMode,
    opponent_on_right: bool,
    was_hit: bool,
    tick: u64,
) -> MatchInput {
    let mut input = MatchInput::none();
    match mode {
        DummyMode::Stand | DummyMode::Cpu => {}
        DummyMode::Crouch => input.down = true,
        DummyMode::JumpLoop => {
            // Hold "up" for the first part of each period, release for the rest so
            // the press edge re-arms and the jump repeats.
            if tick % JUMP_PERIOD_TICKS < JUMP_HOLD_TICKS {
                input.up = true;
            }
        }
        DummyMode::BlockAll => set_block(&mut input, opponent_on_right),
        DummyMode::BlockAfterFirst => {
            if was_hit {
                set_block(&mut input, opponent_on_right);
            }
        }
    }
    input
}

/// Holds the absolute screen direction *away from* the opponent (the guard
/// direction). Crossup-correct: the side flips with `opponent_on_right`.
fn set_block(input: &mut MatchInput, opponent_on_right: bool) {
    if opponent_on_right {
        // Opponent is to the right ⇒ guard by holding left.
        input.left = true;
    } else {
        input.right = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stand_and_cpu_emit_nothing() {
        assert_eq!(
            dummy_input(DummyMode::Stand, true, false, 0),
            MatchInput::none()
        );
        assert_eq!(
            dummy_input(DummyMode::Cpu, true, true, 7),
            MatchInput::none()
        );
    }

    #[test]
    fn crouch_holds_down() {
        let i = dummy_input(DummyMode::Crouch, true, false, 0);
        assert!(i.down, "crouch holds down");
        assert!(!i.up && !i.left && !i.right, "crouch holds only down");
    }

    #[test]
    fn jumploop_pulses_up_then_releases() {
        // Held at the start of a period, released later — a repeating cadence.
        assert!(
            dummy_input(DummyMode::JumpLoop, true, false, 0).up,
            "up held at period start"
        );
        assert!(
            !dummy_input(DummyMode::JumpLoop, true, false, JUMP_HOLD_TICKS).up,
            "up released after the hold window so the press edge re-arms"
        );
        assert!(
            dummy_input(DummyMode::JumpLoop, true, false, JUMP_PERIOD_TICKS).up,
            "up held again at the next period start"
        );
    }

    #[test]
    fn blockall_holds_away_from_opponent() {
        // Opponent on the right ⇒ guard left.
        let right = dummy_input(DummyMode::BlockAll, true, false, 0);
        assert!(
            right.left && !right.right,
            "guard left when opponent is right"
        );
        // Opponent on the left ⇒ guard right (crossup flips the direction).
        let left = dummy_input(DummyMode::BlockAll, false, false, 0);
        assert!(
            left.right && !left.left,
            "guard right when opponent is left"
        );
    }

    #[test]
    fn blockall_is_crossup_correct_each_tick() {
        // A jump-over flips opponent_on_right tick-to-tick; the guard direction
        // must follow it without any latched state.
        let before = dummy_input(DummyMode::BlockAll, true, false, 10);
        let after = dummy_input(DummyMode::BlockAll, false, false, 11);
        assert!(before.left, "guarding left before the cross-up");
        assert!(after.right, "guarding right after the cross-up");
    }

    #[test]
    fn blockafterfirst_only_guards_once_hit() {
        // Not yet hit: stands (lets the first hit land).
        let pre = dummy_input(DummyMode::BlockAfterFirst, true, false, 0);
        assert_eq!(pre, MatchInput::none(), "stands until the first hit lands");
        // After being hit: guards away from the opponent.
        let post = dummy_input(DummyMode::BlockAfterFirst, true, true, 1);
        assert!(
            post.left,
            "guards left after the first hit (opponent right)"
        );
        // Crossup-correct after the hit too.
        let post_crossup = dummy_input(DummyMode::BlockAfterFirst, false, true, 2);
        assert!(post_crossup.right, "guards right after a cross-up post-hit");
    }

    #[test]
    fn mode_cycle_visits_every_stance_and_wraps() {
        let mut m = DummyMode::Stand;
        let mut seen = vec![m];
        for _ in 0..6 {
            m = m.cycle_next();
            seen.push(m);
        }
        assert_eq!(seen.first(), seen.last(), "cycle wraps back to Stand");
        assert!(seen.contains(&DummyMode::BlockAll));
        assert!(seen.contains(&DummyMode::BlockAfterFirst));
        assert!(seen.contains(&DummyMode::Cpu));
    }

    #[test]
    fn is_cpu_only_for_cpu_mode() {
        assert!(DummyMode::Cpu.is_cpu());
        for m in [
            DummyMode::Stand,
            DummyMode::Crouch,
            DummyMode::JumpLoop,
            DummyMode::BlockAll,
            DummyMode::BlockAfterFirst,
        ] {
            assert!(!m.is_cpu(), "{m:?} is not CPU");
        }
    }
}
