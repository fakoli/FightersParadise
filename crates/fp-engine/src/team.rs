//! # Team / Turns / Simul match flow (T017, feature F010)
//!
//! [`TeamMatch`] generalizes the hardcoded two-fighter [`Match`](crate::Match)
//! into a coordinator that supports **more than one fighter per side**, while
//! leaving the 1v1 [`Match`](crate::Match) untouched as the underlying combat
//! primitive.
//!
//! A team match has a *roster* per side (a non-empty list of
//! [`Player`](crate::Player)s) and a [`TeamMode`] that decides how those rosters
//! are fought:
//!
//! - [`TeamMode::Single`] — one fighter per side; the default and exactly the
//!   classic 1v1 flow. (Building a [`TeamMatch`] with one-fighter rosters in this
//!   mode behaves identically to a bare [`Match`](crate::Match).)
//! - [`TeamMode::Turns`] — each side fields **one active fighter at a time**; the
//!   active pair fights a 1v1 [`Match`](crate::Match). When a side's active
//!   fighter is knocked out, that side **hands off** to its next reserve (full
//!   life) while the surviving fighter keeps its remaining life and meter. A side
//!   loses the team match once it has no reserves left to send in.
//! - [`TeamMode::Simul`] — **all** fighters are active simultaneously. The lead
//!   pair fights the underlying [`Match`](crate::Match); every reserve fighter on
//!   each side is also ticked each frame (so it runs its own state machine /
//!   animates). A side is defeated only when **every** fighter on it is KO'd.
//!
//! ## Why compose [`Match`](crate::Match) rather than rewrite it
//!
//! The 1v1 [`Match`](crate::Match) already encodes the full MUGEN-ish per-frame
//! pipeline — facing-relative input, both characters' state-machine ticks,
//! `Target*` throw ops, bidirectional combat + priority clash, push/bounds,
//! face-the-opponent, hit-sparks, and the best-of-N round flow. [`TeamMatch`]
//! reuses all of that for the *active pair* and layers team selection/hand-off on
//! top, so multi-fighter support does not duplicate (or risk regressing) the
//! combat core.
//!
//! Nothing here panics: rosters are clamped to a bounded size at construction,
//! indices stay in range, and the underlying [`Match`](crate::Match) already
//! degrades safely on bad content.

use fp_character::StageView;
use fp_formats::air::AirFile;
use serde::{Deserialize, Serialize};

use crate::{GameMode, Match, MatchInput, Player, PlayerDriver, StageBounds, Winner};

/// How a [`TeamMatch`]'s two rosters are fought.
///
/// See the [module docs](self) for the full description of each mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TeamMode {
    /// One fighter per side — the classic 1v1 flow (the default).
    #[default]
    Single,
    /// All fighters on a side are active at once; the side is beaten only when
    /// every one of its fighters is knocked out.
    Simul,
    /// One active fighter per side at a time; a knockout hands off to the next
    /// reserve, and a side loses when it runs out of fighters.
    Turns,
}

/// Identifies one of a [`TeamMatch`]'s two sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    /// The left / player-1 side.
    P1,
    /// The right / player-2 side.
    P2,
}

/// Whether a [`TeamMatch`] is still being contested or has been decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TeamMatchState {
    /// At least one fighter remains on each side; the match continues.
    InProgress,
    /// The match has been decided; see [`TeamMatch::outcome`] / [`TeamMatch::winner`].
    Over,
}

/// The decided result of a [`TeamMatch`] (T028).
///
/// Unlike [`TeamMatch::winner`] (which is `Option<Side>` and is `None` on a draw),
/// this captures the genuine three-way outcome — including a **double-KO draw**,
/// where both sides are wiped out on the same frame and neither is awarded the win.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TeamOutcome {
    /// The left / player-1 side won.
    P1,
    /// The right / player-2 side won.
    P2,
    /// A genuine draw — both sides were eliminated together (e.g. a double-KO).
    Draw,
}

impl TeamOutcome {
    /// The winning [`Side`] this outcome favours, or [`None`] for a [`TeamOutcome::Draw`].
    #[must_use]
    pub fn winning_side(self) -> Option<Side> {
        match self {
            TeamOutcome::P1 => Some(Side::P1),
            TeamOutcome::P2 => Some(Side::P2),
            TeamOutcome::Draw => None,
        }
    }
}

/// A hard ceiling on how many fighters a single side's roster may hold, so a
/// caller cannot grow the simulation without bound. MUGEN's `Simul`/`Turns`
/// team sizes are small (its `team.x.size` caps at 4); this is a generous limit.
const MAX_TEAM_SIZE: usize = 8;

