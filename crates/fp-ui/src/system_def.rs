//! Typed `system.def` motif model + parser.
//!
//! A MUGEN motif's `system.def` is the INI-style file describing the engine's
//! **out-of-fight** screens: the title-screen main menu, the character-select
//! grid, and the versus screen, plus the shared file references (sprite/sound
//! data, fonts, the `select.def` filename, intro/logo storyboards).
//!
//! This module parses that file — via the shared [`fp_formats::def::DefFile`]
//! INI reader, so it inherits BOM/CRLF tolerance, `;` comment stripping, and the
//! split-on-first-`=` rule — into a typed [`SystemDef`]. Parsing is pure (no GPU,
//! no I/O beyond the `DefFile` read) and **never panics**: unknown keys/sections
//! are ignored, a malformed value falls back to a sensible default with a
//! `tracing::warn!`, and a missing section yields that section's defaults.
//!
//! It mirrors the [`crate::screenpack`] (`fight.def`) parser pattern: a flat set
//! of `parse_*` helpers reading scalar/pair values off the `DefFile`.
//!
//! # Sections modelled
//!
//! - `[Info]` — motif `name` / `author`.
//! - `[Files]` — `spr` / `snd` data, `font1..fontN` font paths, the `select`
//!   (`select.def`) filename, the `fight` (`fight.def`) filename, and the
//!   `logo.storyboard` / `intro.storyboard` references.
//! - `[Title Info]` — the title-screen main menu: `menu.pos`, the item /
//!   active-item fonts, item spacing, the visible-item window count, and the
//!   `menu.itemname.<id>` entries parsed in MUGEN canonical order (each enabled
//!   iff its value is a non-empty quoted string).
//! - `[Select Info]` — the character-select grid geometry (`rows`, `columns`,
//!   `pos`, `cell.size`, `cell.spacing`), the per-player cursor start cells +
//!   move/done sounds, and the portrait offset.
//! - `[VS Screen]` — the per-player portrait positions and name text placements.

use fp_formats::def::DefFile;

use crate::screenpack::Pos;

/// The canonical MUGEN title-menu items, in the fixed order MUGEN lists them.
///
/// A motif enables an item by giving its `menu.itemname.<id>` key a non-empty
/// quoted string; the order here is the order the items appear in the on-screen
/// menu, regardless of the order the keys appear in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItemKind {
    /// Single-player arcade ladder.
    Arcade,
    /// 1-vs-1 versus.
    Versus,
    /// Team arcade ladder.
    TeamArcade,
    /// Team versus.
    TeamVersus,
    /// Team co-op.
    TeamCoop,
    /// Survival ladder.
    Survival,
    /// Survival co-op.
    SurvivalCoop,
    /// Training mode.
    Training,
    /// Watch (AI-vs-AI) mode.
    Watch,
    /// Options screen.
    Options,
    /// Exit the engine.
    Exit,
}

impl MenuItemKind {
    /// The `menu.itemname.<key>` suffix MUGEN uses for this item.
    pub const fn key(self) -> &'static str {
        match self {
            MenuItemKind::Arcade => "arcade",
            MenuItemKind::Versus => "versus",
            MenuItemKind::TeamArcade => "teamarcade",
            MenuItemKind::TeamVersus => "teamversus",
            MenuItemKind::TeamCoop => "teamcoop",
            MenuItemKind::Survival => "survival",
            MenuItemKind::SurvivalCoop => "survivalcoop",
            MenuItemKind::Training => "training",
            MenuItemKind::Watch => "watch",
            MenuItemKind::Options => "options",
            MenuItemKind::Exit => "exit",
        }
    }

    /// All title-menu kinds, in MUGEN canonical display order.
    pub const ALL: [MenuItemKind; 11] = [
        MenuItemKind::Arcade,
        MenuItemKind::Versus,
        MenuItemKind::TeamArcade,
        MenuItemKind::TeamVersus,
        MenuItemKind::TeamCoop,
        MenuItemKind::Survival,
        MenuItemKind::SurvivalCoop,
        MenuItemKind::Training,
        MenuItemKind::Watch,
        MenuItemKind::Options,
        MenuItemKind::Exit,
    ];
}

