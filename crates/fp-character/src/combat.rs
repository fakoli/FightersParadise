//! Hit detection and application between two [`Character`]s (task 6.3b).
//!
//! This module wires the **pure** combat primitives of [`fp_combat`] to two
//! live [`Character`]s. It is the bridge between geometry/decision (which
//! `fp-combat` owns and tests in isolation) and mutation (which belongs to a
//! `Character`):
//!
//! 1. **Detect** — pull the attacker's current AIR-frame `Clsn1` (attack) boxes
//!    and the defender's current `Clsn2` (hurt) boxes and test them with
//!    [`fp_combat::detect_hit`], positioned by each character's `pos`/`facing`.
//! 2. **Resolve** — build a [`fp_combat::DefenderState`] from the live defender
//!    and call the pure [`fp_combat::resolve_hit`] to get a
//!    [`fp_combat::HitOutcome`] *recipe*.
//! 3. **Apply** — mutate the defender (life, velocity, get-hit state,
//!    [`GetHitVars`](crate::GetHitVars), hit-pause/shake) and the attacker
//!    (hit-pause, move-connection flags) from that recipe.
//!
//! The single public entry point is [`resolve_attack`], which performs one tick
//! of attacker → defender combat. It is `hitonce`-aware (a move connects at most
//! once until a fresh `HitDef` resets it) and never panics: a missing
//! animation, an empty box set, or an absent `HitDef` all degrade to "no hit".
//!
//! Knockback orientation follows the contract in [`fp_combat::HitOutcome`]: the
//! resolved `knockback` is **attacker-facing-relative** (positive `x` = away
//! from the attacker in its forward direction). [`resolve_attack`] mirrors it by
//! the **attacker's** facing before storing it on the defender, so the defender
//! is always pushed away from the attacker regardless of which way either faces.

use fp_combat::{detect_hit, resolve_hit, ClsnBox, ClsnFacing, DefenderState, HitResult, Stance};
use fp_core::{Rect, Vec2};
use fp_formats::air::{AirFile, AnimAction};

use crate::{Character, Facing, StateType};

/// What [`resolve_attack`] decided and applied for one attacker → defender tick.
///
/// Returned by value so a caller (the round coordinator, a test) can react to
/// the outcome — play a spark, update a combo counter — without re-deriving it.
/// A [`None`] return from [`resolve_attack`] means *no contact happened* (no
/// active `HitDef`, no box overlap, the move already connected under `hitonce`,
/// or the hitflag excluded the defender's state); a [`Some`] value is only ever
/// produced for an effective [`HitResult::Hit`] or [`HitResult::Guard`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AttackResolution {
    /// Whether the attack landed cleanly or was guarded. (Never
    /// [`HitResult::Miss`]: a miss is reported as [`None`] from
    /// [`resolve_attack`].)
    pub result: HitResult,
    /// Damage actually subtracted from the defender's life (the hit or guard
    /// value, after life clamping is applied to the field on the character).
    pub damage: i32,
    /// World-space knockback velocity written onto the defender, already mirrored
    /// by the attacker's facing (points away from the attacker).
    pub knockback: Vec2<f32>,
    /// The state number the defender was sent into (the get-hit / guard state).
    pub defender_state: i32,
    /// Hit-pause ticks set on the attacker (`pausetime.p1`).
    pub attacker_hitpause: i32,
    /// Hit-pause ticks set on the defender (`pausetime.p2`).
    pub defender_hitpause: i32,
    /// The attacker's `HitDef` sound to play for this connection: the `hitsound`
    /// on a clean [`HitResult::Hit`], the `guardsound` on a [`HitResult::Guard`].
    /// [`None`] when the relevant sound is unset on the `HitDef`. (A miss never
    /// produces an [`AttackResolution`], so this is keyed off `result`.)
    pub hit_sound: Option<fp_combat::SoundId>,
}

