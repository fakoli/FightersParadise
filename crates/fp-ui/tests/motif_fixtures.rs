//! Real- and shipped-fixture tests for the `system.def` / `select.def` parsers.
//!
//! Two layers:
//!
//! - **Real KFM motif (gated):** parses the bundled
//!   `test-assets/kfm-motif-sffv1/{system.def,select.def}`. If that directory is
//!   absent (it is local-only, e.g. on CI), each test logs a skip and returns —
//!   so the suite stays green without the binary fixtures.
//! - **Shipped default motif (non-gated):** the original `assets/data/system.def`
//!   and `assets/data/select.def` ship in-tree, so these always run and must
//!   parse cleanly into the expected default menu + trainingdummy roster.

use std::path::{Path, PathBuf};

use fp_formats::def::DefFile;
use fp_ui::{MenuItemKind, SelectDef, SelectSlot, SystemDef};

/// Resolve a path under the workspace `test-assets/kfm-motif-sffv1/` directory.
///
/// `CARGO_MANIFEST_DIR` points at `crates/fp-ui`; go up two levels to the
/// workspace root.
fn motif_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-assets/kfm-motif-sffv1")
        .join(name)
}

/// Resolve a path under the in-tree shipped `assets/data/` directory.
fn data_asset(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../assets/data")
        .join(name)
}

// ----------------------------------------------------------------------------
// Real KFM motif (gated on test-assets/)
// ----------------------------------------------------------------------------

#[test]
fn real_kfm_system_def_parses_menu_grid_and_files() {
    let path = motif_fixture("system.def");
    if !path.exists() {
        eprintln!("skipping: {} not present (test-assets/ is local-only)", path.display());
        return;
    }

    let def = DefFile::load(&path).expect("kfm system.def must load");
    let sys = SystemDef::parse(&def);

    // [Info] / [Files]
    assert_eq!(sys.name, "KFM");
    assert_eq!(sys.author, "Elecbyte");
    assert_eq!(sys.spr, "system.sff");
    assert_eq!(sys.snd, "system.snd");
    assert_eq!(sys.select_file, "select.def");
    assert_eq!(sys.fight_file, "fight.def");
    assert_eq!(sys.intro_storyboard, "intro.def");
    assert_eq!(sys.logo_storyboard, "logo.def");
    // font1..font3 in the real file.
    assert_eq!(
        sys.fonts,
        vec![
            "font/f-4x6.fnt".to_string(),
            "font/f-6x9.fnt".to_string(),
            "font/jg.fnt".to_string(),
        ]
    );

    // [Title Info] — the KFM motif enables ALL 11 canonical items.
    let kinds: Vec<_> = sys.title.items.iter().map(|i| i.kind).collect();
    assert_eq!(kinds, MenuItemKind::ALL.to_vec(), "all 11 items enabled in order");
    // Spot-check a few labels.
    assert_eq!(sys.title.items[0].label, "ARCADE");
    assert_eq!(sys.title.items[1].label, "VS MODE");
    assert_eq!(sys.title.items[7].label, "TRAINING");
    assert_eq!(sys.title.window_visible_items, 5);
    assert_eq!(sys.title.menu_pos, fp_ui::Pos::new(159, 157));

    // [Select Info] — a 2x5 grid.
    assert_eq!(sys.select_info.rows, 2);
    assert_eq!(sys.select_info.columns, 5);
    assert_eq!(sys.select_info.pos, fp_ui::Pos::new(90, 170));
    assert_eq!(sys.select_info.cell_size, fp_ui::Pos::new(27, 27));
    assert_eq!(sys.select_info.cell_spacing, fp_ui::Pos::new(2, 2));
    assert_eq!(sys.select_info.p1_cursor.start_cell, (0, 0));
    assert_eq!(sys.select_info.p1_cursor.move_snd, Some((100, 0)));

    // [VS Screen]
    assert_eq!(sys.vs_screen.p1.pos, fp_ui::Pos::new(20, 31));
    assert_eq!(sys.vs_screen.p2.pos, fp_ui::Pos::new(299, 31));
}

#[test]
fn real_kfm_select_def_parses_roster() {
    let path = motif_fixture("select.def");
    if !path.exists() {
        eprintln!("skipping: {} not present (test-assets/ is local-only)", path.display());
        return;
    }

    let text = std::fs::read_to_string(&path).expect("kfm select.def readable");
    let sel = SelectDef::parse(&text);

    // The real file lists a single character: `kfm, stages/kfm.def`.
    let chars: Vec<_> = sel
        .slots
        .iter()
        .filter_map(|s| match s {
            SelectSlot::Character(e) => Some(e),
            _ => None,
        })
        .collect();
    assert_eq!(chars.len(), 1, "exactly one roster character in real select.def");
    let kfm = chars[0];
    assert_eq!(kfm.name, "kfm");
    assert_eq!(kfm.def_path, "kfm/kfm.def", "bare name resolves to kfm/kfm.def");
    assert_eq!(kfm.stage.as_deref(), Some("stages/kfm.def"));

    // [Options] arcade.maxmatches is present.
    assert_eq!(sel.arcade_maxmatches.first().copied(), Some(6));
    assert_eq!(sel.team_maxmatches.first().copied(), Some(4));
}

// ----------------------------------------------------------------------------
// Shipped default motif (NON-gated: ships in-tree, always runs)
// ----------------------------------------------------------------------------

#[test]
fn shipped_default_system_def_parses() {
    let path = data_asset("system.def");
    let def = DefFile::load(&path).expect("shipped default system.def must load");
    let sys = SystemDef::parse(&def);

    assert_eq!(sys.name, "Fighters Paradise");
    assert_eq!(sys.select_file, "select.def");
    // Points at the shipped bitmap font.
    assert_eq!(sys.fonts, vec!["font.fnt".to_string()]);

    // Exactly the three enabled items, in canonical order.
    let kinds: Vec<_> = sys.title.items.iter().map(|i| i.kind).collect();
    assert_eq!(
        kinds,
        vec![MenuItemKind::Versus, MenuItemKind::Training, MenuItemKind::Exit]
    );
    let labels: Vec<_> = sys.title.items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, vec!["VS MODE", "TRAINING", "EXIT"]);

    // A 1x4 grid geometry is present.
    assert_eq!(sys.select_info.rows, 1);
    assert_eq!(sys.select_info.columns, 4);
    assert_eq!(sys.select_info.cell_size, fp_ui::Pos::new(27, 27));
}

#[test]
fn shipped_default_select_def_resolves_trainingdummy() {
    let path = data_asset("select.def");
    let text = std::fs::read_to_string(&path).expect("shipped default select.def readable");
    let sel = SelectDef::parse(&text);

    let chars: Vec<_> = sel
        .slots
        .iter()
        .filter_map(|s| match s {
            SelectSlot::Character(e) => Some(e),
            _ => None,
        })
        .collect();
    assert!(!chars.is_empty(), "default roster has at least one character");

    let dummy = chars[0];
    assert_eq!(dummy.name, "Training Dummy");
    assert_eq!(dummy.def_path, "../trainingdummy/trainingdummy.def");

    // The default roster also carries a randomselect icon.
    assert!(
        sel.slots.iter().any(|s| matches!(s, SelectSlot::RandomSelect)),
        "default roster includes a randomselect slot"
    );

    // The resolved def path actually points at the shipped trainingdummy.def,
    // relative to the data/ directory.
    let resolved = data_asset(&dummy.def_path);
    assert!(
        resolved.exists(),
        "trainingdummy.def must resolve to a real file: {}",
        resolved.display()
    );
}