/// One title-menu item: its [`MenuItemKind`] and the on-screen display label.
///
/// Only *enabled* items (those whose `menu.itemname.<id>` value was a non-empty
/// quoted string) appear in [`TitleInfo::items`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    /// Which canonical menu action this item triggers.
    pub kind: MenuItemKind,
    /// The display label shown on the menu (the text inside the quotes).
    pub label: String,
}

/// The title-screen `[Title Info]` layout: the main-menu geometry and the
/// ordered list of enabled menu items.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TitleInfo {
    /// Anchor position the menu is drawn from (`menu.pos`).
    pub menu_pos: Pos,
    /// Font slot for inactive menu items (first int of `menu.item.font`).
    pub item_font: usize,
    /// Font slot for the highlighted item (first int of `menu.item.active.font`).
    pub item_active_font: usize,
    /// `(dx, dy)` spacing between successive items (`menu.item.spacing`).
    pub item_spacing: Pos,
    /// How many items are visible in the scroll window
    /// (`menu.window.visibleitems`). `0` if unspecified.
    pub window_visible_items: u32,
    /// The enabled menu items, in MUGEN canonical order.
    pub items: Vec<MenuItem>,
}

/// One player's cursor configuration on the select grid: the cell the cursor
/// starts on and its move / done sounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CursorSide {
    /// `(column, row)` cell the cursor starts on (`pN.cursor.startcell`).
    pub start_cell: (i32, i32),
    /// `(group, sample)` cursor-move sound, if authored.
    pub move_snd: Option<(i32, i32)>,
    /// `(group, sample)` cursor-confirm sound, if authored.
    pub done_snd: Option<(i32, i32)>,
}

/// The `[Select Info]` character-select grid geometry + cursors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SelectInfo {
    /// Number of grid rows.
    pub rows: u32,
    /// Number of grid columns.
    pub columns: u32,
    /// Top-left anchor position the grid is drawn from (`pos`).
    pub pos: Pos,
    /// `(w, h)` pixel size of each cell (`cell.size`).
    pub cell_size: Pos,
    /// `(dx, dy)` spacing between cells (`cell.spacing`). MUGEN allows a single
    /// scalar, which is taken as both x and y.
    pub cell_spacing: Pos,
    /// P1's cursor configuration.
    pub p1_cursor: CursorSide,
    /// P2's cursor configuration.
    pub p2_cursor: CursorSide,
    /// Small-portrait offset within a cell (`portrait.offset`).
    pub portrait_offset: Pos,
}

/// One player's placement on the `[VS Screen]`: portrait position + name text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VsSide {
    /// Big-portrait anchor position (`pN.pos`).
    pub pos: Pos,
    /// Name-text anchor position (`pN.name.pos`).
    pub name_pos: Pos,
    /// Font slot for the name text (first int of `pN.name.font`).
    pub name_font: usize,
}

/// The versus-screen `[VS Screen]` layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VsScreen {
    /// P1's side.
    pub p1: VsSide,
    /// P2's side.
    pub p2: VsSide,
}

/// A fully parsed `system.def` motif.
///
/// Build one with [`SystemDef::parse`] from a [`DefFile`]. Every field carries a
/// [`Default`], so a partial or malformed `system.def` still yields a usable (if
/// sparse) model.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SystemDef {
    /// Motif display name (`[Info] name`).
    pub name: String,
    /// Motif author (`[Info] author`).
    pub author: String,
    /// `[Files] spr` sprite-data filename.
    pub spr: String,
    /// `[Files] snd` sound-data filename.
    pub snd: String,
    /// Font paths from `[Files] font1..fontN`, in slot order. `fonts[0]` is
    /// `font1` (MUGEN system fonts are 1-indexed). Gaps fill with empty strings
    /// so indices stay stable.
    pub fonts: Vec<String>,
    /// `[Files] select` — the `select.def` filename.
    pub select_file: String,
    /// `[Files] fight` — the `fight.def` screenpack filename.
    pub fight_file: String,
    /// `[Files] logo.storyboard` — optional logo storyboard `.def`.
    pub logo_storyboard: String,
    /// `[Files] intro.storyboard` — optional intro storyboard `.def`.
    pub intro_storyboard: String,
    /// The title-screen menu layout.
    pub title: TitleInfo,
    /// The character-select grid geometry.
    pub select_info: SelectInfo,
    /// The versus-screen layout.
    pub vs_screen: VsScreen,
}

