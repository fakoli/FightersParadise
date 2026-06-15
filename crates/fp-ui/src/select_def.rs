//! Typed `select.def` roster model + parser.
//!
//! A MUGEN motif's `select.def` lists the character-select roster: under
//! `[Characters]`, each non-comment line is a character slot, plus an
//! `[ExtraStages]` list and an `[Options]` block. Unlike `system.def`, the
//! roster lines are **not** `key = value` pairs — they are positional,
//! comma-separated records. Two field layouts are accepted:
//!
//! ```text
//! charname [, stagefile] [, key=value ...]            ; MUGEN classic
//! displayname, deffile [, stagefile] [, key=value ...]; explicit-def form
//! ```
//!
//! In the classic MUGEN form (e.g. `kfm, stages/kfm.def`) the first field is the
//! character — a bare `kfm` resolving to `kfm/kfm.def` — and the second is the
//! stage. When the **second** field itself names a `.def` (e.g.
//! `Training Dummy, ../trainingdummy/trainingdummy.def`), the first field is
//! taken as the display name and the second as the explicit def path. This lets
//! one parser handle both the real KFM roster and our shipped default.
//!
//! Because [`fp_formats::def::DefFile`] only captures `key = value` lines (it
//! drops the bare roster rows), this module does its own line-oriented scan —
//! but it follows the same MUGEN text conventions: BOM/CRLF-tolerant,
//! case-insensitive section headers, and `;` / `//` / `#` comment stripping.
//!
//! Special slot tokens are recognised: `randomselect` (the random-pick icon),
//! `blank` / `empty` (an explicit empty cell). Optional trailing `key=value`
//! params capture `order`, `music`, and `includestage`.
//!
//! Parsing is pure and **never panics**: a malformed line is recorded as best it
//! can (an empty slot at worst), never an error.

/// One entry on the character-select grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectSlot {
    /// A real character: a display name + its `.def` path + optional params.
    Character(RosterEntry),
    /// The `randomselect` icon — picks a random character when chosen.
    RandomSelect,
    /// An explicit empty cell (`blank` / `empty`, or a stray empty line that
    /// reached the parser inside `[Characters]`).
    Empty,
}

/// A single roster character parsed from a `[Characters]` line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RosterEntry {
    /// The display name as written in the first field.
    ///
    /// In the classic MUGEN form this is the character folder/name (it also
    /// drives [`def_path`](Self::def_path)); in the explicit-def form it is a
    /// free-text label distinct from the def path.
    pub name: String,
    /// The resolved character `.def` path.
    ///
    /// Either the explicit second field (when it names a `.def`), the first
    /// field verbatim (when it is itself a path/`.def`), or — for a bare
    /// classic-form name `kfm` — `kfm/kfm.def`.
    pub def_path: String,
    /// Optional stage `.def` (the second positional field), if present and not
    /// the literal `random`.
    pub stage: Option<String>,
    /// Optional `order=` priority (arcade ladder ordering). `None` if unset.
    pub order: Option<i32>,
    /// Optional `music=` BGM override path. `None` if unset.
    pub music: Option<String>,
    /// `includestage=` flag: whether this character's stage is offered in VS /
    /// Watch stage selection. MUGEN default is `true` (included).
    pub include_stage: bool,
}

/// A fully parsed `select.def` roster.
///
/// Build one with [`SelectDef::parse`]. Never panics; malformed lines degrade to
/// the closest reasonable slot.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SelectDef {
    /// The character-select grid slots, in file order (including random/empty).
    pub slots: Vec<SelectSlot>,
    /// `[ExtraStages]` stage `.def` paths (available in VS / Watch).
    pub extra_stages: Vec<String>,
    /// `[Options]` `arcade.maxmatches` per-order match caps, if present.
    pub arcade_maxmatches: Vec<i32>,
    /// `[Options]` `team.maxmatches` per-order match caps, if present.
    pub team_maxmatches: Vec<i32>,
}

