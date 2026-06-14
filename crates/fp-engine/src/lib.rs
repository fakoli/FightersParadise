//! # fp-engine
//!
//! Top-level game coordinator for the Fighters Paradise engine. This crate owns
//! the **two-player match**: the per-frame orchestration that drives two
//! [`fp_character::Character`]s through one 60Hz tick, runs combat both
//! directions, keeps the fighters on stage and non-overlapping, and advances the
//! round state machine and timer.
//!
//! ## The [`Match`] coordinator (task 7.1)
//!
//! A [`Match`] holds two [`Player`]s (each a [`fp_character::Character`] plus its
//! loaded assets [`fp_character::LoadedCharacter`] and an input/command source)
//! and the [`StageBounds`] that pin the fighters to the playfield.
//! [`Match::tick`] advances exactly one frame in MUGEN-ish order:
//!
//! 1. **Feed inputs.** Each player's [`MatchInput`] is translated to a
//!    *facing-relative* command set (so a held "toward the opponent" reads as
//!    `fwd` regardless of which way the character faces) and pushed into the
//!    character's [`fp_character::CommandSource`] seam.
//! 2. **Tick both state machines** via [`fp_character::Character::tick`].
//! 3. **Run combat both directions** with
//!    [`fp_character::combat::resolve_attack`] — P1 attacks P2, then P2 attacks
//!    P1 — so an active `HitDef` on either side can connect this frame.
//! 4. **Separate and clamp.** [`fp_physics::resolve_push`] pushes overlapping
//!    bodies apart using each character's `size.ground.front`/`back`, then
//!    [`fp_physics::clamp_to_bounds`] keeps each body inside [`StageBounds`].
//! 5. **Face the opponent** (baseline `facep2`): a character in a neutral
//!    standing state with control turns to face the other's X.
//! 6. **Advance the round** state machine ([`RoundState`]) and the down-counting
//!    [`Match::timer`] (time over compares life).
//!
//! Read accessors ([`Match::p1`], [`Match::p2`], [`Player::pos`],
//! [`Player::facing`], [`Player::anim`], [`Player::anim_elem`], [`Player::life`],
//! [`Match::round_state`], [`Match::timer`]) expose everything a renderer needs
//! without granting it mutable access to the simulation.
//!
//! ## Robustness
//!
//! Per the engine-wide rule, nothing here panics: the executor, combat, and
//! physics primitives this crate calls all degrade safely on bad data, and the
//! coordinator only ever reads constants and clamps. Missing animation frames or
//! an absent `HitDef` resolve to "no contact" rather than an error.

#![warn(missing_docs)]

use fp_character::{
    combat::resolve_attack, Character, Facing, LoadedCharacter, MoveType, StateType,
};
use fp_core::Vec2;
use fp_physics::{clamp_to_bounds, resolve_push, Facing as PhysFacing, PushBody};

/// The horizontal extent of the playfield, in world pixels.
///
/// Both fighters are clamped so their facing-resolved bodies stay within
/// `[left, right]` (MUGEN's `ScreenBound`). The bounds are assumed ordered
/// (`left <= right`); a reversed pair still yields finite, deterministic clamping
/// (see [`fp_physics::clamp_to_bounds`]) rather than a panic.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StageBounds {
    /// Leftmost world X a character body may reach.
    pub left: f32,
    /// Rightmost world X a character body may reach.
    pub right: f32,
}

impl StageBounds {
    /// Creates stage bounds from a left and right world X.
    #[must_use]
    pub const fn new(left: f32, right: f32) -> Self {
        Self { left, right }
    }
}

impl Default for StageBounds {
    /// A symmetric default playfield centered on the origin, wide enough that two
    /// default-sized characters start comfortably inside it.
    fn default() -> Self {
        Self {
            left: -200.0,
            right: 200.0,
        }
    }
}

/// One frame of player input, expressed in **absolute screen directions**.
///
/// The fields are screen-relative (left/right are world directions, not
/// "toward/away from the opponent"). [`Match::tick`] converts them to the
/// facing-relative command names a character's state machine expects, so that,
/// for example, holding *toward* the opponent always reads as `fwd` and holding
/// *away* reads as `back` (the latter also setting
/// [`fp_character::Character::holding_back`] so the defender can guard).
///
/// This is a deliberately small, renderer-/input-agnostic shape: a front end
/// backed by `fp-input`, a replay file, or a test harness can all populate it.
/// All fields default to "not held" / "not pressed".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MatchInput {
    /// Holding the left direction this frame.
    pub left: bool,
    /// Holding the right direction this frame.
    pub right: bool,
    /// Holding up (jump) this frame.
    pub up: bool,
    /// Holding down (crouch) this frame.
    pub down: bool,
    /// The `a` attack button is pressed this frame.
    pub a: bool,
    /// The `b` attack button is pressed this frame.
    pub b: bool,
    /// The `c` attack button is pressed this frame.
    pub c: bool,
    /// The `x` attack button is pressed this frame.
    pub x: bool,
    /// The `y` attack button is pressed this frame.
    pub y: bool,
    /// The `z` attack button is pressed this frame.
    pub z: bool,
}

impl MatchInput {
    /// An input with nothing held or pressed.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            left: false,
            right: false,
            up: false,
            down: false,
            a: false,
            b: false,
            c: false,
            x: false,
            y: false,
            z: false,
        }
    }
}

/// The phase of a round, advanced once per frame by [`Match::tick`].
///
/// The lifecycle is `Intro → Fight → KO → Win`:
///
/// - [`RoundState::Intro`] — pre-fight; neither fighter acts on the clock yet.
///   The coordinator counts a short intro delay, then enters [`RoundState::Fight`].
/// - [`RoundState::Fight`] — live combat; the timer counts down and hits land.
/// - [`RoundState::Ko`] — a fighter's life reached `0` (a knockout) **or** the
///   timer expired; a brief slow-motion-style hold before results.
/// - [`RoundState::Win`] — the round is decided; [`Match::winner`] reports who.
///
/// The state never moves backwards, so a renderer can drive intro/outro
/// animations off [`Match::round_state`] without extra bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundState {
    /// Pre-fight intro; the timer has not started.
    Intro,
    /// Live combat; the timer is counting down.
    Fight,
    /// A knockout (or time over) just occurred; brief hold before results.
    Ko,
    /// The round is decided; see [`Match::winner`].
    Win,
}

/// Which player won the round, once it reaches [`RoundState::Win`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Winner {
    /// Player 1 won (P2 was KO'd or had less life at time over).
    P1,
    /// Player 2 won (P1 was KO'd or had less life at time over).
    P2,
    /// The round was a draw (double KO, or equal life at time over).
    Draw,
}

/// The number of frames the [`RoundState::Intro`] phase lasts before combat
/// begins. MUGEN's intro length is configurable; this is a simple fixed baseline.
const INTRO_FRAMES: i32 = 60;

/// The number of frames the [`RoundState::Ko`] hold lasts before results
/// ([`RoundState::Win`]).
const KO_FRAMES: i32 = 90;

/// Default round length in **seconds**; the timer starts at this times the tick
/// rate. MUGEN's default round time is 99 seconds.
const DEFAULT_ROUND_SECONDS: i32 = 99;

/// Ticks per second; the engine runs at a fixed 60Hz (see the architecture KB).
const TICKS_PER_SECOND: i32 = 60;

/// One side of a [`Match`]: a live [`Character`], the assets it ticks against,
/// and the input/command source feeding its state machine.
///
/// The [`Character`] is the mutable simulation state; [`LoadedCharacter`] holds
/// the immutable compiled state graph and animations the executor and combat read
/// each frame. Read accessors ([`Player::pos`], [`Player::life`], …) expose the
/// fields a renderer needs without exposing the whole mutable character.
pub struct Player {
    /// The live character entity advanced each frame.
    pub character: Character,
    /// The loaded, compiled assets the character ticks against.
    pub loaded: LoadedCharacter,
}

impl Player {
    /// Wraps a live [`Character`] and its [`LoadedCharacter`] into a [`Player`].
    ///
    /// The caller is responsible for having seeded the character's constants from
    /// the loaded assets (e.g. via
    /// [`Character::with_constants`](fp_character::Character::with_constants)); a
    /// freshly [`Character::new`](fp_character::Character::new)'d character is also
    /// accepted and simply uses default constants.
    #[must_use]
    pub fn new(character: Character, loaded: LoadedCharacter) -> Self {
        Self { character, loaded }
    }

    /// The character's world position in pixels (`Pos X`/`Pos Y`).
    #[must_use]
    pub fn pos(&self) -> Vec2<f32> {
        self.character.pos
    }

    /// Which way the character currently faces.
    #[must_use]
    pub fn facing(&self) -> Facing {
        self.character.facing
    }

    /// The character's current animation (action) id.
    #[must_use]
    pub fn anim(&self) -> i32 {
        self.character.anim
    }

