//! Directory-based content discovery: character rosters and motif sets.
//!
//! MUGEN organises bring-your-own content on disk by convention rather than by a
//! single index file:
//!
//! - **Characters** live one-per-folder under a `chars/` directory, each folder
//!   holding a same-named `.def` (`chars/kfm/kfm.def`). A flat directory of bare
//!   `*.def` files is also accepted (so a quick scratch folder of characters
//!   works without the nested layout). The scanner accepts either a game *root*
//!   that holds a `chars/` subdirectory **or** the `chars/` directory itself.
//! - **Motif sets** (screenpacks) live one-per-folder under a `data/` directory,
//!   each folder holding a `system.def` (`data/default/system.def`).
//!
//! This module is the pure scanner for both: given a directory it walks the
//! filesystem and returns a typed list of what it found, in a deterministic
//! (case-insensitively sorted) order. It follows the workspace contract — it
//! **never panics** on bad content: an unreadable directory yields an empty list
//! with a `tracing::warn!`, and a subfolder that does not hold the expected
//! `.def` is skipped (logged at `debug!`, since an asset-less subfolder is
//! normal, not an error).
//!
//! Discovery is decoupled from parsing on purpose. A [`CharEntry`] only resolves
//! a name + `.def` path; the caller loads/validates the character. Likewise a
//! [`MotifEntry`] only names a `system.def` path; the caller parses it via
//! [`crate::SystemDef`]. This keeps the scan cheap (no file reads beyond the
//! directory listing and an `is_file` check) and keeps `fp-ui` free of any
//! character-loading dependency.

use std::path::{Path, PathBuf};

/// One discovered character: a display name and the resolved `.def` path the
/// caller loads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CharEntry {
    /// The character's name — its folder name in the nested
    /// `chars/<name>/<name>.def` layout, or the file stem in a flat `*.def`
    /// directory.
    pub name: String,
    /// The resolved path of the character's `.def` file.
    pub def_path: PathBuf,
}

/// One discovered motif/screenpack set: a display name and its `system.def` path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MotifEntry {
    /// The motif's name — its subfolder name under the scanned `data/` directory.
    pub name: String,
    /// The resolved path of the motif's `system.def` file.
    pub system_def_path: PathBuf,
}

/// Scans `dir` for characters and returns the discovered roster.
///
/// Two layouts are recognised, in this priority:
///
/// 1. **Nested** — each immediate subfolder `dir/<name>/` that contains a
///    same-named `<name>.def` is a character (the MUGEN-standard `chars/` layout).
/// 2. **Flat** — any bare `dir/<file>.def` is a character whose name is the file
///    stem (a convenience for a scratch folder of loose `.def`s).
///
/// Both layouts may coexist: a `dir` holding `kfm/kfm.def` plus a loose
/// `ryu.def` yields both. A subfolder with no matching `.def` (or that cannot be
/// read) is skipped with a `tracing::debug!` — never a panic — so an empty or
/// asset-less folder does not abort the scan. An unreadable `dir` itself yields
/// an empty roster with a `tracing::warn!`.
///
/// The result is sorted case-insensitively by name (then by path) so the roster
/// order is stable regardless of the filesystem's listing order, and any
/// duplicate `(name, def_path)` is collapsed.
#[must_use]
pub fn discover_chars(dir: &Path) -> Vec<CharEntry> {
    let scan = resolve_container(dir, "chars");
    let read = match std::fs::read_dir(&scan) {
        Ok(rd) => rd,
        Err(e) => {
            tracing::warn!("char discovery: cannot read {}: {e}", scan.display());
            return Vec::new();
        }
    };

    let mut entries: Vec<CharEntry> = Vec::new();

    for item in read {
        let item = match item {
            Ok(it) => it,
            Err(e) => {
                tracing::debug!(
                    "char discovery: skipping unreadable entry in {}: {e}",
                    scan.display()
                );
                continue;
            }
        };
        let path = item.path();
        let file_type = match item.file_type() {
            Ok(ft) => ft,
            Err(e) => {
                tracing::debug!("char discovery: cannot stat {}: {e}", path.display());
                continue;
            }
        };

        if file_type.is_dir() {
            // Nested layout: dir/<name>/<name>.def.
            if let Some(name) = file_name_str(&path) {
                let candidate = path.join(format!("{name}.def"));
                if candidate.is_file() {
                    entries.push(CharEntry {
                        name: name.to_string(),
                        def_path: candidate,
                    });
                } else {
                    tracing::debug!(
                        "char discovery: subfolder {} has no {name}.def; skipping",
                        path.display()
                    );
                }
            }
        } else if is_def_file(&path) {
            // Flat layout: a loose dir/<file>.def.
            if let Some(stem) = file_stem_str(&path) {
                entries.push(CharEntry {
                    name: stem.to_string(),
                    def_path: path,
                });
            }
        }
    }

    entries.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
            .then_with(|| a.def_path.cmp(&b.def_path))
    });
    entries.dedup();
    tracing::info!(
        "char discovery: {} character(s) under {}",
        entries.len(),
        scan.display()
    );
    entries
}