impl SystemDef {
    /// Parses a [`DefFile`] (already read from a `system.def`) into a typed model.
    ///
    /// Tolerant by design: unknown sections/keys are ignored, malformed numeric
    /// values fall back to defaults (with a `tracing::warn!`), and absent
    /// sections yield that section's [`Default`]. Never panics.
    pub fn parse(def: &DefFile) -> Self {
        Self {
            name: def.get("Info", "name").unwrap_or("").to_string(),
            author: def.get("Info", "author").unwrap_or("").to_string(),
            spr: def.get("Files", "spr").unwrap_or("").to_string(),
            snd: def.get("Files", "snd").unwrap_or("").to_string(),
            fonts: parse_fonts(def),
            select_file: def.get("Files", "select").unwrap_or("").to_string(),
            fight_file: def.get("Files", "fight").unwrap_or("").to_string(),
            logo_storyboard: def
                .get("Files", "logo.storyboard")
                .unwrap_or("")
                .to_string(),
            intro_storyboard: def
                .get("Files", "intro.storyboard")
                .unwrap_or("")
                .to_string(),
            title: parse_title_info(def),
            select_info: parse_select_info(def),
            vs_screen: parse_vs_screen(def),
        }
    }
}

/// Collects `[Files] font1..fontN` into a dense, slot-indexed vector.
///
/// MUGEN system fonts are 1-indexed (`font1` is the first), and slots are
/// contiguous; collection stops at the first missing `fontN`.
fn parse_fonts(def: &DefFile) -> Vec<String> {
    let mut fonts = Vec::new();
    let mut n = 1;
    loop {
        let key = format!("font{n}");
        match def.get("Files", &key) {
            Some(path) if !path.is_empty() => {
                fonts.push(path.to_string());
                n += 1;
            }
            _ => break,
        }
    }
    fonts
}

/// Parses `[Title Info]`.
fn parse_title_info(def: &DefFile) -> TitleInfo {
    TitleInfo {
        menu_pos: parse_pos(def, "Title Info", "menu.pos").unwrap_or_default(),
        item_font: parse_font_slot(def, "Title Info", "menu.item.font"),
        item_active_font: parse_font_slot(def, "Title Info", "menu.item.active.font"),
        item_spacing: parse_pos(def, "Title Info", "menu.item.spacing").unwrap_or_default(),
        window_visible_items: parse_u32(def, "Title Info", "menu.window.visibleitems"),
        items: parse_menu_items(def),
    }
}

/// Collects the enabled `menu.itemname.<id>` entries in MUGEN canonical order.
///
/// An item is enabled iff its value (after the `DefFile` quote-stripping) is a
/// non-empty string — i.e. the author wrote `"LABEL"`, not `""` and not a
/// missing/blank key. The label is that quoted text.
fn parse_menu_items(def: &DefFile) -> Vec<MenuItem> {
    let mut items = Vec::new();
    for kind in MenuItemKind::ALL {
        let key = format!("menu.itemname.{}", kind.key());
        if let Some(raw) = def.get("Title Info", &key) {
            // `DefFile` already stripped surrounding quotes, so a quoted "" or a
            // bare empty value both arrive as "" here -> disabled. Any other
            // (non-empty) text is an enabled item.
            if !raw.is_empty() {
                items.push(MenuItem {
                    kind,
                    label: raw.to_string(),
                });
            }
        }
    }
    items
}