/// A match between two **rosters** of fighters, supporting Simul (all active) and
/// Turns (sequential hand-off) flow on top of the 1v1 [`Match`](crate::Match).
///
/// Construct one with [`TeamMatch::new`] (default [`TeamMode::Single`]) or
/// [`TeamMatch::with_mode`], passing at least one fighter per side, then drive it
/// once per 60Hz frame with [`TeamMatch::tick`] exactly like a [`Match`]. The
/// **active pair** — the lead fighter on each side — is fought through an inner
/// [`Match`] exposed via [`TeamMatch::active`]; read team state through
/// [`TeamMatch::state`] / [`TeamMatch::winner`] / the per-side roster accessors.
///
/// # Invariants
///
/// - The inner [`Match`] always holds exactly one active fighter per side.
/// - Each side's reserve list holds at most `MAX_TEAM_SIZE - 1` fighters
///   (the roster is clamped at construction).
/// - In [`TeamMode::Single`] no reserves ever enter the fight (extra fighters, if
///   supplied, stay inert).
pub struct TeamMatch {
    /// Each side's **reserve** fighters (those not currently in the inner
    /// [`Match`]), in send-in order. `reserves.0` is P1's, `reserves.1` is P2's.
    /// The active fighter of each side lives inside [`inner`](Self::inner), not
    /// here.
    reserves: (Vec<Player>, Vec<Player>),
    /// Index (1-based count of the active fighter) of the currently active
    /// fighter on each side. Starts at `0` (the lead); in Turns it advances on a
    /// KO hand-off. Purely for diagnostics / a HUD.
    active_index: (usize, usize),
    /// The inner 1v1 [`Match`] fighting the active pair. Rebuilt on a Turns
    /// hand-off; otherwise persistent across the whole team match.
    ///
    /// Stored in an [`Option`] purely so a Turns hand-off can move the active
    /// [`Player`]s **out** of the match (via [`Match::into_players`], which
    /// consumes the match) and rebuild it; it is always `Some` between ticks. The
    /// private [`inner`](Self::inner_ref) / [`inner_mut`](Self::inner_mut)
    /// accessors hide the `Option`.
    inner: Option<Match>,
    /// How the rosters are fought.
    mode: TeamMode,
    /// The stage bounds, kept so a rebuilt inner [`Match`] reuses them.
    bounds: StageBounds,
    /// Whether the team match is still in progress or decided.
    state: TeamMatchState,
    /// The decided outcome once [`state`](Self::state) is [`TeamMatchState::Over`]
    /// (T028): the winning [`Side`], or a genuine [`TeamOutcome::Draw`] on a
    /// double-elimination. [`TeamMatch::winner`] projects this onto `Option<Side>`.
    outcome: Option<TeamOutcome>,
    /// The shared common-effects (`fightfx`) animation set to install on the inner
    /// [`Match`], if any. Kept here so it survives a Turns hand-off rebuild (the
    /// rebuilt inner match is re-seeded with it). Installed via
    /// [`TeamMatch::set_common_fx`]; `None` means the inner match has no common set
    /// (the pre-asset behaviour — common sparks are a best-effort skip).
    common_fx: Option<AirFile>,
    /// The match-time [`GameMode`] (F027 / T066) to install on the inner
    /// [`Match`]. Kept here so a Turns hand-off rebuild re-seeds the new inner
    /// match with it. Defaults to [`GameMode::Versus`]; set via
    /// [`TeamMatch::set_game_mode`]. Training is reachable from the 1v1 menu path
    /// ([`TeamMode::Single`]); the multi-fighter team modes leave it `Versus`.
    game_mode: GameMode,
}

impl TeamMatch {
    /// Builds a team match in [`TeamMode::Single`] (1v1) from two single fighters —
    /// a drop-in for the classic two-`Player` [`Match`].
    ///
    /// Equivalent to [`TeamMatch::with_mode`] with one-element rosters and
    /// [`TeamMode::Single`].
    #[must_use]
    pub fn new(p1: Player, p2: Player, bounds: StageBounds) -> Self {
        Self::with_mode(vec![p1], vec![p2], bounds, TeamMode::Single)
    }