/// Performs one tick of attacker → defender combat: detect, resolve, and apply.
///
/// Given the two live characters and each one's loaded [`AirFile`] (for the
/// current-frame collision boxes), this:
///
/// 1. Returns [`None`] immediately if the attacker has no
///    [`active_hitdef`](Character::active_hitdef), or if that move has already
///    connected (the `hitonce` / `numhits = 1` rule — tracked via
///    [`Character::move_connect`]).
/// 2. Extracts the attacker's current AIR-frame `Clsn1` boxes and the defender's
///    current `Clsn2` boxes and tests them with [`fp_combat::detect_hit`],
///    positioned by `pos`/`facing`. No overlap → [`None`].
/// 3. Builds a [`fp_combat::DefenderState`] from the defender (stance and
///    airborne-ness from [`StateType`], `holding_back` from `holding_back`) and
///    calls [`fp_combat::resolve_hit`]. A [`HitResult::Miss`] → [`None`].
/// 4. **Applies** the [`fp_combat::HitOutcome`]:
///    - `defender.life -= damage`, clamped to `>= 0`;
///    - `defender.vel` = the outcome knockback **mirrored by the attacker's
///      facing** (so the defender is pushed away from the attacker);
///    - the defender is sent into the outcome's get-hit state via
///      [`Character::change_state`];
///    - the defender's [`GetHitVars`](crate::GetHitVars) are populated from the
///      outcome;
///    - hit-pause is set on the attacker (`pausetime.p1`) and the defender
///      (`pausetime.p2`), and the defender's shake timer from `shaketime`;
///    - the attacker's [`move_connect`](Character::move_connect) records the
///      hit/guard (drives `MoveHit`/`MoveGuarded`/`MoveContact` and `hitonce`).
///
/// # Arguments
///
/// - `attacker` / `attacker_air` — the attacking character and its animations.
/// - `defender` / `defender_air` — the defending character and its animations.
/// - `defender_states` — the defender's compiled state graph, used to apply the
///   get-hit statedef's entry parameters when the defender changes state. Pass
///   [`LoadedCharacter::states`](crate::LoadedCharacter::states); an empty map
///   is fine (the cursor still updates, no entry params apply).
///
/// `holding_back` on the defender is read from
/// [`Character::holding_back`]; callers that have the defender's input wired
/// should set it each tick before calling. With no input wired it is `false`
/// (the attack lands rather than being blocked), matching the task's "else false
/// for now" rule.
///
/// Returns `Some(`[`AttackResolution`]`)` describing the applied effect on a hit
/// or guard, or [`None`] when nothing connected. **Never panics**: every missing
/// frame, empty box set, or unknown state degrades to a safe no-op.
pub fn resolve_attack(
    attacker: &mut Character,
    attacker_air: &AirFile,
    defender: &mut Character,
    defender_air: &AirFile,
    defender_states: &std::collections::HashMap<i32, crate::CompiledState>,
) -> Option<AttackResolution> {
    // (1) An active HitDef that has not already connected this move (hitonce).
    let hitdef = attacker.active_hitdef?;
    if attacker.move_connect.contact() {
        // The move already connected; `hitonce`/`numhits = 1` forbids a second
        // hit until a fresh HitDef resets `move_connect`.
        return None;
    }

    // (2) Detect: attacker Clsn1 vs defender Clsn2 at their world positions.
    let clsn1 = current_frame_clsn1(attacker_air, attacker.anim, attacker.anim_elem);
    let clsn2 = current_frame_clsn2(defender_air, defender.anim, defender.anim_elem);
    if clsn1.is_empty() || clsn2.is_empty() {
        // No attack or hurt boxes on the current frame: cannot connect.
        return None;
    }
    let contact = detect_hit(
        &clsn1,
        attacker.pos,
        to_clsn_facing(attacker.facing),
        &clsn2,
        defender.pos,
        to_clsn_facing(defender.facing),
    );
    if !contact {
        return None;
    }

    // (3) Resolve against the defender's situation (pure logic in fp-combat).
    let defender_state = DefenderState::new(
        stance_of(defender),
        defender.holding_back,
        defender.state_type == StateType::Air,
    );
    let outcome = resolve_hit(&hitdef, defender_state);
    if !outcome.is_effective() {
        // Miss: the hitflag excluded the defender's stance. No effect, and the
        // move is NOT marked as connected (it did not actually touch).
        return None;
    }

    // (4) Apply the outcome to the defender.
    //
    // Knockback is attacker-facing-relative; mirror its x by the ATTACKER's
    // facing so a positive (away) x pushes the defender away from the attacker
    // regardless of which way the attacker faces. Y is never mirrored.
    let attacker_sign = attacker.facing.sign() as f32;
    let knockback = Vec2::new(outcome.knockback.x * attacker_sign, outcome.knockback.y);

    defender.life = (defender.life - outcome.damage).max(0);
    defender.vel = knockback;

    // Populate the defender's GetHitVars from the resolved outcome before the
    // ChangeState, so the get-hit state's entry expressions can read them.
    let guarded = matches!(outcome.result, HitResult::Guard);
    let gh = &mut defender.get_hit_vars;
    gh.xvel = knockback.x;
    gh.yvel = knockback.y;
    // On a guard there is no fall/airborne arc (yvel and fall are forced to 0), so
    // GetHitVar(yaccel) should be 0 too; only a true hit carries the gravity arc.
    gh.yaccel = if guarded {
        0.0
    } else {
        defender.constants.movement.yaccel
    };
    gh.damage = outcome.damage;
    gh.hitshaketime = outcome.shaketime;
    gh.hittime = outcome.hittime;
    gh.slidetime = outcome.slidetime;
    gh.ctrltime = outcome.ctrltime;
    gh.fall = i32::from(outcome.fall);
    gh.guarded = i32::from(guarded);
    gh.chainid = hitdef.chainid;
    if outcome.fall {
        gh.yvel = outcome.fall_yvelocity;
    }

    // Send the defender into the get-hit / guard state.
    defender.change_state(defender_states, outcome.gethit_state);

    // Hit-pause / shake on both participants.
    attacker.hitpause = outcome.pausetime;
    defender.hitpause = outcome.shaketime;
    defender.shaketime = outcome.shaketime;

    // Mark the attacker's move as connected (drives MoveHit/MoveGuarded/
    // MoveContact and enforces hitonce on the next call).
    if guarded {
        attacker.move_connect.guarded = true;
    } else {
        attacker.move_connect.hit = true;
    }

    // Pick the impact sound from the attacker's HitDef: the guardsound when the
    // attack was guarded, the hitsound on a clean hit. Either may be `None`.
    let hit_sound = if guarded {
        hitdef.resources.guardsound
    } else {
        hitdef.resources.hitsound
    };

    Some(AttackResolution {
        result: outcome.result,
        damage: outcome.damage,
        knockback,
        defender_state: outcome.gethit_state,
        attacker_hitpause: outcome.pausetime,
        defender_hitpause: outcome.shaketime,
        hit_sound,
    })
}

