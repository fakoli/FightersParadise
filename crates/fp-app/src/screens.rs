//! In-app screen state machine: Title menu -> Character-Select -> Stage-Select
//! -> Fight -> Title, plus a Title -> Setup -> Title side branch.
//!
//! This module owns the **pure** menu/cursor/transition logic for the app's
//! out-of-fight flow (the [`Screen`] state machine, the title menu, the select
//! grid, the stage-select list, the setup/options screen, and the roster-pick ->
//! which-`.def`-to-load decision), kept free of SDL2 and the GPU so it is
//! unit-testable headlessly. The SDL2 window, 60Hz accumulator loop, and GPU
//! rendering that drives it live in `main.rs`.
//!
//! Every screen is driven by a single source-agnostic [`MenuInput`] (built in
//! `main.rs` from the keyboard **or** a game controller via the existing
//! `controller_to_match_input`/`merge_match_input` plumbing), so controller and
//! keyboard navigation are identical at this layer and the same unit tests cover
//! both sources (a controller D-pad is just another way to set `MenuInput::up`).
//!
//! The flow:
//! - **Title** ([`TitleMenu`]) renders the enabled motif menu items as text with
//!   a highlighted cursor. `VS MODE` -> Select (both players pick); `TRAINING`
//!   -> Select (P1 picks, P2 mirrors); `SETUP`/`OPTIONS` -> Setup; `EXIT`/quit
//!   leaves the app. A missing motif falls back to a built-in minimal menu
//!   (`VS` / `TRAINING` / `SETUP` / `EXIT`).
//! - **Select** ([`SelectScreen`]) renders the `select.def` roster as a text
//!   grid with a P1 cursor (and a P2 cursor in VS). Confirming P1 (then P2 in
//!   VS) yields a [`MatchPick`] naming the character `.def`(s) to load.
//! - **Stage-Select** ([`StageSelect`]) renders the available stages (the shipped
//!   dojo backdrop plus any discovered stage `.def`) as a vertical list with one
//!   cursor. Confirming yields a [`StageChoice`] naming the stage the match
//!   loads; cancelling returns to character-select.
//! - **Setup** ([`SetupScreen`]) is the options screen (T042): it edits the live
//!   [`InputConfig`] — the input device choice plus the player-1 keyboard
//!   binding for each [`InputAction`] — navigable by controller and keyboard.
//!   Selecting an action enters a "press a key" capture mode; the next key the
//!   player presses (delivered via [`SetupScreen::capture_key`]) is bound to that
//!   action and immediately takes effect in-match. Back returns to Title.
//! - **Fight** runs the existing two-player [`fp_engine::Match`] over the chosen
//!   stage; on match-over it returns to Title.
//!
//! Nothing here panics: a missing motif/roster degrades to a built-in fallback,
//! an empty roster yields no pick (the caller stays on Title), the stage list is
//! never empty (the dojo backdrop is always present), and `RandomSelect` is
//! resolved deterministically against a caller-supplied seed.

use std::path::{Path, PathBuf};

use fp_ui::{MenuItemKind, RosterEntry, SelectDef, SelectInfo, SelectSlot, SystemDef};

// Re-export the HUD-customization model (T046) so the rest of the app can refer
// to it as `screens::HudConfig` / `screens::BarColor` / `screens::HudElement`,
// matching how `InputConfig` and friends are namespaced under `screens`.
pub use fp_ui::{BarColor, HudConfig, HudElement};

// Re-export the CPU difficulty knob (T069) and teaching-mode knob (T070) so the
// Setup/Options screen can own `screens::AiDifficulty` / `screens::BehaviorMode`
// selectors without `main.rs` reaching into `fp_input`, matching how the HUD/input
// config types are namespaced under `screens`.
pub use fp_input::{AiDifficulty, BehaviorMode};

// `SelectSlot` is used both in `SelectScreen::new` and `stage_entries_from_roster`.

/// An edge-detected, source-agnostic menu input for one frame.
///
/// Each field is `true` only on the frame the corresponding control transitions
/// from released to pressed (a rising edge), so a single key/button press moves
/// the cursor exactly one cell rather than skidding while held. The app builds
/// one of these per frame from the keyboard + controller (see
/// [`MenuInput::from_edges`]); the pure screen logic consumes only this.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MenuInput {
    /// Move the cursor up (rising edge this frame).
    pub up: bool,
    /// Move the cursor down (rising edge this frame).
    pub down: bool,
    /// Move the cursor left (rising edge this frame).
    pub left: bool,
    /// Move the cursor right (rising edge this frame).
    pub right: bool,
    /// Confirm / select the highlighted item (rising edge this frame).
    pub confirm: bool,
    /// Back / cancel (rising edge this frame).
    pub back: bool,
    /// Info / character-details action (rising edge this frame). On the
    /// character-select screen this opens the movelist / character-info screen
    /// (T071) for the highlighted character.
    pub info: bool,
}

impl MenuInput {
    /// Builds the per-frame rising-edge input from this frame's held state
    /// (`now`) and the previous frame's held state (`prev`).
    ///
    /// A field is set iff it is held now and was not held last frame, giving
    /// one-shot navigation from a held control.
    #[must_use]
    pub fn from_edges(now: HeldMenuInput, prev: HeldMenuInput) -> Self {
        Self {
            up: now.up && !prev.up,
            down: now.down && !prev.down,
            left: now.left && !prev.left,
            right: now.right && !prev.right,
            confirm: now.confirm && !prev.confirm,
            back: now.back && !prev.back,
            info: now.info && !prev.info,
        }
    }
}

/// The raw held state of the menu controls this frame (before edge detection).
///
/// The app fills this from the live keyboard/controller every frame, then turns
/// it into the rising-edge [`MenuInput`] via [`MenuInput::from_edges`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HeldMenuInput {
    /// Up direction held this frame.
    pub up: bool,
    /// Down direction held this frame.
    pub down: bool,
    /// Left direction held this frame.
    pub left: bool,
    /// Right direction held this frame.
    pub right: bool,
    /// A confirm button held this frame.
    pub confirm: bool,
    /// A back/cancel button held this frame.
    pub back: bool,
    /// An info / character-details button held this frame (T071).
    pub info: bool,
}

/// Which players pick a character on the select screen, decided by the title
/// menu choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectMode {
    /// Versus: both P1 and P2 choose a character.
    Versus,
    /// Training: only P1 chooses; P2 mirrors P1's pick (an idle dummy).
    Training,
}

/// One entry in the title menu: a display label and the action it triggers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleEntry {
    /// The text shown on the menu line.
    pub label: String,
    /// What choosing this entry does.
    pub action: TitleAction,
}

/// What a title-menu entry does when confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleAction {
    /// Go to the character-select screen in the given mode.
    Select(SelectMode),
    /// Open the setup / options screen (T042: input configuration + remapping).
    Setup,
    /// Quit the application.
    Quit,
    /// A recognised-but-unimplemented item (Arcade/Survival/Watch/...): selectable
    /// but a no-op (stays on the title screen) so the menu still reads completely.
    NoOp,
}

/// The title-screen main menu: the enabled entries plus the cursor position.
///
/// Built from a motif [`SystemDef`] ([`TitleMenu::from_system`]) or the built-in
/// fallback ([`TitleMenu::fallback`]). Navigation wraps and never panics on an
/// empty list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleMenu {
    /// The menu entries, in display order.
    pub entries: Vec<TitleEntry>,
    /// The currently highlighted entry index (`0` when empty).
    pub cursor: usize,
}

impl TitleMenu {
    /// Builds the title menu from a motif [`SystemDef`], mapping each enabled
    /// canonical menu item to a [`TitleAction`]:
    ///
    /// - `Versus`/`TeamVersus` -> Select(Versus),
    /// - `Training` -> Select(Training),
    /// - `Options` -> Setup (the input-configuration screen, T042),
    /// - `Exit` -> Quit,
    /// - everything else (Arcade, Survival, Watch, ...) -> a selectable
    ///   no-op so it still appears but does nothing yet.
    ///
    /// When the motif enables no usable items at all, falls back to
    /// [`TitleMenu::fallback`] so there is always something to pick.
    #[must_use]
    pub fn from_system(system: &SystemDef) -> Self {
        let entries: Vec<TitleEntry> = system
            .title
            .items
            .iter()
            .map(|item| TitleEntry {
                label: item.label.clone(),
                action: title_action_for(item.kind),
            })
            .collect();
        if entries.is_empty() {
            return Self::fallback();
        }
        Self { entries, cursor: 0 }
    }

    /// The built-in minimal menu used when no motif (or an item-less motif) is
    /// available: `VS MODE` / `TRAINING` / `SETUP` / `EXIT`.
    #[must_use]
    pub fn fallback() -> Self {
        Self {
            entries: vec![
                TitleEntry {
                    label: "VS MODE".to_string(),
                    action: TitleAction::Select(SelectMode::Versus),
                },
                TitleEntry {
                    label: "TRAINING".to_string(),
                    action: TitleAction::Select(SelectMode::Training),
                },
                TitleEntry {
                    label: "SETUP".to_string(),
                    action: TitleAction::Setup,
                },
                TitleEntry {
                    label: "EXIT".to_string(),
                    action: TitleAction::Quit,
                },
            ],
            cursor: 0,
        }
    }

    /// Applies one frame of input, returning the action when an item is confirmed
    /// (or [`TitleAction::Quit`] on back/Esc). Up/Down move the cursor with wrap;
    /// confirm reports the highlighted item's action. A no-op (returns `None`)
    /// when nothing actionable happened this frame.
    pub fn update(&mut self, input: MenuInput) -> Option<TitleAction> {
        if self.entries.is_empty() {
            // Nothing to navigate; back still quits so the app is never trapped.
            return input.back.then_some(TitleAction::Quit);
        }
        if input.up {
            self.cursor = wrap_dec(self.cursor, self.entries.len());
        }
        if input.down {
            self.cursor = wrap_inc(self.cursor, self.entries.len());
        }
        if input.back {
            return Some(TitleAction::Quit);
        }
        if input.confirm {
            return self.entries.get(self.cursor).map(|e| e.action);
        }
        None
    }
}

/// Maps a canonical motif menu item to the app action it triggers. See
/// [`TitleMenu::from_system`] for the mapping rationale.
fn title_action_for(kind: MenuItemKind) -> TitleAction {
    match kind {
        MenuItemKind::Versus | MenuItemKind::TeamVersus => TitleAction::Select(SelectMode::Versus),
        MenuItemKind::Training => TitleAction::Select(SelectMode::Training),
        // Options opens the setup / input-configuration screen (T042).
        MenuItemKind::Options => TitleAction::Setup,
        MenuItemKind::Exit => TitleAction::Quit,
        // Recognised but not yet implemented: keep them visible but inert.
        MenuItemKind::Arcade
        | MenuItemKind::TeamArcade
        | MenuItemKind::TeamCoop
        | MenuItemKind::Survival
        | MenuItemKind::SurvivalCoop
        | MenuItemKind::Watch => TitleAction::NoOp,
    }
}

/// The result of a completed character-select: which `.def`(s) the fight should
/// load, already resolved to filesystem paths relative to the `select.def`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchPick {
    /// Player 1's chosen character `.def` path.
    pub p1_def: PathBuf,
    /// Player 2's chosen character `.def` path. In Training this equals
    /// [`MatchPick::p1_def`] (P2 mirrors P1 as an idle dummy).
    pub p2_def: PathBuf,
    /// The display name of P1's pick (for logging / a future VS screen).
    pub p1_name: String,
    /// The display name of P2's pick.
    pub p2_name: String,
}

/// A pickable roster cell: a real character (its [`RosterEntry`]) or the random
/// icon. Empty `select.def` slots are dropped when the grid is built, so the
/// cursor only ever lands on a choosable cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RosterCell {
    /// A concrete character the player can pick.
    Character(RosterEntry),
    /// The random-select icon: picks a random concrete character on confirm.
    Random,
}