    /// Builds a team match from two **rosters** and a [`TeamMode`].
    ///
    /// Each roster's lead fighter (front of the list) becomes the active fighter
    /// inside the inner [`Match`]; the rest become that side's reserves. Rosters
    /// are clamped to at most [`MAX_TEAM_SIZE`] fighters (overflow is dropped with
    /// a warning). In [`TeamMode::Single`] only the lead of each roster ever
    /// fights; any extra fighters are inert.
    ///
    /// # Panics
    ///
    /// This constructor requires **at least one** fighter per side to build the
    /// inner [`Match`]. Passing an empty roster for a side is a programmer error;
    /// it panics with a clear message rather than fabricating a fighter, because
    /// (unlike bad *content*, which is degraded gracefully) an empty roster is a
    /// caller-side API misuse that cannot be papered over. Construct with
    /// non-empty rosters (the engine and `fp-app` always do).
    #[must_use]
    pub fn with_mode(
        mut p1_roster: Vec<Player>,
        mut p2_roster: Vec<Player>,
        bounds: StageBounds,
        mode: TeamMode,
    ) -> Self {
        Self::clamp_roster(&mut p1_roster, Side::P1);
        Self::clamp_roster(&mut p2_roster, Side::P2);

        assert!(
            !p1_roster.is_empty() && !p2_roster.is_empty(),
            "TeamMatch requires at least one fighter per side"
        );

        let (p1_lead, p1_reserves) = split_lead(p1_roster);
        let (p2_lead, p2_reserves) = split_lead(p2_roster);

        let mut inner = Match::new(p1_lead, p2_lead, bounds);
        // (T028) In a multi-fighter mode the inner 1v1 match must NOT run its own
        // best-of-N round flow: a decided inner round would heal a knocked-out
        // fighter and start a fresh round, masking the team-level KO the team flow
        // decides eliminations from. Single-round mode makes the first decided round
        // final (no life restore) and surfaces a genuine double-KO as a draw. Single
        // mode keeps the normal 1v1 round flow so 1v1 behaviour is unchanged.
        inner.set_single_round(mode != TeamMode::Single);

        Self {
            reserves: (p1_reserves, p2_reserves),
            active_index: (0, 0),
            inner: Some(inner),
            mode,
            bounds,
            state: TeamMatchState::InProgress,
            outcome: None,
            common_fx: None,
            game_mode: GameMode::Versus,
        }
    }

    /// Assigns every fighter's [`Character::ai_level`](fp_character::Character::ai_level)
    /// from its **side's** input [`PlayerDriver`] (T052).
    ///
    /// All members of a side — the active lead **and** its reserves — inherit that
    /// side's driver, so a CPU team's whole roster reads its difficulty's
    /// [`AiDifficulty::ai_level`](fp_input::AiDifficulty::ai_level) (`1..=8`) from
    /// the `AILevel` trigger, while a human side's roster stays at level `0`. The
    /// level is a one-time identity assignment that survives a Turns hand-off
    /// (the reserve was set here before it ever enters the inner match). Call once
    /// after construction; with no call both sides keep the human default (`0`).
    pub fn set_drivers(&mut self, p1_driver: PlayerDriver, p2_driver: PlayerDriver) {
        // The active pair lives in the inner 1v1 match.
        self.inner_mut().set_drivers(p1_driver, p2_driver);
        // Every reserve inherits its side's driver, so a Turns hand-off promotes a
        // fighter that already carries the correct AI level.
        let p1_level = p1_driver.ai_level();
        let p2_level = p2_driver.ai_level();
        for reserve in &mut self.reserves.0 {
            reserve.character.set_ai_level(p1_level);
        }
        for reserve in &mut self.reserves.1 {
            reserve.character.set_ai_level(p2_level);
        }
    }

    /// Installs the shared common-effects (`fightfx`) animation set on the inner
    /// [`Match`] (see [`Match::set_common_fx`]).
    ///
    /// Stored on the team match so a Turns hand-off rebuild re-seeds the new inner
    /// match with the same set; calling it is optional and best-effort, exactly like
    /// the 1v1 path. Replaces any previously installed set.
    pub fn set_common_fx(&mut self, air: AirFile) {
        self.inner_mut().set_common_fx(air.clone());
        self.common_fx = Some(air);
    }

    /// Sets the match-time [`GameMode`] (F027 / T066) on the inner [`Match`].
    ///
    /// Stored on the team match so a Turns hand-off rebuild re-seeds the new inner
    /// match with the same mode. [`GameMode::Versus`] (the default) keeps the
    /// normal round flow; [`GameMode::Training`] disables round termination so the
    /// Lab fight runs indefinitely. Training is used only on the 1v1 menu path
    /// ([`TeamMode::Single`]); the multi-fighter team modes leave it `Versus`.
    pub fn set_game_mode(&mut self, mode: GameMode) {
        self.inner_mut().set_game_mode(mode);
        self.game_mode = mode;
    }

    /// The match-time [`GameMode`] this team is being fought under (F027 / T066).
    /// See [`TeamMatch::set_game_mode`].
    #[must_use]
    pub fn game_mode(&self) -> GameMode {
        self.game_mode
    }

    /// Enables/disables the per-side "infinite life" training toggle (F027 /
    /// T067) on the active inner [`Match`]. See [`Match::set_infinite_life`].
    pub fn set_infinite_life(&mut self, side: Side, enabled: bool) {
        self.inner_mut().set_infinite_life(side, enabled);
    }

    /// Whether "infinite life" is on for the given side (F027 / T067).
    #[must_use]
    pub fn infinite_life(&self, side: Side) -> bool {
        self.inner_ref().infinite_life(side)
    }