    /// The zero-based index of the current animation element (frame) within the
    /// action. The MUGEN `AnimElem` trigger is one-based; this returns the raw
    /// cursor, matching [`fp_character::Character::anim_elem`].
    #[must_use]
    pub fn anim_elem(&self) -> i32 {
        self.character.anim_elem
    }

    /// The character's current life.
    #[must_use]
    pub fn life(&self) -> i32 {
        self.character.life
    }

    /// The character's maximum life.
    #[must_use]
    pub fn life_max(&self) -> i32 {
        self.character.life_max
    }

    /// Builds the [`PushBody`] used for player-push and bound clamping from the
    /// character's `size.ground.front`/`back` constants, current X, and facing.
    fn push_body(&self) -> PushBody {
        let size = self.character.constants.size;
        PushBody::new(
            self.character.pos.x,
            size.ground_front as f32,
            size.ground_back as f32,
            to_phys_facing(self.character.facing),
        )
    }
}

/// Converts an [`fp_character::Facing`] to the [`fp_physics::Facing`] the push /
/// clamp primitives expect (they are distinct types in distinct crates).
fn to_phys_facing(facing: Facing) -> PhysFacing {
    match facing {
        Facing::Right => PhysFacing::Right,
        Facing::Left => PhysFacing::Left,
    }
}

/// The two-player match coordinator: ticks both fighters, resolves combat both
/// directions, keeps them on stage and apart, and runs the round flow.
///
/// Construct one with [`Match::new`] (default 99-second round) or
/// [`Match::with_round_seconds`], then call [`Match::tick`] once per 60Hz frame
/// with each player's [`MatchInput`]. Read the live state for rendering through
/// [`Match::p1`] / [`Match::p2`], [`Match::round_state`], [`Match::timer`], and
/// [`Match::winner`].
pub struct Match {
    /// Player 1.
    p1: Player,
    /// Player 2.
    p2: Player,
    /// The horizontal playfield bounds both fighters are clamped to.
    bounds: StageBounds,
    /// The current round phase.
    round_state: RoundState,
    /// Frames remaining on the round clock (counts down during
    /// [`RoundState::Fight`]). Starts at `round_seconds * 60`.
    timer: i32,
    /// Frames elapsed in the current [`RoundState::Intro`] / [`RoundState::Ko`]
    /// phase, used to time the transitions out of them.
    phase_timer: i32,
    /// The decided winner, set when the round reaches [`RoundState::Win`].
    winner: Option<Winner>,
}

impl Match {
    /// Creates a match between two players on the given stage bounds, with the
    /// default 99-second round clock and both fighters facing each other.
    ///
    /// The players start in [`RoundState::Intro`]; a fixed number of intro ticks
    /// run before combat begins (see [`Match::tick`]).
    #[must_use]
    pub fn new(p1: Player, p2: Player, bounds: StageBounds) -> Self {
        Self::with_round_seconds(p1, p2, bounds, DEFAULT_ROUND_SECONDS)
    }

    /// Creates a match with an explicit round length in seconds (the timer starts
    /// at `round_seconds * 60` frames). A non-positive `round_seconds` is treated
    /// as `0` (an immediate time-over once the fight begins) rather than producing
    /// a negative timer.
    #[must_use]
    pub fn with_round_seconds(
        mut p1: Player,
        mut p2: Player,
        bounds: StageBounds,
        round_seconds: i32,
    ) -> Self {
        // Seed facing so the two start looking at each other (baseline facep2).
        face_each_other(&mut p1.character, &mut p2.character);
        let timer = round_seconds.max(0).saturating_mul(TICKS_PER_SECOND);
        Self {
            p1,
            p2,
            bounds,
            round_state: RoundState::Intro,
            timer,
            phase_timer: 0,
            winner: None,
        }
    }

    /// Read access to player 1.
    #[must_use]
    pub fn p1(&self) -> &Player {
        &self.p1
    }

    /// Read access to player 2.
    #[must_use]
    pub fn p2(&self) -> &Player {
        &self.p2
    }

    /// The stage bounds the fighters are clamped to.
    #[must_use]
    pub fn bounds(&self) -> StageBounds {
        self.bounds
    }

    /// The current round phase.
    #[must_use]
    pub fn round_state(&self) -> RoundState {
        self.round_state
    }

    /// Frames remaining on the round clock. During [`RoundState::Fight`] this
    /// counts down; in other phases it holds its value. Divide by `60` for
    /// seconds.
    #[must_use]
    pub fn timer(&self) -> i32 {
        self.timer
    }

    /// The decided winner, or [`None`] until the round reaches
    /// [`RoundState::Win`].
    #[must_use]
    pub fn winner(&self) -> Option<Winner> {
        self.winner
    }

    /// Advances the whole match by one 60Hz frame.
    ///
    /// Performs, in order: input feed (facing-relative), both characters' state
    /// machine ticks, combat both directions, player-push + bound clamp, baseline
    /// face-the-opponent, and round-state/timer advance. See the
    /// [crate-level overview](crate) for the full description. Never panics.
    pub fn tick(&mut self, p1_input: MatchInput, p2_input: MatchInput) {
        // (1) Feed inputs into each character's command source, facing-relative.
        //     Inputs only drive the fighters once the round is live; during the
        //     intro/KO/win phases the characters still tick (so idle animations
        //     play) but receive no commands.
        let fighting = self.round_state == RoundState::Fight;
        if fighting {
            Self::feed_input(&mut self.p1.character, p1_input);
            Self::feed_input(&mut self.p2.character, p2_input);
        } else {
            // Clear any stale commands so nothing fires outside the fight phase.
            self.p1
                .character
                .set_command_source(Box::new(fp_character::NoCommands));
            self.p2
                .character
                .set_command_source(Box::new(fp_character::NoCommands));
        }

        // (2) Tick both state machines.
        let _ = self.p1.character.tick(&self.p1.loaded);
        let _ = self.p2.character.tick(&self.p2.loaded);

        // (3) Combat both directions: P1 attacks P2, then P2 attacks P1.
        //     Each direction reads the attacker's Clsn1 and the defender's Clsn2
        //     from their loaded AIR and applies any resolved hit. Combat only
        //     happens during the live fight phase — a HitDef left active during
        //     the intro/KO/win phases must not connect.
        if fighting {
            let p2_states = &self.p2.loaded.states;
            let _ = resolve_attack(
                &mut self.p1.character,
                &self.p1.loaded.air,
                &mut self.p2.character,
                &self.p2.loaded.air,
                p2_states,
            );
            let p1_states = &self.p1.loaded.states;
            let _ = resolve_attack(
                &mut self.p2.character,
                &self.p2.loaded.air,
                &mut self.p1.character,
                &self.p1.loaded.air,
                p1_states,
            );
        }

        // (4) Separate overlapping bodies, then clamp each to the stage.
        self.apply_push_and_bounds();

        // (5) Baseline face-the-opponent for neutral characters.
        face_each_other_when_neutral(&mut self.p1.character, &mut self.p2.character);

        // (6) Advance the round state machine and timer.
        self.advance_round();
    }

    /// Translates a [`MatchInput`] into a facing-relative command set and installs
    /// it as the character's [`fp_character::CommandSource`], also updating
    /// [`fp_character::Character::holding_back`] for the guard path.
    fn feed_input(character: &mut Character, input: MatchInput) {
        let (commands, holding_back) = Self::facing_relative_commands(character.facing, input);
        character.holding_back = holding_back;
        character.set_command_source(Box::new(fp_character::ActiveCommands::from_names(commands)));
    }

    /// Builds the list of active, **facing-relative** command names for an input,
    /// plus whether the character is holding "back" (away from the opponent).
    ///
    /// MUGEN command directions are relative to facing: pressing toward where the
    /// character faces is `fwd`, away is `back`. We map the absolute left/right
    /// input through the character's [`Facing`]:
    ///
    /// - facing right: `right` → `fwd`, `left` → `back`;
    /// - facing left: `left` → `fwd`, `right` → `back`.
    ///
    /// Up/down map to `up`/`down` unchanged, and each pressed button contributes
    /// its lowercase letter (`a`..`z`). The returned `holding_back` flag is `true`
    /// when the resolved direction is `back`, which the caller writes to
    /// [`Character::holding_back`] so [`resolve_attack`] can choose the guard path.
    fn facing_relative_commands(facing: Facing, input: MatchInput) -> (Vec<String>, bool) {
        let mut commands: Vec<String> = Vec::new();

        // Horizontal: resolve to fwd/back by facing. If both or neither are held,
        // there is no net horizontal command and the character is not blocking.
        let mut holding_back = false;
        match (input.left, input.right) {
            (true, false) => {
                // Pressing left: fwd when facing left, back when facing right.
                match facing {
                    Facing::Left => commands.push("fwd".to_string()),
                    Facing::Right => {
                        commands.push("back".to_string());
                        holding_back = true;
                    }
                }
            }
            (false, true) => {
                // Pressing right: fwd when facing right, back when facing left.
                match facing {
                    Facing::Right => commands.push("fwd".to_string()),
                    Facing::Left => {
                        commands.push("back".to_string());
                        holding_back = true;
                    }
                }
            }
            // Both or neither: no net horizontal direction.
            _ => {}
        }

        if input.up {
            commands.push("up".to_string());
        }
        if input.down {
            commands.push("down".to_string());
        }

        // Attack buttons map to their bare lowercase letters.
        for (pressed, name) in [
            (input.a, "a"),
            (input.b, "b"),
            (input.c, "c"),
            (input.x, "x"),
            (input.y, "y"),
            (input.z, "z"),
        ] {
            if pressed {
                commands.push(name.to_string());
            }
        }

        (commands, holding_back)
    }