impl SelectDef {
    /// Parses a `select.def` from raw text.
    ///
    /// Tolerates BOM/CRLF, strips `;` / `//` / `#` comments, and is
    /// case-insensitive on section headers and special tokens. Never panics.
    pub fn parse(text: &str) -> Self {
        let mut out = SelectDef::default();
        let mut section = Section::None;

        // Strip a leading UTF-8 BOM so the first line parses cleanly.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);

        for raw_line in text.lines() {
            let line = strip_comment(raw_line).trim();
            if line.is_empty() {
                continue;
            }

            // Section header [Name]?
            if line.starts_with('[') && line.ends_with(']') {
                let name = line[1..line.len() - 1].trim().to_ascii_lowercase();
                section = Section::from_name(&name);
                continue;
            }

            match section {
                Section::Characters => out.slots.push(parse_slot(line)),
                Section::ExtraStages => out.extra_stages.push(line.to_string()),
                Section::Options => apply_option(&mut out, line),
                Section::None | Section::Other => {}
            }
        }

        out
    }
}

/// Which `select.def` section the scanner is currently inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    None,
    Characters,
    ExtraStages,
    Options,
    Other,
}

impl Section {
    fn from_name(name: &str) -> Self {
        match name {
            "characters" => Section::Characters,
            "extrastages" => Section::ExtraStages,
            "options" => Section::Options,
            _ => Section::Other,
        }
    }
}

/// Parses one `[Characters]` line into a [`SelectSlot`].
fn parse_slot(line: &str) -> SelectSlot {
    // Split into comma fields, trimming each.
    let fields: Vec<&str> = line.split(',').map(str::trim).collect();
    let first = fields.first().copied().unwrap_or("").trim();

    // Special tokens (case-insensitive), only when they stand alone.
    let lower = first.to_ascii_lowercase();
    if lower == "randomselect" {
        return SelectSlot::RandomSelect;
    }
    if lower == "blank" || lower == "empty" {
        return SelectSlot::Empty;
    }
    if first.is_empty() {
        return SelectSlot::Empty;
    }

    let mut entry = RosterEntry {
        name: first.to_string(),
        include_stage: true,
        ..RosterEntry::default()
    };

    // Inspect the second positional field (if it is a real value, not a
    // `key=value` param). It is either an explicit `.def` (explicit-def form,
    // field 1 was a display name) or a stage (classic form, field 1 was the
    // character).
    let mut idx = 1;
    let second = fields
        .get(1)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && !s.contains('='));

    if let Some(second) = second {
        idx = 2;
        if is_explicit_def_form(first, second) {
            // Explicit-def form: field 1 is the display label, field 2 the def.
            entry.def_path = second.to_string();
            // A third non-param field, if any, is the stage.
            if let Some(third) = fields.get(2).map(|s| s.trim()) {
                if !third.is_empty() && !third.contains('=') {
                    if !third.eq_ignore_ascii_case("random") {
                        entry.stage = Some(third.to_string());
                    }
                    idx = 3;
                }
            }
        } else {
            // Classic form: field 1 resolves to the def, field 2 is the stage.
            entry.def_path = resolve_def_path(first);
            if !second.eq_ignore_ascii_case("random") {
                entry.stage = Some(second.to_string());
            }
        }
    } else {
        // Only a character field present.
        entry.def_path = resolve_def_path(first);
    }

    // Remaining fields are `key=value` optional params.
    for &field in &fields[idx.min(fields.len())..] {
        apply_param(&mut entry, field.trim());
    }

    SelectSlot::Character(entry)
}

/// Whether a field already names a `.def` file (ends in `.def`, ignoring case).
fn is_def_path(field: &str) -> bool {
    field.to_ascii_lowercase().ends_with(".def")
}