    /// Enables/disables the per-side "infinite meter" training toggle (F027 /
    /// T067) on the active inner [`Match`]. See [`Match::set_infinite_meter`].
    pub fn set_infinite_meter(&mut self, side: Side, enabled: bool) {
        self.inner_mut().set_infinite_meter(side, enabled);
    }

    /// Whether "infinite meter" is on for the given side (F027 / T067).
    #[must_use]
    pub fn infinite_meter(&self, side: Side) -> bool {
        self.inner_ref().infinite_meter(side)
    }

    /// Resets both fighters of the active pair to their round-start positions,
    /// facing, and full life without advancing the round (the training
    /// "reset position" key, F027 / T067). See [`Match::reset_positions`].
    pub fn reset_positions(&mut self) {
        self.inner_mut().reset_positions();
    }

    /// The inner 1v1 [`Match`] (always present between ticks). Panics only if the
    /// match was left taken out across a tick boundary, which the hand-off path
    /// never does — it takes and re-installs within a single `tick`.
    fn inner_ref(&self) -> &Match {
        self.inner
            .as_ref()
            .expect("inner match is always present between ticks")
    }

    /// Mutable access to the inner 1v1 [`Match`]. See [`inner_ref`](Self::inner_ref).
    fn inner_mut(&mut self) -> &mut Match {
        self.inner
            .as_mut()
            .expect("inner match is always present between ticks")
    }

    /// Clamps a roster to at most [`MAX_TEAM_SIZE`] fighters, dropping (and
    /// warn-logging) any overflow.
    fn clamp_roster(roster: &mut Vec<Player>, side: Side) {
        if roster.len() > MAX_TEAM_SIZE {
            tracing::warn!(
                ?side,
                requested = roster.len(),
                cap = MAX_TEAM_SIZE,
                "team roster exceeds cap; dropping overflow fighters"
            );
            roster.truncate(MAX_TEAM_SIZE);
        }
    }

    /// The team mode this match is being fought under.
    #[must_use]
    pub fn mode(&self) -> TeamMode {
        self.mode
    }

    /// The stage bounds the fighters are clamped to.
    #[must_use]
    pub fn bounds(&self) -> StageBounds {
        self.bounds
    }

    /// Read access to the inner 1v1 [`Match`] fighting the **active pair**.
    ///
    /// In [`TeamMode::Single`]/[`TeamMode::Simul`] this is the lead pair; in
    /// [`TeamMode::Turns`] it is whichever fighters are currently front-line. A
    /// renderer / HUD draws the active fighters from here exactly as for a 1v1
    /// match, and reads [`Match::round_state`] / [`Match::timer`] from it.
    #[must_use]
    pub fn active(&self) -> &Match {
        self.inner_ref()
    }

    /// The active (front-line) fighter on the given side.
    #[must_use]
    pub fn active_player(&self, side: Side) -> &Player {
        match side {
            Side::P1 => self.inner_ref().p1(),
            Side::P2 => self.inner_ref().p2(),
        }
    }

    /// The reserve fighters (not yet sent in) on the given side, in order. Empty
    /// once a side has only its active fighter (or none) left.
    #[must_use]
    pub fn reserves(&self, side: Side) -> &[Player] {
        match side {
            Side::P1 => &self.reserves.0,
            Side::P2 => &self.reserves.1,
        }
    }

    /// The number of fighters still standing on the given side: the active fighter
    /// (if still alive) plus the **living** reserves waiting to come in.
    ///
    /// A side is beaten when this reaches `0`. Drives the Turns hand-off decision
    /// and a team-status HUD. A reserve whose life has reached `0` (e.g. a Simul
    /// teammate hit while off the active stage) no longer counts.
    #[must_use]
    pub fn fighters_remaining(&self, side: Side) -> usize {
        let living_reserves = self.reserves(side).iter().filter(|p| p.life() > 0).count();
        let active_alive = self.active_player(side).life() > 0;
        living_reserves + usize::from(active_alive)
    }

    /// The 1-based ordinal of the active fighter on the given side (`1` for the
    /// lead, `2` after the first Turns hand-off, …). For a HUD / diagnostics.
    #[must_use]
    pub fn active_ordinal(&self, side: Side) -> usize {
        let zero_based = match side {
            Side::P1 => self.active_index.0,
            Side::P2 => self.active_index.1,
        };
        zero_based + 1
    }

    /// Whether the team match is still being contested or has been decided.
    #[must_use]
    pub fn state(&self) -> TeamMatchState {
        self.state
    }

    /// The side that won the whole team match, or [`None`] until it is decided **or**
    /// when it ended in a genuine [`TeamOutcome::Draw`] (a double-KO).
    ///
    /// To distinguish "still in progress" from "ended in a draw" use
    /// [`TeamMatch::outcome`] (which returns `Some(TeamOutcome::Draw)` for a draw).
    #[must_use]
    pub fn winner(&self) -> Option<Side> {
        self.outcome.and_then(TeamOutcome::winning_side)
    }