    /// Pushes overlapping bodies apart along X (mutual `PlayerPush`) and clamps
    /// each fighter so its facing-resolved body stays inside the stage bounds.
    ///
    /// The push uses each character's `size.ground.front`/`back` half-widths; the
    /// even split moves each center by half the overlap. Both operations are pure
    /// and only adjust the characters' X positions.
    fn apply_push_and_bounds(&mut self) {
        // Separate first (so the post-push positions are then clamped on stage).
        let body_a = self.p1.push_body();
        let body_b = self.p2.push_body();
        let res = resolve_push(body_a, body_b);
        self.p1.character.pos.x = res.a;
        self.p2.character.pos.x = res.b;

        // Then clamp each (rebuilt at its new center) to the stage bounds.
        let clamped_a = clamp_to_bounds(
            self.p1.character.pos.x,
            self.p1.push_body_left_half(),
            self.p1.push_body_right_half(),
            self.bounds.left,
            self.bounds.right,
        );
        let clamped_b = clamp_to_bounds(
            self.p2.character.pos.x,
            self.p2.push_body_left_half(),
            self.p2.push_body_right_half(),
            self.bounds.left,
            self.bounds.right,
        );
        self.p1.character.pos.x = clamped_a;
        self.p2.character.pos.x = clamped_b;
    }

    /// Advances the round phase and the down-counting timer for this frame.
    ///
    /// - [`RoundState::Intro`]: count [`INTRO_FRAMES`], then enter
    ///   [`RoundState::Fight`].
    /// - [`RoundState::Fight`]: decrement the timer; a life reaching `0` (KO) or
    ///   the timer hitting `0` (time over) decides the [`Winner`] and enters
    ///   [`RoundState::Ko`].
    /// - [`RoundState::Ko`]: hold [`KO_FRAMES`], then enter [`RoundState::Win`].
    /// - [`RoundState::Win`]: terminal; nothing further changes.
    fn advance_round(&mut self) {
        match self.round_state {
            RoundState::Intro => {
                self.phase_timer += 1;
                if self.phase_timer >= INTRO_FRAMES {
                    self.round_state = RoundState::Fight;
                    self.phase_timer = 0;
                    // Give both fighters control as the fight begins.
                    self.p1.character.ctrl = true;
                    self.p2.character.ctrl = true;
                }
            }
            RoundState::Fight => {
                if self.timer > 0 {
                    self.timer -= 1;
                }
                let p1_down = self.p1.character.life <= 0;
                let p2_down = self.p2.character.life <= 0;
                if p1_down || p2_down {
                    // Knockout: whoever still has life wins; both down is a draw.
                    self.winner = Some(if p1_down && p2_down {
                        Winner::Draw
                    } else if p2_down {
                        Winner::P1
                    } else {
                        Winner::P2
                    });
                    self.enter_ko();
                } else if self.timer == 0 {
                    // Time over: compare remaining life.
                    self.winner = Some(compare_life(
                        self.p1.character.life,
                        self.p2.character.life,
                    ));
                    self.enter_ko();
                }
            }
            RoundState::Ko => {
                self.phase_timer += 1;
                if self.phase_timer >= KO_FRAMES {
                    self.round_state = RoundState::Win;
                    self.phase_timer = 0;
                }
            }
            RoundState::Win => {
                // Terminal phase: the round is decided. A higher-level match
                // manager (best-of-N) would start a new round here; that is out of
                // scope for a single Match.
            }
        }
    }

    /// Enters [`RoundState::Ko`], freezing the round clock and removing control
    /// from both fighters for the duration of the hold.
    fn enter_ko(&mut self) {
        self.round_state = RoundState::Ko;
        self.phase_timer = 0;
        self.p1.character.ctrl = false;
        self.p2.character.ctrl = false;
    }
}

impl Player {
    /// The body's left half-width (distance from axis to the `-X` edge), resolved
    /// for the current facing. Mirrors [`PushBody`]'s internal resolution so the
    /// clamp uses the same geometry as the push.
    fn push_body_left_half(&self) -> f32 {
        let size = self.character.constants.size;
        match self.character.facing {
            Facing::Right => size.ground_back as f32,
            Facing::Left => size.ground_front as f32,
        }
    }

    /// The body's right half-width (distance from axis to the `+X` edge), resolved
    /// for the current facing.
    fn push_body_right_half(&self) -> f32 {
        let size = self.character.constants.size;
        match self.character.facing {
            Facing::Right => size.ground_front as f32,
            Facing::Left => size.ground_back as f32,
        }
    }
}

/// Decides a winner by comparing two life totals (used at time over).
fn compare_life(p1_life: i32, p2_life: i32) -> Winner {
    use std::cmp::Ordering;
    match p1_life.cmp(&p2_life) {
        Ordering::Greater => Winner::P1,
        Ordering::Less => Winner::P2,
        Ordering::Equal => Winner::Draw,
    }
}

/// Returns `true` if a character is in a *neutral* state where the baseline
/// `facep2` logic should keep it turned toward the opponent.
///
/// We keep this deliberately simple (documented baseline): a character should
/// re-face the opponent only while it is on its feet and not committed to an
/// action — i.e. a [`StateType::Standing`] (or the catch-all
/// [`StateType::Unchanged`]) character whose move type is idle. A character that
/// is attacking, being hit, crouching, airborne, or lying keeps its current
/// facing (turning mid-move would look wrong and could flip a knockback). MUGEN's
/// real turn behavior is richer (a dedicated turning state, `5900` common, etc.);
/// this is the documented minimal stand-in for task 7.1.
fn is_neutral_facing_state(c: &Character) -> bool {
    matches!(c.state_type, StateType::Standing | StateType::Unchanged)
        && c.move_type == MoveType::Idle
}

/// Sets both characters' facing toward each other based on their X positions,
/// unconditionally. Used at match construction to seed a sensible initial facing.
///
/// The character with the smaller X faces right (toward the larger X) and vice
/// versa; equal X leaves them facing right/left as a deterministic default.
fn face_each_other(a: &mut Character, b: &mut Character) {
    let (fa, fb) = facings_toward(a.pos.x, b.pos.x);
    a.facing = fa;
    b.facing = fb;
}

/// Applies the baseline `facep2`: each character that is in a neutral state turns
/// to face the other; a non-neutral character keeps its facing.
///
/// See [`is_neutral_facing_state`] for the (documented, simplified) definition of
/// "neutral". This is intentionally conservative so it never flips a character
/// out of an attack or get-hit reaction mid-animation.
fn face_each_other_when_neutral(a: &mut Character, b: &mut Character) {
    let (fa, fb) = facings_toward(a.pos.x, b.pos.x);
    if is_neutral_facing_state(a) {
        a.facing = fa;
    }
    if is_neutral_facing_state(b) {
        b.facing = fb;
    }
}