/// The character-select screen: the choosable roster, the grid geometry, the two
/// players' cursor positions, and whichever player has locked in a pick.
///
/// Built from a [`SelectDef`] + [`SelectInfo`] ([`SelectScreen::new`]). In
/// [`SelectMode::Training`] only P1 picks (P2 mirrors). Navigation is grid-aware
/// (columns from the motif geometry) and wraps; it never panics on an empty or
/// single-entry roster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectScreen {
    /// Which players are choosing.
    pub mode: SelectMode,
    /// The choosable cells (characters + random), in roster order.
    pub cells: Vec<RosterCell>,
    /// Number of grid columns (>= 1), from the motif `[Select Info] columns`.
    pub columns: usize,
    /// P1's cursor index into [`SelectScreen::cells`].
    pub p1_cursor: usize,
    /// P2's cursor index into [`SelectScreen::cells`].
    pub p2_cursor: usize,
    /// P1's locked-in cell index, once confirmed.
    pub p1_locked: Option<usize>,
    /// P2's locked-in cell index, once confirmed (always set together with
    /// [`SelectScreen::p1_locked`] in Training, where P2 mirrors P1).
    pub p2_locked: Option<usize>,
    /// The directory `select.def` lives in, used to resolve a cell's relative
    /// `.def` path to a real filesystem path.
    base_dir: PathBuf,
}

/// What one frame of select-screen input produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectOutcome {
    /// Still choosing; stay on the select screen.
    Pending,
    /// Both players have locked in: load these characters and fight.
    Done(MatchPick),
    /// The player cancelled back to the title menu.
    Cancelled,
    /// The player pressed Info on a concrete character cell: open the
    /// movelist / character-info screen (T071) for the character at this resolved
    /// `.def` path. The select screen is left untouched so it resumes when the
    /// info screen is dismissed.
    ShowInfo(PathBuf),
}

impl SelectScreen {
    /// Builds the select screen from a parsed roster and the motif grid geometry.
    ///
    /// `select_path` is the `select.def`'s own path; its parent directory is the
    /// base every cell's `.def` is resolved against (matching how MUGEN resolves
    /// roster `.def`s relative to the `select.def`). Empty roster slots are
    /// dropped so the cursor only lands on choosable cells. Cursor start cells
    /// come from the motif but are clamped into range. `columns` is forced to at
    /// least 1 so grid math never divides by zero.
    #[must_use]
    pub fn new(
        mode: SelectMode,
        select: &SelectDef,
        info: &SelectInfo,
        select_path: &Path,
    ) -> Self {
        let cells: Vec<RosterCell> = select
            .slots
            .iter()
            .filter_map(|slot| match slot {
                SelectSlot::Character(e) => Some(RosterCell::Character(e.clone())),
                SelectSlot::RandomSelect => Some(RosterCell::Random),
                SelectSlot::Empty => None,
            })
            .collect();
        let columns = (info.columns as usize).max(1);
        let base_dir = select_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let clamp_start = |cell: (i32, i32)| -> usize {
            // start_cell is (column, row); flatten to a linear index, clamped.
            let col = cell.0.max(0) as usize;
            let row = cell.1.max(0) as usize;
            let idx = row * columns + col;
            if cells.is_empty() {
                0
            } else {
                idx.min(cells.len() - 1)
            }
        };
        Self {
            mode,
            p1_cursor: clamp_start(info.p1_cursor.start_cell),
            p2_cursor: clamp_start(info.p2_cursor.start_cell),
            columns,
            p1_locked: None,
            p2_locked: None,
            base_dir,
            cells,
        }
    }

    /// Whether the roster has no choosable cell.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Applies one frame of input.
    ///
    /// In Versus both cursors are live until each locks; in Training only P1's is.
    /// `back` cancels to the title (when no one has locked yet). Directions move
    /// the not-yet-locked cursor(s) on the grid (wrapping); confirm locks the
    /// cursor's cell. Once both sides are locked, resolves the picks (including
    /// `Random`, via `rng_seed`) into a [`MatchPick`] and returns
    /// [`SelectOutcome::Done`].
    ///
    /// `rng_seed` is a deterministic-friendly counter (e.g. a frame tick) used to
    /// resolve `Random`; the same seed always picks the same character, so tests
    /// are reproducible.
    pub fn update(&mut self, input: MenuInput, rng_seed: u64) -> SelectOutcome {
        if self.is_empty() {
            // Nothing to choose; only allow cancelling back so we're never stuck.
            return if input.back {
                SelectOutcome::Cancelled
            } else {
                SelectOutcome::Pending
            };
        }
        // Back cancels only while nobody has committed; after a lock it is inert
        // (the match is already being assembled).
        if input.back && self.p1_locked.is_none() && self.p2_locked.is_none() {
            return SelectOutcome::Cancelled;
        }

        // Info opens the character-info / movelist screen (T071) for the cell
        // under the *active* (not-yet-locked) cursor, but only for a concrete
        // character — Random has no single `.def` to describe. It does not move
        // the cursor or lock anything, so the select screen resumes unchanged when
        // the info screen is dismissed.
        if input.info {
            let cursor = if self.p1_locked.is_none() {
                self.p1_cursor
            } else {
                self.p2_cursor
            };
            if let Some(path) = self.info_def_path(cursor) {
                return SelectOutcome::ShowInfo(path);
            }
        }

        let len = self.cells.len();

        // Move P1's cursor while it isn't locked.
        if self.p1_locked.is_none() {
            self.p1_cursor = move_cursor(self.p1_cursor, input, self.columns, len);
            if input.confirm {
                self.p1_locked = Some(self.p1_cursor);
                // Training: P2 mirrors P1 immediately (an idle dummy of the same
                // character), so the single confirm completes the screen.
                if self.mode == SelectMode::Training {
                    self.p2_locked = Some(self.p1_cursor);
                }
            }
        } else if self.mode == SelectMode::Versus && self.p2_locked.is_none() {
            // Versus: once P1 is locked, the SAME confirm frame must not also lock
            // P2 — but here P1 locked on a previous frame, so P2 navigates/locks
            // with its own input now.
            self.p2_cursor = move_cursor(self.p2_cursor, input, self.columns, len);
            if input.confirm {
                self.p2_locked = Some(self.p2_cursor);
            }
        }

        match (self.p1_locked, self.p2_locked) {
            (Some(p1), Some(p2)) => SelectOutcome::Done(self.build_pick(p1, p2, rng_seed)),
            _ => SelectOutcome::Pending,
        }
    }

    /// Resolves two locked cell indices into a concrete [`MatchPick`], turning
    /// `Random` cells into a deterministic concrete pick and resolving each
    /// character's `.def` relative to the `select.def` directory.
    fn build_pick(&self, p1: usize, p2: usize, rng_seed: u64) -> MatchPick {
        let (p1_entry, p1_name) = self.resolve_cell(p1, rng_seed);
        // Offset P2's seed so two Randoms on the same frame don't always collide.
        let (p2_entry, p2_name) = self.resolve_cell(p2, rng_seed.wrapping_add(0x9E37_79B9));
        MatchPick {
            p1_def: self.base_dir.join(&p1_entry.def_path),
            p2_def: self.base_dir.join(&p2_entry.def_path),
            p1_name,
            p2_name,
        }
    }

    /// Resolves a cell index to the concrete character `.def` path the info
    /// screen should describe (T071), or `None` for a `Random` cell / out-of-range
    /// index. Resolves the entry's relative `.def` against the `select.def`
    /// directory, the same way [`build_pick`](Self::build_pick) does.
    fn info_def_path(&self, index: usize) -> Option<PathBuf> {
        match self.cells.get(index) {
            Some(RosterCell::Character(e)) => Some(self.base_dir.join(&e.def_path)),
            _ => None,
        }
    }

    /// Resolves one cell index to a concrete roster entry + its display name.
    ///
    /// A `Character` cell resolves to itself; a `Random` cell picks a concrete
    /// character deterministically from `seed`. If the roster somehow holds only
    /// `Random` cells (no concrete character), falls back to a synthetic entry so
    /// the caller still gets a usable (if empty) path rather than panicking — the
    /// loader then degrades gracefully.
    fn resolve_cell(&self, index: usize, seed: u64) -> (RosterEntry, String) {
        match self.cells.get(index) {
            Some(RosterCell::Character(e)) => (e.clone(), e.name.clone()),
            Some(RosterCell::Random) | None => self
                .random_character(seed)
                .map(|e| (e.clone(), e.name.clone()))
                .unwrap_or_else(|| (RosterEntry::default(), String::new())),
        }
    }

    /// Picks a concrete character from the roster deterministically from `seed`,
    /// or `None` when the roster holds no concrete character at all.
    fn random_character(&self, seed: u64) -> Option<&RosterEntry> {
        let concrete: Vec<&RosterEntry> = self
            .cells
            .iter()
            .filter_map(|c| match c {
                RosterCell::Character(e) => Some(e),
                RosterCell::Random => None,
            })
            .collect();
        if concrete.is_empty() {
            return None;
        }
        let idx = (seed % concrete.len() as u64) as usize;
        concrete.get(idx).copied()
    }
}

// ---------------------------------------------------------------------------
// Character-Info / Movelist screen (T071)
// ---------------------------------------------------------------------------

/// The movelist / character-info screen (T071): a character's display name,
/// author, and a list of moves derived from its `.cmd` command definitions.
///
/// Built from a loaded character ([`InfoScreen::from_loaded`]) or, when the
/// character failed to load, from a fallback that still shows *something* rather
/// than trapping the player ([`InfoScreen::load_failed`]). Pure data: the app
/// draws it and dismisses it on Back/Confirm — it never holds the whole loaded
/// character.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoScreen {
    /// The character's display name (`[Info] displayname`, falling back to
    /// `[Info] name`). May be empty if the `.def` had neither.
    pub display_name: String,
    /// The character's author credit (`[Info] author`), shown verbatim. Empty
    /// when the `.def` declares none.
    pub author: String,
    /// The formatted movelist: each entry is a `(command-name, motion)` pair.
    pub moves: Vec<fp_character::MoveEntry>,
}

/// What one frame of info-screen input produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfoOutcome {
    /// Stay on the info screen.
    Pending,
    /// Dismiss the info screen and return to character-select.
    Dismissed,
}

impl InfoScreen {
    /// Builds the info screen from a loaded character, deriving the movelist from
    /// its parsed `.cmd` ([`fp_character::movelist_from_cmd`]). A character with
    /// no `.cmd` (or an empty one) yields an empty movelist, which the renderer
    /// shows as a "no moves listed" note — never an error.
    #[must_use]
    pub fn from_loaded(loaded: &fp_character::LoadedCharacter) -> Self {
        let moves = fp_character::movelist_from_cmd(loaded.cmd.as_ref());
        Self {
            display_name: loaded.displayname.clone(),
            author: loaded.author.clone(),
            moves,
        }
    }

    /// Builds a fallback info screen for a character that failed to load, so the
    /// Info action never traps the player on a blank screen. `label` is the
    /// roster display name (already known before the load attempt).
    #[must_use]
    pub fn load_failed(label: &str) -> Self {
        Self {
            display_name: label.to_string(),
            author: String::new(),
            moves: Vec::new(),
        }
    }

    /// Applies one frame of input: any of Back/Confirm/Info dismisses the screen
    /// back to character-select.
    #[must_use]
    pub fn update(&self, input: MenuInput) -> InfoOutcome {
        if input.back || input.confirm || input.info {
            InfoOutcome::Dismissed
        } else {
            InfoOutcome::Pending
        }
    }
}

// ---------------------------------------------------------------------------
// Stage-Select (T041)
// ---------------------------------------------------------------------------

/// How a selectable stage is realised when the match loads it.
///
/// Fighters Paradise has two kinds of stage background: a MUGEN `[BGdef]`-style
/// stage `.def` (with parallax layers + a following camera) and the shipped
/// full-window backdrop image (the default dojo). The stage-select screen offers
/// both, and the kind tells `main.rs` which loader to drive for the pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageKind {
    /// A MUGEN stage `.def`: parsed by `fp_stage::Stage` and rendered with its
    /// own `[BGdef]` sprite layers and a fighter-following camera.
    Def,
    /// The shipped clean-room backdrop image (the default dojo): drawn as a
    /// full-window RGBA image behind the fighters, with no camera follow.
    Backdrop,
}