/// Parses `[Select Info]` grid geometry + cursors.
fn parse_select_info(def: &DefFile) -> SelectInfo {
    SelectInfo {
        rows: parse_u32(def, "Select Info", "rows"),
        columns: parse_u32(def, "Select Info", "columns"),
        pos: parse_pos(def, "Select Info", "pos").unwrap_or_default(),
        cell_size: parse_pos(def, "Select Info", "cell.size").unwrap_or_default(),
        cell_spacing: parse_pos_or_scalar(def, "Select Info", "cell.spacing"),
        p1_cursor: parse_cursor_side(def, "p1"),
        p2_cursor: parse_cursor_side(def, "p2"),
        portrait_offset: parse_pos(def, "Select Info", "portrait.offset").unwrap_or_default(),
    }
}

/// Parses one player's `[Select Info]` cursor config.
fn parse_cursor_side(def: &DefFile, side: &str) -> CursorSide {
    CursorSide {
        start_cell: parse_pos(def, "Select Info", &format!("{side}.cursor.startcell"))
            .map(|p| (p.x, p.y))
            .unwrap_or((0, 0)),
        move_snd: parse_int_pair_opt(def, "Select Info", &format!("{side}.cursor.move.snd")),
        done_snd: parse_int_pair_opt(def, "Select Info", &format!("{side}.cursor.done.snd")),
    }
}

/// Parses `[VS Screen]`.
fn parse_vs_screen(def: &DefFile) -> VsScreen {
    VsScreen {
        p1: parse_vs_side(def, "p1"),
        p2: parse_vs_side(def, "p2"),
    }
}

/// Parses one player's `[VS Screen]` side.
fn parse_vs_side(def: &DefFile, side: &str) -> VsSide {
    VsSide {
        pos: parse_pos(def, "VS Screen", &format!("{side}.pos")).unwrap_or_default(),
        name_pos: parse_pos(def, "VS Screen", &format!("{side}.name.pos")).unwrap_or_default(),
        name_font: parse_font_slot(def, "VS Screen", &format!("{side}.name.font")),
    }
}

/// Parses a `[section] key = x, y` position pair into a [`Pos`].
///
/// Returns `None` if the key is absent. A present-but-malformed value warns and
/// returns `None` (the caller substitutes a default).
fn parse_pos(def: &DefFile, section: &str, key: &str) -> Option<Pos> {
    let raw = def.get(section, key)?;
    match parse_int_pair(raw) {
        Some((x, y)) => Some(Pos::new(x, y)),
        None => {
            tracing::warn!(
                section,
                key,
                raw,
                "system.def: malformed position; ignoring"
            );
            None
        }
    }
}

/// Parses a `[section] key` value that may be either a `x, y` pair or a single
/// scalar (MUGEN's `cell.spacing` may be written as one number meaning both
/// axes). Defaults to `(0, 0)`.
fn parse_pos_or_scalar(def: &DefFile, section: &str, key: &str) -> Pos {
    match def.get(section, key) {
        Some(raw) => {
            let mut it = int_tokens(raw);
            match (it.next(), it.next()) {
                (Some(x), Some(y)) => Pos::new(x, y),
                (Some(x), None) => Pos::new(x, x),
                _ => {
                    tracing::warn!(
                        section,
                        key,
                        raw,
                        "system.def: malformed spacing; defaulting"
                    );
                    Pos::default()
                }
            }
        }
        None => Pos::default(),
    }
}

/// Reads a `[section] key = N` font-slot index (first int), defaulting to `0`.
fn parse_font_slot(def: &DefFile, section: &str, key: &str) -> usize {
    match def.get(section, key) {
        Some(raw) => first_int(raw).map(|n| n.max(0) as usize).unwrap_or(0),
        None => 0,
    }
}