/// Scans `dir` for motif/screenpack sets and returns the discovered list.
///
/// Each immediate subfolder `dir/<name>/` that contains a `system.def` is a
/// motif (the MUGEN-standard `data/<motif>/` convention). A subfolder with no
/// `system.def` (or that cannot be read) is skipped with a `tracing::debug!` —
/// never a panic. An unreadable `dir` yields an empty list with a
/// `tracing::warn!`.
///
/// The result is sorted case-insensitively by name (then by path) so motif order
/// is stable, and duplicate entries are collapsed.
#[must_use]
pub fn discover_motifs(dir: &Path) -> Vec<MotifEntry> {
    let scan = resolve_container(dir, "data");
    let read = match std::fs::read_dir(&scan) {
        Ok(rd) => rd,
        Err(e) => {
            tracing::warn!("motif discovery: cannot read {}: {e}", scan.display());
            return Vec::new();
        }
    };

    let mut entries: Vec<MotifEntry> = Vec::new();

    for item in read {
        let item = match item {
            Ok(it) => it,
            Err(e) => {
                tracing::debug!(
                    "motif discovery: skipping unreadable entry in {}: {e}",
                    scan.display()
                );
                continue;
            }
        };
        let path = item.path();
        let is_dir = item.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        if !is_dir {
            continue;
        }
        let Some(name) = file_name_str(&path) else {
            continue;
        };
        let candidate = path.join("system.def");
        if candidate.is_file() {
            entries.push(MotifEntry {
                name: name.to_string(),
                system_def_path: candidate,
            });
        } else {
            tracing::debug!(
                "motif discovery: subfolder {} has no system.def; skipping",
                path.display()
            );
        }
    }

    entries.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
            .then_with(|| a.system_def_path.cmp(&b.system_def_path))
    });
    entries.dedup();
    tracing::info!(
        "motif discovery: {} motif(s) under {}",
        entries.len(),
        scan.display()
    );
    entries
}

/// Whether `path` names a `.def` file (case-insensitive extension).
fn is_def_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("def"))
}

/// The final path component as a `&str`, if it is valid UTF-8.
fn file_name_str(path: &Path) -> Option<&str> {
    path.file_name().and_then(|n| n.to_str())
}

/// The file stem (name without extension) as a `&str`, if valid UTF-8.
fn file_stem_str(path: &Path) -> Option<&str> {
    path.file_stem().and_then(|n| n.to_str())
}