    /// The decided three-way outcome of the team match (T028), or [`None`] until it
    /// is decided. Unlike [`TeamMatch::winner`] this reports a genuine double-KO as
    /// [`TeamOutcome::Draw`] rather than collapsing it to a side.
    #[must_use]
    pub fn outcome(&self) -> Option<TeamOutcome> {
        self.outcome
    }

    /// Advances the team match by one 60Hz frame.
    ///
    /// Ticks the inner [`Match`] (the active pair) with the two sides' inputs, then:
    ///
    /// - In [`TeamMode::Simul`], ticks every **reserve** fighter's state machine so
    ///   the off-screen team members keep animating, and checks whether a whole
    ///   side has been wiped out.
    /// - In [`TeamMode::Turns`], detects when the active fighter on a side has been
    ///   knocked out and, if that side still has a reserve, **hands off** to the
    ///   next fighter by rebuilding the inner [`Match`] (the survivor keeps its
    ///   life/meter; the incoming fighter starts fresh). A side with no reserve
    ///   left loses the team match.
    /// - In [`TeamMode::Single`], behaves exactly like the underlying [`Match`].
    ///
    /// Never panics. Once the team match is [`TeamMatchState::Over`] this is a
    /// no-op.
    pub fn tick(&mut self, p1_input: MatchInput, p2_input: MatchInput) {
        if self.state == TeamMatchState::Over {
            return;
        }

        // (1) Drive the active pair through the full 1v1 pipeline. In Simul each
        //     active fighter's live teammate (its side's lead reserve) is supplied
        //     so the `partner` redirect resolves to a real ally instead of `0`
        //     (T027). The reserve rosters are separate storage from the inner
        //     match's two players, so borrowing a reserve immutably while ticking
        //     the inner match mutably does not alias.
        match self.mode {
            TeamMode::Simul => {
                let p1_partner = self.reserves.0.first().map(|p| &p.character);
                let p2_partner = self.reserves.1.first().map(|p| &p.character);
                self.inner
                    .as_mut()
                    .expect("inner match is always present between ticks")
                    .tick_with_partners(p1_input, p2_input, p1_partner, p2_partner);
                self.tick_reserves();
                self.resolve_simul();
            }
            TeamMode::Turns => {
                // One active fighter per side at a time — no simultaneous teammate,
                // so `partner` stays unset.
                self.inner_mut().tick(p1_input, p2_input);
                self.resolve_turns();
            }
            TeamMode::Single => {
                self.inner_mut().tick(p1_input, p2_input);
                self.sync_single_result();
            }
        }
    }

    /// Ticks every reserve fighter's state machine one frame (Simul), so a team's
    /// off-screen members animate rather than freeze. Each reserve sees no opponent
    /// (it is not in the active pair) and runs against its own loaded assets.
    fn tick_reserves(&mut self) {
        let stage: StageView = self.bounds.view();
        for reserve in self.reserves.0.iter_mut().chain(self.reserves.1.iter_mut()) {
            reserve.tick_standalone(stage);
        }
    }

    /// Simul resolution: a side is defeated only when **every** fighter on it
    /// (active + reserves) is knocked out. While both sides still field at least
    /// one fighter the match continues.
    fn resolve_simul(&mut self) {
        let p1_alive = self.fighters_remaining(Side::P1) > 0;
        let p2_alive = self.fighters_remaining(Side::P2) > 0;
        match (p1_alive, p2_alive) {
            (true, true) => {}
            (true, false) => self.declare_winner(Side::P1),
            (false, true) => self.declare_winner(Side::P2),
            (false, false) => self.declare_winner_from_inner(),
        }
    }

    /// Turns resolution: when the active fighter on a side is knocked out, hand off
    /// to that side's next reserve (if any). A side with no reserve left loses.
    fn resolve_turns(&mut self) {
        let p1_down = self.inner_ref().p1().life() <= 0;
        let p2_down = self.inner_ref().p2().life() <= 0;

        if !p1_down && !p2_down {
            // Nobody was knocked out this frame. In Turns we only hand off on an
            // actual KO, so a non-KO inner decision (e.g. a time-over life
            // comparison) ends the team match by the inner verdict — including a
            // genuine equal-life time-over draw (the inner match is single-round).
            if let Some(w) = self.inner_ref().match_winner() {
                self.declare_outcome(outcome_of(w));
            }
            return;
        }

        // At least one active fighter is down. A side can continue if it was not
        // downed, or it was downed but has a reserve to send in.
        let p1_can_continue = !p1_down || self.has_reserve(Side::P1);
        let p2_can_continue = !p2_down || self.has_reserve(Side::P2);

        match (p1_can_continue, p2_can_continue) {
            (false, false) => self.declare_winner_from_inner(),
            (false, true) => self.declare_winner(Side::P2),
            (true, false) => self.declare_winner(Side::P1),
            (true, true) => self.hand_off(p1_down, p2_down),
        }
    }