/// Maps a [`Character`] [`StateType`] to the combat [`Stance`] used for
/// guard/hit flag gating and the stance-based common get-hit state.
///
/// Standing and the catch-all "unchanged" map to [`Stance::Stand`]; crouching to
/// [`Stance::Crouch`]; air to [`Stance::Air`]. Lying maps to [`Stance::Stand`]
/// (MUGEN has no distinct lying hit-flag letter; a downed character is gated as a
/// ground target).
fn stance_of(defender: &Character) -> Stance {
    match defender.state_type {
        StateType::Crouching => Stance::Crouch,
        StateType::Air => Stance::Air,
        StateType::Standing | StateType::Lying | StateType::Unchanged => Stance::Stand,
    }
}

/// Converts a [`Character`] [`Facing`] into the [`ClsnFacing`] that
/// [`fp_combat::detect_hit`] expects (they are distinct types in distinct
/// crates).
fn to_clsn_facing(facing: Facing) -> ClsnFacing {
    match facing {
        Facing::Right => ClsnFacing::Right,
        Facing::Left => ClsnFacing::Left,
    }
}

/// Converts an AIR-frame collision [`Rect`] (top-left + size) into the
/// corner-pair [`ClsnBox`] that the `fp-combat`/`fp-physics` detection path uses.
fn rect_to_clsn(r: &Rect) -> ClsnBox {
    ClsnBox::new(r.x, r.y, r.right(), r.bottom())
}

/// Returns the attacker's `Clsn1` (attack) boxes for the current animation frame
/// as [`ClsnBox`]es, or an empty vector if the action/frame/boxes are absent.
fn current_frame_clsn1(air: &AirFile, anim: i32, elem: i32) -> Vec<ClsnBox> {
    current_frame_clsn(air, anim, elem, FrameBoxes::Attack)
}

/// Returns the defender's `Clsn2` (hurt) boxes for the current animation frame
/// as [`ClsnBox`]es, or an empty vector if the action/frame/boxes are absent.
fn current_frame_clsn2(air: &AirFile, anim: i32, elem: i32) -> Vec<ClsnBox> {
    current_frame_clsn(air, anim, elem, FrameBoxes::Hurt)
}

/// Which collision-box set to pull from a frame.
#[derive(Debug, Clone, Copy)]
enum FrameBoxes {
    /// `Clsn1` — attack boxes.
    Attack,
    /// `Clsn2` — hurt boxes.
    Hurt,
}

/// Shared frame-box extraction: looks up the action, clamps the (zero-based)
/// element index into range, and converts the selected box set. Any missing
/// piece yields an empty vector — never a panic.
fn current_frame_clsn(air: &AirFile, anim: i32, elem: i32, which: FrameBoxes) -> Vec<ClsnBox> {
    let Some(action) = air.action(anim) else {
        return Vec::new();
    };
    let Some(frame) = frame_at(action, elem) else {
        return Vec::new();
    };
    let rects = match which {
        FrameBoxes::Attack => &frame.clsn1,
        FrameBoxes::Hurt => &frame.clsn2,
    };
    rects.iter().map(rect_to_clsn).collect()
}