/// Resolve which directory to actually scan for content of a given kind.
///
/// Accepts either the content container passed directly, or a MUGEN-style game
/// *root* that holds the conventionally-named subdirectory (`chars/`, `data/`):
/// when `dir/<sub>/` exists as a directory, that subdirectory is scanned;
/// otherwise `dir` itself is. This lets a caller point at a game root (which
/// holds `chars/`) **or** at the container directly, and both work.
fn resolve_container(dir: &Path, sub: &str) -> PathBuf {
    let nested = dir.join(sub);
    if nested.is_dir() {
        nested
    } else {
        dir.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique scratch directory for one test, rooted in the OS temp dir and
    /// tagged with the process id + a per-test label so parallel tests never
    /// collide. Removed and recreated up-front so a leaked prior run is clean.
    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fp_ui_discovery_{}_{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Creates `dir/<name>/<name>.def` with a trivial body.
    fn make_char_folder(root: &Path, name: &str) {
        let folder = root.join(name);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join(format!("{name}.def")), "[Info]\nname = test\n").unwrap();
    }

    #[test]
    fn nested_layout_yields_both_characters() {
        // chars/foo/foo.def + chars/bar/bar.def -> a two-entry roster.
        let dir = scratch("nested");
        make_char_folder(&dir, "foo");
        make_char_folder(&dir, "bar");

        let roster = discover_chars(&dir);
        assert_eq!(roster.len(), 2, "both characters discovered");
        // Sorted case-insensitively: bar before foo.
        assert_eq!(roster[0].name, "bar");
        assert_eq!(roster[0].def_path, dir.join("bar").join("bar.def"));
        assert_eq!(roster[1].name, "foo");
        assert_eq!(roster[1].def_path, dir.join("foo").join("foo.def"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn game_root_with_chars_subdir_is_discovered() {
        // Point at a game ROOT that holds a `chars/` subdir (the natural
        // "point at a directory that has a chars folder" usage): discovery
        // descends into `chars/` automatically.
        let root = scratch("gameroot-chars");
        let chars = root.join("chars");
        std::fs::create_dir_all(&chars).unwrap();
        make_char_folder(&chars, "foo");
        make_char_folder(&chars, "bar");

        let roster = discover_chars(&root);
        assert_eq!(
            roster.len(),
            2,
            "characters under <root>/chars/ are discovered when pointing at the root"
        );
        assert_eq!(roster[0].name, "bar");
        assert_eq!(roster[0].def_path, chars.join("bar").join("bar.def"));
        assert_eq!(roster[1].name, "foo");

        // Pointing directly at the chars/ container still works (unchanged).
        assert_eq!(discover_chars(&chars).len(), 2);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn game_root_with_data_subdir_discovers_motifs() {
        // Point at a game ROOT that holds a `data/` subdir: motif discovery
        // descends into `data/` automatically.
        let root = scratch("gameroot-data");
        let motif = root.join("data").join("default");
        std::fs::create_dir_all(&motif).unwrap();
        std::fs::write(motif.join("system.def"), "[Info]\nname = test\n").unwrap();

        let motifs = discover_motifs(&root);
        assert_eq!(
            motifs.len(),
            1,
            "a motif under <root>/data/ is discovered when pointing at the root"
        );
        assert_eq!(motifs[0].name, "default");
        assert_eq!(motifs[0].system_def_path, motif.join("system.def"));

        // Pointing directly at the data/ container still works.
        assert_eq!(discover_motifs(&root.join("data")).len(), 1);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn flat_directory_of_defs_is_discovered() {
        // A flat dir of loose *.def files (no nested folders).
        let dir = scratch("flat");
        std::fs::write(dir.join("ryu.def"), "[Info]\n").unwrap();
        std::fs::write(dir.join("ken.def"), "[Info]\n").unwrap();
        // A non-.def file must be ignored.
        std::fs::write(dir.join("readme.txt"), "notes").unwrap();

        let roster = discover_chars(&dir);
        assert_eq!(roster.len(), 2, "two loose .def characters, txt ignored");
        assert_eq!(roster[0].name, "ken");
        assert_eq!(roster[0].def_path, dir.join("ken.def"));
        assert_eq!(roster[1].name, "ryu");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nested_and_flat_layouts_coexist() {
        let dir = scratch("mixed");
        make_char_folder(&dir, "kfm");
        std::fs::write(dir.join("loose.def"), "[Info]\n").unwrap();

        let names: Vec<String> = discover_chars(&dir).into_iter().map(|e| e.name).collect();
        assert!(names.iter().any(|n| n == "kfm"));
        assert!(names.iter().any(|n| n == "loose"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn assetless_subfolder_is_skipped_not_panicked() {
        // chars/empty/ has no empty.def -> skipped; chars/good/good.def kept.
        let dir = scratch("assetless");
        std::fs::create_dir_all(dir.join("empty")).unwrap();
        // A subfolder whose .def is mis-named (does not match the folder).
        let mismatch = dir.join("mismatch");
        std::fs::create_dir_all(&mismatch).unwrap();
        std::fs::write(mismatch.join("other.def"), "[Info]\n").unwrap();
        make_char_folder(&dir, "good");

        let roster = discover_chars(&dir);
        assert_eq!(roster.len(), 1, "only the well-formed character is kept");
        assert_eq!(roster[0].name, "good");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_directory_yields_empty_roster_no_panic() {
        let missing = std::env::temp_dir().join(format!(
            "fp_ui_discovery_{}_does_not_exist",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&missing);
        let roster = discover_chars(&missing);
        assert!(
            roster.is_empty(),
            "unreadable dir -> empty roster, no panic"
        );
    }

    #[test]
    fn motifs_discovered_under_data_dir() {
        // data/default/system.def + data/dark/system.def -> two motifs.
        let dir = scratch("motifs");
        for name in ["default", "dark"] {
            let folder = dir.join(name);
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join("system.def"), "[Info]\nname = m\n").unwrap();
        }
        // A subfolder without a system.def must be skipped.
        std::fs::create_dir_all(dir.join("notamotif")).unwrap();

        let motifs = discover_motifs(&dir);
        assert_eq!(
            motifs.len(),
            2,
            "two valid motifs, the empty folder skipped"
        );
        // Sorted case-insensitively: dark before default.
        assert_eq!(motifs[0].name, "dark");
        assert_eq!(
            motifs[0].system_def_path,
            dir.join("dark").join("system.def")
        );
        assert_eq!(motifs[1].name, "default");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn motif_discovery_missing_dir_no_panic() {
        let missing =
            std::env::temp_dir().join(format!("fp_ui_motif_{}_does_not_exist", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        assert!(discover_motifs(&missing).is_empty());
    }
}
