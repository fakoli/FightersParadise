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
    /// The defender's induced stun in ticks — the hit-stun on a clean
    /// [`HitResult::Hit`] or the guard-stun on a [`HitResult::Guard`] (the
    /// `outcome.hittime` written onto the defender's
    /// [`GetHitVars::hittime`](crate::GetHitVars::hittime)). This is the
    /// "defender held for N frames" number the frame-advantage readout subtracts
    /// the attacker's remaining recovery from; see
    /// [`crate::framedata::frame_advantage`].
    pub stun: i32,
    /// The attacker's `HitDef` sound to play for this connection: the `hitsound`
    /// on a clean [`HitResult::Hit`], the `guardsound` on a [`HitResult::Guard`].
    /// [`None`] when the relevant sound is unset on the `HitDef`. (A miss never
    /// produces an [`AttackResolution`], so this is keyed off `result`.)
    pub hit_sound: Option<fp_combat::SoundId>,
    /// The state the **attacker** should be sent into on this connection — the
    /// HitDef's `p1stateno` ([`None`] when the param was absent). Throws set this
    /// (KFM `p1stateno = 810`) so the attacker enters its throw animation.
    ///
    /// [`resolve_attack`] does **not** change the attacker's state itself: the
    /// attacker's state graph is not in hand here (only the *defender*'s states
    /// are passed in), so applying `p1stateno` is deferred to a downstream owner
    /// of both characters (`fp-engine`, task P8b), which has the attacker's
    /// compiled states.
    pub attacker_state: Option<i32>,
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
///      outcome, including `GetHitVar(animtype)` — set from the HitDef's
///      `air_animtype` when the defender is airborne, else its ground `animtype`
///      (so common1 get-hit states pick the correct reaction, not always Light);
///    - hit-pause (the impact freeze) is set on the attacker (`pausetime.p1`) and
///      the defender (`pausetime.p2`) via `max(current, new)` so a re-armed move
///      never shortens an active freeze; the defender's shake timer is set the
///      same way. A guarded hit falls back to `pausetime` because the modeled
///      [`fp_combat::HitDef`] carries no distinct `guard.pausetime`; a miss
///      returns [`None`] before this step and so pauses neither participant;
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

    // (2b) Attack-attribute invulnerability — the defender's NotHitBy / HitBy
    // windows (faithfulness audit P9). BEFORE resolving/applying the hit, consult
    // the DEFENDER's active mask slots against the ATTACKER's HitDef `attr`. The
    // mask blocks the hit when, for any active slot:
    //
    //   - NotHitBy (exclude): the attacker attr IS in the slot's attr set, or
    //   - HitBy   (include): the attacker attr is NOT in the slot's attr set.
    //
    // A hit must pass BOTH active slots (either one blocking is enough to drop
    // it). When blocked we return `None` — exactly like a geometric miss: no
    // damage, no state change, no hit-pause, and the move is NOT marked connected
    // (the attack simply passes through the invulnerable defender, as in MUGEN).
    // An inactive/expired slot, or an empty NotHitBy set, blocks nothing; an
    // empty HitBy set blocks everything (full invuln) — see [`crate::invuln`].
    if defender.invuln.blocks(&hitdef.attr) {
        return None;
    }

    // (2b·T080) SuperPause `unhittable` invulnerability. The `SuperPause` triggerer
    // carries a [`SuperPauseEffect`](crate::SuperPauseEffect) for the pause window;
    // while it is active and `unhittable = 1`, the triggerer cannot be hit. Drop the
    // hit exactly like a `NotHitBy` block (it passes through — no damage, no state
    // change, no hit-pause, NOT marked connected), matching MUGEN's behavior of a
    // super's startup flash protecting the attacker.
    if defender.superpause_effect.blocks_incoming() {
        return None;
    }

    // (2c) HitOverride (audit #9b). BEFORE the normal get-hit, consult the
    // DEFENDER's armed override slots against the ATTACKER's HitDef `attr`. On the
    // first matching active slot MUGEN redirects the defender to the slot's
    // `stateno` *instead of* the normal get-hit: no damage, knockback, or get-hit
    // state is applied (the override state fully takes over the reaction — armor /
    // parry / counter logic). The hit still COUNTS as a connection — the attacker
    // registers move-contact and a target, and `hitonce` is consumed — so a single
    // HitDef cannot re-trigger the override. The slot is consumed (disarmed) on a
    // match, matching MUGEN. (Simplification: MUGEN's `forceair` / damage-applied
    // variants are not modeled; the common armor/counter case — redirect with no
    // damage — is implemented faithfully.)
    if let Some((slot, override_state)) = defender.hit_overrides.matching(&hitdef.attr) {
        defender.hit_overrides.consume(slot);
        defender.change_state(defender_states, override_state);
        // T061: the defender's current get-hit reaction is now an override-redirected
        // one, so `HitOverridden` reads 1 for the duration of that reaction. A later
        // *normal* get-hit (below) clears the flag again.
        defender.hit_overridden = true;
        // The attacker still connected: flag move-contact + target and consume
        // hitonce so the same HitDef cannot fire the override again.
        attacker.move_connect.hit = true;
        attacker.has_target = true;
        // Power gain on an override match (#18 × #9b): MUGEN still counts the hit
        // as a *connection* for the attacker, so the attacker's `getpower.hit`
        // still accrues. The DEFENDER, however, does NOT run the normal get-hit
        // (the override state takes over the reaction), so its `givepower` is
        // deliberately NOT granted here — the defender's meter is the override
        // state's own concern. KFM authors `getpower = 0`, so this adds nothing
        // for KFM; it matters only for a character that gains meter on contact.
        attacker.add_power_clamped(hitdef.getpower.hit);
        return Some(AttackResolution {
            result: HitResult::Hit,
            damage: 0,
            knockback: Vec2::new(0.0, 0.0),
            defender_state: override_state,
            attacker_hitpause: 0,
            defender_hitpause: 0,
            // The override state takes over the defender's reaction entirely, so
            // there is no engine-imposed hit/guard stun to count here.
            stun: 0,
            hit_sound: None,
            attacker_state: hitdef.p1stateno,
        });
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

    // (3b) Air-juggle limit (#16). MUGEN gates a hit against an AIRBORNE defender
    // on the juggle system: the attacker's current move costs `[Statedef] juggle`
    // points, charged to the defender's per-combo juggle pool (seeded from the
    // defender's `[Data] airjuggle`, refilled when it lands). When the pool can no
    // longer pay the move's cost, the hit is dropped — exactly like a geometric
    // miss: no damage, no state change, no hit-pause, and the move is NOT marked
    // connected (so the attacker can retry once the juggle refills). A grounded
    // defender is never juggle-gated, and a move with `juggle = 0` (the default,
    // or a non-attack state) costs nothing and so is never blocked.
    //
    // Simplification: MUGEN draws the per-hit cost from the HitDef-bearing move's
    // statedef; we read it off the attacker's CURRENT state (`cur_juggle_cost`,
    // set on entry). This is the common case (a HitDef fires from the attack state
    // whose header carries `juggle`). The pool is decremented only on a hit that
    // actually lands (passing this gate), so a dropped juggle does not over-charge.
    if defender_state.airborne {
        let cost = attacker.cur_juggle_cost;
        if cost > 0 {
            if defender.juggle_points < cost {
                // Out of juggle: the hit does not combo. Pass through like a miss.
                return None;
            }
            defender.juggle_points -= cost;
        }
    }

    // (4) Apply the outcome to the defender.
    //
    // Knockback is attacker-facing-relative; mirror its x by the ATTACKER's
    // facing so a positive (away) x pushes the defender away from the attacker
    // regardless of which way the attacker faces. Y is never mirrored.
    let attacker_sign = attacker.facing.sign() as f32;
    let knockback = Vec2::new(outcome.knockback.x * attacker_sign, outcome.knockback.y);

    // Scale damage by the attacker's attack multiplier and the defender's defence
    // multiplier (MUGEN AttackMulSet / DefenceMulSet; both default 1.0, so the base
    // damage is unchanged when neither is set). The attacker's active SuperPause
    // `p2defmul` (T080) folds in here too: it scales the OPPONENT's (the defender's)
    // effective defence for the pause window, so it multiplies the damage the
    // defender takes. It lives on the attacker (the triggerer) and is the neutral
    // `1.0` outside an active SuperPause window, so the base damage is unchanged for
    // ordinary hits. final = round(base * atk * def * p2defmul).
    let applied_damage = (outcome.damage as f32
        * attacker.attack_mul
        * defender.defence_mul
        * attacker.superpause_effect.active_p2defmul())
    .round()
    .clamp(0.0, i32::MAX as f32) as i32;

    defender.life = (defender.life - applied_damage).max(0);
    defender.vel = knockback;
    // T061: a normal (non-overridden) hit landed and now drives the reaction, so
    // any prior `HitOverridden` state is no longer current.
    defender.hit_overridden = false;

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
    gh.damage = applied_damage;
    // GetHitVar(animtype): the reaction-animation code the defender's common1
    // get-hit states (5000-5xxx) branch on. The DEFENDER's airborne state is
    // known here (it built `defender_state.airborne` above), so pick the HitDef's
    // `air_animtype` when the defender is airborne, else its ground `animtype`.
    // (Previously never set, so it was always 0 = Light — the P7 bug.)
    let reaction_animtype = if defender_state.airborne {
        hitdef.air_animtype
    } else {
        hitdef.animtype
    };
    gh.animtype = reaction_animtype.code();
    gh.hitshaketime = outcome.shaketime;
    gh.hittime = outcome.hittime;
    gh.slidetime = outcome.slidetime;
    gh.ctrltime = outcome.ctrltime;
    gh.fall = i32::from(outcome.fall);
    gh.guarded = i32::from(guarded);
    gh.chainid = hitdef.chainid;
    // Fall velocities/damage for the `HitFallVel`/`HitFallDamage` controllers
    // (audit #23). On a falling hit the fall Y velocity is the HitDef's
    // `fall.yvelocity`; the fall X velocity is the HitDef's authored
    // `fall.xvelocity` when present, else "no change" — MUGEN leaves the
    // defender's current X velocity, which here is the imparted knockback X.
    if outcome.fall {
        gh.yvel = outcome.fall_yvelocity;
        gh.fall_yvel = outcome.fall_yvelocity;
        gh.fall_xvel = hitdef.fall_xvelocity.unwrap_or(knockback.x);
    }
    // `fall.damage` is carried from the HitDef and surfaces via
    // `GetHitVar(fall.damage)`; the `HitFallDamage` controller in the authored
    // get-hit state subtracts it from life when the defender lands.
    gh.fall_damage = hitdef.fall_damage;

    // Send the defender into the get-hit / guard state.
    defender.change_state(defender_states, outcome.gethit_state);

    // Hit-pause / shake on both participants (task 6.5 — the impact freeze).
    //
    // On a *connecting* hit MUGEN freezes both players: the attacker for
    // `pausetime.p1` and the defender for `pausetime.p2`. We take the per-side
    // pause via `max(current, new)` so a fresh connection never shortens an
    // already-running freeze (a multi-hit move that re-arms mid-pause keeps the
    // longer of the two). A miss never reaches this point (it returns `None`
    // above), so a miss pauses NEITHER participant — exactly the required rule.
    //
    // HITSTOP STRENGTH-SCALING (T073): the attacker's hit-stop is surfaced
    // verbatim from the connecting `HitDef`'s `pausetime.p1`, so a heavy move
    // (large authored `pausetime`) freezes the attacker longer than a light one
    // and "reads heavier" — no separate strength system is invented; the freeze
    // is data-driven straight from the HitDef. The executor counts this freeze
    // down one tick at a time (see `Character::hitpause`), so a hit with
    // `pausetime.p1 = 0` imparts no attacker hit-stop at all.
    //
    // GUARD PAUSETIME FALLBACK: MUGEN's `HitDef` can carry a distinct
    // `guard.pausetime`; [`fp_combat::HitDef`] does not model that field yet, so
    // [`fp_combat::resolve_hit`] reports `pausetime.p1`/`pausetime.p2` for *both*
    // the clean-hit and the guard branches. The guard case therefore falls back
    // to the ordinary `pausetime` here — the documented behavior until a separate
    // `guard.pausetime` is added to `fp-combat` (out of scope for this crate).
    attacker.hitpause = attacker.hitpause.max(outcome.pausetime);
    defender.hitpause = defender.hitpause.max(outcome.shaketime);
    defender.shaketime = defender.shaketime.max(outcome.shaketime);

    // On-hit super-meter gain (audit #18). MUGEN grants the ATTACKER `getpower`
    // and the DEFENDER `givepower` when a HitDef connects: the `hit` component on
    // a clean hit, the `guard` component on a block. Both are clamped to
    // `[0, power_max]` via the shared `add_power_clamped` path (so this never
    // overflows or leaves the meter out of range). KFM authors `getpower = 0` on
    // every attack, which the controller stores as an explicit `(0, 0)` — so this
    // adds nothing for KFM and its statedef-`poweradd` meter source is NOT double-
    // counted. This is the SECONDARY, damage-proportional default gain; the
    // primary `poweradd`/`PowerAdd`/`PowerSet` path is entirely independent.
    let (atk_gain, def_gain) = if guarded {
        (hitdef.getpower.guard, hitdef.givepower.guard)
    } else {
        (hitdef.getpower.hit, hitdef.givepower.hit)
    };
    attacker.add_power_clamped(atk_gain);
    defender.add_power_clamped(def_gain);

    // Mark the attacker's move as connected (drives MoveHit/MoveGuarded/
    // MoveContact and enforces hitonce on the next call).
    if guarded {
        attacker.move_connect.guarded = true;
    } else {
        attacker.move_connect.hit = true;
    }

    // The defender the attacker just hit becomes the attacker's target: its
    // `Target*` controllers now act on the opponent (throws use this — KFM state
    // 810). In this flat 1-v-1 model the target is the opponent and stays set;
    // MUGEN's per-target release (move end / explicit redirect) is deferred.
    attacker.has_target = true;

    // Pick the impact sound from the attacker's HitDef: the guardsound when the
    // attack was guarded, the hitsound on a clean hit. Either may be `None`.
    let hit_sound = if guarded {
        hitdef.resources.guardsound
    } else {
        hitdef.resources.hitsound
    };

    Some(AttackResolution {
        result: outcome.result,
        damage: applied_damage,
        knockback,
        defender_state: outcome.gethit_state,
        attacker_hitpause: outcome.pausetime,
        defender_hitpause: outcome.shaketime,
        stun: outcome.hittime,
        hit_sound,
        // `p1stateno` is parsed onto the HitDef but applied to the attacker
        // downstream (P8b), since the attacker's state graph is not in hand here.
        attacker_state: hitdef.p1stateno,
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
    use crate::{Facing, MoveConnect, MoveType, StageView, StateType};
    use fp_combat::{AnimType, Damage, HitDef, HitFlags, HitTimes, PauseTime};
    use fp_formats::air::{AnimAction, AnimFrame, BlendMode};
    use fp_vm::{eval, parse_str, EvalContext, Value};
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
            ..Default::default()
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
        assert!(
            d.vel.x > 0.0,
            "defender pushed away (+x) from a right-facer"
        );
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
    fn attack_and_defence_multipliers_scale_damage() {
        // base hit damage = 30 (make_attacker HitDef); multipliers default 1.0.
        let states = HashMap::new();

        // AttackMul 2.0 -> doubled.
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        a.attack_mul = 2.0;
        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert_eq!(res.damage, 60, "attack_mul 2.0 doubles damage");
        assert_eq!(d.life, 1000 - 60);
        assert_eq!(d.get_hit_vars.damage, 60);

        // DefenceMul 0.5 -> halved (attack default 1.0).
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        d.defence_mul = 0.5;
        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert_eq!(res.damage, 15, "defence_mul 0.5 halves damage");
        assert_eq!(d.life, 1000 - 15);

        // Combined 2.0 * 0.5 = base (30, unchanged).
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        a.attack_mul = 2.0;
        d.defence_mul = 0.5;
        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert_eq!(res.damage, 30, "2.0 * 0.5 leaves base damage");
    }

    #[test]
    fn superpause_unhittable_defender_blocks_hit() {
        // T080: an active `unhittable` SuperPause window on the defender drops the
        // hit entirely (pass-through, like NotHitBy): no damage, no connection.
        let states = HashMap::new();
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        d.life = 1000;
        d.superpause_effect = crate::SuperPauseEffect {
            unhittable: true,
            p2defmul: 1.0,
            remaining: 10,
        };
        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states);
        assert!(res.is_none(), "unhittable window blocks the hit");
        assert_eq!(d.life, 1000, "no damage applied");
        assert!(!a.move_connect.contact(), "the move did not connect");
    }

    #[test]
    fn superpause_unhittable_zero_does_not_block() {
        // Negative control: an active window with `unhittable = 0` lets the hit land.
        let states = HashMap::new();
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        d.life = 1000;
        d.superpause_effect = crate::SuperPauseEffect {
            unhittable: false,
            p2defmul: 1.0,
            remaining: 10,
        };
        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert_eq!(res.damage, 30, "hit lands at base damage");
    }

    #[test]
    fn superpause_p2defmul_on_attacker_scales_defender_damage() {
        // T080: the attacker's (triggerer's) active `p2defmul` multiplies the
        // damage the defender takes; an inactive window leaves the base damage.
        let states = HashMap::new();
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        d.life = 1000;
        a.superpause_effect = crate::SuperPauseEffect {
            unhittable: true,
            p2defmul: 2.0,
            remaining: 10,
        };
        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert_eq!(res.damage, 60, "p2defmul 2.0 doubles defender damage");
        assert_eq!(d.life, 1000 - 60);
    }

    #[test]
    fn superpause_inactive_window_is_inert() {
        // A window with `remaining = 0` neither blocks nor scales (the default state
        // every character carries outside a SuperPause).
        let states = HashMap::new();
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        d.life = 1000;
        a.superpause_effect = crate::SuperPauseEffect {
            unhittable: true,
            p2defmul: 5.0,
            remaining: 0,
        };
        d.superpause_effect = crate::SuperPauseEffect {
            unhittable: true,
            p2defmul: 1.0,
            remaining: 0,
        };
        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert_eq!(res.damage, 30, "inactive windows leave base damage");
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
        let hitsound = SoundId {
            group: 5,
            sample: 0,
            common: false,
        };
        let guardsound = SoundId {
            group: 6,
            sample: 1,
            common: true,
        };

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
        assert_eq!(
            res.hit_sound,
            Some(guardsound),
            "a guard uses the guardsound"
        );
    }

    /// Spark SOURCE coverage on the `resolve_attack` connect path (T002 / FL2a):
    /// the spark a connecting hit spawns is sourced from the *connecting*
    /// `HitDef`'s `sparkno`, classified by [`fp_combat::SparkSource`] exactly as
    /// `fp-engine` does when it spawns the effect. Both MUGEN cases are asserted:
    ///
    /// - a bare (non-`S`) `sparkno` → a COMMON `fightfx` spark (shared set), and
    /// - an `S`-prefixed `sparkno` (encoded negative by `parse_sparkno`) → an
    ///   attacker-OWN spark (the attacker's own SFF), NOT the common set.
    ///
    /// This is the `fp-character`-side assertion the acceptance criteria require:
    /// it confirms the own-vs-common distinction survives onto the resolved hit.
    #[test]
    fn connecting_hit_resolves_correct_spark_source_for_common_and_own() {
        use fp_combat::SparkSource;

        // ---- Common case: a bare non-negative sparkno → common fightfx set. ----
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.resources.sparkno = 2; // bare → common action 2
        }
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();
        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("clean hit");
        assert_eq!(
            res.result,
            HitResult::Hit,
            "the common-spark hit must connect"
        );
        // The connecting attacker's HitDef carries the source the engine reads.
        let sparkno = a
            .active_hitdef
            .as_ref()
            .expect("active hitdef")
            .resources
            .sparkno;
        assert_eq!(
            SparkSource::classify(sparkno),
            SparkSource::Common { anim: 2 },
            "a bare sparkno on a connecting hit sources the common fightfx set"
        );

        // ---- Own case: an `S`-prefixed sparkno (negative) → attacker's own. ----
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            // `S5` is encoded as -5 by `parse_sparkno`; set the equivalent value.
            hd.resources.sparkno = -5;
        }
        let (mut d, d_air) = make_defender();
        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("clean hit");
        assert_eq!(res.result, HitResult::Hit, "the own-spark hit must connect");
        let sparkno = a
            .active_hitdef
            .as_ref()
            .expect("active hitdef")
            .resources
            .sparkno;
        assert_eq!(
            SparkSource::classify(sparkno),
            SparkSource::Own { anim: 5 },
            "an S-prefixed (negative) sparkno sources the attacker's OWN set, not common"
        );
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

    /// AC1/AC4 (task 6.5): a clean hit sets the hit-pause on BOTH characters from
    /// the HitDef's `pausetime` (attacker from `p1`, defender from `p2`); a miss
    /// sets NEITHER.
    #[test]
    fn hit_sets_hitpause_on_both_miss_sets_neither() {
        // ---- Clean hit: both paused from pausetime (p1 = p2 = 8 here). ----
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();
        resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert_eq!(a.hitpause, 8, "attacker paused from pausetime.p1");
        assert_eq!(d.hitpause, 8, "defender paused from pausetime.p2");
        assert_eq!(a.hitpause_time(), 8, "accessor agrees on the attacker");
        assert_eq!(d.hitpause_time(), 8, "accessor agrees on the defender");

        // ---- Miss (out of reach): neither participant is paused. ----
        let (mut a2, a_air2) = make_attacker();
        let (mut d2, d_air2) = make_defender();
        d2.pos = Vec2::new(500.0, 0.0); // far out of reach -> no contact
        assert!(resolve_attack(&mut a2, &a_air2, &mut d2, &d_air2, &states).is_none());
        assert_eq!(a2.hitpause, 0, "a miss does not pause the attacker");
        assert_eq!(d2.hitpause, 0, "a miss does not pause the defender");
    }

    /// AC1 (task 6.5): a re-armed connection uses `max(current, new)` so it never
    /// SHORTENS an already-running freeze (a multi-hit move keeps the longer pause).
    #[test]
    fn re_armed_hit_never_shortens_an_active_pause() {
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();

        // Pre-load a longer pause than this HitDef's pausetime (8) would set.
        a.hitpause = 20;
        d.hitpause = 20;
        resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert_eq!(
            a.hitpause, 20,
            "longer existing attacker pause is preserved"
        );
        assert_eq!(
            d.hitpause, 20,
            "longer existing defender pause is preserved"
        );
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
            poweradd: None,
            sprpriority: None,
            juggle: None,
            facep2: None,
            hitdefpersist: None,
            movehitpersist: None,
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

    /// P8a: a connecting hit makes the defender the attacker's target — the
    /// attacker's `has_target` flips true so its `Target*` controllers fire.
    #[test]
    fn connecting_hit_sets_attacker_has_target() {
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();
        assert!(!a.has_target, "no target before any hit");

        resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert!(
            a.has_target,
            "defender became the attacker's target on connect"
        );
    }

    /// P8a: `AttackResolution.attacker_state` carries the HitDef's `p1stateno`
    /// (the attacker's throw-anim state) on a connecting hit. fp-engine (P8b)
    /// applies it; resolve_attack does not change the attacker's state here.
    #[test]
    fn connecting_hit_reports_p1stateno_as_attacker_state() {
        let (mut a, a_air) = make_attacker();
        let attacker_state_before = a.state_no;
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.p1stateno = Some(810);
        }
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert_eq!(res.attacker_state, Some(810));
        assert_eq!(
            a.state_no, attacker_state_before,
            "resolve_attack does NOT move the attacker; p1stateno is applied downstream"
        );
    }

    /// P8a: with no `p1stateno` on the HitDef, `attacker_state` is `None`.
    #[test]
    fn connecting_hit_without_p1stateno_reports_none() {
        let (mut a, a_air) = make_attacker();
        // sample_hitdef leaves p1stateno at its default (None).
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert_eq!(res.attacker_state, None);
    }

    /// P8a (Proctor): a **guarded** connection — not only a clean hit — also
    /// establishes the target. MUGEN's target set includes anyone the attacker's
    /// HitDef contacted, blocked or not.
    #[test]
    fn guarded_hit_also_sets_attacker_has_target() {
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        d.holding_back = true; // guardflag MA admits a standing block
        let states = HashMap::new();
        assert!(!a.has_target);

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("guards");
        assert_eq!(res.result, HitResult::Guard);
        assert!(
            a.has_target,
            "a guarded contact still makes the defender a target"
        );
    }

    /// P8a (Proctor): the lifecycle simplification — once `has_target` is set it
    /// **stays** set. A second `resolve_attack` blocked by `hitonce` returns
    /// `None` yet does not clear the flag (no per-target release in this model).
    #[test]
    fn has_target_persists_after_hitonce_blocks_second_call() {
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();

        resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("first connects");
        assert!(a.has_target);

        // hitonce blocks the second call (returns None) but the target stays set.
        assert!(resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).is_none());
        assert!(
            a.has_target,
            "has_target is sticky; no release in the flat 1-v-1 model"
        );
    }

    /// P8a (Proctor): `p1stateno` is independent of `p2stateno`. Setting both on
    /// the HitDef routes the attacker (p1) and defender (p2) to their distinct
    /// states — the exact KFM throw shape (p1stateno=810 thrower, p2stateno=820
    /// victim).
    #[test]
    fn p1stateno_and_p2stateno_are_reported_independently() {
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.p1stateno = Some(810);
            hd.p2stateno = Some(820);
        }
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert_eq!(res.attacker_state, Some(810), "p1stateno -> attacker_state");
        assert_eq!(res.defender_state, 820, "p2stateno -> defender_state");
        assert_eq!(d.state_no, 820, "defender actually moved to p2stateno");
    }

    /// P8a (Proctor): a **miss** establishes no target and reports no
    /// `attacker_state` — `resolve_attack` returns `None` before any of the P8a
    /// bookkeeping runs.
    #[test]
    fn miss_sets_no_target_and_no_attacker_state() {
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        // Move the defender far out of reach so the boxes never overlap.
        d.pos = Vec2::new(10_000.0, 0.0);
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.p1stateno = Some(810);
        }
        let states = HashMap::new();

        assert!(resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).is_none());
        assert!(!a.has_target, "a whiff establishes no target");
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

    // ---- #18: on-hit power gain (getpower / givepower) --------------------

    /// A clean hit grants the attacker its HitDef `getpower.hit` and the defender
    /// its `givepower.hit`, both clamped into `[0, power_max]`.
    #[test]
    fn clean_hit_grants_getpower_to_attacker_and_givepower_to_defender() {
        use fp_combat::PowerGain;
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.getpower = PowerGain { hit: 70, guard: 35 };
            hd.givepower = PowerGain { hit: 60, guard: 30 };
        }
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();
        assert_eq!(a.power, 0);
        assert_eq!(d.power, 0);

        resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert_eq!(a.power, 70, "attacker gains getpower.hit on a clean hit");
        assert_eq!(d.power, 60, "defender gains givepower.hit on a clean hit");
    }

    /// A guarded hit uses the GUARD components, not the hit components.
    #[test]
    fn guarded_hit_grants_guard_power_components() {
        use fp_combat::PowerGain;
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.getpower = PowerGain { hit: 70, guard: 35 };
            hd.givepower = PowerGain { hit: 60, guard: 30 };
        }
        let (mut d, d_air) = make_defender();
        d.holding_back = true; // guardflag MA admits a standing block
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("guards");
        assert_eq!(res.result, HitResult::Guard);
        assert_eq!(a.power, 35, "attacker gains getpower.guard on a block");
        assert_eq!(d.power, 30, "defender gains givepower.guard on a block");
    }

    /// `getpower = 0` (KFM's suppression on every attack) adds NO attacker power,
    /// so the statedef-`poweradd` meter source is not double-counted by this path.
    #[test]
    fn zero_getpower_suppresses_attacker_gain() {
        use fp_combat::PowerGain;
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.getpower = PowerGain { hit: 0, guard: 0 };
            hd.givepower = PowerGain { hit: 60, guard: 30 };
        }
        a.power = 100; // pre-existing meter from a `poweradd` path
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();

        resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert_eq!(
            a.power, 100,
            "getpower = 0 leaves the attacker meter untouched"
        );
        assert_eq!(d.power, 60, "givepower still applies to the defender");
    }

    /// Power gain clamps to `[0, power_max]`: a huge getpower cannot exceed the
    /// meter cap, and a miss grants no power at all.
    #[test]
    fn power_gain_clamps_to_max_and_miss_grants_nothing() {
        use fp_combat::PowerGain;
        // Clamp at power_max.
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.getpower = PowerGain {
                hit: i32::MAX,
                guard: 0,
            };
        }
        a.power_max = 3000;
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();
        resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert_eq!(
            a.power, 3000,
            "getpower clamps at power_max, never overflows"
        );

        // A miss (out of reach) grants no power to either participant.
        let (mut a2, a_air2) = make_attacker();
        if let Some(hd) = a2.active_hitdef.as_mut() {
            hd.getpower = PowerGain { hit: 70, guard: 35 };
            hd.givepower = PowerGain { hit: 60, guard: 30 };
        }
        let (mut d2, d_air2) = make_defender();
        d2.pos = Vec2::new(10_000.0, 0.0);
        assert!(resolve_attack(&mut a2, &a_air2, &mut d2, &d_air2, &states).is_none());
        assert_eq!(a2.power, 0, "a whiff grants no attacker power");
        assert_eq!(d2.power, 0, "a whiff grants no defender power");
    }

    // ---- #9b: HitOverride redirects the defender instead of the normal hit -

    /// An armed `HitOverride` slot whose attr matches the attacker redirects the
    /// defender to the override state, applies NO damage/knockback, consumes the
    /// slot, and still registers the attacker's contact (move_connect + target).
    #[test]
    fn hit_override_redirects_defender_and_suppresses_damage() {
        use crate::invuln::AttackAttrSet;
        let (mut a, a_air) = make_attacker(); // HitDef attr defaults to S, NA
        let (mut d, d_air) = make_defender();
        // Arm slot 0 to override a standing normal attack -> state 700.
        d.hit_overrides
            .arm(0, AttackAttrSet::parse("S, NA"), 700, 30);
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states)
            .expect("override fires as a connection");
        assert_eq!(res.result, HitResult::Hit);
        assert_eq!(res.damage, 0, "override applies no damage");
        assert_eq!(
            res.defender_state, 700,
            "defender redirected to the override state"
        );
        assert_eq!(
            d.state_no, 700,
            "defender actually entered the override state"
        );
        assert_eq!(d.life, 1000, "no life lost under a hit override");
        assert_eq!(
            d.vel,
            Vec2::new(0.0, 0.0),
            "no knockback under a hit override"
        );
        // The attacker still registered a connection (so hitonce holds).
        assert!(a.move_connect.hit);
        assert!(a.has_target);
        // The slot was consumed.
        assert!(
            !d.hit_overrides.slots[0].is_active(),
            "matching slot consumed"
        );
    }

    /// On a `HitOverride` match the attacker still counts the hit as a connection,
    /// so its `getpower.hit` accrues; the defender's `givepower` does NOT, since
    /// the override state replaces the normal get-hit reaction.
    #[test]
    fn hit_override_grants_attacker_getpower_but_not_defender_givepower() {
        use crate::invuln::AttackAttrSet;
        use fp_combat::PowerGain;
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.getpower = PowerGain { hit: 70, guard: 35 };
            hd.givepower = PowerGain { hit: 60, guard: 30 };
        }
        let (mut d, d_air) = make_defender();
        d.hit_overrides
            .arm(0, AttackAttrSet::parse("S, NA"), 700, 30);
        let states = HashMap::new();

        resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("override fires");
        assert_eq!(
            a.power, 70,
            "attacker still gains getpower.hit on an override connection"
        );
        assert_eq!(
            d.power, 0,
            "defender's givepower is NOT granted under an override"
        );
    }

    /// A `HitOverride` whose attr does NOT match the attacker is ignored — the
    /// normal get-hit applies.
    #[test]
    fn hit_override_non_matching_attr_falls_through_to_normal_hit() {
        use crate::invuln::AttackAttrSet;
        let (mut a, a_air) = make_attacker(); // attr S, NA
        let (mut d, d_air) = make_defender();
        // Override only throws; the attacker's S,NA is a normal strike -> no match.
        d.hit_overrides
            .arm(0, AttackAttrSet::parse(", NT,ST,HT"), 700, 30);
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("normal hit");
        assert_eq!(res.result, HitResult::Hit);
        assert_eq!(
            res.damage, 30,
            "normal hit damage applied (override did not match)"
        );
        assert_eq!(d.state_no, 5000, "normal get-hit state, not the override");
        assert!(
            d.hit_overrides.slots[0].is_active(),
            "non-matching slot is NOT consumed"
        );
    }

    /// An inactive (expired) `HitOverride` slot never fires.
    #[test]
    fn hit_override_inactive_slot_does_not_fire() {
        use crate::invuln::AttackAttrSet;
        let (mut a, a_air) = make_attacker();
        let (mut d, d_air) = make_defender();
        d.hit_overrides
            .arm(0, AttackAttrSet::parse("S, NA"), 700, 0); // time 0 = inactive
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("normal hit");
        assert_eq!(
            res.defender_state, 5000,
            "inactive override slot is ignored"
        );
        assert_eq!(d.life, 1000 - 30, "normal damage applied");
    }

    // ---- T061: HitOverride sets/clears the `hit_overridden` flag (HitOverridden) -

    /// T061: a matching `HitOverride` sets the defender's `hit_overridden` flag (the
    /// state behind the `HitOverridden` trigger); a subsequent normal
    /// (non-overridden) hit clears it. This is the `HitOverridden` acceptance
    /// criterion exercised through the real `resolve_attack` pipeline.
    #[test]
    fn target_and_hit_triggers_hit_overridden_set_then_cleared() {
        use crate::invuln::AttackAttrSet;
        let (mut a, a_air) = make_attacker(); // HitDef attr defaults to S, NA
        let (mut d, d_air) = make_defender();
        assert!(!d.hit_overridden, "no hit taken yet → flag clear");

        // Arm a matching override and connect: the flag latches on.
        d.hit_overrides
            .arm(0, AttackAttrSet::parse("S, NA"), 700, 30);
        let states = HashMap::new();
        resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("override fires");
        assert!(
            d.hit_overridden,
            "an override-redirected get-hit sets HitOverridden"
        );

        // A second, normal (non-overridden) hit replaces the reaction and clears
        // the flag — the slot was consumed, so this hit is no longer overridden.
        let (mut a2, a_air2) = make_attacker();
        resolve_attack(&mut a2, &a_air2, &mut d, &d_air, &states).expect("normal hit");
        assert!(!d.hit_overridden, "a normal get-hit clears HitOverridden");
    }

    // ---- #23: fall.damage / fall.xvelocity flow from HitDef -> GetHitVars --

    /// Builds a single-controller `[State n]` with the given controller `type`
    /// and a single always-true trigger (`trigger1 = 1`), plus a trivial
    /// one-frame action `0`. Lets the combat tests drive a real get-hit
    /// controller through [`Character::tick_with`] without the executor crate's
    /// private test harness.
    fn state_with_ctrl(number: i32, ctrl_type: &str) -> (HashMap<i32, CompiledState>, AirFile) {
        use crate::loader::{
            CompiledController, CompiledExpr, CompiledState, CompiledTriggerGroup,
        };
        let controller = CompiledController {
            state_number: number,
            label: String::new(),
            controller_type: Some(ctrl_type.to_string()),
            triggerall: Vec::new(),
            triggers: vec![CompiledTriggerGroup {
                number: 1,
                conditions: vec![CompiledExpr::compile("1")],
            }],
            persistent: None,
            ignorehitpause: None,
            params: HashMap::new(),
        };
        let state = CompiledState {
            number,
            state_type: None,
            movetype: None,
            physics: None,
            anim: None,
            ctrl: None,
            velset: None,
            poweradd: None,
            sprpriority: None,
            juggle: None,
            facep2: None,
            hitdefpersist: None,
            movehitpersist: None,
            controllers: vec![controller],
        };
        let mut states = HashMap::new();
        states.insert(number, state);
        let air = air_with(0, Vec::new(), Vec::new());
        (states, air)
    }

    /// End-to-end (#23): a falling HitDef carries its authored `fall.damage` /
    /// `fall.xvelocity` onto the defender's [`GetHitVars`] in `resolve_attack`,
    /// and then the defender's `HitFallDamage` controller subtracts that value
    /// from life — proving the controller is NOT fed a constant 0 on real
    /// content (KFM authors `fall.damage = 70`).
    #[test]
    fn falling_hit_propagates_fall_damage_then_hitfalldamage_drops_life() {
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.fall = true;
            hd.fall_yvelocity = -7.0;
            hd.fall_xvelocity = Some(-2.5);
            hd.fall_damage = 70; // KFM's authored sweep value.
        }
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();

        resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");

        // The fall vars were carried from the HitDef (not the hardcoded 0).
        assert!(
            d.get_hit_vars.fall != 0,
            "defender is in a falling reaction"
        );
        assert_eq!(
            d.get_hit_vars.fall_damage, 70,
            "fall.damage carried from HitDef"
        );
        assert!(
            (d.get_hit_vars.fall_yvel - (-7.0)).abs() < 1e-4,
            "fall.yvelocity carried"
        );
        assert!(
            (d.get_hit_vars.fall_xvel - (-2.5)).abs() < 1e-4,
            "authored fall.xvelocity carried (not the knockback X)"
        );
        // Surfaces through the evaluator too (the get-hit state reads it this way).
        assert_eq!(ev_against("GetHitVar(fall.damage)", &d), Value::Int(70));

        // Now tick the defender's HitFallDamage controller on the populated vars:
        // life must actually drop by 70 (proves the controller is not inert).
        // Clear the post-hit freeze so normal controllers run this tick (the
        // landing happens after hit-stun elapses in real play).
        let life_before = d.life;
        d.hitpause = 0;
        d.shaketime = 0;
        let (fall_states, fall_air) = state_with_ctrl(5050, "HitFallDamage");
        d.state_no = 5050;
        d.state_time = 0;
        d.tick_with(&fall_states, &fall_air, None, StageView::default());
        assert_eq!(
            d.life,
            life_before - 70,
            "HitFallDamage subtracts the HitDef's authored fall.damage on landing"
        );
    }

    /// When the author omits `fall.xvelocity` (the common case), the fall X
    /// velocity falls back to the imparted knockback X — MUGEN's "no change".
    #[test]
    fn falling_hit_without_authored_xvel_uses_knockback_x() {
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.fall = true;
            hd.fall_yvelocity = -6.0;
            hd.fall_xvelocity = None; // not authored
            hd.fall_damage = 0;
        }
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();

        resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        // fall_xvel mirrors the (mirrored) knockback X, which is non-zero here.
        assert!(
            d.get_hit_vars.fall_xvel.abs() > 0.0,
            "absent fall.xvelocity defaults to the knockback X"
        );
        assert_eq!(d.get_hit_vars.fall_damage, 0, "no fall.damage authored");
    }

    // ---- P7: GetHitVar(animtype) populated from the HitDef ----------------

    /// Evaluates an expression string against a character (the same eval path
    /// MUGEN's common1 get-hit states use to read `GetHitVar(animtype)`). Panics
    /// only on a test-author parse error.
    fn ev_against(expr: &str, ch: &Character) -> Value {
        let ast = parse_str(expr).expect("test expression should parse");
        eval(&ast, ch as &dyn EvalContext)
    }

    /// P7: a HitDef authored `animtype = Hard` connecting on a GROUNDED defender
    /// sets `gh.animtype == 2` (the Hard code), readable back through the real
    /// `GetHitVar(animtype)` evaluation path the get-hit states use.
    #[test]
    fn ground_hard_animtype_sets_gethitvar_to_two() {
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.animtype = AnimType::Hard;
        }
        let (mut d, d_air) = make_defender();
        d.state_type = StateType::Standing; // grounded -> ground animtype
        let states = HashMap::new();

        resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");

        // Direct field: Hard -> 2.
        assert_eq!(d.get_hit_vars.animtype, 2, "Hard ground animtype code");
        // And via the evaluator (proves the get-hit state will pick non-Light).
        assert_eq!(ev_against("GetHitVar(animtype)", &d), Value::Int(2));
        assert_eq!(ev_against("GetHitVar(animtype) = 2", &d), Value::Int(1));
        assert_ne!(d.get_hit_vars.animtype, 0, "not the always-Light bug value");
    }

    /// P7: a Light HitDef on a grounded defender reads back as 0, and a `Med`
    /// HitDef as 1 — covering the ordinary ground reactions either side of Hard.
    #[test]
    fn ground_light_and_med_animtype_codes() {
        // Light -> 0.
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.animtype = AnimType::Light;
        }
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();
        resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert_eq!(d.get_hit_vars.animtype, 0, "Light ground animtype code");
        assert_eq!(ev_against("GetHitVar(animtype)", &d), Value::Int(0));

        // Medium -> 1.
        let (mut a2, a_air2) = make_attacker();
        if let Some(hd) = a2.active_hitdef.as_mut() {
            hd.animtype = AnimType::Medium;
        }
        let (mut d2, d_air2) = make_defender();
        resolve_attack(&mut a2, &a_air2, &mut d2, &d_air2, &states).expect("connects");
        assert_eq!(d2.get_hit_vars.animtype, 1, "Medium ground animtype code");
        assert_eq!(ev_against("GetHitVar(animtype)", &d2), Value::Int(1));
    }

    /// P7: an AIRBORNE defender uses the HitDef's `air_animtype` (not the ground
    /// `animtype`) — here ground = Light(0) but air = Up(4), and the air value is
    /// the one that lands in `GetHitVar(animtype)`.
    #[test]
    fn airborne_defender_uses_air_animtype() {
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.animtype = AnimType::Light; // ground reaction
            hd.air_animtype = AnimType::Up; // distinct air reaction (code 4)
        }
        let (mut d, d_air) = make_defender();
        d.state_type = StateType::Air; // airborne -> air_animtype path
        let states = HashMap::new();

        resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects air target");
        assert_eq!(
            d.get_hit_vars.animtype, 4,
            "air defender uses air_animtype (Up=4)"
        );
        assert_eq!(ev_against("GetHitVar(animtype)", &d), Value::Int(4));
    }

    /// P7: when `air.animtype` is absent it defaults to the ground `animtype` (the
    /// MUGEN rule). The executor seeds `air_animtype = animtype` on parse; this
    /// asserts the resolve side honors that — an airborne defender hit by a
    /// `animtype = Hard` HitDef whose `air_animtype` was left equal to `animtype`
    /// still reads Hard(2) in the air.
    #[test]
    fn airborne_defaults_to_ground_animtype_when_air_absent() {
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            // Mirror what ctrl_hit_def does when `air.animtype` is absent: both
            // slots carry the parsed ground value.
            hd.animtype = AnimType::Hard;
            hd.air_animtype = AnimType::Hard;
        }
        let (mut d, d_air) = make_defender();
        d.state_type = StateType::Air;
        let states = HashMap::new();

        resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert_eq!(
            d.get_hit_vars.animtype, 2,
            "absent air.animtype defaults to the ground animtype (Hard=2)"
        );
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

        let report = d.tick_with(&states2, &d_air2, None, StageView::default());
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
        let report = a.tick_with(&states2, &a_air, None, StageView::default());
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

    // =====================================================================
    // Audit P9: NotHitBy / HitBy attack-attribute invulnerability windows.
    // resolve_attack consults the DEFENDER's mask vs the ATTACKER HitDef attr
    // before applying a hit; a blocked hit returns None (passes through).
    // =====================================================================

    use crate::invuln::{AttackAttrSet, InvulnMode, InvulnSlot};
    use fp_combat::{AttackAttr, AttackKind, AttackPower, StateClass};

    /// Sets the sample HitDef's `attr` so the mask tests can target a known
    /// attacker attribute (the default sample_hitdef leaves `attr` at S, NA).
    fn attacker_with_attr(a: &mut Character, s: &str) {
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.attr = AttackAttr::parse(s);
        }
    }

    /// Builds an active NotHitBy/HitBy slot covering `value` for `time` ticks.
    fn active_slot(value: &str, mode: InvulnMode, time: i32) -> InvulnSlot {
        InvulnSlot {
            attrs: AttackAttrSet::parse(value),
            mode,
            time_remaining: time,
            ignore_hitpause: false,
        }
    }

    /// P9 AC2/AC3: a defender with a NotHitBy slot covering the attacker's attr
    /// is NOT hit (resolve_attack -> None), takes no damage, no state change, no
    /// hit-pause, and the attacker move is not flagged connected.
    #[test]
    fn nothitby_covering_attr_blocks_the_hit() {
        let (mut a, a_air) = make_attacker();
        attacker_with_attr(&mut a, "S, NA"); // standing normal attack
        let (mut d, d_air) = make_defender();
        // NotHitBy SCA blocks all classes/pairs -> covers S, NA.
        d.invuln.slot1 = active_slot("SCA", InvulnMode::NotHitBy, 5);
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states);
        assert!(res.is_none(), "NotHitBy covering the attr drops the hit");
        assert_eq!(d.life, 1000, "no damage while invulnerable");
        assert_eq!(d.state_no, 0, "no get-hit state change");
        assert_eq!(a.hitpause, 0, "a blocked hit pauses nobody");
        assert!(
            !a.move_connect.contact(),
            "blocked hit does not connect the move"
        );
    }

    /// P9 AC2/AC3: once the NotHitBy window EXPIRES (time_remaining hits 0) the
    /// same attack lands normally.
    #[test]
    fn hit_lands_once_nothitby_window_expires() {
        let (mut a, a_air) = make_attacker();
        attacker_with_attr(&mut a, "S, NA");
        let (mut d, d_air) = make_defender();
        d.invuln.slot1 = active_slot("SCA", InvulnMode::NotHitBy, 1);
        let states = HashMap::new();

        // Active: blocked.
        assert!(resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).is_none());
        assert_eq!(d.life, 1000);

        // Expire the window and re-arm the (still active) move.
        d.invuln.slot1.time_remaining = 0;
        a.move_connect.reset();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states);
        assert!(res.is_some(), "expired window lets the hit land");
        assert_eq!(d.life, 1000 - 30, "full damage once invuln expired");
    }

    /// P9 AC2: NotHitBy that does NOT cover the attacker's attr does not block.
    /// Defender has NotHitBy for throws only; a normal attack still lands.
    #[test]
    fn nothitby_not_covering_attr_allows_the_hit() {
        let (mut a, a_air) = make_attacker();
        attacker_with_attr(&mut a, "S, NA"); // a normal attack, not a throw
        let (mut d, d_air) = make_defender();
        // "Can't be thrown" window: throws only.
        d.invuln.slot1 = active_slot(", NT,ST,HT", InvulnMode::NotHitBy, 12);
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states);
        assert!(
            res.is_some(),
            "a normal attack is not a throw -> not blocked"
        );
        assert_eq!(d.life, 1000 - 30);
    }

    /// P9 AC2: a NotHitBy throw window blocks a THROW HitDef.
    #[test]
    fn nothitby_throw_window_blocks_a_throw() {
        let (mut a, a_air) = make_attacker();
        attacker_with_attr(&mut a, "S, NT"); // a standing normal THROW
        let (mut d, d_air) = make_defender();
        d.invuln.slot1 = active_slot(", NT,ST,HT", InvulnMode::NotHitBy, 12);
        let states = HashMap::new();

        assert!(
            resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).is_none(),
            "the throw window blocks a throw"
        );
        assert_eq!(d.life, 1000);
    }

    /// P9 AC2: HitBy (include) blocks an attr that is NOT in the admitted set.
    /// HitBy admits only throws; a normal attack is blocked.
    #[test]
    fn hitby_excluding_attr_blocks_the_hit() {
        let (mut a, a_air) = make_attacker();
        attacker_with_attr(&mut a, "S, NA"); // a normal attack
        let (mut d, d_air) = make_defender();
        // Can ONLY be hit by throws -> a normal attack is excluded.
        d.invuln.slot1 = active_slot(", NT,ST,HT", InvulnMode::HitBy, 10);
        let states = HashMap::new();

        assert!(
            resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).is_none(),
            "HitBy admitting only throws blocks a normal attack"
        );
        assert_eq!(d.life, 1000);
    }

    /// P9 AC2: HitBy admits an attr that IS in the set (the hit lands).
    #[test]
    fn hitby_including_attr_allows_the_hit() {
        let (mut a, a_air) = make_attacker();
        attacker_with_attr(&mut a, "S, NA");
        let (mut d, d_air) = make_defender();
        // Can be hit by standing normal attacks -> S, NA is admitted.
        d.invuln.slot1 = active_slot("S, NA", InvulnMode::HitBy, 10);
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states);
        assert!(
            res.is_some(),
            "HitBy admitting S,NA lets a S,NA attack land"
        );
        assert_eq!(d.life, 1000 - 30);
    }

    /// P9 AC1/AC2: BOTH slots are enforced — a hit must pass both. Slot 1 admits
    /// the attr (would allow) but slot 2 (NotHitBy covering it) blocks: the hit
    /// is dropped because either slot blocking is enough.
    #[test]
    fn both_slots_enforced_either_can_block() {
        let (mut a, a_air) = make_attacker();
        attacker_with_attr(&mut a, "S, NA");
        let (mut d, d_air) = make_defender();
        // Slot 1: HitBy admits S,NA (allows). Slot 2: NotHitBy SCA (blocks all).
        d.invuln.slot1 = active_slot("S, NA", InvulnMode::HitBy, 10);
        d.invuln.slot2 = active_slot("SCA", InvulnMode::NotHitBy, 10);
        let states = HashMap::new();

        assert!(
            resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).is_none(),
            "slot 2 blocks even though slot 1 would allow"
        );
        assert_eq!(d.life, 1000);
    }

    /// P9 AC2: attack-class matching by state-type AND pair. A NotHitBy limited
    /// to AIR attacks does not block a STANDING attack of the same pair.
    #[test]
    fn attr_match_respects_statetype_class() {
        let (mut a, a_air) = make_attacker();
        attacker_with_attr(&mut a, "S, NA"); // STANDING normal attack
        let (mut d, d_air) = make_defender();
        // Only air normal-attacks are excluded.
        d.invuln.slot1 = active_slot("A, NA", InvulnMode::NotHitBy, 10);
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states);
        assert!(
            res.is_some(),
            "a standing attack is not in the air-only window"
        );
        assert_eq!(d.life, 1000 - 30);
    }

    /// P9 AC3: an unparseable NotHitBy spec is the empty set -> blocks NOTHING
    /// (the MUGEN-safe reading: a garbage NotHitBy never grants invulnerability).
    #[test]
    fn unparseable_nothitby_is_safe_blocks_nothing() {
        let (mut a, a_air) = make_attacker();
        attacker_with_attr(&mut a, "S, NA");
        let (mut d, d_air) = make_defender();
        d.invuln.slot1 = active_slot("garbage!!", InvulnMode::NotHitBy, 10);
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states);
        assert!(res.is_some(), "garbage NotHitBy is inert; the hit lands");
        assert_eq!(d.life, 1000 - 30);
    }

    /// P9 AC3: an unparseable HitBy spec is the empty set -> blocks EVERYTHING
    /// (the MUGEN-safe reading: "can only be hit by <nothing>" = full invuln).
    #[test]
    fn unparseable_hitby_blocks_everything() {
        let (mut a, a_air) = make_attacker();
        attacker_with_attr(&mut a, "S, NA");
        let (mut d, d_air) = make_defender();
        d.invuln.slot1 = active_slot("garbage!!", InvulnMode::HitBy, 10);
        let states = HashMap::new();

        assert!(
            resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).is_none(),
            "garbage HitBy admits nothing -> blocks every hit"
        );
        assert_eq!(d.life, 1000);
    }

    /// P9 AC2: an inactive (expired) slot never blocks, even if it would cover
    /// the attr. Default masks (all inactive) leave existing behavior intact.
    #[test]
    fn inactive_slot_does_not_block() {
        let (mut a, a_air) = make_attacker();
        attacker_with_attr(&mut a, "S, NA");
        let (mut d, d_air) = make_defender();
        // Covers the attr but time_remaining = 0 -> inactive.
        d.invuln.slot1 = active_slot("SCA", InvulnMode::NotHitBy, 0);
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states);
        assert!(res.is_some(), "an inactive slot blocks nothing");
        assert_eq!(d.life, 1000 - 30);
    }

    /// P9: a wildcard NotHitBy (`*`) blocks any attr.
    #[test]
    fn wildcard_nothitby_blocks_any_attr() {
        for spec in ["*", "SCA"] {
            for attr_s in ["S, NA", "C, HP", "A, ST"] {
                let (mut a, a_air) = make_attacker();
                attacker_with_attr(&mut a, attr_s);
                let (mut d, d_air) = make_defender();
                d.invuln.slot1 = active_slot(spec, InvulnMode::NotHitBy, 5);
                let states = HashMap::new();
                assert!(
                    resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).is_none(),
                    "NotHitBy {spec:?} should block attr {attr_s:?}"
                );
            }
        }
        // Verify the AttackAttr letters parse the way we expect.
        assert_eq!(
            AttackAttr::parse("A, ST").class,
            StateClass::Air,
            "sanity: A class parses"
        );
        assert_eq!(AttackAttr::parse("C, HP").power, AttackPower::Hyper);
        assert_eq!(AttackAttr::parse("A, ST").kind, AttackKind::Throw);
    }

    // ---- Proctor (Audit P9): additional resolve_attack edge semantics -----
    // Forge's block above covers NotHitBy/HitBy block-vs-allow, both-slots,
    // statetype matching, expiry, unparseable, and inactive. These pin the
    // pass-through invariants the spec calls out but no test asserted: a blocked
    // hit establishes NO target and leaves the attacker move un-connected (so a
    // later, vulnerable contact still lands), slot 1 blocking on its own, and a
    // HitBy that excludes by STATETYPE only.

    /// P9 (Proctor): a NotHitBy-blocked hit passes through cleanly — it sets NO
    /// target on the attacker and does not flag the move connected, so an
    /// IDENTICAL second contact (once the window expires) still lands. This is the
    /// "the attack simply passes through" MUGEN rule: invulnerability is not a
    /// connection.
    #[test]
    fn blocked_hit_sets_no_target_and_later_hit_still_lands() {
        let (mut a, a_air) = make_attacker();
        attacker_with_attr(&mut a, "S, NA");
        let (mut d, d_air) = make_defender();
        d.invuln.slot1 = active_slot("SCA", InvulnMode::NotHitBy, 1);
        let states = HashMap::new();

        // Blocked: no contact bookkeeping at all.
        assert!(resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).is_none());
        assert!(!a.has_target, "an invuln-blocked hit establishes no target");
        assert!(
            !a.move_connect.contact(),
            "blocked hit does not connect the move"
        );

        // Window expires; the SAME move (never marked connected, so hitonce does
        // NOT forbid it) now lands on the no-longer-invulnerable defender.
        d.invuln.slot1.time_remaining = 0;
        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states);
        assert!(
            res.is_some(),
            "the pass-through move still lands after expiry"
        );
        assert!(
            a.has_target,
            "the landing hit finally establishes the target"
        );
        assert_eq!(d.life, 1000 - 30);
    }

    /// P9 (Proctor): slot 1 blocking on its own drops the hit even when slot 2 is
    /// inactive. (Forge's `both_slots_enforced_either_can_block` proves slot 2
    /// blocking; this proves the slot-1 side of "either slot blocking is enough".)
    #[test]
    fn slot1_blocks_alone_with_slot2_inactive() {
        let (mut a, a_air) = make_attacker();
        attacker_with_attr(&mut a, "S, NA");
        let (mut d, d_air) = make_defender();
        d.invuln.slot1 = active_slot("SCA", InvulnMode::NotHitBy, 6); // blocks
        d.invuln.slot2 = active_slot("SCA", InvulnMode::NotHitBy, 0); // inactive
        let states = HashMap::new();

        assert!(
            resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).is_none(),
            "slot 1 alone blocks; the inactive slot 2 is irrelevant"
        );
        assert_eq!(d.life, 1000);
    }

    /// P9 (Proctor): HitBy admitting only AIR attacks blocks a STANDING attack of
    /// the otherwise-admitted pair — the include filter is by state-type too, not
    /// just the PK pair. (`A, NA` admits only air normal-attacks; a `S, NA`
    /// attacker is excluded and therefore blocked.)
    #[test]
    fn hitby_excludes_by_statetype_not_just_pair() {
        let (mut a, a_air) = make_attacker();
        attacker_with_attr(&mut a, "S, NA"); // STANDING normal attack
        let (mut d, d_air) = make_defender();
        d.invuln.slot1 = active_slot("A, NA", InvulnMode::HitBy, 10); // only air NA admitted
        let states = HashMap::new();

        assert!(
            resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).is_none(),
            "HitBy(air NA) excludes a standing NA -> blocked"
        );
        assert_eq!(d.life, 1000);

        // The same HitBy admits the matching AIR attack (control: it is NOT a
        // blanket block — the include filter genuinely lets the listed attr in).
        let (mut a2, a_air2) = make_attacker();
        attacker_with_attr(&mut a2, "A, NA"); // AIR normal attack (admitted)
        let (mut d2, d_air2) = make_defender();
        d2.invuln.slot1 = active_slot("A, NA", InvulnMode::HitBy, 10);
        let res = resolve_attack(&mut a2, &a_air2, &mut d2, &d_air2, &states);
        assert!(res.is_some(), "the admitted air NA attack lands");
        assert_eq!(d2.life, 1000 - 30);
    }

    /// P9 (Proctor): an empty (default) mask never blocks — the existing combat
    /// suite passes only because the default `InvulnMask` is inert. Pin it
    /// explicitly so a future default change cannot silently grant invuln.
    #[test]
    fn default_mask_blocks_nothing() {
        let (mut a, a_air) = make_attacker();
        attacker_with_attr(&mut a, "S, NA");
        let (mut d, d_air) = make_defender();
        assert_eq!(
            d.invuln,
            crate::invuln::InvulnMask::default(),
            "fresh defender has the default mask"
        );
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states);
        assert!(
            res.is_some(),
            "the default (all-inactive) mask never blocks"
        );
        assert_eq!(d.life, 1000 - 30);
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
                assert_eq!(
                    defender.life,
                    life_before - 30,
                    "real KFM punch dealt 30 dmg"
                );
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

    /// P7 gated real-KFM: find a real HitDef state that authors a non-Light
    /// `animtype` (KFM's heavy attacks use `animtype = Hard`), parse that real
    /// authored token through the SAME [`fp_combat::AnimType::parse`] the HitDef
    /// controller uses, drive a connect on a grounded defender, and assert
    /// `GetHitVar(animtype) != 0` — proving the defender's common1 get-hit state
    /// will branch to a non-Light reaction. Skips cleanly when test-assets/ is
    /// absent or no non-Light HitDef is found.
    ///
    /// We read the controller's raw `animtype` and build the HitDef directly
    /// (rather than firing the controller) because KFM's heavy HitDefs gate on
    /// `p2bodydist`, which needs an opponent handle this single-entity resolve
    /// path does not thread — but the *authoring → AnimType → gh.animtype* chain
    /// under test is exactly the same.
    #[test]
    fn real_kfm_hard_animtype_drives_nonlight_gethitvar() {
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

        // Find a HitDef controller whose authored `animtype` parses to a
        // non-Light reaction (KFM uses `animtype = Hard` on its heavy attacks).
        let mut found: Option<fp_combat::AnimType> = None;
        'scan: for state in lc.states.values() {
            for c in &state.controllers {
                let is_hitdef = c
                    .controller_type
                    .as_deref()
                    .is_some_and(|t| t.eq_ignore_ascii_case("HitDef"));
                if !is_hitdef {
                    continue;
                }
                if let Some(p) = c.params.get("animtype") {
                    let at = fp_combat::AnimType::parse(p.raw());
                    if at != fp_combat::AnimType::Light {
                        found = Some(at);
                        break 'scan;
                    }
                }
            }
        }
        let Some(animtype) = found else {
            eprintln!("skipping: no non-Light HitDef animtype found in KFM");
            return;
        };
        assert_ne!(animtype.code(), 0, "found a non-Light authored animtype");

        // Verify the real fixture carries the boxes we need for a connection.
        let clsn1 = current_frame_clsn1(&lc.air, 200, 2);
        let clsn2 = current_frame_clsn2(&lc.air, 0, 0);
        if clsn1.is_empty() || clsn2.is_empty() {
            eprintln!("skipping: KFM action 200/0 frames lack expected Clsn boxes");
            return;
        }

        // Build a HitDef carrying the REAL authored ground animtype, on the
        // punch's active frame, and connect it on a grounded KFM defender.
        let mut attacker = Character::with_constants(lc.constants);
        attacker.anim = 200;
        attacker.anim_elem = 2;
        attacker.pos = Vec2::new(0.0, 0.0);
        attacker.facing = Facing::Right;
        attacker.move_type = MoveType::Attack;
        let mut hd = sample_hitdef();
        hd.animtype = animtype; // the real authored reaction (e.g. Hard)
        attacker.active_hitdef = Some(hd);

        let mut connected = false;
        for dx in 0..=120 {
            let mut defender = Character::with_constants(lc.constants);
            defender.anim = 0;
            defender.anim_elem = 0;
            defender.facing = Facing::Left;
            defender.pos = Vec2::new(dx as f32, 0.0);
            defender.life = lc.constants.life_max;
            defender.state_type = StateType::Standing; // grounded -> ground animtype
            attacker.move_connect.reset();

            if resolve_attack(&mut attacker, &lc.air, &mut defender, &lc.air, &lc.states).is_some()
            {
                assert_ne!(
                    defender.get_hit_vars.animtype, 0,
                    "real KFM Hard HitDef must set a non-Light GetHitVar(animtype)"
                );
                assert_eq!(
                    defender.get_hit_vars.animtype,
                    animtype.code(),
                    "gh.animtype matches the authored reaction code"
                );
                connected = true;
                break;
            }
        }
        assert!(connected, "real KFM punch should connect at some offset");
    }

    // =====================================================================
    // Proctor (task 6.5): additional hit-pause coverage for resolve_attack.
    // The symmetric `pausetime { p1: 8, p2: 8 }` of `sample_hitdef` cannot
    // distinguish which side reads `p1` versus `p2`; these tests use an
    // ASYMMETRIC pausetime and exercise the guard fallback so the two sides
    // are pinned independently.
    // =====================================================================

    /// AC1: with an ASYMMETRIC `pausetime`, the attacker is paused from `p1` and
    /// the defender from `p2` — proving the two sides are not accidentally swapped
    /// or sharing one value (the symmetric `sample_hitdef` cannot show this).
    #[test]
    fn asymmetric_pausetime_attacker_from_p1_defender_from_p2() {
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.pausetime = PauseTime { p1: 6, p2: 11 };
        }
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert_eq!(a.hitpause, 6, "attacker hitpause from pausetime.p1");
        assert_eq!(d.hitpause, 11, "defender hitpause from pausetime.p2");
        assert_eq!(d.shaketime, 11, "defender shaketime from pausetime.p2");
        // The resolution recipe carries the same per-side values.
        assert_eq!(res.attacker_hitpause, 6);
        assert_eq!(res.defender_hitpause, 11);
    }

    /// AC1: the DEFENDER side also uses `max(current, new)` — a longer existing
    /// defender pause is preserved even when the attacker's would be replaced.
    /// (The existing `re_armed_*` test pre-loads BOTH sides equally; this pins the
    /// defender's `max` independently with an asymmetric existing pause.)
    #[test]
    fn defender_pause_uses_max_independently_of_attacker() {
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.pausetime = PauseTime { p1: 8, p2: 8 };
        }
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();

        // Defender already mid-freeze for longer than this hit would set; attacker
        // starts fresh.
        d.hitpause = 15;
        d.shaketime = 15;
        a.hitpause = 0;

        resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert_eq!(a.hitpause, 8, "fresh attacker takes the new pausetime.p1");
        assert_eq!(
            d.hitpause, 15,
            "longer existing defender pause is preserved"
        );
        assert_eq!(
            d.shaketime, 15,
            "longer existing defender shake is preserved"
        );
    }

    /// AC1: a GUARDED hit still pauses both participants. The documented fallback
    /// (no distinct `guard.pausetime` in `fp-combat`) means a guard uses the
    /// ordinary `pausetime`; verify both sides are set on a block.
    #[test]
    fn guard_sets_hitpause_via_pausetime_fallback() {
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.pausetime = PauseTime { p1: 9, p2: 14 };
        }
        let (mut d, d_air) = make_defender();
        d.holding_back = true; // guardflag MA admits a standing block
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("guards");
        assert_eq!(res.result, HitResult::Guard);
        // Guard falls back to `pausetime` (p1 on the attacker, p2 on the defender).
        assert_eq!(
            a.hitpause, 9,
            "guard pauses the attacker (pausetime.p1 fallback)"
        );
        assert_eq!(
            d.hitpause, 14,
            "guard pauses the defender (pausetime.p2 fallback)"
        );
        assert_eq!(
            d.shaketime, 14,
            "guard shaketime from the pausetime.p2 fallback"
        );
    }

    /// AC1: a zero `pausetime` HitDef connects (damage applies) but freezes
    /// NEITHER side — the gate is a no-op when the move authored no hit-stop.
    #[test]
    fn zero_pausetime_hit_connects_but_pauses_neither() {
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.pausetime = PauseTime { p1: 0, p2: 0 };
        }
        let (mut d, d_air) = make_defender();
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert_eq!(res.result, HitResult::Hit);
        assert_eq!(d.life, 1000 - 30, "damage still applies with no hit-stop");
        assert_eq!(
            a.hitpause, 0,
            "zero pausetime.p1 leaves the attacker unpaused"
        );
        assert_eq!(
            d.hitpause, 0,
            "zero pausetime.p2 leaves the defender unpaused"
        );
        assert_eq!(d.shaketime, 0, "zero pausetime.p2 leaves no shake");
    }

    /// AC4: end-to-end across both sides. A hit with asymmetric pausetime, then
    /// each character is ticked through its OWN freeze to zero and the resume tick
    /// runs normally. Proves resolve_attack -> executor gate integrate with the
    /// per-side durations actually counted down.
    #[test]
    fn both_sides_count_their_own_freeze_down_then_resume() {
        let (mut a, a_air) = make_attacker();
        if let Some(hd) = a.active_hitdef.as_mut() {
            hd.pausetime = PauseTime { p1: 2, p2: 3 };
        }
        let (mut d, d_air) = make_defender();
        let states: HashMap<i32, CompiledState> = HashMap::new();
        resolve_attack(&mut a, &a_air, &mut d, &d_air, &states).expect("connects");
        assert_eq!(a.hitpause, 2);
        assert_eq!(d.hitpause, 3);

        // Attacker: 2 frozen ticks, then resume.
        assert!(
            a.tick_with(&states, &a_air, None, StageView::default())
                .hitpaused
        );
        assert_eq!(a.hitpause, 1);
        assert!(
            a.tick_with(&states, &a_air, None, StageView::default())
                .hitpaused
        );
        assert_eq!(a.hitpause, 0);
        assert!(
            !a.tick_with(&states, &a_air, None, StageView::default())
                .hitpaused,
            "attacker resumes after 2 ticks"
        );

        // Defender: 3 frozen ticks, then resume.
        assert!(
            d.tick_with(&states, &d_air, None, StageView::default())
                .hitpaused
        );
        assert!(
            d.tick_with(&states, &d_air, None, StageView::default())
                .hitpaused
        );
        assert!(
            d.tick_with(&states, &d_air, None, StageView::default())
                .hitpaused
        );
        assert_eq!(d.hitpause, 0);
        assert!(
            !d.tick_with(&states, &d_air, None, StageView::default())
                .hitpaused,
            "defender resumes after 3 ticks"
        );
    }

    /// P9 AC3 gated real-KFM: drive KFM's get-up state (`common1` `[Statedef 5120]`,
    /// which carries `[State 5120, 3] type = NotHitBy / value = SCA / time = 1`),
    /// tick the defender so the controller fires, confirm the NotHitBy slot is
    /// active, then confirm a real connecting attack is **ignored** during the
    /// window — and lands once the window expires. Skips cleanly when KFM assets
    /// are absent or the expected state/boxes are missing.
    #[test]
    fn real_kfm_getup_nothitby_ignores_hit_during_window() {
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

        // Confirm the get-up state authors a NotHitBy controller; skip otherwise.
        let Some(state_5120) = lc.states.get(&5120) else {
            eprintln!("skipping: KFM common1 has no [Statedef 5120]");
            return;
        };
        let has_nothitby = state_5120.controllers.iter().any(|c| {
            c.controller_type
                .as_deref()
                .is_some_and(|t| t.eq_ignore_ascii_case("NotHitBy"))
        });
        if !has_nothitby {
            eprintln!("skipping: KFM [Statedef 5120] has no NotHitBy controller");
            return;
        }

        // The boxes resolve_attack needs: KFM punch Clsn1 (action 200 elem 2) and
        // an idle Clsn2 (action 0). Skip if a future asset edit removes them.
        let clsn1 = current_frame_clsn1(&lc.air, 200, 2);
        let clsn2 = current_frame_clsn2(&lc.air, 0, 0);
        if clsn1.is_empty() || clsn2.is_empty() {
            eprintln!("skipping: KFM action 200/0 frames lack expected Clsn boxes");
            return;
        }

        // Put the defender into the get-up state (5120) and tick once so its
        // NotHitBy controller fires. The slot is decremented at the top of the
        // tick (from 0, stays 0) THEN set to time=1 by the controller, so after
        // the tick it is active.
        let mut defender = Character::with_constants(lc.constants);
        defender.change_state(&lc.states, 5120);
        defender.anim = 0;
        defender.anim_elem = 0;
        defender.facing = Facing::Left;
        defender.life = lc.constants.life_max;
        let _ = defender.tick(&lc, None, StageView::default());
        assert!(
            defender.invuln.slot1.is_active() || defender.invuln.slot2.is_active(),
            "KFM get-up state must arm a NotHitBy slot"
        );

        // A KFM stand-punch attacker (S, NA) — the SCA NotHitBy window covers it.
        let mut attacker = Character::with_constants(lc.constants);
        attacker.state_no = 200;
        attacker.anim = 200;
        attacker.anim_elem = 2;
        attacker.facing = Facing::Right;
        attacker.move_type = MoveType::Attack;
        let mut hd = sample_hitdef();
        hd.attr = AttackAttr::parse("S, NA");
        attacker.active_hitdef = Some(hd);

        // Sweep offsets until the real boxes overlap; assert the hit is IGNORED
        // (None) while the NotHitBy window is active.
        let mut overlapped = false;
        for dx in 0..=120 {
            let mut d = clone_for_offset(&defender, dx as f32, lc.constants.life_max);
            attacker.move_connect.reset();
            // Force boxes to the punch's active frame regardless of any anim
            // advance during the single tick above.
            d.anim = 0;
            d.anim_elem = 0;
            let res = resolve_attack(&mut attacker, &lc.air, &mut d, &lc.air, &lc.states);
            // While invulnerable the boxes may overlap but the hit must be None.
            if d.invuln.blocks(&AttackAttr::parse("S, NA")) {
                // Detect overlap independently of the mask by clearing it.
                let mut d_clear = clone_for_offset(&d, dx as f32, lc.constants.life_max);
                d_clear.invuln = crate::invuln::InvulnMask::default();
                d_clear.anim = 0;
                d_clear.anim_elem = 0;
                attacker.move_connect.reset();
                if resolve_attack(&mut attacker, &lc.air, &mut d_clear, &lc.air, &lc.states)
                    .is_some()
                {
                    overlapped = true;
                    assert!(res.is_none(), "NotHitBy window ignores the connecting hit");
                    assert_eq!(
                        d.life, lc.constants.life_max,
                        "no damage while invulnerable"
                    );
                    break;
                }
            }
        }
        assert!(
            overlapped,
            "real KFM punch should geometrically overlap the get-up defender at some offset"
        );
    }

    /// Test helper: clones `src` at a new x offset with reset life and a fresh
    /// (un-paused) baseline, carrying over the invuln mask, for the box-sweep
    /// gated tests.
    fn clone_for_offset(src: &Character, x: f32, life: i32) -> Character {
        let mut c = Character::with_constants(src.constants);
        c.state_no = src.state_no;
        c.state_type = src.state_type;
        c.facing = src.facing;
        c.invuln = src.invuln.clone();
        c.pos = Vec2::new(x, 0.0);
        c.life = life;
        c
    }

    // ====================================================================
    // #16: air-juggle limit. The attacker's `cur_juggle_cost` (set on entry
    // from the move's `[Statedef] juggle`) is charged to the AIRBORNE defender's
    // `juggle_points` pool; an exhausted pool drops the hit.
    // ====================================================================

    /// An airborne defender's juggle pool is decremented by the attacker's
    /// per-move juggle cost on a connecting hit.
    #[test]
    fn airborne_hit_decrements_juggle_points() {
        let (mut a, a_air) = make_attacker();
        a.cur_juggle_cost = 4;
        let (mut d, d_air) = make_defender();
        d.state_type = StateType::Air; // airborne defender
        d.juggle_points = 15;
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states);
        assert!(
            res.is_some(),
            "first juggle hit lands while the pool can pay"
        );
        assert_eq!(
            d.juggle_points, 11,
            "pool charged the move's juggle cost (15 - 4)"
        );
    }

    /// When the pool can no longer pay the move's juggle cost, the hit is dropped
    /// (no damage, no connection) — the juggle limit.
    #[test]
    fn airborne_hit_blocked_when_juggle_exhausted() {
        let (mut a, a_air) = make_attacker();
        a.cur_juggle_cost = 4;
        let (mut d, d_air) = make_defender();
        d.state_type = StateType::Air;
        d.juggle_points = 3; // less than the 4-point cost
        let life_before = d.life;
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states);
        assert!(res.is_none(), "insufficient juggle drops the hit");
        assert_eq!(
            d.life, life_before,
            "no damage applied on a juggle-dropped hit"
        );
        assert_eq!(
            d.juggle_points, 3,
            "pool not charged when the hit is dropped"
        );
        assert!(
            !a.move_connect.contact(),
            "a dropped juggle hit does not connect"
        );
    }

    /// A GROUNDED defender is never juggle-gated, even with an empty pool and a
    /// costly move.
    #[test]
    fn grounded_hit_ignores_juggle_pool() {
        let (mut a, a_air) = make_attacker();
        a.cur_juggle_cost = 30;
        let (mut d, d_air) = make_defender();
        d.state_type = StateType::Standing; // grounded
        d.juggle_points = 0; // empty pool must not matter on the ground
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states);
        assert!(
            res.is_some(),
            "a grounded hit lands regardless of juggle points"
        );
        assert_eq!(d.juggle_points, 0, "grounded hit does not spend juggle");
    }

    /// A move with `juggle = 0` (no cost) is never juggle-blocked, even airborne
    /// with an empty pool.
    #[test]
    fn zero_cost_move_is_never_juggle_blocked() {
        let (mut a, a_air) = make_attacker();
        a.cur_juggle_cost = 0; // non-attack / cost-free move
        let (mut d, d_air) = make_defender();
        d.state_type = StateType::Air;
        d.juggle_points = 0;
        let states = HashMap::new();

        let res = resolve_attack(&mut a, &a_air, &mut d, &d_air, &states);
        assert!(
            res.is_some(),
            "a zero-cost move lands even with an empty pool"
        );
    }
}