/// Reads a `[section] key = N` non-negative count, defaulting to `0` when absent
/// or malformed.
fn parse_u32(def: &DefFile, section: &str, key: &str) -> u32 {
    match def.get(section, key) {
        Some(raw) => first_int(raw).map(|n| n.max(0) as u32).unwrap_or(0),
        None => 0,
    }
}

/// Reads a `[section] key = a, b` int pair, returning `None` if absent or
/// malformed (warning on a present-but-malformed value).
fn parse_int_pair_opt(def: &DefFile, section: &str, key: &str) -> Option<(i32, i32)> {
    let raw = def.get(section, key)?;
    match parse_int_pair(raw) {
        Some(p) => Some(p),
        None => {
            tracing::warn!(
                section,
                key,
                raw,
                "system.def: malformed int pair; ignoring"
            );
            None
        }
    }
}

/// Parses a two-integer pair from a comma/whitespace-separated value.
fn parse_int_pair(s: &str) -> Option<(i32, i32)> {
    let mut it = int_tokens(s);
    let a = it.next()?;
    let b = it.next()?;
    Some((a, b))
}

/// The first integer in a comma/whitespace-separated value, or `None`.
fn first_int(s: &str) -> Option<i32> {
    int_tokens(s).next()
}

/// Tokenises `s` on commas/whitespace and parses each token as an `i32`,
/// skipping non-numeric tokens.
fn int_tokens(s: &str) -> impl Iterator<Item = i32> + '_ {
    s.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse::<i32>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
; A synthetic system.def motif
[Info]
name = "Test Motif"
author = "FP Tests"

[Files]
spr = system.sff
snd = system.snd
select = select.def
fight = fight.def
logo.storyboard = logo.def
intro.storyboard = intro.def
font1 = font/f-4x6.fnt
font2 = font/f-6x9.fnt
font3 = font/jg.fnt

[Title Info]
menu.pos = 159,157
menu.item.font = 3,0,0
menu.item.active.font = 3,5,0
menu.item.spacing = 0, 13
menu.window.visibleitems = 5
menu.itemname.arcade = "ARCADE"
menu.itemname.versus = "VS MODE"
menu.itemname.teamarcade = ""
menu.itemname.training = "TRAINING"
menu.itemname.options = "OPTIONS"
menu.itemname.exit = "EXIT"

[Select Info]
rows = 2
columns = 5
pos = 90,170
cell.size = 27,27
cell.spacing = 2
p1.cursor.startcell = 0,0
p1.cursor.move.snd = 100,0
p1.cursor.done.snd = 100,1
p2.cursor.startcell = 1,4
p2.cursor.move.snd = 100,0
p2.cursor.done.snd = 100,1
portrait.offset = 0,0

[VS Screen]
p1.pos = 20,31
p1.name.pos = 78,190
p1.name.font = 3,0,0
p2.pos = 299,31
p2.name.pos = 241,190
p2.name.font = 3,0,0