/// Decides whether `first, second` is the *explicit-def* roster form
/// (`displayname, deffile`) rather than the classic MUGEN form
/// (`charname, stagefile`).
///
/// A MUGEN character reference (folder/filename) never contains whitespace,
/// whereas a human-readable display name like `Training Dummy` does. So we treat
/// a slot as explicit-def iff the first field carries a space (is a label, not a
/// path) **and** the second field names a `.def`. Otherwise it is classic form,
/// where the second field is the stage. This cleanly separates
/// `kfm, stages/kfm.def` (classic) and `boss/boss.def, stages/arena.def`
/// (classic) from `Training Dummy, ../trainingdummy/trainingdummy.def`.
fn is_explicit_def_form(first: &str, second: &str) -> bool {
    first.chars().any(char::is_whitespace) && is_def_path(second)
}

/// Resolves the first roster field into a `.def` path.
///
/// MUGEN treats a bare `kfm` as `kfm/kfm.def`; a field that already contains a
/// path separator or ends in `.def` is taken as-is.
fn resolve_def_path(first: &str) -> String {
    if first.contains('/') || first.contains('\\') || is_def_path(first) {
        first.to_string()
    } else {
        format!("{first}/{first}.def")
    }
}

/// Applies one trailing `key=value` roster param to an entry.
fn apply_param(entry: &mut RosterEntry, field: &str) {
    let Some(eq) = field.find('=') else {
        // Not a param and not a stage (stage was already consumed) -> ignore.
        return;
    };
    let key = field[..eq].trim().to_ascii_lowercase();
    let value = field[eq + 1..].trim();
    match key.as_str() {
        "order" => match value.parse::<i32>() {
            Ok(n) => entry.order = Some(n),
            Err(_) => tracing::warn!(value, "select.def: malformed order; ignoring"),
        },
        "music" => entry.music = Some(value.to_string()),
        "includestage" => {
            // 0 -> excluded; anything else (incl. malformed) keeps the default.
            entry.include_stage = value != "0";
        }
        _ => {}
    }
}

/// Applies one `[Options]` `key = value` line.
fn apply_option(out: &mut SelectDef, line: &str) {
    let Some(eq) = line.find('=') else {
        return;
    };
    let key = line[..eq].trim().to_ascii_lowercase();
    let value = &line[eq + 1..];
    let nums: Vec<i32> = int_list(value);
    match key.as_str() {
        "arcade.maxmatches" => out.arcade_maxmatches = nums,
        "team.maxmatches" => out.team_maxmatches = nums,
        _ => {}
    }
}

/// Parses a comma/whitespace-separated list of ints, skipping non-numeric
/// tokens.
fn int_list(s: &str) -> Vec<i32> {
    s.split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse::<i32>().ok())
        .collect()
}