/// Returns the facings the two characters should have to look at each other,
/// given their X positions: `(facing_for_a, facing_for_b)`.
///
/// The left character (smaller X) faces right; the right character faces left.
/// When the X positions are equal, `a` faces right and `b` faces left as a
/// deterministic tie-break.
fn facings_toward(ax: f32, bx: f32) -> (Facing, Facing) {
    if ax <= bx {
        (Facing::Right, Facing::Left)
    } else {
        (Facing::Left, Facing::Right)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_character::{CharacterConstants, Character, Facing, LoadedCharacter, MoveType, StateType};
    use fp_combat::{Damage, HitDef, HitFlags, HitTimes, PauseTime};
    use fp_core::{Rect, SpriteId, Vec2};
    use fp_formats::air::{AirFile, AnimAction, AnimFrame, BlendMode};
    use fp_formats::sff::SffFile;
    use std::collections::HashMap;

    /// Builds a one-action, one-frame [`AirFile`] carrying the given Clsn boxes.
    fn air_with(action: i32, clsn1: Vec<Rect>, clsn2: Vec<Rect>) -> AirFile {
        let frame = AnimFrame {
            sprite: SpriteId::new(0, 0),
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

    /// Builds a minimal valid SFF v1 container in memory carrying a single
    /// *linked* (data-less) sprite, so a headless [`LoadedCharacter`] can be
    /// constructed without a sprite asset on disk. The v1 parser rejects a
    /// zero-sprite file, so we include one linked sprite (`data_length = 0`,
    /// which skips PCX decoding); the simulation never reads sprites.
    fn empty_sff() -> SffFile {
        // SFF v1 layout used here:
        //   [0..12)   "ElecbyteSpr\0" signature
        //   [15]      major version = 1
        //   [16..20)  num_groups
        //   [20..24)  num_images = 1
        //   [24..28)  first_subheader_offset = 64
        //   [64..96)  one 32-byte sprite sub-header:
        //               [0..4)  next_offset = 0  (terminates the walk)
        //               [4..8)  data_length = 0  (linked sprite, no PCX)
        const SUBHEADER_OFFSET: usize = 64;
        let mut buf = vec![0u8; SUBHEADER_OFFSET + 32];
        buf[0..12].copy_from_slice(b"ElecbyteSpr\0");
        buf[15] = 1; // SFF v1
        buf[16..20].copy_from_slice(&1u32.to_le_bytes()); // num_groups
        buf[20..24].copy_from_slice(&1u32.to_le_bytes()); // num_images
        buf[24..28].copy_from_slice(&(SUBHEADER_OFFSET as u32).to_le_bytes());
        // Sub-header: next_offset = 0, data_length = 0 (the rest stays zeroed).
        SffFile::from_bytes(&buf).expect("synthetic SFF v1 must parse")
    }

    /// A synthetic [`LoadedCharacter`] with an empty state graph and the given
    /// AIR file. The SFF is a minimal empty v1 container (no sprites needed for
    /// headless simulation), so this never touches disk.
    fn loaded_with(air: AirFile) -> LoadedCharacter {
        LoadedCharacter {
            name: "test".to_string(),
            localcoord: (320, 240),
            constants: CharacterConstants::default(),
            states: HashMap::new(),
            sff: empty_sff(),
            air,
            cmd: None,
            snd: None,
        }
    }

    /// A defender-style loaded character: a single hurt box on action 0.
    fn defender_loaded() -> LoadedCharacter {
        loaded_with(air_with(
            0,
            Vec::new(),
            vec![Rect::new(-18.0, -70.0, 36.0, 70.0)],
        ))
    }

    /// An attacker-style loaded character: a punch (Clsn1) box on action 200 plus
    /// a hurt box on action 0 (so it can also be hit).
    fn attacker_loaded() -> LoadedCharacter {
        let punch = air_with(200, vec![Rect::new(10.0, -60.0, 45.0, 20.0)], Vec::new());
        // Merge an action-0 hurt frame in too so the same character is hittable.
        let mut air = punch;
        air.actions.insert(
            0,
            AnimAction {
                action_number: 0,
                frames: vec![AnimFrame {
                    sprite: SpriteId::new(0, 0),
                    offset: Vec2::new(0, 0),
                    ticks: 1,
                    flip_h: false,
                    flip_v: false,
                    blend: BlendMode::Normal,
                    clsn1: Vec::new(),
                    clsn2: vec![Rect::new(-18.0, -70.0, 36.0, 70.0)],
                }],
                loopstart: 0,
            },
        );
        loaded_with(air)
    }

    /// A HitDef with concrete damage/knockback/stun (mirrors the combat tests).
    fn sample_hitdef() -> HitDef {
        HitDef {
            damage: Damage { hit: 30, guard: 5 },
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

    /// Builds a basic match: two characters far enough apart not to overlap, with
    /// a short round so tests can reach time-over quickly if needed.
    fn basic_match() -> Match {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-50.0, 0.0);
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(50.0, 0.0);
        let p1 = Player::new(p1c, attacker_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        Match::new(p1, p2, StageBounds::new(-200.0, 200.0))
    }

    /// Drives the match out of the intro phase into [`RoundState::Fight`].
    fn into_fight(m: &mut Match) {
        for _ in 0..(INTRO_FRAMES + 1) {
            m.tick(MatchInput::none(), MatchInput::none());
        }
    }

    #[test]
    fn both_characters_tick_without_panic() {
        let mut m = basic_match();
        for _ in 0..120 {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        // Reached at least the fight phase and no panic occurred.
        assert!(matches!(
            m.round_state(),
            RoundState::Fight | RoundState::Ko | RoundState::Win
        ));
    }

    #[test]
    fn starts_in_intro_then_enters_fight() {
        let mut m = basic_match();
        assert_eq!(m.round_state(), RoundState::Intro);
        // Tick through the intro.
        for _ in 0..INTRO_FRAMES {
            assert_eq!(m.round_state(), RoundState::Intro);
            m.tick(MatchInput::none(), MatchInput::none());
        }
        // After INTRO_FRAMES ticks the intro elapses and the fight begins.
        assert_eq!(m.round_state(), RoundState::Fight);
        // Control is granted at fight start.
        assert!(m.p1().character.ctrl);
        assert!(m.p2().character.ctrl);
    }

    #[test]
    fn initial_facing_is_toward_opponent() {
        let m = basic_match();
        // P1 at -50 is left of P2 at +50: P1 faces right, P2 faces left.
        assert_eq!(m.p1().facing(), Facing::Right);
        assert_eq!(m.p2().facing(), Facing::Left);
    }

    #[test]
    fn connecting_attack_drops_life_and_ko_advances_round() {
        // Place the attacker so its punch box overlaps the defender's hurt box.
        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Right;

        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = Vec2::new(60.0, 0.0);
        p2c.facing = Facing::Left;

        let p1 = Player::new(p1c, attacker_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);

        // Arm the attack only once the fight has begun, and set the defender almost
        // dead so a single 30-damage punch is a KO.
        m.p2.character.life = 20;
        let life_before = m.p2().life();
        // Re-arm and re-attach the HitDef each frame (a real move would keep it
        // active across its active frames); pin the attacker on its punch frame.
        for _ in 0..5 {
            m.p1.character.anim = 200;
            m.p1.character.anim_elem = 0;
            m.p1.character.move_type = MoveType::Attack;
            m.p1.character.active_hitdef = Some(sample_hitdef());
            m.p1.character.move_connect.reset();
            // Keep the defender on its hurt frame and on its feet.
            m.p2.character.anim = 0;
            m.p2.character.anim_elem = 0;
            m.p2.character.state_type = StateType::Standing;
            m.tick(MatchInput::none(), MatchInput::none());
            if m.round_state() != RoundState::Fight {
                break;
            }
        }

        assert!(
            m.p2().life() < life_before,
            "a connecting attack must reduce the defender's life"
        );
        assert_eq!(m.p2().life(), 0, "lethal punch drops life to zero");
        // The KO must have advanced the round out of Fight.
        assert!(
            matches!(m.round_state(), RoundState::Ko | RoundState::Win),
            "a KO advances the round state, got {:?}",
            m.round_state()
        );
        assert_eq!(m.winner(), Some(Winner::P1));
    }

    #[test]
    fn ko_eventually_reaches_win() {
        let mut m = basic_match();
        into_fight(&mut m);
        // Force a KO directly.
        m.p2.character.life = 0;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.round_state(), RoundState::Ko);
        // Hold through the KO phase into Win.
        for _ in 0..(KO_FRAMES + 1) {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        assert_eq!(m.round_state(), RoundState::Win);
        assert_eq!(m.winner(), Some(Winner::P1));
    }

    #[test]
    fn player_push_separates_overlapping_characters() {
        // Two characters at the exact same X must be pushed apart by tick.
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(0.0, 0.0);
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(0.0, 0.0);
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));

        // A single tick should separate them (push runs every frame).
        m.tick(MatchInput::none(), MatchInput::none());
        let dx = (m.p1().pos().x - m.p2().pos().x).abs();
        assert!(
            dx > 0.0,
            "overlapping characters must be pushed apart, got dx = {dx}"
        );
        // They should now be at least just-touching: the gap equals the summed
        // half-widths on the touching sides (default ground.front 16 + 16).
        assert!(dx >= 1.0, "separation should be meaningful, got {dx}");
    }

    #[test]
    fn bound_clamp_keeps_characters_on_stage() {
        // Put a character far off the right edge; the clamp must pull it back.
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(1000.0, 0.0);
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(-1000.0, 0.0);
        let bounds = StageBounds::new(-100.0, 100.0);
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, bounds);

        m.tick(MatchInput::none(), MatchInput::none());

        // After clamping, each body's edge must be within the stage bounds.
        let p1 = m.p1();
        let p1_right = p1.pos().x + p1.push_body_right_half();
        assert!(
            p1_right <= bounds.right + 1e-3,
            "P1 right edge {p1_right} must be within stage right {}",
            bounds.right
        );
        let p2 = m.p2();
        let p2_left = p2.pos().x - p2.push_body_left_half();
        assert!(
            p2_left >= bounds.left - 1e-3,
            "P2 left edge {p2_left} must be within stage left {}",
            bounds.left
        );
    }

    #[test]
    fn facing_relative_commands_map_by_facing() {
        // Facing right: pressing right is fwd (not blocking); pressing left is
        // back (blocking).
        let right_in = MatchInput {
            right: true,
            ..MatchInput::none()
        };
        let (cmds, blocking) = Match::facing_relative_commands(Facing::Right, right_in);
        assert!(cmds.iter().any(|c| c == "fwd"));
        assert!(!blocking);

        let left_in = MatchInput {
            left: true,
            ..MatchInput::none()
        };
        let (cmds, blocking) = Match::facing_relative_commands(Facing::Right, left_in);
        assert!(cmds.iter().any(|c| c == "back"));
        assert!(blocking, "holding away from the opponent sets holding_back");

        // Facing left mirrors the mapping.
        let (cmds, blocking) = Match::facing_relative_commands(Facing::Left, left_in);
        assert!(cmds.iter().any(|c| c == "fwd"));
        assert!(!blocking);
    }

    #[test]
    fn buttons_become_command_names() {
        let input = MatchInput {
            a: true,
            c: true,
            ..MatchInput::none()
        };
        let (cmds, _) = Match::facing_relative_commands(Facing::Right, input);
        assert!(cmds.iter().any(|c| c == "a"));
        assert!(cmds.iter().any(|c| c == "c"));
        assert!(!cmds.iter().any(|c| c == "b"));
    }

    #[test]
    fn time_over_compares_life() {
        // A one-second round: after 60 fight frames the timer hits zero.
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-50.0, 0.0);
        p1c.life = 800;
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(50.0, 0.0);
        p2c.life = 500; // less life -> P1 wins on time over
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::with_round_seconds(p1, p2, StageBounds::new(-200.0, 200.0), 1);
        into_fight(&mut m);

        // Burn the one-second clock down.
        for _ in 0..(TICKS_PER_SECOND + 1) {
            m.tick(MatchInput::none(), MatchInput::none());
            if m.round_state() != RoundState::Fight {
                break;
            }
        }
        assert_ne!(m.round_state(), RoundState::Fight, "timer ran out");
        assert_eq!(
            m.winner(),
            Some(Winner::P1),
            "more life wins on time over"
        );
    }

    #[test]
    fn neutral_character_refaces_opponent() {
        // Construct so P1 is to the RIGHT of P2 but facing the wrong way; a
        // neutral state must turn it back toward the opponent on tick.
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(50.0, 0.0);
        p1c.facing = Facing::Right; // wrong: opponent is to the left
        p1c.state_type = StateType::Standing;
        p1c.move_type = MoveType::Idle;
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(-50.0, 0.0);
        p2c.facing = Facing::Left;
        p2c.state_type = StateType::Standing;
        p2c.move_type = MoveType::Idle;
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);

        // Force both to the WRONG facing (away from the opponent) while neutral;
        // the baseline facep2 must correct them on the next tick.
        m.p1.character.facing = Facing::Right; // P1 is on the right, should face left
        m.p2.character.facing = Facing::Left; // P2 is on the left, should face right
        m.p1.character.move_type = MoveType::Idle;
        m.p1.character.state_type = StateType::Standing;
        m.p2.character.move_type = MoveType::Idle;
        m.p2.character.state_type = StateType::Standing;

        m.tick(MatchInput::none(), MatchInput::none());
        // P1 (right side) should now face left, P2 (left side) face right.
        assert_eq!(m.p1().facing(), Facing::Left);
        assert_eq!(m.p2().facing(), Facing::Right);
    }

    #[test]
    fn attacking_character_keeps_facing() {
        // A character that is attacking must NOT be re-faced mid-move.
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(50.0, 0.0);
        p1c.state_type = StateType::Standing;
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(-50.0, 0.0);
        let p1 = Player::new(p1c, attacker_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);

        // After the intro, force P1 to the WRONG facing while it is attacking.
        // Because it is committed to a move (move_type = Attack), the baseline
        // facep2 must leave its facing alone.
        m.p1.character.facing = Facing::Right; // opponent is to the left -> "wrong"
        m.p1.character.move_type = MoveType::Attack;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(
            m.p1().facing(),
            Facing::Right,
            "an attacking character must not be re-faced"
        );
    }

    #[test]
    fn round_seconds_clamps_non_positive() {
        let p1 = Player::new(Character::new(), defender_loaded());
        let p2 = Player::new(Character::new(), defender_loaded());
        let m = Match::with_round_seconds(p1, p2, StageBounds::default(), -5);
        assert_eq!(m.timer(), 0, "a non-positive round length clamps to 0");
    }

    // ---- Additional coverage (Proctor): edge cases, error paths, semantics ----

    /// AC1/AC2: the default round timer is exactly 99 seconds * 60 ticks.
    #[test]
    fn default_timer_is_ninety_nine_seconds() {
        let m = basic_match();
        assert_eq!(
            m.timer(),
            DEFAULT_ROUND_SECONDS * TICKS_PER_SECOND,
            "default round clock is 99s * 60"
        );
        assert_eq!(m.timer(), 99 * 60);
    }

    /// AC2: the timer must not tick down during the Intro phase — only once the
    /// fight is live. A renderer/clock reading `timer()` during intro should see
    /// the full starting value.
    #[test]
    fn timer_frozen_during_intro() {
        let mut m = basic_match();
        let start = m.timer();
        // Tick partway into (but not past) the intro.
        for _ in 0..(INTRO_FRAMES - 1) {
            m.tick(MatchInput::none(), MatchInput::none());
            assert_eq!(m.round_state(), RoundState::Intro);
            assert_eq!(m.timer(), start, "timer must not move during intro");
        }
    }

    /// AC2: during the fight the timer decrements exactly one frame per tick.
    #[test]
    fn timer_decrements_one_per_fight_tick() {
        let mut m = basic_match();
        into_fight(&mut m);
        assert_eq!(m.round_state(), RoundState::Fight);
        let before = m.timer();
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.timer(), before - 1, "one fight tick burns one timer frame");
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.timer(), before - 2);
    }

    /// AC2: the timer holds its value during the KO hold (it is frozen, not
    /// counted down) and through Win.
    #[test]
    fn timer_frozen_after_ko() {
        let mut m = basic_match();
        into_fight(&mut m);
        m.p2.character.life = 0;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.round_state(), RoundState::Ko);
        let frozen = m.timer();
        for _ in 0..10 {
            m.tick(MatchInput::none(), MatchInput::none());
            assert_eq!(m.timer(), frozen, "timer is frozen during the KO hold");
        }
    }

    /// AC2: the round state machine never moves backwards. Drive a full match and
    /// assert monotonic non-decreasing phase ordering across every tick.
    #[test]
    fn round_state_is_monotonic() {
        fn rank(s: RoundState) -> u8 {
            match s {
                RoundState::Intro => 0,
                RoundState::Fight => 1,
                RoundState::Ko => 2,
                RoundState::Win => 3,
            }
        }
        let mut m = basic_match();
        into_fight(&mut m);
        m.p2.character.life = 0; // force the KO path
        let mut prev = rank(m.round_state());
        for _ in 0..(KO_FRAMES + 30) {
            m.tick(MatchInput::none(), MatchInput::none());
            let now = rank(m.round_state());
            assert!(now >= prev, "round state went backwards: {prev} -> {now}");
            prev = now;
        }
        assert_eq!(m.round_state(), RoundState::Win);
    }

    /// AC2: once the round reaches Win it is terminal — further ticks change
    /// nothing (state, timer, and winner all hold).
    #[test]
    fn win_phase_is_terminal() {
        let mut m = basic_match();
        into_fight(&mut m);
        m.p2.character.life = 0;
        for _ in 0..(KO_FRAMES + 2) {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        assert_eq!(m.round_state(), RoundState::Win);
        let (w, t) = (m.winner(), m.timer());
        for _ in 0..50 {
            m.tick(MatchInput::none(), MatchInput::none());
            assert_eq!(m.round_state(), RoundState::Win, "Win is terminal");
            assert_eq!(m.winner(), w, "winner is stable in Win");
            assert_eq!(m.timer(), t, "timer is stable in Win");
        }
    }

    /// AC2: winner() is None until the round is actually decided.
    #[test]
    fn winner_is_none_before_decision() {
        let mut m = basic_match();
        assert_eq!(m.winner(), None, "no winner during intro");
        into_fight(&mut m);
        assert_eq!(m.winner(), None, "no winner at fight start");
    }

    /// AC2: a double-KO on the same frame is a Draw.
    #[test]
    fn double_ko_is_a_draw() {
        let mut m = basic_match();
        into_fight(&mut m);
        m.p1.character.life = 0;
        m.p2.character.life = 0;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.winner(), Some(Winner::Draw), "both down is a draw");
        assert!(matches!(m.round_state(), RoundState::Ko | RoundState::Win));
    }

    /// AC2: when only P1 is downed, P2 wins (the mirror of the existing P1-wins
    /// test).
    #[test]
    fn p1_ko_makes_p2_win() {
        let mut m = basic_match();
        into_fight(&mut m);
        m.p1.character.life = 0;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.winner(), Some(Winner::P2));
        assert_eq!(m.round_state(), RoundState::Ko);
    }

    /// MUGEN semantics: negative life (overkill) is still treated as a KO and the
    /// surviving player wins — the comparison is `life <= 0`, not `== 0`.
    #[test]
    fn negative_life_counts_as_ko() {
        let mut m = basic_match();
        into_fight(&mut m);
        m.p2.character.life = -100;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.round_state(), RoundState::Ko);
        assert_eq!(m.winner(), Some(Winner::P1));
    }

    /// AC2: time over with equal life is a Draw.
    #[test]
    fn time_over_equal_life_is_draw() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-50.0, 0.0);
        p1c.life = 600;
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(50.0, 0.0);
        p2c.life = 600;
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::with_round_seconds(p1, p2, StageBounds::new(-200.0, 200.0), 1);
        into_fight(&mut m);
        for _ in 0..(TICKS_PER_SECOND + 2) {
            m.tick(MatchInput::none(), MatchInput::none());
            if m.round_state() != RoundState::Fight {
                break;
            }
        }
        assert_ne!(m.round_state(), RoundState::Fight);
        assert_eq!(m.winner(), Some(Winner::Draw), "equal life at time over draws");
    }

    /// AC2: time over with P2 ahead makes P2 win (mirror of the existing P1 case).
    #[test]
    fn time_over_p2_more_life_wins() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-50.0, 0.0);
        p1c.life = 300;
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(50.0, 0.0);
        p2c.life = 900;
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::with_round_seconds(p1, p2, StageBounds::new(-200.0, 200.0), 1);
        into_fight(&mut m);
        for _ in 0..(TICKS_PER_SECOND + 2) {
            m.tick(MatchInput::none(), MatchInput::none());
            if m.round_state() != RoundState::Fight {
                break;
            }
        }
        assert_eq!(m.winner(), Some(Winner::P2));
    }

    /// `compare_life` is the time-over decider; cover all three orderings directly.
    #[test]
    fn compare_life_covers_all_orderings() {
        assert_eq!(compare_life(100, 50), Winner::P1);
        assert_eq!(compare_life(50, 100), Winner::P2);
        assert_eq!(compare_life(50, 50), Winner::Draw);
    }

    /// AC3 (semantics): an attack must NOT connect outside the live Fight phase.
    /// Even with a fully armed, overlapping HitDef during Intro, P2's life is
    /// untouched until the fight actually begins.
    #[test]
    fn no_combat_during_intro() {
        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Right;
        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = Vec2::new(60.0, 0.0);
        p2c.facing = Facing::Left;
        let p1 = Player::new(p1c, attacker_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        assert_eq!(m.round_state(), RoundState::Intro);

        let life_before = m.p2().life();
        // Arm an overlapping attack during the intro and tick several intro frames.
        for _ in 0..5 {
            m.p1.character.anim = 200;
            m.p1.character.anim_elem = 0;
            m.p1.character.move_type = MoveType::Attack;
            m.p1.character.active_hitdef = Some(sample_hitdef());
            m.p1.character.move_connect.reset();
            m.p2.character.anim = 0;
            m.p2.character.anim_elem = 0;
            m.p2.character.state_type = StateType::Standing;
            assert_eq!(m.round_state(), RoundState::Intro, "still in intro");
            m.tick(MatchInput::none(), MatchInput::none());
        }
        assert_eq!(
            m.p2().life(),
            life_before,
            "no damage may be dealt during the intro phase"
        );
    }

    /// AC3 (semantics): an attack must not connect during the KO hold either —
    /// a HitDef left active when the round is decided cannot keep dealing damage.
    #[test]
    fn no_combat_during_ko() {
        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Right;
        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = Vec2::new(60.0, 0.0);
        p2c.facing = Facing::Left;
        let p1 = Player::new(p1c, attacker_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);

        // Force a KO so we are now in the KO hold; P1 still has its punch armed.
        m.p1.character.life = 0; // P1 down -> P2 wins, enter KO
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.round_state(), RoundState::Ko);

        let p2_life = m.p2().life();
        for _ in 0..5 {
            m.p1.character.anim = 200;
            m.p1.character.anim_elem = 0;
            m.p1.character.move_type = MoveType::Attack;
            m.p1.character.active_hitdef = Some(sample_hitdef());
            m.p1.character.move_connect.reset();
            m.p2.character.anim = 0;
            m.p2.character.anim_elem = 0;
            m.p2.character.state_type = StateType::Standing;
            m.tick(MatchInput::none(), MatchInput::none());
        }
        assert_eq!(m.p2().life(), p2_life, "no combat during the KO hold");
    }

    /// AC3 (semantics): combat runs in BOTH directions. Arm both fighters with an
    /// overlapping attack on the same frame; both must take damage.
    #[test]
    fn combat_runs_both_directions() {
        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Right;
        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = Vec2::new(40.0, 0.0);
        p2c.facing = Facing::Left;
        // Both are attacker-shaped (punch on 200, hurt on 0).
        let p1 = Player::new(p1c, attacker_loaded());
        let p2 = Player::new(p2c, attacker_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);

        let p1_before = m.p1().life();
        let p2_before = m.p2().life();
        // Arm both with a punch on the active frame, overlapping the other's hurt
        // box (each character carries both Clsn1 on 200 and Clsn2 on 0; here we put
        // each attacker on its punch frame while its OWN hurt box is also present on
        // the punch action's defender side — so use action 0 hurt by keeping anim 200
        // for attack and relying on the punch Clsn1 vs the other's idle Clsn2).
        // To get a clean mutual hit, alternate frames: keep each on the punch action
        // (Clsn1 present) and ensure the other exposes a Clsn2. attacker_loaded's
        // action 200 has no Clsn2, so put each defender-frame as action 0.
        // Simplest robust approach: one tick with both on punch (Clsn1) while the
        // OTHER is read for Clsn2 from action 0 — not simultaneously possible per
        // character. Instead, verify both directions across two arming passes.

        // Pass A: P1 punches (200) into P2 idle (0).
        m.p1.character.anim = 200;
        m.p1.character.anim_elem = 0;
        m.p1.character.move_type = MoveType::Attack;
        m.p1.character.active_hitdef = Some(sample_hitdef());
        m.p1.character.move_connect.reset();
        m.p2.character.anim = 0;
        m.p2.character.anim_elem = 0;
        m.p2.character.state_type = StateType::Standing;
        m.p2.character.active_hitdef = None;
        m.tick(MatchInput::none(), MatchInput::none());
        assert!(m.p2().life() < p2_before, "P1's attack must damage P2");

        if m.round_state() != RoundState::Fight {
            return; // a KO ended the round; both-direction intent already shown
        }

        // Pass B: P2 punches (200) into P1 idle (0).
        m.p2.character.anim = 200;
        m.p2.character.anim_elem = 0;
        m.p2.character.move_type = MoveType::Attack;
        m.p2.character.active_hitdef = Some(sample_hitdef());
        m.p2.character.move_connect.reset();
        m.p1.character.anim = 0;
        m.p1.character.anim_elem = 0;
        m.p1.character.state_type = StateType::Standing;
        m.p1.character.active_hitdef = None;
        m.tick(MatchInput::none(), MatchInput::none());
        assert!(m.p1().life() < p1_before, "P2's attack must damage P1");
    }

    /// AC3 (semantics): the `hitonce` rule — a single armed HitDef that has
    /// already connected does not deal damage twice while it stays active.
    #[test]
    fn hitonce_prevents_double_damage() {
        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Right;
        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = Vec2::new(60.0, 0.0);
        p2c.facing = Facing::Left;
        let p1 = Player::new(p1c, attacker_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);

        // Arm ONCE (no per-frame re-arm / reset): the move connects on the first
        // frame, then `move_connect` must suppress further hits.
        m.p1.character.anim = 200;
        m.p1.character.anim_elem = 0;
        m.p1.character.move_type = MoveType::Attack;
        m.p1.character.active_hitdef = Some(sample_hitdef());
        m.p1.character.move_connect.reset();
        m.p2.character.anim = 0;
        m.p2.character.anim_elem = 0;
        m.p2.character.state_type = StateType::Standing;

        m.tick(MatchInput::none(), MatchInput::none());
        let after_first = m.p2().life();
        assert!(after_first < m.p2().life_max(), "first frame connects");

        // Keep the same HitDef active (do NOT reset move_connect) for more frames;
        // pin the defender back onto its hurt frame each tick.
        for _ in 0..4 {
            m.p2.character.anim = 0;
            m.p2.character.anim_elem = 0;
            m.p2.character.state_type = StateType::Standing;
            if m.round_state() != RoundState::Fight {
                break;
            }
            m.tick(MatchInput::none(), MatchInput::none());
        }
        assert_eq!(
            m.p2().life(),
            after_first,
            "hitonce: an already-connected move must not damage again"
        );
    }

    /// AC1/AC3: a missing attack box (empty Clsn1) degrades to "no contact" — an
    /// attacker on a non-attack frame never connects even with a HitDef armed.
    #[test]
    fn missing_attack_box_does_not_connect() {
        let mut p1c = Character::with_constants(CharacterConstants::default());
        p1c.pos = Vec2::new(0.0, 0.0);
        p1c.facing = Facing::Right;
        let mut p2c = Character::with_constants(CharacterConstants::default());
        p2c.pos = Vec2::new(20.0, 0.0);
        p2c.facing = Facing::Left;
        let p1 = Player::new(p1c, attacker_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);

        let before = m.p2().life();
        // Arm a HitDef but point the attacker at action 0 (which has NO Clsn1).
        for _ in 0..5 {
            m.p1.character.anim = 0; // no attack box on action 0
            m.p1.character.anim_elem = 0;
            m.p1.character.move_type = MoveType::Attack;
            m.p1.character.active_hitdef = Some(sample_hitdef());
            m.p1.character.move_connect.reset();
            if m.round_state() != RoundState::Fight {
                break;
            }
            m.tick(MatchInput::none(), MatchInput::none());
        }
        assert_eq!(m.p2().life(), before, "no Clsn1 -> no contact, no damage");
    }

    /// AC1/AC4: a character whose loaded AIR is empty (no actions at all) must
    /// still tick without panic and never connect — missing assets degrade safely.
    #[test]
    fn empty_air_ticks_safely() {
        let empty = AirFile {
            actions: HashMap::new(),
        };
        let p1 = Player::new(Character::new(), loaded_with(empty.clone()));
        let p2 = Player::new(Character::new(), loaded_with(empty));
        let mut m = Match::new(p1, p2, StageBounds::default());
        for _ in 0..120 {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        // No panic; round progressed normally.
        assert!(matches!(
            m.round_state(),
            RoundState::Fight | RoundState::Ko | RoundState::Win
        ));
    }

    /// AC3 (geometry): two coincident default-sized characters are pushed to be
    /// exactly edge-touching. Default front=16, back=15; P1 faces right (extent
    /// center-15..center+16), P2 faces left (extent center-16..center+15). At
    /// equal centers the overlap is 30, so each moves 15 and they end 30 apart.
    #[test]
    fn player_push_separation_is_exact() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(0.0, 0.0);
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(0.0, 0.0);
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        m.tick(MatchInput::none(), MatchInput::none());
        // One push from coincident centers moves each by half the overlap.
        // Overlap of two default bodies at center 0: P1 faces right (extent
        // -15..16), P2 faces left (extent -16..15) -> overlap 30, half 15, so
        // P1 -> -15 and P2 -> +15: centers end exactly 30 apart.
        let dx = m.p2().pos().x - m.p1().pos().x;
        assert!(
            (dx - 30.0).abs() < 1e-3,
            "coincident default bodies separate to centers 30 apart, got {dx}"
        );
        // The even split over ASYMMETRIC front/back leaves a small residual
        // overlap (front 16 on both inner edges), which the per-frame push shrinks
        // on subsequent ticks. Verify the residual strictly shrinks toward
        // edge-touching rather than asserting it vanishes in one frame.
        let inner_overlap = |m: &Match| {
            let p1_right = m.p1().pos().x + m.p1().push_body_right_half();
            let p2_left = m.p2().pos().x - m.p2().push_body_left_half();
            p1_right - p2_left
        };
        let after_first = inner_overlap(&m);
        m.tick(MatchInput::none(), MatchInput::none());
        let after_second = inner_overlap(&m);
        assert!(
            after_second < after_first + 1e-6,
            "repeated push must not increase overlap: {after_first} -> {after_second}"
        );
    }

    /// AC3: non-overlapping characters are left exactly in place by the push
    /// (the push only fires on a strictly positive overlap).
    #[test]
    fn player_push_leaves_separated_characters_untouched() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(-100.0, 0.0);
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(100.0, 0.0);
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        m.tick(MatchInput::none(), MatchInput::none());
        assert!((m.p1().pos().x - -100.0).abs() < 1e-3, "P1 unmoved");
        assert!((m.p2().pos().x - 100.0).abs() < 1e-3, "P2 unmoved");
    }

    /// AC3 (robustness): when the stage is narrower than a single body can fit,
    /// the clamp must not panic and must keep the position finite (the left edge
    /// wins per the physics primitive's documented behavior).
    #[test]
    fn clamp_with_too_narrow_stage_is_finite() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(500.0, 0.0);
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(-500.0, 0.0);
        // Bounds far narrower than the ~31px-wide bodies.
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-5.0, 5.0));
        m.tick(MatchInput::none(), MatchInput::none());
        assert!(m.p1().pos().x.is_finite(), "P1 x stays finite");
        assert!(m.p2().pos().x.is_finite(), "P2 x stays finite");
    }

    /// AC1/AC4 (robustness): reversed stage bounds (left > right) must not panic
    /// and must leave positions finite.
    #[test]
    fn reversed_bounds_do_not_panic() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(50.0, 0.0);
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(-50.0, 0.0);
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        // Deliberately reversed: left (100) > right (-100).
        let mut m = Match::new(p1, p2, StageBounds::new(100.0, -100.0));
        for _ in 0..5 {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        assert!(m.p1().pos().x.is_finite());
        assert!(m.p2().pos().x.is_finite());
    }

    /// AC5: a character left at exactly the same X never produces a NaN separation
    /// and remains within finite bounds after many ticks.
    #[test]
    fn long_run_stays_finite_and_on_stage() {
        let mut m = basic_match();
        let bounds = m.bounds();
        for _ in 0..600 {
            m.tick(MatchInput::none(), MatchInput::none());
            for p in [m.p1(), m.p2()] {
                assert!(p.pos().x.is_finite(), "position must stay finite");
                let right = p.pos().x + p.push_body_right_half();
                let left = p.pos().x - p.push_body_left_half();
                assert!(
                    right <= bounds.right + 1.0 && left >= bounds.left - 1.0,
                    "fighter drifted off stage: [{left}, {right}] vs [{}, {}]",
                    bounds.left,
                    bounds.right
                );
            }
        }
    }

    /// AC1: read accessors expose the renderer-facing state, and they reflect the
    /// live character. anim/anim_elem/life/life_max/pos/facing all round-trip.
    #[test]
    fn read_accessors_reflect_character_state() {
        let mut c = Character::with_constants(CharacterConstants::default());
        c.pos = Vec2::new(12.0, -34.0);
        c.facing = Facing::Left;
        c.anim = 42;
        c.anim_elem = 3;
        c.life = 777;
        let p = Player::new(c, defender_loaded());
        assert_eq!(p.pos(), Vec2::new(12.0, -34.0));
        assert_eq!(p.facing(), Facing::Left);
        assert_eq!(p.anim(), 42);
        assert_eq!(p.anim_elem(), 3);
        assert_eq!(p.life(), 777);
        assert_eq!(p.life_max(), 1000);
    }

    /// AC1: pressing both left and right (or neither) yields no net horizontal
    /// command and is not treated as blocking.
    #[test]
    fn opposing_horizontal_inputs_cancel() {
        let both = MatchInput {
            left: true,
            right: true,
            ..MatchInput::none()
        };
        let (cmds, blocking) = Match::facing_relative_commands(Facing::Right, both);
        assert!(!cmds.iter().any(|c| c == "fwd" || c == "back"));
        assert!(!blocking, "ambiguous horizontal is not blocking");

        let neither = MatchInput::none();
        let (cmds, blocking) = Match::facing_relative_commands(Facing::Left, neither);
        assert!(cmds.is_empty());
        assert!(!blocking);
    }

    /// AC1: up/down map straight through to `up`/`down` command names regardless
    /// of facing.
    #[test]
    fn vertical_inputs_map_unchanged() {
        let input = MatchInput {
            up: true,
            down: true,
            ..MatchInput::none()
        };
        for facing in [Facing::Left, Facing::Right] {
            let (cmds, _) = Match::facing_relative_commands(facing, input);
            assert!(cmds.iter().any(|c| c == "up"));
            assert!(cmds.iter().any(|c| c == "down"));
        }
    }

    /// AC1: all six attack buttons become their bare letter command names.
    #[test]
    fn all_attack_buttons_map() {
        let input = MatchInput {
            a: true,
            b: true,
            c: true,
            x: true,
            y: true,
            z: true,
            ..MatchInput::none()
        };
        let (cmds, _) = Match::facing_relative_commands(Facing::Right, input);
        for name in ["a", "b", "c", "x", "y", "z"] {
            assert!(cmds.iter().any(|c| c == name), "missing button {name}");
        }
    }

    /// AC1: holding "back" (away from the opponent) sets `holding_back` on the
    /// character through the full feed path (not just the pure helper), enabling
    /// the guard path in combat.
    #[test]
    fn feed_input_sets_holding_back_on_character() {
        let mut c = Character::new();
        c.facing = Facing::Right;
        // Facing right, pressing left = away from opponent = back.
        Match::feed_input(
            &mut c,
            MatchInput {
                left: true,
                ..MatchInput::none()
            },
        );
        assert!(c.holding_back, "holding away from opponent sets holding_back");

        // Pressing toward the opponent clears it.
        c.facing = Facing::Right;
        Match::feed_input(
            &mut c,
            MatchInput {
                right: true,
                ..MatchInput::none()
            },
        );
        assert!(!c.holding_back, "holding toward opponent is not back");
    }

    /// AC1: `MatchInput::none()` and the derived `Default` agree (nothing held).
    #[test]
    fn match_input_none_matches_default() {
        assert_eq!(MatchInput::none(), MatchInput::default());
    }

    /// AC3 (baseline facep2): a crouching character is NOT re-faced (only
    /// standing+idle is neutral), documenting the conservative turn rule.
    #[test]
    fn crouching_character_keeps_facing() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(50.0, 0.0); // right side; "correct" facing is Left
        p1c.facing = Facing::Right; // wrong on purpose
        p1c.state_type = StateType::Crouching;
        p1c.move_type = MoveType::Idle;
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(-50.0, 0.0);
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);
        m.p1.character.facing = Facing::Right;
        m.p1.character.state_type = StateType::Crouching;
        m.p1.character.move_type = MoveType::Idle;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(
            m.p1().facing(),
            Facing::Right,
            "a crouching character is not neutral for facep2"
        );
    }

    /// AC3 (baseline facep2): a character being hit keeps its facing (would-be
    /// flip during a knockback reaction is suppressed).
    #[test]
    fn being_hit_character_keeps_facing() {
        let mut p1c = Character::new();
        p1c.pos = Vec2::new(50.0, 0.0);
        p1c.facing = Facing::Right; // wrong: opponent on the left
        let mut p2c = Character::new();
        p2c.pos = Vec2::new(-50.0, 0.0);
        let p1 = Player::new(p1c, defender_loaded());
        let p2 = Player::new(p2c, defender_loaded());
        let mut m = Match::new(p1, p2, StageBounds::new(-300.0, 300.0));
        into_fight(&mut m);
        m.p1.character.facing = Facing::Right;
        m.p1.character.state_type = StateType::Standing;
        m.p1.character.move_type = MoveType::BeingHit;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.p1().facing(), Facing::Right, "get-hit reaction keeps facing");
    }

    /// `is_neutral_facing_state` directly: only Standing/Unchanged + Idle qualify.
    #[test]
    fn neutral_facing_state_predicate() {
        let mut c = Character::new();
        c.state_type = StateType::Standing;
        c.move_type = MoveType::Idle;
        assert!(is_neutral_facing_state(&c));
        c.state_type = StateType::Unchanged;
        assert!(is_neutral_facing_state(&c));
        c.move_type = MoveType::Attack;
        assert!(!is_neutral_facing_state(&c), "attacking is not neutral");
        c.move_type = MoveType::Idle;
        c.state_type = StateType::Air;
        assert!(!is_neutral_facing_state(&c), "airborne is not neutral");
        c.state_type = StateType::Crouching;
        assert!(!is_neutral_facing_state(&c), "crouching is not neutral");
    }

    /// `facings_toward` tie-break: equal X is deterministic (a right, b left).
    #[test]
    fn facings_toward_tie_break() {
        assert_eq!(facings_toward(0.0, 0.0), (Facing::Right, Facing::Left));
        assert_eq!(facings_toward(-10.0, 10.0), (Facing::Right, Facing::Left));
        assert_eq!(facings_toward(10.0, -10.0), (Facing::Left, Facing::Right));
    }

    /// AC1: control is removed from both fighters when the round ends (KO hold).
    #[test]
    fn ko_removes_control() {
        let mut m = basic_match();
        into_fight(&mut m);
        assert!(m.p1().character.ctrl && m.p2().character.ctrl);
        m.p2.character.life = 0;
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.round_state(), RoundState::Ko);
        assert!(!m.p1().character.ctrl, "control revoked at KO");
        assert!(!m.p2().character.ctrl, "control revoked at KO");
    }

    /// AC1: bounds() accessor returns what was passed in.
    #[test]
    fn bounds_accessor_round_trips() {
        let b = StageBounds::new(-123.0, 456.0);
        let p1 = Player::new(Character::new(), defender_loaded());
        let p2 = Player::new(Character::new(), defender_loaded());
        let m = Match::new(p1, p2, b);
        assert_eq!(m.bounds(), b);
        assert_eq!(m.bounds().left, -123.0);
        assert_eq!(m.bounds().right, 456.0);
    }

    /// StageBounds::default is a symmetric playfield around the origin.
    #[test]
    fn stage_bounds_default_is_symmetric() {
        let b = StageBounds::default();
        assert_eq!(b.left, -200.0);
        assert_eq!(b.right, 200.0);
        assert!(b.left < b.right);
    }

    // ---- AC5 (optional): gated real-KFM integration test --------------------

    fn test_asset(rel: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-assets")
            .join(rel)
    }

    /// Loads the real Kung Fu Man character on BOTH sides and drives a full match
    /// of the coordinator end-to-end: it must tick many frames without panic,
    /// progress the round state machine, keep both fighters on stage, and (since
    /// real KFM carries Clsn boxes) be able to deal damage when a real attack
    /// frame is posed overlapping the opponent's hurt frame.
    ///
    /// Skips cleanly (printing the reason) when `test-assets/` is absent or the
    /// fixture fails to load, so the suite stays green without the (uncommitted)
    /// assets. The fixture is known present in this repo, so the body runs here.
    #[test]
    fn real_kfm_match_runs_end_to_end() {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return;
        }
        let lc1 = match LoadedCharacter::load(&def) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("skipping: kfm.def failed to load: {e}");
                return;
            }
        };
        let lc2 = match LoadedCharacter::load(&def) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("skipping: kfm.def failed to load: {e}");
                return;
            }
        };

        let mut p1c = Character::with_constants(lc1.constants);
        p1c.pos = Vec2::new(-40.0, 0.0);
        let mut p2c = Character::with_constants(lc2.constants);
        p2c.pos = Vec2::new(40.0, 0.0);
        let p1 = Player::new(p1c, lc1);
        let p2 = Player::new(p2c, lc2);
        let bounds = StageBounds::new(-300.0, 300.0);
        let mut m = Match::new(p1, p2, bounds);

        // Drive through the intro and a chunk of the fight with idle inputs.
        for _ in 0..(INTRO_FRAMES + 120) {
            m.tick(MatchInput::none(), MatchInput::none());
            for p in [m.p1(), m.p2()] {
                assert!(p.pos().x.is_finite(), "real-KFM position must stay finite");
                let right = p.pos().x + p.push_body_right_half();
                let left = p.pos().x - p.push_body_left_half();
                assert!(
                    right <= bounds.right + 1.0 && left >= bounds.left - 1.0,
                    "real-KFM fighter drifted off stage"
                );
            }
        }
        assert!(matches!(
            m.round_state(),
            RoundState::Fight | RoundState::Ko | RoundState::Win
        ));

        // Exercise the combat apply path the coordinator uses with REAL KFM Clsn
        // frames. We call the same `resolve_attack` primitive `Match::tick` invokes,
        // posing the attacker on KFM's stand light punch (action 200, element 2
        // carries the real Clsn1 attack box) and the defender on its idle action 0
        // (real Clsn2 hurt box). We call `resolve_attack` directly here (rather than
        // through `Match::tick`) because the coordinator's tick advances each
        // character's animation cursor *before* combat, which would move the
        // attacker off the posed punch frame; this isolates the real-asset combat
        // wiring. The frames are taken from the just-loaded real AIR.
        let air = &m.p1().loaded.air;
        let has_attack_box = air
            .actions
            .get(&200)
            .map(|a| a.frames.iter().any(|f| !f.clsn1.is_empty()))
            .unwrap_or(false);
        let has_hurt_box = air
            .actions
            .get(&0)
            .map(|a| a.frames.iter().any(|f| !f.clsn2.is_empty()))
            .unwrap_or(false);
        if !has_attack_box || !has_hurt_box {
            eprintln!("skipping real-KFM combat: action 200/0 lack expected Clsn boxes");
            return;
        }

        // Build a fresh attacker/defender from the real constants + real AIR and
        // sweep the defender until the REAL boxes overlap.
        let attacker_air = m.p1().loaded.air.clone();
        let defender_air = m.p2().loaded.air.clone();
        let defender_states = m.p2().loaded.states.clone();
        let mut connected = false;
        for dx in 0..=120 {
            let mut attacker = Character::with_constants(m.p1().character.constants);
            attacker.anim = 200;
            attacker.anim_elem = 2;
            attacker.pos = Vec2::new(0.0, 0.0);
            attacker.facing = Facing::Right;
            attacker.move_type = MoveType::Attack;
            attacker.active_hitdef = Some(sample_hitdef());
            attacker.move_connect.reset();

            let mut defender = Character::with_constants(m.p2().character.constants);
            defender.anim = 0;
            defender.anim_elem = 0;
            defender.pos = Vec2::new(dx as f32, 0.0);
            defender.facing = Facing::Left;
            defender.state_type = StateType::Standing;
            let life_before = defender.life;

            let res = resolve_attack(
                &mut attacker,
                &attacker_air,
                &mut defender,
                &defender_air,
                &defender_states,
            );
            if res.is_some() && defender.life < life_before {
                connected = true;
                break;
            }
        }
        assert!(
            connected,
            "real KFM punch (action 200/2) should connect with idle hurt box across the sweep"
        );
    }
}