/// One selectable stage on the stage-select screen: a display name, the path the
/// match loads, and which [`StageKind`] loader to use for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageEntry {
    /// The name shown on the stage-select line.
    pub name: String,
    /// The path the match loads for this stage: a stage `.def` for
    /// [`StageKind::Def`], or the backdrop image for [`StageKind::Backdrop`].
    pub path: PathBuf,
    /// Which loader realises this stage.
    pub kind: StageKind,
}

impl StageEntry {
    /// Builds the shipped default backdrop entry (the dojo): a
    /// [`StageKind::Backdrop`] pointing at `backdrop_path`.
    #[must_use]
    pub fn backdrop(name: impl Into<String>, backdrop_path: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            path: backdrop_path.into(),
            kind: StageKind::Backdrop,
        }
    }

    /// Builds a stage-`.def` entry ([`StageKind::Def`]) from a name and path.
    #[must_use]
    pub fn def(name: impl Into<String>, def_path: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            path: def_path.into(),
            kind: StageKind::Def,
        }
    }
}

/// The stage a completed stage-select resolved to: the path to load and how to
/// load it (see [`StageKind`]). The match build in `main.rs` consumes this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageChoice {
    /// The chosen stage's display name (for logging / a future VS screen).
    pub name: String,
    /// The chosen stage's path: a stage `.def` or the backdrop image.
    pub path: PathBuf,
    /// Which loader realises the chosen stage.
    pub kind: StageKind,
}

/// What one frame of stage-select input produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageOutcome {
    /// Still choosing; stay on the stage-select screen.
    Pending,
    /// A stage was confirmed: load it and fight.
    Done(StageChoice),
    /// The player cancelled back to character-select.
    Cancelled,
}

/// The stage-select screen: the available stages plus a single cursor.
///
/// Built from a list of [`StageEntry`] (see [`stage_entries_from_roster`]). The
/// list is always non-empty (the caller seeds it with at least the dojo
/// backdrop), so navigation never has to guard an empty list, but the
/// constructor still tolerates one (the cursor pins to 0 and confirm yields
/// nothing) rather than panicking. Up/Down (and Left/Right) move the cursor with
/// wrap; confirm picks the cursor's stage; back cancels to character-select.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageSelect {
    /// The selectable stages, in display order.
    pub entries: Vec<StageEntry>,
    /// The currently highlighted entry index (`0` when empty).
    pub cursor: usize,
}

impl StageSelect {
    /// Builds the stage-select screen from a list of stages, starting the cursor
    /// on the first. An empty list is tolerated (cursor `0`, confirm a no-op).
    #[must_use]
    pub fn new(entries: Vec<StageEntry>) -> Self {
        Self { entries, cursor: 0 }
    }

    /// Applies one frame of input.
    ///
    /// Up/Down (a vertical list) move the single cursor with wrap; Left/Right are
    /// treated as the same vertical step so either axis navigates. `back` cancels
    /// to character-select. `confirm` resolves the highlighted stage into a
    /// [`StageChoice`] and returns [`StageOutcome::Done`]. Returns
    /// [`StageOutcome::Pending`] when nothing actionable happened this frame.
    pub fn update(&mut self, input: MenuInput) -> StageOutcome {
        if self.entries.is_empty() {
            // Nothing to choose; only allow cancelling back so we're never stuck.
            return if input.back {
                StageOutcome::Cancelled
            } else {
                StageOutcome::Pending
            };
        }
        if input.back {
            return StageOutcome::Cancelled;
        }
        let len = self.entries.len();
        // A vertical list: up/down (and left/right, for convenience) step one
        // entry with wrap.
        if input.up || input.left {
            self.cursor = wrap_dec(self.cursor, len);
        }
        if input.down || input.right {
            self.cursor = wrap_inc(self.cursor, len);
        }
        if input.confirm {
            if let Some(entry) = self.entries.get(self.cursor) {
                return StageOutcome::Done(StageChoice {
                    name: entry.name.clone(),
                    path: entry.path.clone(),
                    kind: entry.kind,
                });
            }
        }
        StageOutcome::Pending
    }
}

/// Builds the default stage list from a parsed roster + the dojo backdrop.
///
/// The list always begins with the shipped dojo `backdrop` (`backdrop_name` at
/// `backdrop_path`) so there is always at least one stage to pick, then appends,
/// de-duplicated and in roster order:
/// - each `[Characters]` entry's own `stage` (when `includestage` is set), and
/// - every `[ExtraStages]` stage `.def`,
///
/// each resolved relative to the `select.def` directory (`base_dir`) — matching
/// how MUGEN resolves stage paths in a `select.def`. Pure (no filesystem access):
/// the caller filters the result to the stages that actually exist on disk. The
/// display name is the stage `.def`'s file stem (uppercased by the renderer),
/// which is a reasonable label without reading the file.
#[must_use]
pub fn stage_entries_from_roster(
    select: &SelectDef,
    base_dir: &Path,
    backdrop_name: &str,
    backdrop_path: &Path,
) -> Vec<StageEntry> {
    let mut entries = vec![StageEntry::backdrop(backdrop_name, backdrop_path)];
    let mut seen: Vec<PathBuf> = vec![entries[0].path.clone()];

    let mut push_def = |raw: &str| {
        let raw = raw.trim();
        if raw.is_empty() || raw.eq_ignore_ascii_case("random") {
            return;
        }
        let resolved = base_dir.join(raw);
        if seen.contains(&resolved) {
            return;
        }
        let name = stage_label(&resolved);
        seen.push(resolved.clone());
        entries.push(StageEntry::def(name, resolved));
    };

    // Per-character stages (only when the character offers its stage).
    for slot in &select.slots {
        if let SelectSlot::Character(e) = slot {
            if e.include_stage {
                if let Some(stage) = e.stage.as_deref() {
                    push_def(stage);
                }
            }
        }
    }
    // Then the explicit extra stages.
    for stage in &select.extra_stages {
        push_def(stage);
    }

    entries
}

/// A display label for a stage `.def` path: its file stem, or the whole file
/// name when it has no stem. Used so the stage list reads names without parsing
/// each `.def`'s `[Info]` section.
fn stage_label(path: &Path) -> String {
    path.file_stem()
        .or_else(|| path.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------
// Setup / Options screen + input configuration (T042)
// ---------------------------------------------------------------------------

/// One remappable in-match input: the four absolute screen directions plus the
/// six MUGEN attack buttons (`a b c x y z`).
///
/// This is the engine-facing action a physical key drives; the setup screen lets
/// the player rebind which keyboard key produces each one (see [`InputConfig`]).
/// It is deliberately backend-free (no SDL `Scancode`): the keyboard key bound to
/// an action is carried as an opaque [`KeyCode`], so this whole module stays
/// unit-testable without a window. `main.rs` owns the `Scancode <-> KeyCode`
/// adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InputAction {
    /// Absolute screen direction: up (jump).
    Up,
    /// Absolute screen direction: down (crouch).
    Down,
    /// Absolute screen direction: left.
    Left,
    /// Absolute screen direction: right.
    Right,
    /// Attack button `a` (light punch).
    A,
    /// Attack button `b` (medium punch).
    B,
    /// Attack button `c` (heavy punch).
    C,
    /// Attack button `x` (light kick).
    X,
    /// Attack button `y` (medium kick).
    Y,
    /// Attack button `z` (heavy kick).
    Z,
}

impl InputAction {
    /// Every remappable action, in the order the setup screen lists them
    /// (directions then the punch row then the kick row).
    pub const ALL: [InputAction; 10] = [
        InputAction::Up,
        InputAction::Down,
        InputAction::Left,
        InputAction::Right,
        InputAction::A,
        InputAction::B,
        InputAction::C,
        InputAction::X,
        InputAction::Y,
        InputAction::Z,
    ];

    /// A short uppercase label for the action (matches the menu font's glyph
    /// set), used by the setup-screen renderer.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            InputAction::Up => "UP",
            InputAction::Down => "DOWN",
            InputAction::Left => "LEFT",
            InputAction::Right => "RIGHT",
            InputAction::A => "A",
            InputAction::B => "B",
            InputAction::C => "C",
            InputAction::X => "X",
            InputAction::Y => "Y",
            InputAction::Z => "Z",
        }
    }
}

/// An opaque keyboard key identifier.
///
/// The pure setup logic never needs to know what a key *is* — only that two keys
/// are equal — so a key is carried as a backend-neutral `i32`. `main.rs` builds
/// these from SDL `Scancode`s (their `repr(i32)` value) and reads them back the
/// same way, keeping every SDL type out of this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyCode(pub i32);

/// Which input device the player drives the game with.
///
/// Both are always *available* (the keyboard never detaches and a controller is
/// merged in when present); this is the player's stated *preference*, shown on
/// the setup screen. The match-input path OR's keyboard and controller
/// regardless, so this is presentational today, but it is stored on the live
/// [`InputConfig`] so a future "controller-only" mode has a home.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputDevice {
    /// Keyboard-driven (the default).
    Keyboard,
    /// Game-controller-driven.
    Controller,
}

impl InputDevice {
    /// A short uppercase label for the device, for the setup-screen renderer.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            InputDevice::Keyboard => "KEYBOARD",
            InputDevice::Controller => "CONTROLLER",
        }
    }

    /// The other device (used to toggle the preference left/right on the setup
    /// screen).
    #[must_use]
    pub fn toggled(self) -> Self {
        match self {
            InputDevice::Keyboard => InputDevice::Controller,
            InputDevice::Controller => InputDevice::Keyboard,
        }
    }
}

/// The live input configuration the match input is sampled through: the player's
/// device preference plus the player-1 keyboard binding for each [`InputAction`].
///
/// `main.rs` holds one of these and consults [`InputConfig::key_for`] when it
/// samples the keyboard each frame, so a rebind made on the setup screen changes
/// gameplay immediately. The bindings are an ordered list (one entry per
/// [`InputAction::ALL`]); [`InputConfig::default_with`] seeds it from the app's
/// default key map (`main.rs` passes the [`KeyCode`] for each action), and
/// [`InputConfig::rebind`] replaces a single action's key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputConfig {
    /// The selected input-device preference.
    pub device: InputDevice,
    /// The CPU-opponent difficulty the Setup/Options screen selects (T069),
    /// applied to P2's [`CpuAi`](fp_input::CpuAi) at the next match start. Starts
    /// at [`AiDifficulty::Normal`]; lives here so it persists across matches
    /// alongside the device preference and key bindings.
    pub cpu_difficulty: AiDifficulty,
    /// The CPU teaching [`BehaviorMode`] the Setup/Options screen selects (T070),
    /// applied to P2's [`CpuAi`](fp_input::CpuAi) at the next match start. Starts
    /// at [`BehaviorMode::Ladder`] (the plain difficulty ladder, unchanged from
    /// before); lives here so the chosen teaching mode persists across matches.
    pub cpu_mode: BehaviorMode,
    /// The player-1 keyboard binding for every action, in [`InputAction::ALL`]
    /// order.
    bindings: Vec<(InputAction, KeyCode)>,
}

impl InputConfig {
    /// Builds the config from a per-action default key, supplied by the caller
    /// (so `main.rs` keeps ownership of the concrete SDL `Scancode` defaults).
    ///
    /// `default_key(action)` returns the [`KeyCode`] each action starts bound to.
    /// The device preference starts on [`InputDevice::Keyboard`].
    #[must_use]
    pub fn default_with(mut default_key: impl FnMut(InputAction) -> KeyCode) -> Self {
        let bindings = InputAction::ALL
            .iter()
            .map(|&action| (action, default_key(action)))
            .collect();
        Self {
            device: InputDevice::Keyboard,
            cpu_difficulty: AiDifficulty::Normal,
            cpu_mode: BehaviorMode::Ladder,
            bindings,
        }
    }

    /// The [`KeyCode`] currently bound to `action`, or `None` if (somehow) unset.
    #[must_use]
    pub fn key_for(&self, action: InputAction) -> Option<KeyCode> {
        self.bindings
            .iter()
            .find(|(a, _)| *a == action)
            .map(|(_, k)| *k)
    }