    /// Whether the given side has at least one reserve fighter waiting to come in.
    fn has_reserve(&self, side: Side) -> bool {
        !self.reserves(side).is_empty()
    }

    /// Performs a Turns hand-off: replaces each downed side's active fighter with
    /// its next reserve and rebuilds the inner [`Match`] so the new active pair
    /// fights. A side that was not downed keeps its current fighter (life and meter
    /// intact). At least one of the two flags is expected to be true.
    fn hand_off(&mut self, p1_down: bool, p2_down: bool) {
        let bounds = self.bounds;
        // Move the two active players out of the inner Match so we can swap a
        // downed one for a reserve and rebuild. `into_players` consumes the Match,
        // so we take it out of its slot and leave the slot to be refilled below.
        let (mut p1_active, mut p2_active) = self
            .inner
            .take()
            .map(Match::into_players)
            .expect("inner match is always present between ticks");

        if p1_down {
            if let Some(next) = pop_front(&mut self.reserves.0) {
                self.active_index.0 += 1;
                p1_active = next;
                tracing::info!(
                    ordinal = self.active_index.0 + 1,
                    "P1 Turns hand-off to next fighter"
                );
            }
        }
        if p2_down {
            if let Some(next) = pop_front(&mut self.reserves.1) {
                self.active_index.1 += 1;
                p2_active = next;
                tracing::info!(
                    ordinal = self.active_index.1 + 1,
                    "P2 Turns hand-off to next fighter"
                );
            }
        }

        // (T028) The rebuilt inner match keeps single-round / no-life-restore mode:
        // Turns is never `TeamMode::Single`, so a decided inner round must again end
        // it (and the survivor must not be healed) rather than restart a 1v1 round.
        let mut rebuilt = Match::new(p1_active, p2_active, bounds);
        rebuilt.set_single_round(true);
        // Re-seed the match-time mode (T066) so a hand-off keeps the team's
        // configured mode. (Training is 1v1-only, so this is `Versus` in practice
        // for Turns, but keeping the field authoritative avoids a latent bug.)
        rebuilt.set_game_mode(self.game_mode);
        // Re-seed the shared common-effects set so hit-sparks keep rendering after a
        // hand-off (the old inner match is consumed; this set is owned by the team).
        if let Some(air) = self.common_fx.clone() {
            rebuilt.set_common_fx(air);
        }
        self.inner = Some(rebuilt);
    }

    /// Mirrors a decided inner [`Match`] result onto the team result in
    /// [`TeamMode::Single`], so a single-fighter team match ends exactly when its
    /// underlying 1v1 match does.
    fn sync_single_result(&mut self) {
        if let Some(w) = self.inner_ref().match_winner() {
            self.declare_outcome(outcome_of(w));
        }
    }

    /// Declares `side` the team-match winner and marks the match over.
    fn declare_winner(&mut self, side: Side) {
        self.declare_outcome(match side {
            Side::P1 => TeamOutcome::P1,
            Side::P2 => TeamOutcome::P2,
        });
    }

    /// Records the team match's decided `outcome` and marks it over (idempotent —
    /// a second call after the match is already over is ignored).
    fn declare_outcome(&mut self, outcome: TeamOutcome) {
        if self.state == TeamMatchState::Over {
            return;
        }
        self.state = TeamMatchState::Over;
        self.outcome = Some(outcome);
        tracing::info!(?outcome, "team match over");
    }

    /// Declares the outcome from the inner [`Match`]'s verdict, used when **both**
    /// sides are eliminated together (a double-elimination). The inner match runs in
    /// single-round mode here (Simul/Turns), so its [`Match::match_winner`] is the
    /// genuine round verdict — a true double-KO surfaces as [`Winner::Draw`], which
    /// is recorded as a real [`TeamOutcome::Draw`] rather than a P1-biased tiebreak.
    fn declare_winner_from_inner(&mut self) {
        let inner = self.inner_ref();
        let outcome = inner
            .match_winner()
            .or_else(|| inner.winner())
            .map(outcome_of)
            .unwrap_or(TeamOutcome::Draw);
        self.declare_outcome(outcome);
    }

    /// Mutable access to the inner [`Match`] (test-only).
    #[cfg(test)]
    pub(crate) fn inner_mut_for_test(&mut self) -> &mut Match {
        self.inner_mut()
    }

    /// Mutable access to the `idx`-th reserve fighter on `side`, or [`None`] if
    /// out of range (test-only).
    #[cfg(test)]
    pub(crate) fn reserve_mut_for_test(&mut self, side: Side, idx: usize) -> Option<&mut Player> {
        match side {
            Side::P1 => self.reserves.0.get_mut(idx),
            Side::P2 => self.reserves.1.get_mut(idx),
        }
    }
}

