//! In-app screen state machine: Title menu -> Character-Select -> Fight -> Title.
//!
//! This module owns the **pure** menu/cursor/transition logic for the app's
//! out-of-fight flow (the [`Screen`] state machine, the title menu and select
//! grid navigation, and the roster-pick -> which-`.def`-to-load decision), kept
//! free of SDL2 and the GPU so it is unit-testable headlessly. The SDL2 window,
//! 60Hz accumulator loop, and GPU rendering that drives it live in `main.rs`.
//!
//! The flow:
//! - **Title** ([`TitleMenu`]) renders the enabled motif menu items as text with
//!   a highlighted cursor. `VS MODE` -> Select (both players pick); `TRAINING`
//!   -> Select (P1 picks, P2 mirrors); `EXIT`/quit leaves the app. A missing
//!   motif falls back to a built-in minimal menu (`VS` / `TRAINING` / `EXIT`).
//! - **Select** ([`SelectScreen`]) renders the `select.def` roster as a text
//!   grid with a P1 cursor (and a P2 cursor in VS). Confirming P1 (then P2 in
//!   VS) yields a [`MatchPick`] naming the character `.def`(s) to load.
//! - **Fight** runs the existing two-player [`fp_engine::Match`]; on match-over
//!   it returns to Title.
//!
//! Nothing here panics: a missing motif/roster degrades to a built-in fallback,
//! an empty roster yields no pick (the caller stays on Title), and
//! `RandomSelect` is resolved deterministically against a caller-supplied seed.

use std::path::{Path, PathBuf};

use fp_ui::{MenuItemKind, RosterEntry, SelectDef, SelectInfo, SelectSlot, SystemDef};

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
    /// Quit the application.
    Quit,
    /// A recognised-but-unimplemented item (Arcade/Options/...): selectable but a
    /// no-op (stays on the title screen) so the menu still reads completely.
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
    /// - `Exit` -> Quit,
    /// - everything else (Arcade, Survival, Watch, Options, ...) -> a selectable
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
    /// available: `VS MODE` / `TRAINING` / `EXIT`.
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
        MenuItemKind::Exit => TitleAction::Quit,
        // Recognised but not yet implemented: keep them visible but inert.
        MenuItemKind::Arcade
        | MenuItemKind::TeamArcade
        | MenuItemKind::TeamCoop
        | MenuItemKind::Survival
        | MenuItemKind::SurvivalCoop
        | MenuItemKind::Watch
        | MenuItemKind::Options => TitleAction::NoOp,
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
        // Fallback ships VS / TRAINING / EXIT.
        assert_eq!(menu.entries.len(), 3);
        assert_eq!(menu.entries[0].label, "VS MODE");
        assert_eq!(menu.entries[2].action, TitleAction::Quit);
    }

    #[test]
    fn title_cursor_moves_and_wraps() {
        let mut menu = TitleMenu::fallback(); // 3 entries
        assert_eq!(menu.cursor, 0);
        menu.update(down());
        assert_eq!(menu.cursor, 1);
        menu.update(down());
        assert_eq!(menu.cursor, 2);
        menu.update(down());
        assert_eq!(menu.cursor, 0, "down wraps from last to first");
        menu.update(up());
        assert_eq!(menu.cursor, 2, "up wraps from first to last");
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
}