[UnknownSection]
mystery.key = ignored
"#;

    fn sample() -> SystemDef {
        let def = DefFile::from_str(SAMPLE).unwrap();
        SystemDef::parse(&def)
    }

    #[test]
    fn parses_info_and_files() {
        let s = sample();
        assert_eq!(s.name, "Test Motif");
        assert_eq!(s.author, "FP Tests");
        assert_eq!(s.spr, "system.sff");
        assert_eq!(s.snd, "system.snd");
        assert_eq!(s.select_file, "select.def");
        assert_eq!(s.fight_file, "fight.def");
        assert_eq!(s.logo_storyboard, "logo.def");
        assert_eq!(s.intro_storyboard, "intro.def");
        assert_eq!(
            s.fonts,
            vec![
                "font/f-4x6.fnt".to_string(),
                "font/f-6x9.fnt".to_string(),
                "font/jg.fnt".to_string(),
            ]
        );
    }

    #[test]
    fn parses_title_geometry() {
        let t = sample().title;
        assert_eq!(t.menu_pos, Pos::new(159, 157));
        assert_eq!(t.item_font, 3);
        assert_eq!(t.item_active_font, 3);
        assert_eq!(t.item_spacing, Pos::new(0, 13));
        assert_eq!(t.window_visible_items, 5);
    }

    #[test]
    fn parses_menu_items_in_canonical_order_skipping_empty() {
        let t = sample().title;
        // teamarcade is "" -> disabled. arcade/versus/training/options/exit
        // enabled, and they come back in canonical order regardless of the file
        // order (arcade, versus, training, options, exit).
        let kinds: Vec<_> = t.items.iter().map(|i| i.kind).collect();
        assert_eq!(
            kinds,
            vec![
                MenuItemKind::Arcade,
                MenuItemKind::Versus,
                MenuItemKind::Training,
                MenuItemKind::Options,
                MenuItemKind::Exit,
            ]
        );
        assert_eq!(t.items[0].label, "ARCADE");
        assert_eq!(t.items[1].label, "VS MODE");
        assert_eq!(t.items[2].label, "TRAINING");
    }

    #[test]
    fn quoted_empty_item_is_disabled() {
        let t = sample().title;
        assert!(
            !t.items.iter().any(|i| i.kind == MenuItemKind::TeamArcade),
            "menu.itemname.teamarcade = \"\" must be disabled"
        );
    }

    #[test]
    fn parses_select_grid_geometry() {
        let si = sample().select_info;
        assert_eq!(si.rows, 2);
        assert_eq!(si.columns, 5);
        assert_eq!(si.pos, Pos::new(90, 170));
        assert_eq!(si.cell_size, Pos::new(27, 27));
        // Single scalar spacing -> both axes.
        assert_eq!(si.cell_spacing, Pos::new(2, 2));
        assert_eq!(si.p1_cursor.start_cell, (0, 0));
        assert_eq!(si.p2_cursor.start_cell, (1, 4));
        assert_eq!(si.p1_cursor.move_snd, Some((100, 0)));
        assert_eq!(si.p1_cursor.done_snd, Some((100, 1)));
        assert_eq!(si.portrait_offset, Pos::new(0, 0));
    }

    #[test]
    fn parses_vs_screen() {
        let vs = sample().vs_screen;
        assert_eq!(vs.p1.pos, Pos::new(20, 31));
        assert_eq!(vs.p1.name_pos, Pos::new(78, 190));
        assert_eq!(vs.p1.name_font, 3);
        assert_eq!(vs.p2.pos, Pos::new(299, 31));
        assert_eq!(vs.p2.name_pos, Pos::new(241, 190));
    }

    #[test]
    fn unknown_section_ignored_not_panicked() {
        let s = sample();
        assert_eq!(s.name, "Test Motif");
    }

    #[test]
    fn empty_def_yields_defaults() {
        let def = DefFile::from_str("").unwrap();
        let s = SystemDef::parse(&def);
        assert_eq!(s, SystemDef::default());
        assert!(s.title.items.is_empty());
        assert_eq!(s.select_info.rows, 0);
    }

    #[test]
    fn malformed_values_fall_back() {
        let def = DefFile::from_str(
            "[Title Info]\nmenu.pos = not-a-number\nmenu.itemname.training = \"TRAINING\"\n[Select Info]\nrows = oops\ncolumns = 3\n",
        )
        .unwrap();
        let s = SystemDef::parse(&def);
        assert_eq!(s.title.menu_pos, Pos::default(), "bad pos -> default");
        assert_eq!(s.title.items.len(), 1, "valid item still parses");
        assert_eq!(s.select_info.rows, 0, "bad rows -> default 0");
        assert_eq!(s.select_info.columns, 3, "valid columns still parses");
    }

    #[test]
    fn fonts_stop_at_first_gap() {
        let def =
            DefFile::from_str("[Files]\nfont1 = a.fnt\nfont2 = b.fnt\nfont4 = d.fnt\n").unwrap();
        let s = SystemDef::parse(&def);
        assert_eq!(s.fonts, vec!["a.fnt".to_string(), "b.fnt".to_string()]);
    }
}