/// Splits a non-empty roster into its lead fighter (front of the list) and the
/// remaining reserves (in order). The caller guarantees the roster is non-empty
/// (it is `assert!`ed before this is reached).
fn split_lead(mut roster: Vec<Player>) -> (Player, Vec<Player>) {
    debug_assert!(
        !roster.is_empty(),
        "split_lead requires a non-empty roster; the caller asserts this"
    );
    let lead = roster.remove(0);
    (lead, roster)
}

/// Pops the front (next-in-line) fighter off a reserve list, or [`None`] if empty.
fn pop_front(reserves: &mut Vec<Player>) -> Option<Player> {
    if reserves.is_empty() {
        None
    } else {
        Some(reserves.remove(0))
    }
}

/// Maps a 1v1 [`Winner`] onto the corresponding [`TeamOutcome`] (T028). A 1v1
/// [`Winner::Draw`] — which a single-round inner match yields on a genuine
/// double-KO / equal-life time over — maps to a real [`TeamOutcome::Draw`] rather
/// than a P1-biased tiebreak, so a drawn team match is reported honestly.
fn outcome_of(winner: Winner) -> TeamOutcome {
    match winner {
        Winner::P1 => TeamOutcome::P1,
        Winner::P2 => TeamOutcome::P2,
        Winner::Draw => TeamOutcome::Draw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::*;
    use crate::INTRO_FRAMES;

    /// Drives a team match out of its inner intro into live combat.
    fn into_fight(m: &mut TeamMatch) {
        for _ in 0..(INTRO_FRAMES + 1) {
            m.tick(MatchInput::none(), MatchInput::none());
        }
    }

    #[test]
    fn single_mode_is_default_and_behaves_like_1v1() {
        let p1 = make_player(-50.0);
        let p2 = make_player(50.0);
        let m = TeamMatch::new(p1, p2, StageBounds::new(-200.0, 200.0));
        assert_eq!(m.mode(), TeamMode::Single);
        assert_eq!(m.fighters_remaining(Side::P1), 1);
        assert_eq!(m.fighters_remaining(Side::P2), 1);
        assert!(m.reserves(Side::P1).is_empty());
        assert!(m.reserves(Side::P2).is_empty());
        assert_eq!(m.state(), TeamMatchState::InProgress);
        assert_eq!(m.active_ordinal(Side::P1), 1);
    }

    #[test]
    fn single_mode_ticks_without_panic() {
        let mut m = TeamMatch::new(
            make_player(-50.0),
            make_player(50.0),
            StageBounds::default(),
        );
        for _ in 0..120 {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        // The inner match advanced (game time moved forward).
        assert!(m.active().game_time() > 0);
    }

    #[test]
    fn constructs_two_per_side() {
        let p1_team = vec![make_player(-50.0), make_player(-50.0)];
        let p2_team = vec![make_player(50.0), make_player(50.0)];
        let m = TeamMatch::with_mode(p1_team, p2_team, StageBounds::default(), TeamMode::Simul);
        assert_eq!(m.mode(), TeamMode::Simul);
        assert_eq!(m.fighters_remaining(Side::P1), 2);
        assert_eq!(m.fighters_remaining(Side::P2), 2);
        assert_eq!(m.reserves(Side::P1).len(), 1);
        assert_eq!(m.reserves(Side::P2).len(), 1);
    }

    #[test]
    fn roster_is_clamped_to_cap() {
        let big: Vec<Player> = (0..(MAX_TEAM_SIZE + 4))
            .map(|_| make_player(-50.0))
            .collect();
        let m = TeamMatch::with_mode(
            big,
            vec![make_player(50.0)],
            StageBounds::default(),
            TeamMode::Simul,
        );
        // Active + reserves never exceeds the cap.
        assert_eq!(m.fighters_remaining(Side::P1), MAX_TEAM_SIZE);
        assert_eq!(m.reserves(Side::P1).len(), MAX_TEAM_SIZE - 1);
    }

    #[test]
    fn simul_ticks_all_active_fighters() {
        // Two fighters per side, Simul. Every fighter (active + reserves) should
        // advance its state machine each frame — verified by the reserve's
        // animation timer advancing like the active fighter's.
        let p1_team = vec![make_player(-60.0), make_player(-80.0)];
        let p2_team = vec![make_player(60.0), make_player(80.0)];
        let mut m = TeamMatch::with_mode(p1_team, p2_team, StageBounds::default(), TeamMode::Simul);

        // `state_time` advances by one each non-hit-paused tick, so it is a clean
        // monotonic witness that the reserve's state machine actually ran.
        let reserve_start = m.reserves(Side::P1)[0].character.state_time;
        const FRAMES: i32 = 30;
        for _ in 0..FRAMES {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        let reserve_end = m.reserves(Side::P1)[0].character.state_time;
        assert_eq!(
            reserve_end - reserve_start,
            FRAMES,
            "reserve fighter must tick once per frame in Simul (start={reserve_start}, end={reserve_end})"
        );
        // Both sides still field everyone — nobody was KO'd.
        assert_eq!(m.fighters_remaining(Side::P1), 2);
        assert_eq!(m.fighters_remaining(Side::P2), 2);
        assert_eq!(m.state(), TeamMatchState::InProgress);
    }

    #[test]
    fn simul_side_defeated_only_when_all_ko() {
        let p1_team = vec![make_player(-60.0), make_player(-80.0)];
        let p2_team = vec![make_player(60.0), make_player(80.0)];
        let mut m = TeamMatch::with_mode(p1_team, p2_team, StageBounds::default(), TeamMode::Simul);
        into_fight(&mut m);

        // KO P2's active fighter only — the reserve keeps P2 alive.
        m.kill_active(Side::P2);
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(
            m.state(),
            TeamMatchState::InProgress,
            "one KO must not end a Simul match while a reserve survives"
        );

        // KO the remaining P2 reserve too, and re-KO the active in case the inner
        // round flow restored it: now the whole side is wiped → P1 wins.
        m.kill_reserve(Side::P2, 0);
        m.kill_active(Side::P2);
        m.tick(MatchInput::none(), MatchInput::none());
        assert_eq!(m.state(), TeamMatchState::Over);
        assert_eq!(m.winner(), Some(Side::P1));
    }

    #[test]
    fn turns_hands_off_to_next_fighter_on_ko() {
        // P1 has one fighter; P2 has two. KO P2's active → P2's reserve takes over
        // and the match continues with a fresh, full-life P2.
        let p1_team = vec![make_player(-60.0)];
        let p2_team = vec![make_player(60.0), make_player(80.0)];
        let mut m = TeamMatch::with_mode(p1_team, p2_team, StageBounds::default(), TeamMode::Turns);
        into_fight(&mut m);

        assert_eq!(m.fighters_remaining(Side::P2), 2);
        assert_eq!(m.active_ordinal(Side::P2), 1);
        let reserve_life_max = m.reserves(Side::P2)[0].life_max();

        // Knock out P2's active fighter.
        m.kill_active(Side::P2);
        m.tick(MatchInput::none(), MatchInput::none());

        // The match continues (P2 still had a reserve) and the reserve is now the
        // active fighter at full life — the hand-off happened.
        assert_eq!(
            m.state(),
            TeamMatchState::InProgress,
            "Turns must hand off, not end, while a reserve remains"
        );
        assert!(
            m.reserves(Side::P2).is_empty(),
            "the reserve should have been promoted to active"
        );
        assert_eq!(m.fighters_remaining(Side::P2), 1);
        assert_eq!(m.active_ordinal(Side::P2), 2);
        assert_eq!(
            m.active_player(Side::P2).life(),
            reserve_life_max,
            "the incoming fighter starts at full life"
        );
    }

    #[test]
    fn turns_ends_when_a_side_runs_out_of_fighters() {
        // 1 vs 1 in Turns: KO P2's only fighter → P2 has no reserve → P1 wins.
        let p1_team = vec![make_player(-60.0)];
        let p2_team = vec![make_player(60.0)];
        let mut m = TeamMatch::with_mode(p1_team, p2_team, StageBounds::default(), TeamMode::Turns);
        into_fight(&mut m);

        m.kill_active(Side::P2);
        m.tick(MatchInput::none(), MatchInput::none());

        assert_eq!(m.state(), TeamMatchState::Over);
        assert_eq!(m.winner(), Some(Side::P1));
    }

    #[test]
    fn turns_survivor_keeps_its_life_through_a_handoff() {
        // P1 (single fighter) takes some damage; P2 (two fighters) loses its
        // active. After the hand-off P1 should still have its reduced life (not be
        // reset), while the incoming P2 fighter is fresh.
        let p1_team = vec![make_player(-60.0)];
        let p2_team = vec![make_player(60.0), make_player(80.0)];
        let mut m = TeamMatch::with_mode(p1_team, p2_team, StageBounds::default(), TeamMode::Turns);
        into_fight(&mut m);

        let wounded = m.active_player(Side::P1).life_max() - 25;
        m.set_active_life(Side::P1, wounded);
        m.kill_active(Side::P2);
        m.tick(MatchInput::none(), MatchInput::none());

        assert_eq!(m.state(), TeamMatchState::InProgress);
        assert_eq!(
            m.active_player(Side::P1).life(),
            wounded,
            "the survivor keeps its remaining life across a hand-off"
        );
    }
}