    /// Rebinds `action` to `key`, replacing its previous binding.
    ///
    /// Returns the action whose binding was displaced if `key` was already bound
    /// to a *different* action (so the caller can clear the stale binding to keep
    /// keys unique), or `None` if `key` was free / already on `action`. The new
    /// binding takes effect the next time the keyboard is sampled.
    pub fn rebind(&mut self, action: InputAction, key: KeyCode) -> Option<InputAction> {
        // Find any other action already holding this key, to report the clash.
        let displaced = self
            .bindings
            .iter()
            .find(|(a, k)| *a != action && *k == key)
            .map(|(a, _)| *a);
        for (a, k) in &mut self.bindings {
            if *a == action {
                *k = key;
            }
        }
        displaced
    }
}

/// What one frame of setup-screen input produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupOutcome {
    /// Still on the setup screen.
    Pending,
    /// The player left the setup screen (back/cancel): return to the title menu.
    Exit,
    /// The player chose the HUD-customization row: open the
    /// [`HudCustomizeScreen`] (T046).
    OpenHudCustomize,
}

/// One selectable row on the setup screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupRow {
    /// The input-device preference toggle (Keyboard / Controller).
    Device,
    /// The CPU-difficulty selector (Easy / Normal / Hard) (T069).
    CpuDifficulty,
    /// The CPU teaching-mode selector (Ladder / Pure Blocker / Reactive DP /
    /// Whiff Punisher) (T070).
    CpuMode,
    /// Opens the HUD-customization screen (T046).
    HudCustomize,
    /// A remappable action's key binding.
    Action(InputAction),
}

/// What one setup-screen row represents, for the renderer to label/walk each row
/// unambiguously (the device toggle, the HUD-customization entry, or a remappable
/// action's key binding). See [`SetupScreen::row_kinds`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupRowKind {
    /// The input-device preference toggle row.
    Device,
    /// The CPU-difficulty selector row (Easy / Normal / Hard) (T069).
    CpuDifficulty,
    /// The CPU teaching-mode selector row (Ladder / Pure Blocker / Reactive DP /
    /// Whiff Punisher) (T070).
    CpuMode,
    /// The HUD-customization entry row (T046).
    HudCustomize,
    /// A remappable action's key-binding row.
    Action(InputAction),
}

/// The setup / options screen (T042): edits the live [`InputConfig`].
///
/// A single vertical cursor walks the rows: the device-preference toggle first,
/// then one row per [`InputAction`]. Up/Down move the cursor (wrapping);
/// Left/Right toggle the device when the device row is highlighted. Confirm on an
/// action row enters *capture* mode ([`SetupScreen::awaiting_key`] becomes true);
/// the next key the app delivers via [`SetupScreen::capture_key`] is bound to
/// that action (and immediately affects in-match input). Back/cancel leaves
/// capture mode if armed, otherwise returns to the title via [`SetupOutcome`].
///
/// Navigation is source-agnostic ([`MenuInput`]), so a controller and the
/// keyboard drive it identically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupScreen {
    /// The highlighted row index into [`SetupScreen::rows`].
    pub cursor: usize,
    /// While `Some`, the screen is in key-capture mode for this action and the
    /// next [`SetupScreen::capture_key`] rebinds it. Public so the renderer can
    /// show a "PRESS A KEY" prompt.
    pub capturing: Option<InputAction>,
    /// The selectable rows (device toggle + one per action), in display order.
    rows: Vec<SetupRow>,
}

impl Default for SetupScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl SetupScreen {
    /// Builds the setup screen: cursor on the first row, not capturing.
    ///
    /// Rows, in display order: the input-device toggle, the CPU-difficulty
    /// selector (T069), the CPU teaching-mode selector (T070), the
    /// HUD-customization entry (T046), then one key-binding row per
    /// [`InputAction`].
    #[must_use]
    pub fn new() -> Self {
        let mut rows = vec![
            SetupRow::Device,
            SetupRow::CpuDifficulty,
            SetupRow::CpuMode,
            SetupRow::HudCustomize,
        ];
        rows.extend(InputAction::ALL.iter().map(|&a| SetupRow::Action(a)));
        Self {
            cursor: 0,
            capturing: None,
            rows,
        }
    }

    /// Whether the screen is waiting for a key press to bind (capture mode).
    #[must_use]
    pub fn awaiting_key(&self) -> bool {
        self.capturing.is_some()
    }

    /// The number of selectable rows (device toggle + one per action).
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Whether the device-preference row is highlighted.
    #[must_use]
    pub fn on_device_row(&self) -> bool {
        matches!(self.rows.get(self.cursor), Some(SetupRow::Device))
    }

    /// Whether the CPU-difficulty selector row is highlighted (T069).
    #[must_use]
    pub fn on_cpu_difficulty_row(&self) -> bool {
        matches!(self.rows.get(self.cursor), Some(SetupRow::CpuDifficulty))
    }

    /// Whether the CPU teaching-mode selector row is highlighted (T070).
    #[must_use]
    pub fn on_cpu_mode_row(&self) -> bool {
        matches!(self.rows.get(self.cursor), Some(SetupRow::CpuMode))
    }

    /// Whether the HUD-customization row is highlighted (T046).
    #[must_use]
    pub fn on_hud_row(&self) -> bool {
        matches!(self.rows.get(self.cursor), Some(SetupRow::HudCustomize))
    }

    /// What each row represents, in display order, for the renderer to label and
    /// walk each row unambiguously (the device toggle, the HUD-customization
    /// entry, or an action's key binding). Parallel to [`SetupScreen::row_count`].
    #[must_use]
    pub fn row_kinds(&self) -> Vec<SetupRowKind> {
        self.rows
            .iter()
            .map(|r| match r {
                SetupRow::Device => SetupRowKind::Device,
                SetupRow::CpuDifficulty => SetupRowKind::CpuDifficulty,
                SetupRow::CpuMode => SetupRowKind::CpuMode,
                SetupRow::HudCustomize => SetupRowKind::HudCustomize,
                SetupRow::Action(a) => SetupRowKind::Action(*a),
            })
            .collect()
    }

    /// The [`InputAction`] of the highlighted row, or `None` on the device row.
    #[must_use]
    pub fn selected_action(&self) -> Option<InputAction> {
        match self.rows.get(self.cursor) {
            Some(SetupRow::Action(a)) => Some(*a),
            _ => None,
        }
    }

    /// Applies one frame of menu input, editing `config` in place.
    ///
    /// While *capturing* a key, navigation is suspended and only `back` cancels
    /// the capture (so a mis-fired confirm can't trap the player); the actual
    /// rebind happens in [`SetupScreen::capture_key`]. Otherwise:
    /// - Up/Down move the cursor (wrapping);
    /// - Left/Right toggle the [`InputDevice`] preference when the device row is
    ///   highlighted;
    /// - Left/Right step the [`AiDifficulty`] selector (Easy↔Normal↔Hard,
    ///   saturating) when the CPU-difficulty row is highlighted (T069);
    /// - Left/Right step the [`BehaviorMode`] selector (Ladder↔Pure Blocker↔
    ///   Reactive DP↔Whiff Punisher, wrapping) when the CPU teaching-mode row is
    ///   highlighted (T070);
    /// - Confirm on the device row toggles it too; confirm on the CPU-difficulty
    ///   row steps it one harder (wrapping back to Easy from Hard so it stays
    ///   reachable); confirm on the CPU teaching-mode row steps it one forward
    ///   (wrapping); confirm on an action row arms capture mode for that action;
    /// - Back returns [`SetupOutcome::Exit`] (to the title).
    pub fn update(&mut self, input: MenuInput, config: &mut InputConfig) -> SetupOutcome {
        if self.capturing.is_some() {
            // Armed: ignore navigation; back cancels the capture without binding.
            if input.back {
                self.capturing = None;
            }
            return SetupOutcome::Pending;
        }

        if input.back {
            return SetupOutcome::Exit;
        }

        let len = self.rows.len();
        if len == 0 {
            return SetupOutcome::Pending;
        }
        if input.up {
            self.cursor = wrap_dec(self.cursor, len);
        }
        if input.down {
            self.cursor = wrap_inc(self.cursor, len);
        }

        match self.rows.get(self.cursor).copied() {
            // Device row: either a horizontal step or confirm flips the preference.
            Some(SetupRow::Device) if input.left || input.right || input.confirm => {
                config.device = config.device.toggled();
            }
            // CPU-difficulty row (T069): Left steps easier, Right steps harder
            // (both saturating at the ends); Confirm steps harder but wraps from
            // Hard back to Easy so the selector stays fully reachable with only a
            // confirm key.
            Some(SetupRow::CpuDifficulty) if input.left => {
                config.cpu_difficulty = config.cpu_difficulty.easier();
            }
            Some(SetupRow::CpuDifficulty) if input.right => {
                config.cpu_difficulty = config.cpu_difficulty.harder();
            }
            Some(SetupRow::CpuDifficulty) if input.confirm => {
                config.cpu_difficulty = match config.cpu_difficulty {
                    AiDifficulty::Hard => AiDifficulty::Easy,
                    other => other.harder(),
                };
            }
            // CPU teaching-mode row (T070): Left/Right step the `BehaviorMode`
            // selector (Ladder → Pure Blocker → Reactive DP → Whiff Punisher),
            // wrapping both ways; Confirm steps forward too — so the selector is
            // fully reachable with only a confirm key.
            Some(SetupRow::CpuMode) if input.left => {
                config.cpu_mode = config.cpu_mode.prev();
            }
            Some(SetupRow::CpuMode) if input.right || input.confirm => {
                config.cpu_mode = config.cpu_mode.next();
            }
            // HUD-customization row + confirm: open the HUD-customization screen.
            Some(SetupRow::HudCustomize) if input.confirm => {
                return SetupOutcome::OpenHudCustomize;
            }
            // Action row + confirm: arm capture so the next key press rebinds it.
            Some(SetupRow::Action(action)) if input.confirm => {
                self.capturing = Some(action);
            }
            _ => {}
        }
        SetupOutcome::Pending
    }

    /// Binds the captured action to `key` and leaves capture mode.
    ///
    /// Called by the app when, after a confirm armed capture
    /// ([`SetupScreen::awaiting_key`]), the player presses a key. A no-op (returns
    /// `None`) when not capturing. On a successful bind returns the
    /// `(action, displaced)` pair: `displaced` is `Some(other)` if `key` was
    /// already bound to a different action, which the caller may clear to keep
    /// keys unique. The rebind takes effect immediately in `config`.
    pub fn capture_key(
        &mut self,
        key: KeyCode,
        config: &mut InputConfig,
    ) -> Option<(InputAction, Option<InputAction>)> {
        let action = self.capturing.take()?;
        let displaced = config.rebind(action, key);
        Some((action, displaced))
    }
}

// ---------------------------------------------------------------------------
// HUD-customization screen (T046)
// ---------------------------------------------------------------------------

/// What one frame of HUD-customization-screen input produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HudCustomizeOutcome {
    /// Still on the HUD-customization screen.
    Pending,
    /// The player left the screen (back/cancel): return to the setup screen.
    Exit,
}

/// One selectable row on the HUD-customization screen.
///
/// The first two rows cycle the life- and power-bar colors; the rest toggle one
/// [`HudElement`]'s visibility each. Used both by [`HudCustomizeScreen::update`]
/// and by the renderer to label each row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HudRow {
    /// Cycle the life-bar color through [`BarColor::PRESETS`].
    LifeColor,
    /// Cycle the power-bar color through [`BarColor::PRESETS`].
    PowerColor,
    /// Toggle the given element's visibility.
    Visibility(HudElement),
}

impl HudRow {
    /// A short uppercase label (matching the HUD font's glyph set) for the row,
    /// for the customization-screen renderer.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            HudRow::LifeColor => "LIFE BAR COLOR",
            HudRow::PowerColor => "POWER BAR COLOR",
            HudRow::Visibility(e) => e.label(),
        }
    }
}