/// Resolves the frame at a possibly-out-of-range zero-based element index,
/// clamping into `0..frames.len()`. Returns `None` only for an empty action.
fn frame_at(action: &AnimAction, elem: i32) -> Option<&fp_formats::air::AnimFrame> {
    if action.frames.is_empty() {
        return None;
    }
    let max = action.frames.len() - 1;
    let idx = if elem < 0 {
        0
    } else {
        (elem as usize).min(max)
    };
    action.frames.get(idx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Facing, MoveConnect, MoveType, StateType};
    use fp_combat::{Damage, HitDef, HitFlags, HitTimes, PauseTime};
    use fp_formats::air::{AnimAction, AnimFrame, BlendMode};
    use std::collections::HashMap;

    /// Builds a one-action, one-frame AIR file whose single frame carries the
    /// given `Clsn1`/`Clsn2` boxes (as top-left + size [`Rect`]s).
    fn air_with(action: i32, clsn1: Vec<Rect>, clsn2: Vec<Rect>) -> AirFile {
        let frame = AnimFrame {
            sprite: fp_core::SpriteId::new(0, 0),
            offset: Vec2::new(0, 0),
            ticks: 1,
            flip_h: false,
            flip_v: false,
            blend: BlendMode::Normal,
            clsn1,
            clsn2,
        };
        let mut actions = HashMap::new();
        actions.insert(
            action,
            AnimAction {
                action_number: action,
                frames: vec![frame],
                loopstart: 0,
            },
        );
        AirFile { actions }
    }

    /// A HitDef with concrete damage/knockback/stun for the apply tests.
    fn sample_hitdef() -> HitDef {
        HitDef {
            damage: Damage { hit: 30, guard: 5 },
            // Standing-guardable, hits mid/air/fall (so a standing defender that
            // is NOT holding back takes a clean hit).
            guardflag: HitFlags::parse("MA"),
            hitflag: HitFlags::parse("MAF"),
            ground_velocity: Vec2::new(4.0, -3.0),
            air_velocity: Vec2::new(4.0, -6.0),
            guard_velocity: -2.0,
            hittimes: HitTimes {
                ground: 12,
                air: 20,
                guard: 8,
            },
            pausetime: PauseTime { p1: 8, p2: 8 },
            ..HitDef::default()
        }
    }

    /// An attacker at `x = 0` facing right with a punch box reaching to local
    /// x = 55, an active HitDef, and a fresh (un-connected) move.
    fn make_attacker() -> (Character, AirFile) {
        let mut a = Character::new();
        a.pos = Vec2::new(0.0, 0.0);
        a.facing = Facing::Right;
        a.anim = 200;
        a.anim_elem = 0;
        a.move_type = MoveType::Attack;
        a.active_hitdef = Some(sample_hitdef());
        a.move_connect = MoveConnect::default();
        // Clsn1 attack box, top-left + size: x 10..55, y -60..-40.
        let air = air_with(200, vec![Rect::new(10.0, -60.0, 45.0, 20.0)], Vec::new());
        (a, air)
    }

    /// A standing defender at `x = 60` facing left with a hurt box about its
    /// axis (world ~42..78), full life.
    fn make_defender() -> (Character, AirFile) {
        let mut d = Character::new();
        d.pos = Vec2::new(60.0, 0.0);
        d.facing = Facing::Left;
        d.anim = 0;
        d.anim_elem = 0;
        d.life = 1000;
        d.state_type = StateType::Standing;
        // Clsn2 hurt box, top-left + size: x -18..18 about axis, y -70..0.
        let air = air_with(0, Vec::new(), vec![Rect::new(-18.0, -70.0, 36.0, 70.0)]);
        (d, air)
    }

    #[test]
    fn overlapping_active_hitdef_damages_and_knocks_back() {
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states)
            .expect("overlapping attack should connect");

        // Life dropped by the hit damage, clamped at >= 0.
        assert_eq!(res.result, HitResult::Hit);
        assert_eq!(d.life, 1000 - 30);
        assert_eq!(res.damage, 30);

        // Defender entered the standing common get-hit state (5000).
        assert_eq!(d.state_no, 5000);
        assert_eq!(res.defender_state, 5000);

        // Knockback points AWAY from the attacker. Attacker faces right, so the
        // away direction is +x; the defender is to the attacker's right and is
        // pushed further right (positive x).
        assert!(d.vel.x > 0.0, "defender pushed away (+x) from a right-facer");
        assert_eq!(d.vel.x, 4.0);
        assert_eq!(d.vel.y, -3.0);

        // Hit-pause set on both participants.
        assert_eq!(a.hitpause, 8);
        assert_eq!(d.hitpause, 8);
        assert_eq!(d.shaketime, 8);

        // GetHitVars populated.
        assert_eq!(d.get_hit_vars.damage, 30);
        assert_eq!(d.get_hit_vars.hittime, 12);
        assert_eq!(d.get_hit_vars.guarded, 0);
        assert_eq!(d.get_hit_vars.xvel, 4.0);

        // Attacker move flagged connected (MoveHit / MoveContact).
        assert!(a.move_connect.hit);
        assert!(a.move_connect.contact());
    }

    #[test]
    fn knockback_mirrors_attacker_facing_left() {
        // Same geometry but the attacker faces LEFT and sits to the defender's
        // right; the defender must be pushed in -x (away to the left).
        let (mut a, a_air) = make_attacker();
        a.pos = Vec2::new(60.0, 0.0);
        a.facing = Facing::Left;
        let (mut d, d_air) = make_defender();
        d.pos = Vec2::new(0.0, 0.0);
        d.facing = Facing::Right;
        let states = HashMap::new();

        resolve_attack(&mut a, &a_air, &mut d, &d_air, &states)
            .expect("attack should connect when boxes overlap");

        assert!(d.vel.x < 0.0, "defender pushed away (-x) from a left-facer");
        assert_eq!(d.vel.x, -4.0);
    }

    #[test]
    fn non_overlapping_does_nothing() {
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        // Move the defender far out of reach.
        d.pos = Vec2::new(500.0, 0.0);
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states);
        assert!(res.is_none(), "no overlap -> no resolution");
        assert_eq!(d.life, 1000, "life unchanged on a miss");
        assert_eq!(d.vel, Vec2::new(0.0, 0.0), "velocity unchanged");
        assert_eq!(d.state_no, 0, "state unchanged");
        assert!(!a.move_connect.contact(), "move not marked connected");
        assert_eq!(a.hitpause, 0);
    }

    #[test]
    fn blocking_defender_takes_guard_damage() {
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        // Defender holds back (away from the attacker) -> guards. The HitDef's
        // guardflag admits a standing defender (`MA` includes `M` = mid).
        d.holding_back = true;
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states)
            .expect("a guarded attack still resolves (as a guard)");

        assert_eq!(res.result, HitResult::Guard);
        // Guard damage, not hit damage.
        assert_eq!(d.life, 1000 - 5);
        assert_eq!(res.damage, 5);
        // Guard knockback is the (mirrored) guard_velocity, Y zero.
        assert_eq!(d.vel.x, -2.0); // guard_velocity -2 * attacker sign +1
        assert_eq!(d.vel.y, 0.0);
        // GetHitVar(guarded) set.
        assert_eq!(d.get_hit_vars.guarded, 1);
        // Attacker recorded a guard (MoveGuarded / MoveContact, not MoveHit).
        assert!(a.move_connect.guarded);
        assert!(!a.move_connect.hit);
        assert!(a.move_connect.contact());
    }

    #[test]
    fn resolution_carries_hitsound_on_hit_and_guardsound_on_guard() {
        use fp_combat::SoundId;
        let hitsound = SoundId { group: 5, sample: 0, common: false };
        let guardsound = SoundId { group: 6, sample: 1, common: true };

        // ---- Clean hit → resolution carries the hitsound. ----
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.resources.hitsound = Some(hitsound);
            hd.resources.guardsound = Some(guardsound);
        }
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();
        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("clean hit");
        assert_eq!(res.result, HitResult::Hit);
        assert_eq!(res.hit_sound, Some(hitsound), "clean hit uses the hitsound");

        // ---- Guarded → resolution carries the guardsound. ----
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.resources.hitsound = Some(hitsound);
            hd.resources.guardsound = Some(guardsound);
        }
        let (mut d, d_air) = make_defender();
        d.holding_back = true; // guardflag MA admits a standing block
        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("guarded hit");
        assert_eq!(res.result, HitResult::Guard);
        assert_eq!(res.hit_sound, Some(guardsound), "a guard uses the guardsound");
    }

    #[test]
    fn resolution_hit_sound_is_none_when_hitdef_has_no_sounds() {
        // Default HitDef resources have no sounds; a clean hit carries None.
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();
        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("clean hit");
        assert_eq!(res.result, HitResult::Hit);
        assert_eq!(res.hit_sound, None, "no hitsound on the HitDef => None");
    }

    #[test]
    fn no_active_hitdef_is_a_no_op() {
        let (mut a, a_air) = make_attacker();
        a.active_hitdef = None;
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();

        assert!(resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).is_none());
        assert_eq!(d.life, 1000);
    }

    #[test]
    fn hitonce_prevents_a_second_connection() {
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();

        // First connection lands.
        assert!(resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).is_some());
        let life_after_first = d.life;

        // Second call with the SAME (still-active) HitDef does nothing: the move
        // already connected (hitonce / numhits = 1).
        assert!(resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).is_none());
        assert_eq!(d.life, life_after_first, "no second hit under hitonce");

        // Resetting the move (a fresh HitDef) re-arms it.
        a.move_connect.reset();
        assert!(resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).is_some());
        assert!(d.life < life_after_first, "re-armed move connects again");
    }

    #[test]
    fn missing_frame_degrades_safely() {
        // Attacker's current anim has no matching action: no boxes, no panic.
        let (mut a, a_air) = make_attacker();
        a.anim = 9999; // not present in a_air
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();

        assert!(resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).is_none());
        assert_eq!(d.life, 1000);
    }

    #[test]
    fn life_clamps_at_zero() {
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        d.life = 10; // less than the 30 hit damage
        let states = HashMap::new();

        resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert_eq!(d.life, 0, "life clamps at zero, never negative");
    }

    // =====================================================================
    // Proctor (task 6.3b): edge-case, error-path, and MUGEN-semantics
    // coverage for resolve_attack, layered on top of Forge's tests. Each
    // block is annotated with the acceptance criterion (AC) it exercises.
    // All synthetic except the gated real-KFM test at the end.
    // =====================================================================

    use crate::{CompiledState, LoadedCharacter};
    use std::path::{Path, PathBuf};

    /// Builds a `CompiledState` for `n` whose entry parameters are the raw
    /// statedef header values (`type`/`movetype`/`physics`), with no controllers.
    /// `change_state` reads exactly these public fields on entry, so this needs no
    /// impl-side helper.
    fn gethit_compiled_state(
        n: i32,
        state_type: Option<&str>,
        movetype: Option<&str>,
        physics: Option<&str>,
    ) -> CompiledState {
        CompiledState {
            number: n,
            state_type: state_type.map(str::to_string),
            movetype: movetype.map(str::to_string),
            physics: physics.map(str::to_string),
            anim: None,
            ctrl: None,
            velset: None,
            controllers: Vec::new(),
        }
    }

    // ---- AC1: detection edge / error paths (degrade safely, never panic) ----

    /// An attacker frame with NO Clsn1 attack boxes cannot connect even when the
    /// defender's hurt box would overlap. Mirrors a non-attacking pose.
    #[test]
    fn attacker_without_clsn1_boxes_does_not_connect() {
        let mut a = Character::new();
        a.pos = Vec2::new(0.0, 0.0);
        a.facing = Facing::Right;
        a.anim = 200;
        a.active_hitdef = Some(sample_hitdef());
        // Action 200 exists but carries no Clsn1 boxes.
        let a_air = air_with(200, Vec::new(), Vec::new());
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();

        assert!(resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).is_none());
        assert_eq!(d.life, 1000);
        assert!(!a.move_connect.contact());
    }

    /// A defender frame with NO Clsn2 hurt boxes cannot be hit (no target).
    #[test]
    fn defender_without_clsn2_boxes_is_untouchable() {
        let (mut a, a_air) = make_attacker();
        let mut d = Character::new();
        d.pos = Vec2::new(60.0, 0.0);
        d.life = 1000;
        // Action 0 exists but has no Clsn2 boxes.
        let d_air = air_with(0, Vec::new(), Vec::new());
        let states = HashMap::new();

        assert!(resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).is_none());
        assert_eq!(d.life, 1000);
    }

    /// A negative `anim_elem` clamps to frame 0 rather than panicking or missing,
    /// so a freshly-entered state (elem may be transiently -1) still detects.
    #[test]
    fn negative_anim_elem_clamps_to_first_frame() {
        let (mut a, a_air) = make_attacker();
        a.anim_elem = -5;
        let (mut d, d_air) = make_defender();
        d.anim_elem = -3;
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states);
        assert!(res.is_some(), "negative elem clamps to frame 0, still hits");
        assert_eq!(d.life, 1000 - 30);
    }

    /// An out-of-range (too-large) `anim_elem` clamps to the LAST frame, never
    /// indexing past the action's frame list.
    #[test]
    fn out_of_range_anim_elem_clamps_to_last_frame() {
        let (mut a, a_air) = make_attacker();
        a.anim_elem = 9999; // action 200 has only one frame
        let (mut d, d_air) = make_defender();
        d.anim_elem = 9999;
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states);
        assert!(res.is_some(), "huge elem clamps to last frame, still hits");
    }

    /// The defender's animation action being absent (unknown anim id) degrades to
    /// "no hurt boxes" -> no hit, no panic.
    #[test]
    fn defender_missing_action_degrades_safely() {
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        d.anim = 424242; // not present in d_air
        let states = HashMap::new();

        assert!(resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).is_none());
        assert_eq!(d.life, 1000);
    }

    /// Touching boundaries (edge-touching boxes) is the same overlap call
    /// fp-combat tests own; here we assert the wiring respects a clean separation:
    /// boxes that share only an edge gap do not connect.
    #[test]
    fn just_out_of_reach_does_not_connect() {
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        // Attacker punch reaches world x=55 (10+45). Defender hurt box left edge
        // is at pos.x - 18. Put the left edge at 56 so there is a 1px gap.
        d.pos = Vec2::new(74.0, 0.0); // left edge 74-18 = 56 > 55
        let states = HashMap::new();

        assert!(resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).is_none());
        assert_eq!(d.life, 1000);
    }

    // ---- AC2: MUGEN apply semantics --------------------------------------

    /// An UNBLOCKABLE HitDef (empty guardflag) ignores `holding_back`: the
    /// defender takes a clean hit even while holding back.
    #[test]
    fn empty_guardflag_is_unblockable_even_holding_back() {
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.guardflag = HitFlags::empty(); // unblockable
        }
        let (mut d, d_air) = make_defender();
        d.holding_back = true; // would normally guard
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states)
            .expect("unblockable attack connects");
        assert_eq!(res.result, HitResult::Hit, "empty guardflag => not guarded");
        assert_eq!(d.life, 1000 - 30, "took full hit damage, not guard damage");
        assert!(a.move_connect.hit);
        assert!(!a.move_connect.guarded);
    }

    /// A holding-back defender whose stance the GUARDFLAG does not admit takes a
    /// clean hit (guard requires the guardflag to admit the stance). Here the
    /// guardflag is `A` (air only) but the defender stands.
    #[test]
    fn holding_back_but_guardflag_excludes_stance_is_a_hit() {
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.guardflag = HitFlags::parse("A"); // only air can block
            hd.hitflag = HitFlags::parse("MAF"); // standing still gets hit
        }
        let (mut d, d_air) = make_defender();
        d.holding_back = true;
        d.state_type = StateType::Standing;
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states)
            .expect("connects as a hit, guard not admitted");
        assert_eq!(res.result, HitResult::Hit);
        assert_eq!(d.life, 1000 - 30);
    }

    /// A hit whose HITFLAG excludes the defender's stance is a MISS reported as
    /// `None`: no damage, no state change, move NOT flagged connected.
    #[test]
    fn hitflag_excludes_stance_is_a_miss() {
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            // Hits only air targets; a grounded standing defender is excluded.
            hd.hitflag = HitFlags::parse("A");
            hd.guardflag = HitFlags::empty();
        }
        let (mut d, d_air) = make_defender();
        d.state_type = StateType::Standing; // Stance::Stand, not admitted by "A"
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states);
        assert!(res.is_none(), "hitflag excludes stance => reported as miss");
        assert_eq!(d.life, 1000, "no damage on a miss");
        assert_eq!(d.state_no, 0, "state unchanged on a miss");
        assert_eq!(a.hitpause, 0, "no hitpause on a miss");
        assert!(!a.move_connect.contact(), "miss does not flag connection");
    }

    /// An AIRBORNE defender takes the AIR knockback velocity and is sent into the
    /// air common get-hit state (5020), not the standing one.
    #[test]
    fn airborne_defender_uses_air_velocity_and_air_gethit_state() {
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        d.state_type = StateType::Air; // Stance::Air, airborne
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states)
            .expect("air target is hit by an 'A'-flagged HitDef");
        // sample_hitdef air_velocity is (4.0, -6.0); attacker faces right (+1).
        assert_eq!(d.vel.x, 4.0);
        assert_eq!(d.vel.y, -6.0, "air knockback Y differs from ground (-3)");
        assert_eq!(res.defender_state, 5020, "air common get-hit state");
        assert_eq!(d.state_no, 5020);
        // Air hittime (20) flows into GetHitVars.
        assert_eq!(d.get_hit_vars.hittime, 20);
    }

    /// A CROUCHING defender (admitted by the `M` = H|L guardflag/hitflag) is sent
    /// into the crouch common get-hit state (5010).
    #[test]
    fn crouching_defender_uses_crouch_gethit_state() {
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        d.state_type = StateType::Crouching; // Stance::Crouch
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states)
            .expect("'M' hitflag admits a crouching defender");
        assert_eq!(res.defender_state, 5010, "crouch common get-hit state");
        assert_eq!(d.state_no, 5010);
        // Ground knockback (crouch is grounded).
        assert_eq!(d.vel.y, -3.0);
    }

    /// A `p2stateno` override on the HitDef sends the defender into that explicit
    /// state instead of the stance-based common state.
    #[test]
    fn p2stateno_override_targets_explicit_state() {
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.p2stateno = Some(1234);
        }
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert_eq!(res.defender_state, 1234);
        assert_eq!(d.state_no, 1234, "defender forced into p2stateno");
    }

    /// A `fall` hit overrides the defender's GetHitVar(yvel) with the HitDef's
    /// `fall.yvelocity` and sets GetHitVar(fall).
    #[test]
    fn fall_hit_sets_fall_and_overrides_yvel() {
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.fall = true;
            hd.fall_yvelocity = -9.5;
        }
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();

        resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert_eq!(d.get_hit_vars.fall, 1, "GetHitVar(fall) set");
        assert_eq!(
            d.get_hit_vars.yvel, -9.5,
            "fall.yvelocity overrides the GetHitVar yvel"
        );
    }

    /// GetHitVars carry-through: yaccel comes from the defender's movement
    /// constants, chainid from the HitDef, and the float velocities match.
    #[test]
    fn gethitvars_fully_populated_from_outcome_and_constants() {
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.chainid = 7;
        }
        let (mut d, d_air) = make_defender();
        d.constants.movement.yaccel = 0.55;
        let states = HashMap::new();

        resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        let gh = &d.get_hit_vars;
        assert_eq!(gh.yaccel, 0.55, "yaccel sourced from defender constants");
        assert_eq!(gh.chainid, 7, "chainid copied from the HitDef");
        assert_eq!(gh.xvel, d.vel.x, "GetHitVar xvel mirrors applied velocity");
        assert_eq!(gh.yvel, d.vel.y, "GetHitVar yvel mirrors applied velocity");
        assert_eq!(gh.slidetime, 12, "ground slidetime mirrors ground hittime");
        assert_eq!(gh.ctrltime, 12, "ground ctrltime mirrors ground hittime");
        assert_eq!(gh.hitshaketime, 8, "shaketime from pausetime.p2");
    }

    /// Guard knockback Y is always zero and the defender does NOT fall on a block,
    /// regardless of a `fall` flag on the HitDef.
    #[test]
    fn guard_never_falls_and_y_is_zero() {
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.fall = true; // would fall on a clean hit
            hd.fall_yvelocity = -9.0;
        }
        let (mut d, d_air) = make_defender();
        d.holding_back = true;
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("guards");
        assert_eq!(res.result, HitResult::Guard);
        assert_eq!(d.vel.y, 0.0, "guard pushback is purely horizontal");
        assert_eq!(d.get_hit_vars.fall, 0, "a block never falls");
        assert_eq!(d.get_hit_vars.yvel, 0.0);
    }

    /// Knockback points away from the attacker regardless of the DEFENDER's
    /// facing — mirroring is by the ATTACKER's facing only. Two same-facing
    /// characters: attacker on the left facing right pushes the defender +x even
    /// though the defender also faces right.
    #[test]
    fn knockback_independent_of_defender_facing() {
        let (mut a, a_air) = make_attacker();
        a.facing = Facing::Right;
        let (mut d, d_air) = make_defender();
        d.facing = Facing::Right; // same facing as attacker
        let states = HashMap::new();

        resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert_eq!(d.vel.x, 4.0, "pushed +x by a right-facing attacker");
    }

    // ---- AC2/AC3: hitpause gates the executor tick -----------------------

    /// After a hit, the defender's positive `hitpause` makes a subsequent
    /// `tick` skip normal processing and count both hitpause and shaketime down
    /// by one, leaving life/state untouched. This is the integration between
    /// resolve_attack's apply step and the executor's hit-pause gate.
    #[test]
    fn hitpause_gates_the_executor_tick() {
        let (mut a, a_air) = make_attacker();
        let (mut d, _d_air) = make_defender();
        let states = HashMap::new();

        // Drive a hit through a separate defender air clone so resolve_attack and
        // the later tick both see frames; the tick uses the LoadedCharacter below.
        let d_air2 = air_with(0, Vec::new(), vec![Rect::new(-18.0, -70.0, 36.0, 70.0)]);
        resolve_attack(&mut a, &a_air, &mut d, &d_air2, &states).expect("connects");

        let hp_before = d.hitpause;
        let shake_before = d.shaketime;
        assert!(hp_before > 0, "defender is paused after a hit");
        assert_eq!(hp_before, 8);
        assert_eq!(shake_before, 8);

        // Drive the executor via the public `tick_with` seam (no Sff needed). The
        // defender is in state 5000 (its get-hit state); with no such state
        // compiled, the tick must still gate purely on hitpause and not panic.
        let states2: HashMap<i32, CompiledState> = HashMap::new();
        let life_before = d.life;
        let state_before = d.state_no;

        let report = d.tick_with(&states2, &d_air2);
        assert!(report.hitpaused, "tick is gated by hit-pause");
        assert_eq!(d.hitpause, hp_before - 1, "hitpause counts down by one");
        assert_eq!(d.shaketime, shake_before - 1, "shaketime counts down too");
        assert_eq!(d.life, life_before, "no state processing while paused");
        assert_eq!(d.state_no, state_before, "state frozen during hit-pause");
    }

    /// The attacker's hit-pause likewise gates its own tick (the attacker freezes
    /// for `pausetime.p1` after connecting).
    #[test]
    fn attacker_hitpause_gates_its_tick() {
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();
        resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");

        assert_eq!(a.hitpause, 8);
        let states2: HashMap<i32, CompiledState> = HashMap::new();
        let report = a.tick_with(&states2, &a_air);
        assert!(report.hitpaused);
        assert_eq!(a.hitpause, 7, "attacker hit-pause decremented");
    }

    // ---- AC1: change_state applies entry params from the defender states ----

    /// When the defender's get-hit state exists in the compiled state map,
    /// `change_state` applies its entry params (e.g. movetype = H). This proves
    /// the `defender_states` argument is threaded through to the transition.
    #[test]
    fn defender_entry_params_apply_on_gethit_state() {
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        // A [Statedef 5000] whose entry sets movetype = H.
        let mut states: HashMap<i32, CompiledState> = HashMap::new();
        states.insert(
            5000,
            gethit_compiled_state(5000, Some("S"), Some("H"), Some("N")),
        );

        resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert_eq!(d.state_no, 5000);
        assert_eq!(
            d.move_type,
            MoveType::BeingHit,
            "5000 statedef entry set movetype = H"
        );
    }

    // ---- AC4 (optional): gated real-KFM integration test -----------------

    fn test_asset(rel: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-assets")
            .join(rel)
    }

    /// Loads real KFM and resolves an attack using its REAL animation frames:
    /// the attacker poses KFM's stand light punch (action 200), whose element 2
    /// carries the real `Clsn1` attack box, and the defender stands in its idle
    /// action 0, whose frame carries the real `Clsn2` hurt boxes. A synthetic
    /// `HitDef` is attached to the attacker (the get-hit *resolution* under test,
    /// not the HitDef-controller firing which `executor` already covers), and the
    /// defender is swept across nearby x offsets until the real boxes overlap.
    ///
    /// This exercises [`resolve_attack`]'s real-asset Clsn extraction and apply
    /// path end-to-end. Skips cleanly (with a printed reason) only when
    /// `test-assets/` is absent or the real frames carry no boxes; the fixture is
    /// known to ship both, so the assertion runs in this repo.
    #[test]
    fn real_kfm_resolve_attack_damages_defender() {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return;
        }
        let lc = match LoadedCharacter::load(&def) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("skipping: kfm.def failed to load: {e}");
                return;
            }
        };

        // Verify the real fixture actually carries the boxes we depend on; if a
        // future asset edit removes them, skip rather than fail spuriously.
        // Action 200 element 2 holds the punch's Clsn1; action 0 holds Clsn2.
        let clsn1 = current_frame_clsn1(&lc.air, 200, 2);
        let clsn2 = current_frame_clsn2(&lc.air, 0, 0);
        if clsn1.is_empty() || clsn2.is_empty() {
            eprintln!("skipping: KFM action 200/0 frames lack expected Clsn boxes");
            return;
        }

        // Attacker frozen on the punch's active (Clsn1-bearing) frame, with a
        // concrete HitDef so resolution has damage/knockback to apply.
        let mut attacker = Character::with_constants(lc.constants);
        attacker.state_no = 200;
        attacker.anim = 200;
        attacker.anim_elem = 2;
        attacker.pos = Vec2::new(0.0, 0.0);
        attacker.facing = Facing::Right;
        attacker.move_type = MoveType::Attack;
        attacker.active_hitdef = Some(sample_hitdef());

        // Sweep the defender across nearby offsets until the REAL boxes overlap.
        let mut connected = false;
        for dx in 0..=120 {
            let mut defender = Character::with_constants(lc.constants);
            defender.state_no = 0;
            defender.anim = 0;
            defender.anim_elem = 0;
            defender.facing = Facing::Left;
            defender.pos = Vec2::new(dx as f32, 0.0);
            defender.life = lc.constants.life_max;
            attacker.move_connect.reset(); // re-arm for each placement attempt
            let life_before = defender.life;

            if let Some(r) =
                resolve_attack(&mut attacker, &lc.air, &mut defender, &lc.air, &lc.states)
            {
                assert_eq!(r.result, fp_combat::HitResult::Hit);
                assert_eq!(defender.life, life_before - 30, "real KFM punch dealt 30 dmg");
                assert!(defender.state_no >= 5000, "entered a get-hit state");
                // The applied knockback points away from the right-facing
                // attacker. We assert it via the resolution recipe and the
                // durable GetHitVar, NOT `defender.vel`: KFM's real
                // `[Statedef 5000]` carries `velset = 0,0`, so entering the
                // get-hit state zeroes the live velocity (MUGEN-faithful — the
                // 5000 state re-applies motion from `GetHitVar(xvel/yvel)`).
                assert!(r.knockback.x > 0.0, "recipe knockback away (+x)");
                assert!(
                    defender.get_hit_vars.xvel > 0.0,
                    "GetHitVar(xvel) records away-knockback for the 5000 state"
                );
                assert!(attacker.move_connect.contact(), "attacker move connected");
                assert_eq!(attacker.hitpause, 8, "hit-pause set on the attacker");
                connected = true;
                break;
            }
        }
        assert!(
            connected,
            "real KFM punch Clsn1 should overlap idle Clsn2 at some offset"
        );
    }
}