/// Strips `;`, `//`, and `#` comments from a line (whichever appears first).
fn strip_comment(line: &str) -> &str {
    let mut cut = line.len();
    if let Some(p) = line.find(';') {
        cut = cut.min(p);
    }
    if let Some(p) = line.find("//") {
        cut = cut.min(p);
    }
    if let Some(p) = line.find('#') {
        cut = cut.min(p);
    }
    &line[..cut]
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
; A synthetic select.def
[Characters]
kfm, stages/kfm.def
hero, random, order=2, music=sound/bgm.mp3
boss/boss.def, stages/arena.def, includestage=0
Training Dummy, ../trainingdummy/trainingdummy.def, stages/dojo.def
randomselect
blank
   ; a comment-only line
chars/extra/extra.def

[ExtraStages]
stages/bonus.def
stages/another.def

[Options]
arcade.maxmatches = 6,1,1,0,0,0,0,0,0,0
team.maxmatches = 4,1,1
unknown.option = whatever
"#;

    fn sample() -> SelectDef {
        SelectDef::parse(SAMPLE)
    }

    #[test]
    fn classic_form_bare_name_resolves_to_def_with_stage() {
        // `kfm, stages/kfm.def`: bare name (no space) -> classic MUGEN form,
        // field 1 resolves to kfm/kfm.def, field 2 is the stage.
        let s = sample();
        let SelectSlot::Character(e) = &s.slots[0] else {
            panic!("expected character slot");
        };
        assert_eq!(e.name, "kfm");
        assert_eq!(e.def_path, "kfm/kfm.def");
        assert_eq!(e.stage.as_deref(), Some("stages/kfm.def"));
        assert!(e.include_stage);
        assert_eq!(e.order, None);
    }

    #[test]
    fn explicit_def_form_displayname_and_def_and_stage() {
        // `Training Dummy, ../trainingdummy/trainingdummy.def, stages/dojo.def`:
        // field 1 has a space (a label) and field 2 is a .def -> explicit-def
        // form, field 2 is the def, field 3 the stage.
        let s = sample();
        let SelectSlot::Character(e) = &s.slots[3] else {
            panic!("expected character slot");
        };
        assert_eq!(e.name, "Training Dummy");
        assert_eq!(e.def_path, "../trainingdummy/trainingdummy.def");
        assert_eq!(e.stage.as_deref(), Some("stages/dojo.def"));
    }

    #[test]
    fn random_stage_is_not_a_stage_and_params_parse() {
        let s = sample();
        let SelectSlot::Character(e) = &s.slots[1] else {
            panic!("expected character slot");
        };
        assert_eq!(e.name, "hero");
        assert_eq!(e.def_path, "hero/hero.def");
        assert_eq!(e.stage, None, "`random` stage is not stored as a path");
        assert_eq!(e.order, Some(2));
        assert_eq!(e.music.as_deref(), Some("sound/bgm.mp3"));
    }

    #[test]
    fn explicit_def_path_and_includestage_zero() {
        let s = sample();
        let SelectSlot::Character(e) = &s.slots[2] else {
            panic!("expected character slot");
        };
        assert_eq!(e.def_path, "boss/boss.def", "explicit .def path kept as-is");
        assert_eq!(e.stage.as_deref(), Some("stages/arena.def"));
        assert!(!e.include_stage, "includestage=0 excludes the stage");
    }

    #[test]
    fn special_tokens_and_path_field() {
        let s = sample();
        assert_eq!(s.slots[4], SelectSlot::RandomSelect);
        assert_eq!(s.slots[5], SelectSlot::Empty);
        let SelectSlot::Character(e) = &s.slots[6] else {
            panic!("expected character slot for a slash path");
        };
        // Bare `chars/extra/extra.def` (no space) -> classic form, taken as-is.
        assert_eq!(e.def_path, "chars/extra/extra.def");
    }

    #[test]
    fn parses_extra_stages_and_options() {
        let s = sample();
        assert_eq!(
            s.extra_stages,
            vec![
                "stages/bonus.def".to_string(),
                "stages/another.def".to_string()
            ]
        );
        assert_eq!(s.arcade_maxmatches, vec![6, 1, 1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(s.team_maxmatches, vec![4, 1, 1]);
    }

    #[test]
    fn comments_and_blank_lines_skipped() {
        // The comment-only line inside [Characters] must not produce a slot.
        // Slots: 0 kfm, 1 hero, 2 boss, 3 Training Dummy, 4 randomselect,
        // 5 blank, 6 chars/extra.
        let s = sample();
        assert_eq!(s.slots.len(), 7);
    }

    #[test]
    fn bom_and_crlf_tolerated() {
        let text = "\u{feff}[Characters]\r\nkfm, stages/kfm.def\r\n";
        let s = SelectDef::parse(text);
        assert_eq!(s.slots.len(), 1);
        let SelectSlot::Character(e) = &s.slots[0] else {
            panic!("expected character");
        };
        assert_eq!(e.def_path, "kfm/kfm.def");
    }

    #[test]
    fn malformed_order_is_ignored_not_panicked() {
        let s = SelectDef::parse("[Characters]\nx, random, order=notanumber\n");
        let SelectSlot::Character(e) = &s.slots[0] else {
            panic!("expected character");
        };
        assert_eq!(e.order, None, "bad order ignored, no panic");
    }

    #[test]
    fn empty_input_yields_empty_roster() {
        let s = SelectDef::parse("");
        assert_eq!(s, SelectDef::default());
        assert!(s.slots.is_empty());
    }
}