/// The in-game HUD-customization screen (T046): edits a live [`HudConfig`].
///
/// Reachable from the setup/options screen (a `SetupRow::HudCustomize` row →
/// [`SetupOutcome::OpenHudCustomize`]). A single vertical cursor walks the rows:
/// the life-bar color, the power-bar color, then one row per [`HudElement`].
/// Up/Down move the cursor (wrapping); Left/Right/Confirm on a color row cycle
/// that bar's color through [`BarColor::PRESETS`]; Confirm on an element row
/// toggles that element's visibility. Back/cancel returns to the setup screen.
///
/// Edits are applied in place to the [`HudConfig`] the app holds (and hands to the
/// [`ScreenpackHud`](fp_ui::ScreenpackHud) renderer), so a change is reflected in
/// the model the renderer reads immediately. Navigation is source-agnostic
/// ([`MenuInput`]), so a controller and the keyboard drive it identically. Nothing
/// here panics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HudCustomizeScreen {
    /// The highlighted row index into [`HudCustomizeScreen::rows`].
    pub cursor: usize,
    /// The selectable rows (two color rows + one per element), in display order.
    rows: Vec<HudRow>,
}

impl Default for HudCustomizeScreen {
    fn default() -> Self {
        Self::new()
    }
}

impl HudCustomizeScreen {
    /// Builds the HUD-customization screen: cursor on the first row.
    #[must_use]
    pub fn new() -> Self {
        let mut rows = vec![HudRow::LifeColor, HudRow::PowerColor];
        rows.extend(HudElement::ALL.iter().map(|&e| HudRow::Visibility(e)));
        Self { cursor: 0, rows }
    }

    /// The number of selectable rows (two color rows + one per element).
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// The rows in display order, for the renderer to walk alongside the cursor.
    #[must_use]
    pub fn rows(&self) -> &[HudRow] {
        &self.rows
    }

    /// The highlighted row.
    #[must_use]
    pub fn selected_row(&self) -> Option<HudRow> {
        self.rows.get(self.cursor).copied()
    }

    /// Applies one frame of menu input, editing `config` in place.
    ///
    /// - Up/Down move the cursor (wrapping);
    /// - Left/Right/Confirm on a color row cycle that bar's color through
    ///   [`BarColor::PRESETS`];
    /// - Confirm on an element row toggles that element's visibility;
    /// - Back returns [`HudCustomizeOutcome::Exit`] (to the setup screen).
    ///
    /// Any edit takes effect immediately in `config` — the same [`HudConfig`] the
    /// renderer reads — so the HUD changes on the next frame.
    pub fn update(&mut self, input: MenuInput, config: &mut HudConfig) -> HudCustomizeOutcome {
        if input.back {
            return HudCustomizeOutcome::Exit;
        }
        let len = self.rows.len();
        if len == 0 {
            return HudCustomizeOutcome::Pending;
        }
        if input.up {
            self.cursor = wrap_dec(self.cursor, len);
        }
        if input.down {
            self.cursor = wrap_inc(self.cursor, len);
        }
        match self.rows.get(self.cursor).copied() {
            // A color row: any horizontal step or confirm cycles the next preset.
            Some(HudRow::LifeColor) if input.left || input.right || input.confirm => {
                config.set_life_color(config.life_color().next_preset());
            }
            Some(HudRow::PowerColor) if input.left || input.right || input.confirm => {
                config.set_power_color(config.power_color().next_preset());
            }
            // An element row + confirm: flip its visibility.
            Some(HudRow::Visibility(element)) if input.confirm => {
                config.toggle_visible(element);
            }
            _ => {}
        }
        HudCustomizeOutcome::Pending
    }
}

/// Moves a linear grid cursor one step per asserted direction, wrapping at the
/// grid edges. Horizontal moves wrap within the linear list; vertical moves jump
/// by `columns`, wrapping top/bottom while staying in range.
///
/// `len` must be `> 0` (callers guard the empty roster) and `columns >= 1`. Pure
/// and unit-tested.
fn move_cursor(cursor: usize, input: MenuInput, columns: usize, len: usize) -> usize {
    let mut c = cursor.min(len - 1);
    let columns = columns.max(1);
    if input.left {
        c = wrap_dec(c, len);
    }
    if input.right {
        c = wrap_inc(c, len);
    }
    if input.up {
        c = if c >= columns {
            c - columns
        } else {
            // Wrap to the matching column on the last row.
            let col = c % columns;
            let last_row_start = ((len - 1) / columns) * columns;
            let candidate = last_row_start + col;
            if candidate < len {
                candidate
            } else {
                // That column has no cell on the last (partial) row; step back one
                // row so the cursor stays on a real cell.
                candidate.saturating_sub(columns)
            }
        };
    }
    if input.down {
        let candidate = c + columns;
        c = if candidate < len {
            candidate
        } else {
            // Wrap to the matching column on the first row.
            c % columns
        };
    }
    c.min(len - 1)
}

/// Decrements `i` modulo `len`, wrapping `0 -> len - 1`. `len` must be `> 0`.
fn wrap_dec(i: usize, len: usize) -> usize {
    if i == 0 {
        len - 1
    } else {
        i - 1
    }
}

/// Increments `i` modulo `len`, wrapping `len - 1 -> 0`. `len` must be `> 0`.
fn wrap_inc(i: usize, len: usize) -> usize {
    if i + 1 >= len {
        0
    } else {
        i + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_ui::MenuItem;

    fn mi(kind: MenuItemKind, label: &str) -> MenuItem {
        MenuItem {
            kind,
            label: label.to_string(),
        }
    }

    fn confirm() -> MenuInput {
        MenuInput {
            confirm: true,
            ..MenuInput::default()
        }
    }
    fn down() -> MenuInput {
        MenuInput {
            down: true,
            ..MenuInput::default()
        }
    }
    fn up() -> MenuInput {
        MenuInput {
            up: true,
            ..MenuInput::default()
        }
    }
    fn back() -> MenuInput {
        MenuInput {
            back: true,
            ..MenuInput::default()
        }
    }
    fn info() -> MenuInput {
        MenuInput {
            info: true,
            ..MenuInput::default()
        }
    }

    // ---- edge detection -------------------------------------------------

    #[test]
    fn edge_fires_once_on_rising_then_stays_off_while_held() {
        let held = HeldMenuInput {
            down: true,
            ..Default::default()
        };
        // Rising edge: down was not held last frame.
        let first = MenuInput::from_edges(held, HeldMenuInput::default());
        assert!(first.down, "first press is an edge");
        // Held a second frame: no edge.
        let second = MenuInput::from_edges(held, held);
        assert!(!second.down, "holding does not re-fire");
    }

    // ---- title menu -----------------------------------------------------

    #[test]
    fn title_from_system_maps_items_to_actions_in_order() {
        let title = fp_ui::TitleInfo {
            items: vec![
                mi(MenuItemKind::Versus, "VS MODE"),
                mi(MenuItemKind::Training, "TRAINING"),
                mi(MenuItemKind::Arcade, "ARCADE"),
                mi(MenuItemKind::Exit, "EXIT"),
            ],
            ..fp_ui::TitleInfo::default()
        };
        let system = SystemDef {
            title,
            ..SystemDef::default()
        };
        let menu = TitleMenu::from_system(&system);
        assert_eq!(menu.entries.len(), 4);
        assert_eq!(
            menu.entries[0].action,
            TitleAction::Select(SelectMode::Versus)
        );
        assert_eq!(
            menu.entries[1].action,
            TitleAction::Select(SelectMode::Training)
        );
        assert_eq!(menu.entries[2].action, TitleAction::NoOp, "arcade is inert");
        assert_eq!(menu.entries[3].action, TitleAction::Quit);
        assert_eq!(menu.entries[0].label, "VS MODE");
    }

    #[test]
    fn title_empty_motif_uses_fallback() {
        let menu = TitleMenu::from_system(&SystemDef::default());
        // Fallback ships VS / TRAINING / SETUP / EXIT.
        assert_eq!(menu.entries.len(), 4);
        assert_eq!(menu.entries[0].label, "VS MODE");
        assert_eq!(menu.entries[2].action, TitleAction::Setup);
        assert_eq!(menu.entries[3].action, TitleAction::Quit);
    }

    #[test]
    fn title_cursor_moves_and_wraps() {
        let mut menu = TitleMenu::fallback(); // 4 entries
        assert_eq!(menu.cursor, 0);
        menu.update(down());
        assert_eq!(menu.cursor, 1);
        menu.update(down());
        menu.update(down());
        assert_eq!(menu.cursor, 3);
        menu.update(down());
        assert_eq!(menu.cursor, 0, "down wraps from last to first");
        menu.update(up());
        assert_eq!(menu.cursor, 3, "up wraps from first to last");
    }

    #[test]
    fn title_confirm_returns_highlighted_action() {
        let mut menu = TitleMenu::fallback();
        menu.update(down()); // cursor -> TRAINING
        let action = menu.update(confirm());
        assert_eq!(action, Some(TitleAction::Select(SelectMode::Training)));
    }

    #[test]
    fn title_back_quits() {
        let mut menu = TitleMenu::fallback();
        assert_eq!(menu.update(back()), Some(TitleAction::Quit));
    }

    #[test]
    fn title_empty_entries_back_still_quits() {
        let mut menu = TitleMenu {
            entries: vec![],
            cursor: 0,
        };
        assert_eq!(menu.update(back()), Some(TitleAction::Quit));
        assert_eq!(menu.update(confirm()), None, "no items to confirm");
    }

    // ---- select screen helpers ------------------------------------------

    fn roster_2plus_random() -> SelectDef {
        // Two distinct characters then a random icon. The display names carry a
        // space so the parser uses the explicit-def form (name, deffile), making
        // the resolved def_path exactly the second field.
        SelectDef::parse("[Characters]\nChar Alpha, a/a.def\nChar Beta, b/b.def\nrandomselect\n")
    }

    fn info_grid(cols: u32) -> SelectInfo {
        SelectInfo {
            columns: cols,
            rows: 1,
            ..SelectInfo::default()
        }
    }

    #[test]
    fn select_drops_empty_slots_and_keeps_choosable() {
        let select = SelectDef::parse("[Characters]\nAlpha, a/a.def\nblank\nBeta, b/b.def\n");
        let screen = SelectScreen::new(
            SelectMode::Versus,
            &select,
            &info_grid(3),
            Path::new("data/select.def"),
        );
        assert_eq!(screen.cells.len(), 2, "blank slot dropped");
        assert!(matches!(screen.cells[0], RosterCell::Character(_)));
        assert!(matches!(screen.cells[1], RosterCell::Character(_)));
    }

    #[test]
    fn training_single_confirm_picks_p1_and_mirrors_p2() {
        let select = roster_2plus_random();
        let mut screen = SelectScreen::new(
            SelectMode::Training,
            &select,
            &info_grid(3),
            Path::new("data/select.def"),
        );
        // P1 starts on Alpha (cell 0). Confirm.
        let outcome = screen.update(confirm(), 0);
        let SelectOutcome::Done(pick) = outcome else {
            panic!("training confirm should complete the screen");
        };
        assert_eq!(pick.p1_def, PathBuf::from("data").join("a/a.def"));
        assert_eq!(pick.p2_def, pick.p1_def, "training: P2 mirrors P1");
        assert_eq!(pick.p1_name, "Char Alpha");
        assert_eq!(pick.p2_name, "Char Alpha");
    }

    #[test]
    fn versus_needs_both_confirms() {
        let select = roster_2plus_random();
        let mut screen = SelectScreen::new(
            SelectMode::Versus,
            &select,
            &info_grid(3),
            Path::new("data/select.def"),
        );
        // P1 confirms on Alpha.
        assert_eq!(screen.update(confirm(), 0), SelectOutcome::Pending);
        assert_eq!(screen.p1_locked, Some(0));
        assert!(screen.p2_locked.is_none(), "P2 not locked by P1's confirm");
        // P2 moves to Beta (cell 1) then confirms.
        let mv = MenuInput {
            right: true,
            ..MenuInput::default()
        };
        screen.update(mv, 0);
        let outcome = screen.update(confirm(), 0);
        let SelectOutcome::Done(pick) = outcome else {
            panic!("both confirmed should complete");
        };
        assert_eq!(pick.p1_def, PathBuf::from("data").join("a/a.def"));
        assert_eq!(pick.p2_def, PathBuf::from("data").join("b/b.def"));
        assert_eq!(pick.p1_name, "Char Alpha");
        assert_eq!(pick.p2_name, "Char Beta");
    }

    #[test]
    fn random_resolves_to_a_concrete_character_deterministically() {
        // A roster of only one concrete char + random: random must resolve to it.
        // The display name carries a space (explicit-def form), so def_path is the
        // second field verbatim.
        let select = SelectDef::parse("[Characters]\nThe Only, only/only.def\nrandomselect\n");
        let mut screen = SelectScreen::new(
            SelectMode::Training,
            &select,
            &info_grid(2),
            Path::new("data/select.def"),
        );
        // Move P1 to the random cell (index 1) and confirm.
        let right = MenuInput {
            right: true,
            ..MenuInput::default()
        };
        screen.update(right, 0);
        assert_eq!(screen.p1_cursor, 1);
        let SelectOutcome::Done(pick) = screen.update(confirm(), 12345) else {
            panic!("confirm on random should complete");
        };
        assert_eq!(
            pick.p1_def,
            PathBuf::from("data").join("only/only.def"),
            "random resolves to the only concrete character"
        );
        assert_eq!(pick.p1_name, "The Only");
    }

    #[test]
    fn info_on_character_cell_opens_info_with_resolved_def_path() {
        // T071: pressing Info on a concrete character yields ShowInfo with the
        // `.def` resolved against the select.def directory — without moving the
        // cursor or locking anything.
        let select = roster_2plus_random();
        let mut screen = SelectScreen::new(
            SelectMode::Versus,
            &select,
            &info_grid(3),
            Path::new("data/select.def"),
        );
        // Move to Beta (cell 1) so we exercise the active-cursor resolution.
        screen.update(
            MenuInput {
                right: true,
                ..MenuInput::default()
            },
            0,
        );
        assert_eq!(screen.p1_cursor, 1);
        let outcome = screen.update(info(), 0);
        assert_eq!(
            outcome,
            SelectOutcome::ShowInfo(PathBuf::from("data").join("b/b.def")),
        );
        // The select screen is untouched: still nobody locked, cursor unmoved.
        assert!(screen.p1_locked.is_none());
        assert_eq!(screen.p1_cursor, 1);
    }

    #[test]
    fn info_on_random_cell_is_inert() {
        // Random has no single `.def` to describe, so Info stays Pending.
        let select = roster_2plus_random();
        let mut screen = SelectScreen::new(
            SelectMode::Training,
            &select,
            &info_grid(3),
            Path::new("data/select.def"),
        );
        // Cell 2 is the random icon.
        screen.update(
            MenuInput {
                right: true,
                ..MenuInput::default()
            },
            0,
        );
        screen.update(
            MenuInput {
                right: true,
                ..MenuInput::default()
            },
            0,
        );
        assert_eq!(screen.p1_cursor, 2);
        assert_eq!(screen.update(info(), 0), SelectOutcome::Pending);
    }

    #[test]
    fn info_screen_from_loaded_lists_specials_and_dismisses() {
        // Build an InfoScreen straight from a synthetic loaded character carrying
        // a known `.cmd`, and confirm the movelist + dismiss behaviour without any
        // window or filesystem.
        use fp_formats::cmd::CmdFile;
        let cmd = CmdFile::from_str(
            "[Command]\nname = \"fireball\"\ncommand = ~D, DF, F, a\n\n\
             [Command]\nname = \"holdfwd\"\ncommand = /$F\n",
        )
        .unwrap();
        // A minimal loaded character is awkward to construct here; instead test the
        // pure mapping the screen relies on, then the input handling.
        let moves = fp_character::movelist_from_cmd(Some(&cmd));
        let screen = InfoScreen {
            display_name: "Test Fighter".to_string(),
            author: "Me".to_string(),
            moves,
        };
        // `holdfwd` is filtered as locomotion; `fireball` survives with QCF+a.
        assert_eq!(screen.moves.len(), 1);
        assert_eq!(screen.moves[0].name, "fireball");
        assert_eq!(screen.moves[0].motion, "QCF+a");
        // Any of back / confirm / info dismisses.
        assert_eq!(screen.update(back()), InfoOutcome::Dismissed);
        assert_eq!(screen.update(confirm()), InfoOutcome::Dismissed);
        assert_eq!(screen.update(info()), InfoOutcome::Dismissed);
        assert_eq!(screen.update(MenuInput::default()), InfoOutcome::Pending);
    }

    #[test]
    fn info_screen_load_failed_still_shows_label() {
        // A character that fails to load must not trap the player: the fallback
        // shows the roster label and an empty movelist, never panics.
        let screen = InfoScreen::load_failed("Broken Char");
        assert_eq!(screen.display_name, "Broken Char");
        assert!(screen.author.is_empty());
        assert!(screen.moves.is_empty());
        assert_eq!(screen.update(back()), InfoOutcome::Dismissed);
    }

    #[test]
    fn random_is_deterministic_for_a_seed() {
        let select = roster_2plus_random();
        let make = || {
            SelectScreen::new(
                SelectMode::Training,
                &select,
                &info_grid(3),
                Path::new("data/select.def"),
            )
        };
        // Two screens, same seed, both confirm the random cell (index 2).
        let pick_for = |seed: u64| {
            let mut s = make();
            // jump to random cell (index 2) via two rights
            let right = MenuInput {
                right: true,
                ..MenuInput::default()
            };
            s.update(right, 0);
            s.update(right, 0);
            assert_eq!(s.p1_cursor, 2);
            match s.update(confirm(), seed) {
                SelectOutcome::Done(p) => p.p1_def,
                _ => panic!("done"),
            }
        };
        assert_eq!(pick_for(7), pick_for(7), "same seed -> same pick");
    }

    #[test]
    fn select_back_cancels_to_title() {
        let select = roster_2plus_random();
        let mut screen = SelectScreen::new(
            SelectMode::Versus,
            &select,
            &info_grid(3),
            Path::new("data/select.def"),
        );
        assert_eq!(screen.update(back(), 0), SelectOutcome::Cancelled);
    }

    #[test]
    fn empty_roster_only_cancels_never_panics() {
        let select = SelectDef::parse("[Characters]\nblank\n");
        let mut screen = SelectScreen::new(
            SelectMode::Training,
            &select,
            &info_grid(1),
            Path::new("data/select.def"),
        );
        assert!(screen.is_empty());
        assert_eq!(screen.update(confirm(), 0), SelectOutcome::Pending);
        assert_eq!(screen.update(back(), 0), SelectOutcome::Cancelled);
    }

    #[test]
    fn single_entry_roster_navigation_is_stable() {
        // The shipped trainingdummy roster shape: one (well, repeated) character.
        let select = SelectDef::parse("[Characters]\nOnly, only/only.def\n");
        let mut screen = SelectScreen::new(
            SelectMode::Training,
            &select,
            &info_grid(4),
            Path::new("data/select.def"),
        );
        // Every direction keeps the cursor on the single cell.
        for dir in [
            up(),
            down(),
            MenuInput {
                left: true,
                ..MenuInput::default()
            },
        ] {
            screen.update(dir, 0);
            assert_eq!(screen.p1_cursor, 0, "single-cell cursor never leaves 0");
        }
        assert!(matches!(
            screen.update(confirm(), 0),
            SelectOutcome::Done(_)
        ));
    }

    // ---- grid cursor ----------------------------------------------------

    #[test]
    fn move_cursor_horizontal_wraps_in_list() {
        // 5 cells, 5 columns: left from 0 wraps to 4, right from 4 wraps to 0.
        let left = MenuInput {
            left: true,
            ..MenuInput::default()
        };
        let right = MenuInput {
            right: true,
            ..MenuInput::default()
        };
        assert_eq!(move_cursor(0, left, 5, 5), 4);
        assert_eq!(move_cursor(4, right, 5, 5), 0);
        assert_eq!(move_cursor(2, right, 5, 5), 3);
    }

    #[test]
    fn move_cursor_vertical_jumps_by_columns_and_wraps() {
        // 6 cells, 3 columns -> 2 rows. Down from 0 -> 3; down from 3 wraps -> 0.
        let down = MenuInput {
            down: true,
            ..MenuInput::default()
        };
        let up = MenuInput {
            up: true,
            ..MenuInput::default()
        };
        assert_eq!(move_cursor(0, down, 3, 6), 3);
        assert_eq!(
            move_cursor(3, down, 3, 6),
            0,
            "down from last row wraps to first"
        );
        assert_eq!(
            move_cursor(0, up, 3, 6),
            3,
            "up from first row wraps to last"
        );
        assert_eq!(move_cursor(4, up, 3, 6), 1);
    }

    #[test]
    fn move_cursor_partial_last_row_stays_in_range() {
        // 5 cells, 3 columns -> rows [0,1,2] and [3,4]. Down from cell 2 (col 2)
        // would land on col 2 of the last row (index 5) which doesn't exist; it
        // must clamp into range, never out of bounds.
        let down = MenuInput {
            down: true,
            ..MenuInput::default()
        };
        let c = move_cursor(2, down, 3, 5);
        assert!(c < 5, "cursor stays in range on a partial last row");
    }

    #[test]
    fn move_cursor_single_cell_is_fixed() {
        for dir in [
            MenuInput {
                up: true,
                ..MenuInput::default()
            },
            MenuInput {
                down: true,
                ..MenuInput::default()
            },
            MenuInput {
                left: true,
                ..MenuInput::default()
            },
            MenuInput {
                right: true,
                ..MenuInput::default()
            },
        ] {
            assert_eq!(move_cursor(0, dir, 1, 1), 0);
        }
    }

    // ---- stage select ---------------------------------------------------

    fn right() -> MenuInput {
        MenuInput {
            right: true,
            ..MenuInput::default()
        }
    }

    fn three_stages() -> StageSelect {
        StageSelect::new(vec![
            StageEntry::backdrop("DOJO", "assets/stages/dojo/bg.png"),
            StageEntry::def("ARENA", "stages/arena.def"),
            StageEntry::def("TEMPLE", "stages/temple.def"),
        ])
    }

    #[test]
    fn stage_cursor_moves_and_wraps() {
        let mut s = three_stages();
        assert_eq!(s.cursor, 0);
        assert_eq!(s.update(down()), StageOutcome::Pending);
        assert_eq!(s.cursor, 1);
        s.update(down());
        assert_eq!(s.cursor, 2);
        s.update(down());
        assert_eq!(s.cursor, 0, "down wraps from last to first");
        s.update(up());
        assert_eq!(s.cursor, 2, "up wraps from first to last");
    }

    #[test]
    fn stage_left_right_navigate_like_up_down() {
        // Either axis steps the single cursor (a controller D-pad / stick on
        // either axis can drive the list).
        let mut s = three_stages();
        s.update(right());
        assert_eq!(s.cursor, 1);
        let left = MenuInput {
            left: true,
            ..MenuInput::default()
        };
        s.update(left);
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn stage_confirm_yields_highlighted_choice() {
        let mut s = three_stages();
        s.update(down()); // -> ARENA (a .def stage)
        let StageOutcome::Done(choice) = s.update(confirm()) else {
            panic!("confirm should complete the stage screen");
        };
        assert_eq!(choice.name, "ARENA");
        assert_eq!(choice.path, PathBuf::from("stages/arena.def"));
        assert_eq!(choice.kind, StageKind::Def);
    }

    #[test]
    fn stage_confirm_on_default_yields_backdrop() {
        // The first entry is the shipped dojo backdrop (a full-window image).
        let mut s = three_stages();
        let StageOutcome::Done(choice) = s.update(confirm()) else {
            panic!("confirm should complete");
        };
        assert_eq!(choice.name, "DOJO");
        assert_eq!(choice.kind, StageKind::Backdrop);
        assert_eq!(choice.path, PathBuf::from("assets/stages/dojo/bg.png"));
    }

    #[test]
    fn stage_back_cancels_to_character_select() {
        let mut s = three_stages();
        assert_eq!(s.update(back()), StageOutcome::Cancelled);
    }

    #[test]
    fn stage_empty_list_only_cancels_never_panics() {
        let mut s = StageSelect::new(vec![]);
        assert!(s.entries.is_empty());
        // Confirm/navigation on an empty list is inert (no panic, no pick).
        assert_eq!(s.update(confirm()), StageOutcome::Pending);
        assert_eq!(s.update(down()), StageOutcome::Pending);
        assert_eq!(s.update(back()), StageOutcome::Cancelled);
    }

    #[test]
    fn stage_entries_from_roster_starts_with_backdrop_then_def_stages() {
        // Two characters (one offering a stage, one with includestage=0) plus an
        // extra stage. The display names carry a space so the parser uses the
        // explicit-def form; the stage is the third field.
        let select = SelectDef::parse(
            "[Characters]\n\
             Char Alpha, a/a.def, stages/alpha.def\n\
             Char Beta, b/b.def, stages/beta.def, includestage=0\n\
             [ExtraStages]\n\
             stages/extra.def\n",
        );
        let entries = stage_entries_from_roster(
            &select,
            Path::new("data"),
            "DOJO",
            Path::new("assets/stages/dojo/bg.png"),
        );
        // [0] is always the dojo backdrop.
        assert_eq!(entries[0].kind, StageKind::Backdrop);
        assert_eq!(entries[0].name, "DOJO");
        // Alpha's stage is offered (includestage default true); Beta's is not
        // (includestage=0). The extra stage follows. All resolved under data/.
        let def_paths: Vec<&PathBuf> = entries
            .iter()
            .filter(|e| e.kind == StageKind::Def)
            .map(|e| &e.path)
            .collect();
        assert!(def_paths.contains(&&PathBuf::from("data").join("stages/alpha.def")));
        assert!(def_paths.contains(&&PathBuf::from("data").join("stages/extra.def")));
        assert!(
            !def_paths.contains(&&PathBuf::from("data").join("stages/beta.def")),
            "includestage=0 excludes the character's stage"
        );
        // The .def label is the file stem.
        let alpha = entries
            .iter()
            .find(|e| e.path == PathBuf::from("data").join("stages/alpha.def"))
            .unwrap();
        assert_eq!(alpha.name, "alpha");
    }

    #[test]
    fn stage_entries_from_roster_dedups_repeated_stages() {
        // The same stage named by two characters AND in extra-stages appears once.
        let select = SelectDef::parse(
            "[Characters]\n\
             Char Alpha, a/a.def, stages/shared.def\n\
             Char Beta, b/b.def, stages/shared.def\n\
             [ExtraStages]\n\
             stages/shared.def\n",
        );
        let entries = stage_entries_from_roster(
            &select,
            Path::new("data"),
            "DOJO",
            Path::new("assets/stages/dojo/bg.png"),
        );
        let shared = PathBuf::from("data").join("stages/shared.def");
        let count = entries.iter().filter(|e| e.path == shared).count();
        assert_eq!(count, 1, "a repeated stage is de-duplicated");
    }

    #[test]
    fn stage_entries_from_roster_always_has_backdrop_when_no_stages() {
        // A roster whose only character declares no stage (a bare classic-form
        // name, no second field) still yields the dojo backdrop, so the stage
        // screen always offers a choice.
        let select = SelectDef::parse("[Characters]\nonly\n");
        let entries = stage_entries_from_roster(
            &select,
            Path::new("data"),
            "DOJO",
            Path::new("assets/stages/dojo/bg.png"),
        );
        assert_eq!(entries.len(), 1, "no stages declared -> only the backdrop");
        assert_eq!(entries[0].kind, StageKind::Backdrop);
    }

    // ---- title -> setup mapping -----------------------------------------

    #[test]
    fn options_item_maps_to_setup_action() {
        // A motif `Options` item opens the setup screen (T042), not a no-op.
        let title = fp_ui::TitleInfo {
            items: vec![mi(MenuItemKind::Options, "OPTIONS")],
            ..fp_ui::TitleInfo::default()
        };
        let system = SystemDef {
            title,
            ..SystemDef::default()
        };
        let menu = TitleMenu::from_system(&system);
        assert_eq!(menu.entries[0].action, TitleAction::Setup);
    }

    #[test]
    fn fallback_setup_item_confirms_to_setup_action() {
        // The built-in fallback menu offers SETUP (index 2): confirming it yields
        // the Setup action. This is the action a controller confirm produces too,
        // since the title consumes a source-agnostic MenuInput.
        let mut menu = TitleMenu::fallback();
        menu.update(down()); // -> TRAINING
        menu.update(down()); // -> SETUP
        assert_eq!(menu.update(confirm()), Some(TitleAction::Setup));
    }

    // ---- setup screen (T042) --------------------------------------------

    fn left() -> MenuInput {
        MenuInput {
            left: true,
            ..MenuInput::default()
        }
    }

    /// A default config seeded with a deterministic synthetic key per action
    /// (action index as the opaque KeyCode), so tests don't need SDL scancodes.
    fn config_with_index_keys() -> InputConfig {
        let mut next = 0i32;
        let keys: Vec<(InputAction, KeyCode)> = InputAction::ALL
            .iter()
            .map(|&a| {
                let k = (a, KeyCode(next));
                next += 1;
                k
            })
            .collect();
        // Rebuild via default_with so the public constructor is exercised.
        let lookup = keys.clone();
        InputConfig::default_with(|action| {
            lookup
                .iter()
                .find(|(a, _)| *a == action)
                .map(|(_, k)| *k)
                .unwrap_or(KeyCode(-1))
        })
    }

    #[test]
    fn setup_starts_on_device_row_not_capturing() {
        let s = SetupScreen::new();
        assert_eq!(s.cursor, 0);
        assert!(s.on_device_row());
        assert!(!s.awaiting_key());
        // Rows = device + CPU-difficulty (T069) + CPU teaching-mode (T070) +
        // HUD-customization (T046) + one per action.
        assert_eq!(s.row_count(), 4 + InputAction::ALL.len());
    }

    #[test]
    fn setup_cursor_moves_and_wraps() {
        let mut s = SetupScreen::new();
        let mut cfg = config_with_index_keys();
        let last = s.row_count() - 1;
        assert_eq!(s.update(up(), &mut cfg), SetupOutcome::Pending);
        assert_eq!(s.cursor, last, "up from the first row wraps to the last");
        s.update(down(), &mut cfg);
        assert_eq!(s.cursor, 0, "down from the last row wraps to the first");
        s.update(down(), &mut cfg);
        assert_eq!(s.cursor, 1, "row 1 is the CPU-difficulty row");
        assert!(s.on_cpu_difficulty_row());
        assert_eq!(s.selected_action(), None, "the CPU row is not an action");
        s.update(down(), &mut cfg);
        assert_eq!(s.cursor, 2, "row 2 is the CPU teaching-mode row");
        assert!(s.on_cpu_mode_row());
        assert_eq!(
            s.selected_action(),
            None,
            "the CPU-mode row is not an action"
        );
        s.update(down(), &mut cfg);
        assert_eq!(s.cursor, 3, "row 3 is the HUD-customization row");
        assert!(s.on_hud_row());
        assert_eq!(s.selected_action(), None, "the HUD row is not an action");
        s.update(down(), &mut cfg);
        assert_eq!(s.cursor, 4);
        assert_eq!(s.selected_action(), Some(InputAction::Up));
    }

    #[test]
    fn setup_device_row_toggles_with_horizontal_and_confirm() {
        let mut s = SetupScreen::new();
        let mut cfg = config_with_index_keys();
        assert_eq!(cfg.device, InputDevice::Keyboard);
        s.update(right(), &mut cfg);
        assert_eq!(cfg.device, InputDevice::Controller, "right toggles device");
        s.update(left(), &mut cfg);
        assert_eq!(cfg.device, InputDevice::Keyboard, "left toggles back");
        s.update(confirm(), &mut cfg);
        assert_eq!(
            cfg.device,
            InputDevice::Controller,
            "confirm toggles device"
        );
    }

    #[test]
    fn setup_cpu_difficulty_row_defaults_to_normal_and_steps() {
        // Acceptance #1: the Setup/Options screen exposes a CPU-difficulty
        // selector; default Normal. Acceptance #2: stepping it actually changes
        // the persisted `cpu_difficulty` the next match reads.
        let mut s = SetupScreen::new();
        let mut cfg = config_with_index_keys();
        assert_eq!(
            cfg.cpu_difficulty,
            AiDifficulty::Normal,
            "the selector defaults to Normal"
        );
        // Move from the device row (0) to the CPU-difficulty row (1).
        s.update(down(), &mut cfg);
        assert!(s.on_cpu_difficulty_row());

        // Right steps harder (Normal -> Hard), saturating at Hard.
        s.update(right(), &mut cfg);
        assert_eq!(cfg.cpu_difficulty, AiDifficulty::Hard, "right steps harder");
        s.update(right(), &mut cfg);
        assert_eq!(cfg.cpu_difficulty, AiDifficulty::Hard, "saturates at Hard");

        // Left steps easier (Hard -> Normal -> Easy), saturating at Easy.
        s.update(left(), &mut cfg);
        assert_eq!(
            cfg.cpu_difficulty,
            AiDifficulty::Normal,
            "left steps easier"
        );
        s.update(left(), &mut cfg);
        assert_eq!(cfg.cpu_difficulty, AiDifficulty::Easy);
        s.update(left(), &mut cfg);
        assert_eq!(cfg.cpu_difficulty, AiDifficulty::Easy, "saturates at Easy");

        // Confirm cycles harder, wrapping Hard -> Easy so it stays reachable with
        // a single confirm key.
        s.update(confirm(), &mut cfg); // Easy -> Normal
        assert_eq!(cfg.cpu_difficulty, AiDifficulty::Normal);
        s.update(confirm(), &mut cfg); // Normal -> Hard
        assert_eq!(cfg.cpu_difficulty, AiDifficulty::Hard);
        s.update(confirm(), &mut cfg); // Hard -> Easy (wrap)
        assert_eq!(
            cfg.cpu_difficulty,
            AiDifficulty::Easy,
            "confirm wraps to Easy"
        );
    }

    #[test]
    fn setup_cpu_mode_row_defaults_to_ladder_and_cycles() {
        // T070 acceptance: the Setup/Options screen exposes a CPU teaching-mode
        // selector (defaulting to Ladder), and stepping it changes the persisted
        // `cpu_mode` the next match's CpuAi reads — so the three teaching modes are
        // reachable from the menu, not just dead code.
        let mut s = SetupScreen::new();
        let mut cfg = config_with_index_keys();
        assert_eq!(
            cfg.cpu_mode,
            BehaviorMode::Ladder,
            "the selector defaults to the plain difficulty ladder"
        );
        // Move from device(0) -> CpuDifficulty(1) -> CpuMode(2).
        s.update(down(), &mut cfg);
        s.update(down(), &mut cfg);
        assert!(s.on_cpu_mode_row());
        assert_eq!(
            s.selected_action(),
            None,
            "the CPU-mode row is not an action"
        );

        // Right cycles forward through every teaching mode and wraps.
        s.update(right(), &mut cfg);
        assert_eq!(cfg.cpu_mode, BehaviorMode::PureBlocker);
        s.update(right(), &mut cfg);
        assert_eq!(cfg.cpu_mode, BehaviorMode::ReactiveDP);
        s.update(right(), &mut cfg);
        assert_eq!(cfg.cpu_mode, BehaviorMode::WhiffPunisher);
        s.update(right(), &mut cfg);
        assert_eq!(cfg.cpu_mode, BehaviorMode::Ladder, "right wraps to Ladder");

        // Left cycles the other way and wraps.
        s.update(left(), &mut cfg);
        assert_eq!(cfg.cpu_mode, BehaviorMode::WhiffPunisher, "left wraps back");

        // Confirm also steps forward (so the selector is reachable confirm-only).
        s.update(confirm(), &mut cfg);
        assert_eq!(cfg.cpu_mode, BehaviorMode::Ladder);
    }

    #[test]
    fn setup_cpu_mode_row_kind_is_exposed() {
        // The renderer walks `row_kinds()`; the CPU teaching-mode row must surface
        // as its own kind so it can be labelled distinctly (T070).
        let s = SetupScreen::new();
        let kinds = s.row_kinds();
        assert_eq!(kinds[2], SetupRowKind::CpuMode);
    }

    #[test]
    fn setup_cpu_difficulty_row_kind_is_exposed() {
        // The renderer walks `row_kinds()`; the CPU-difficulty row must surface as
        // its own kind so it can be labelled distinctly.
        let s = SetupScreen::new();
        let kinds = s.row_kinds();
        assert_eq!(kinds[0], SetupRowKind::Device);
        assert_eq!(kinds[1], SetupRowKind::CpuDifficulty);
        assert_eq!(kinds[2], SetupRowKind::CpuMode);
        assert_eq!(kinds[3], SetupRowKind::HudCustomize);
    }

    #[test]
    fn setup_back_exits_to_title() {
        let mut s = SetupScreen::new();
        let mut cfg = config_with_index_keys();
        assert_eq!(s.update(back(), &mut cfg), SetupOutcome::Exit);
    }

    #[test]
    fn setup_confirm_on_action_arms_capture_then_key_rebinds() {
        // Acceptance #3: after remapping a key, the resolved in-match binding for
        // that action changes. Navigate to the `A` action, confirm to arm
        // capture, then press a fresh key and assert the binding moved.
        let mut s = SetupScreen::new();
        let mut cfg = config_with_index_keys();

        // Walk down to the `A` action row: device(0), CpuDifficulty(1),
        // CpuMode(2), HudCustomize(3), Up(4), Down(5), Left(6), Right(7), A(8).
        for _ in 0..8 {
            s.update(down(), &mut cfg);
        }
        assert_eq!(s.selected_action(), Some(InputAction::A));

        let before = cfg.key_for(InputAction::A).unwrap();
        // Confirm arms capture mode (no rebind yet).
        assert_eq!(s.update(confirm(), &mut cfg), SetupOutcome::Pending);
        assert!(s.awaiting_key(), "confirm on an action arms key capture");
        assert_eq!(cfg.key_for(InputAction::A), Some(before), "not yet rebound");

        // A fresh, previously-unbound key.
        let fresh = KeyCode(9999);
        let (rebound, displaced) = s.capture_key(fresh, &mut cfg).expect("captured");
        assert_eq!(rebound, InputAction::A);
        assert_eq!(displaced, None, "the fresh key was not bound elsewhere");
        assert!(!s.awaiting_key(), "capture mode clears after binding");
        assert_eq!(
            cfg.key_for(InputAction::A),
            Some(fresh),
            "the in-match binding for A now resolves to the remapped key"
        );
        assert_ne!(before, fresh, "the binding actually changed");
    }

    #[test]
    fn setup_capture_ignores_navigation_until_key_or_cancel() {
        let mut s = SetupScreen::new();
        let mut cfg = config_with_index_keys();
        // Arm capture on the Up action (row 4: device(0), CpuDifficulty(1),
        // CpuMode(2), HudCustomize(3), Up(4)).
        s.update(down(), &mut cfg);
        s.update(down(), &mut cfg);
        s.update(down(), &mut cfg);
        s.update(down(), &mut cfg);
        assert_eq!(s.selected_action(), Some(InputAction::Up));
        s.update(confirm(), &mut cfg);
        assert!(s.awaiting_key());

        // Navigation is suspended while capturing: the cursor must not move.
        s.update(down(), &mut cfg);
        s.update(up(), &mut cfg);
        assert_eq!(s.cursor, 4, "cursor frozen during capture");
        assert!(s.awaiting_key(), "still capturing");

        // Back cancels the capture without binding and without leaving the screen.
        let before = cfg.key_for(InputAction::Up);
        assert_eq!(s.update(back(), &mut cfg), SetupOutcome::Pending);
        assert!(!s.awaiting_key(), "back cancels capture");
        assert_eq!(cfg.key_for(InputAction::Up), before, "no rebind on cancel");
    }

    #[test]
    fn capture_key_when_not_armed_is_a_noop() {
        let mut s = SetupScreen::new();
        let mut cfg = config_with_index_keys();
        assert!(!s.awaiting_key());
        assert_eq!(s.capture_key(KeyCode(42), &mut cfg), None);
    }

    #[test]
    fn rebind_reports_a_displaced_action_on_a_key_clash() {
        // Binding the `B` action to the key already held by `A` reports A as the
        // displaced action, so the app can keep keys unique if it chooses.
        let mut cfg = config_with_index_keys();
        let a_key = cfg.key_for(InputAction::A).unwrap();
        let displaced = cfg.rebind(InputAction::B, a_key);
        assert_eq!(displaced, Some(InputAction::A));
        assert_eq!(cfg.key_for(InputAction::B), Some(a_key));
    }

    #[test]
    fn input_config_default_with_seeds_every_action() {
        let cfg = config_with_index_keys();
        for action in InputAction::ALL {
            assert!(
                cfg.key_for(action).is_some(),
                "every action has a default binding: {action:?}"
            );
        }
        assert_eq!(cfg.device, InputDevice::Keyboard);
    }

    // ---- HUD-customization screen (T046) --------------------------------

    #[test]
    fn setup_has_a_hud_customization_row_reachable_from_options() {
        // Acceptance #2: the HUD-customization screen is reachable from the
        // setup/options screen. Row 3 is the HUD row; confirming it opens it.
        let mut s = SetupScreen::new();
        let mut cfg = config_with_index_keys();
        s.update(down(), &mut cfg); // device(0) -> CpuDifficulty(1)
        s.update(down(), &mut cfg); // CpuDifficulty(1) -> CpuMode(2)
        s.update(down(), &mut cfg); // CpuMode(2) -> HudCustomize(3)
        assert!(s.on_hud_row());
        assert_eq!(
            s.update(confirm(), &mut cfg),
            SetupOutcome::OpenHudCustomize,
            "confirming the HUD row opens the HUD-customization screen"
        );
        // The other rows do NOT open it (regression: device toggle still toggles).
        let mut s2 = SetupScreen::new();
        assert_ne!(
            s2.update(confirm(), &mut cfg),
            SetupOutcome::OpenHudCustomize,
            "confirming the device row toggles the device, not the HUD screen"
        );
    }

    #[test]
    fn hud_screen_starts_on_life_color_and_lists_every_element() {
        let s = HudCustomizeScreen::new();
        assert_eq!(s.cursor, 0);
        assert_eq!(s.selected_row(), Some(HudRow::LifeColor));
        // Two color rows + one per HudElement.
        assert_eq!(s.row_count(), 2 + HudElement::ALL.len());
        // Each element has exactly one visibility row.
        for e in HudElement::ALL {
            assert!(
                s.rows().contains(&HudRow::Visibility(e)),
                "{e:?} has a visibility row"
            );
        }
    }

    #[test]
    fn hud_screen_cursor_moves_and_wraps() {
        let mut s = HudCustomizeScreen::new();
        let mut cfg = HudConfig::default();
        let last = s.row_count() - 1;
        assert_eq!(s.update(up(), &mut cfg), HudCustomizeOutcome::Pending);
        assert_eq!(s.cursor, last, "up from the first row wraps to the last");
        s.update(down(), &mut cfg);
        assert_eq!(s.cursor, 0, "down from the last row wraps to the first");
    }

    #[test]
    fn hud_screen_back_exits_to_setup() {
        let mut s = HudCustomizeScreen::new();
        let mut cfg = HudConfig::default();
        assert_eq!(s.update(back(), &mut cfg), HudCustomizeOutcome::Exit);
    }

    #[test]
    fn hud_screen_cycles_life_bar_color_into_the_config_the_renderer_reads() {
        // Acceptance #2/#3: changing the life-bar color on the screen is reflected
        // in the HudConfig the renderer reads. The default is the neutral no-op
        // color; confirming on the life-color row steps to the next preset (red).
        let mut s = HudCustomizeScreen::new();
        let mut cfg = HudConfig::default();
        assert!(cfg.is_default(), "starts as the no-op config");
        assert_eq!(s.selected_row(), Some(HudRow::LifeColor));

        assert_eq!(s.update(confirm(), &mut cfg), HudCustomizeOutcome::Pending);
        assert_eq!(
            cfg.life_color(),
            BarColor::RED,
            "the life-bar color advanced to the next preset"
        );
        assert!(!cfg.is_default(), "the config now carries an override");
        // The renderer reads this exact color; a real (non-neutral) tint applies.
        assert!(!fp_ui::bar_tint_palfx(cfg.life_color()).is_identity());
        // Power bar is untouched by editing the life-bar row.
        assert!(cfg.power_color().is_neutral());
    }

    #[test]
    fn hud_screen_left_right_also_cycle_color() {
        let mut s = HudCustomizeScreen::new();
        let mut cfg = HudConfig::default();
        // Right steps the life color forward.
        s.update(right(), &mut cfg);
        assert_eq!(cfg.life_color(), BarColor::RED);
        // Left also advances (the screen cycles in one direction for simplicity).
        s.update(left(), &mut cfg);
        assert_eq!(cfg.life_color(), BarColor::GREEN);
    }

    #[test]
    fn hud_screen_toggles_an_element_visibility() {
        // Acceptance #2/#3: toggling an element on the screen flips its visibility
        // in the HudConfig the renderer reads.
        let mut s = HudCustomizeScreen::new();
        let mut cfg = HudConfig::default();
        // Walk to the POWER visibility row: LifeColor(0), PowerColor(1),
        // Visibility(Life)(2), Visibility(Power)(3).
        for _ in 0..3 {
            s.update(down(), &mut cfg);
        }
        assert_eq!(
            s.selected_row(),
            Some(HudRow::Visibility(HudElement::Power))
        );
        assert!(cfg.is_visible(HudElement::Power), "visible by default");
        s.update(confirm(), &mut cfg);
        assert!(
            !cfg.is_visible(HudElement::Power),
            "the renderer now hides the power bar"
        );
        // Other elements stay visible.
        assert!(cfg.is_visible(HudElement::Life));
        // Toggling back restores the no-op config.
        s.update(confirm(), &mut cfg);
        assert!(cfg.is_visible(HudElement::Power));
        assert!(cfg.is_default());
    }

    #[test]
    fn setup_to_hud_screen_round_trip_drives_a_change() {
        // End-to-end (pure): from the setup screen, open the HUD-customization
        // screen, change a value, and confirm the change lives in the HudConfig
        // the renderer consumes — then exit back to setup.
        let mut setup = SetupScreen::new();
        let mut input_cfg = config_with_index_keys();
        let mut hud_cfg = HudConfig::default();

        // Open the HUD screen from setup: device(0) -> CpuDifficulty(1) ->
        // CpuMode(2) -> HudCustomize(3).
        setup.update(down(), &mut input_cfg); // -> CPU-difficulty row
        setup.update(down(), &mut input_cfg); // -> CPU teaching-mode row
        setup.update(down(), &mut input_cfg); // -> HUD row
        assert_eq!(
            setup.update(confirm(), &mut input_cfg),
            SetupOutcome::OpenHudCustomize
        );

        // Edit the HUD config on the now-open screen.
        let mut hud = HudCustomizeScreen::new();
        hud.update(confirm(), &mut hud_cfg); // cycle life color off neutral
        assert_eq!(hud_cfg.life_color(), BarColor::RED);

        // Back returns to setup; the change persists in the shared config.
        assert_eq!(hud.update(back(), &mut hud_cfg), HudCustomizeOutcome::Exit);
        assert_eq!(hud_cfg.life_color(), BarColor::RED);
        assert!(!hud_cfg.is_default());
    }

    #[test]
    fn hud_row_labels_are_present() {
        assert_eq!(HudRow::LifeColor.label(), "LIFE BAR COLOR");
        assert_eq!(HudRow::PowerColor.label(), "POWER BAR COLOR");
        assert_eq!(HudRow::Visibility(HudElement::Combo).label(), "COMBO");
    }
}
