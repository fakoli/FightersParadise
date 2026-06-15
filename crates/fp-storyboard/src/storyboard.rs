//! Storyboard `.def` parsing and the typed scene model.
//!
//! A MUGEN *storyboard* (also called a cutscene) is an INI-like `.def` file that
//! drives a sequence of full-screen scenes — the title logo, the arcade
//! introduction, an ending, the game-over screen, and so on. This module turns
//! such a file into a strongly-typed [`Storyboard`] model. **Parsing only** —
//! nothing here draws to the screen; rendering lives in a later task.
//!
//! # Format overview
//!
//! ```text
//! [Info]
//! localcoord = 320,240
//!
//! [SceneDef]
//! spr = intro.sff
//!
//! [Scene 0]
//! fadein.time = 120
//! fadeout.time = 30
//! bg.name = BG0
//! end.time = 240
//!
//! [BG0Def]
//!
//! [BG0 Mountains]
//! type = normal
//! spriteno = 0,0
//! start = 0,48
//! velocity = 6
//! tile = 1,0
//!
//! ; embedded animations referenced by layerN.anim
//! [Begin Action 10]
//! 10,0, 0,0, -1
//! ```
//!
//! The parser is built **on top of [`fp_formats::def::DefFile`]** (which handles
//! the BOM / CRLF / comment / quote details) plus a single lightweight pass over
//! the raw text to recover *section ordering*, since [`Scene`]s and background
//! groups must be returned in file order and [`DefFile`] stores its sections in
//! an unordered map. Embedded `[Begin Action N]` blocks are handed to
//! [`fp_formats::air::AirFile`] and exposed as [`Storyboard::animations`].
//!
//! # Robustness
//!
//! Following the engine-wide "never crash on bad content" rule, every accessor
//! substitutes a documented safe default when a key is missing or malformed and
//! logs a `tracing::warn!`. [`Storyboard::load`] only returns `Err` when the file
//! itself cannot be read.

use std::collections::HashMap;
use std::path::Path;

use fp_core::FpResult;
use fp_formats::air::{AirFile, AnimAction};
use fp_formats::def::DefFile;

/// A fully parsed storyboard / cutscene definition.
///
/// Produced by [`Storyboard::load`] or [`Storyboard::from_def`]. Scenes and
/// background groups are stored in file order.
#[derive(Debug, Clone)]
pub struct Storyboard {
    /// The local coordinate space `(width, height)` from `[Info] localcoord`.
    ///
    /// Defaults to `(320, 240)` (the MUGEN default) when absent or malformed.
    pub localcoord: (i32, i32),
    /// Sprite container path from `[SceneDef] spr` (e.g. `"intro.sff"`).
    ///
    /// Empty when the key is absent.
    pub sprite_path: String,
    /// Index of the scene to start playback from (`[SceneDef] startscene`).
    ///
    /// Defaults to `0` when absent or malformed.
    pub start_scene: i32,
    /// Scenes in file order. May be empty for a malformed or stub storyboard.
    pub scenes: Vec<Scene>,
    /// Background-layer groups (`[<name>Def]` + `[<name> <layer>]`) in file order.
    pub bg_groups: Vec<BgGroup>,
    /// Embedded `[Begin Action N]` animations keyed by action number.
    ///
    /// Populated from any AIR-style action blocks found in the file (these are
    /// referenced by [`SceneLayer::anim`]). Empty when none are present.
    pub animations: HashMap<i32, AnimAction>,
}

/// A single full-screen scene within a [`Storyboard`].
#[derive(Debug, Clone)]
pub struct Scene {
    /// Total duration of the scene in ticks (`end.time`). `0` if unspecified.
    pub end_time: i32,
    /// Fade-in duration in ticks (`fadein.time`). `0` if unspecified.
    pub fadein_time: i32,
    /// Fade-out duration in ticks (`fadeout.time`). `0` if unspecified.
    pub fadeout_time: i32,
    /// Optional fade-in color `(r, g, b)` from `fadein.col`.
    pub fadein_col: Option<(u8, u8, u8)>,
    /// Optional fade-out color `(r, g, b)` from `fadeout.col`.
    pub fadeout_col: Option<(u8, u8, u8)>,
    /// Optional background clear color `(r, g, b)` from `clearcolor`.
    pub clearcolor: Option<(u8, u8, u8)>,
    /// Optional name of the background group to display (`bg.name`).
    ///
    /// Matches a [`BgGroup::name`] in [`Storyboard::bg_groups`].
    pub bg_name: Option<String>,
    /// Optional background-music path (`bgm`).
    pub bgm: Option<String>,
    /// Default position for all layers (`layerall.pos`), as authored.
    ///
    /// `None` when the scene **omits** the key — in MUGEN an omitted
    /// `layerall.pos` inherits the previous scene's value (carry-over), which the
    /// [`crate::player::StoryboardPlayer`] resolves at playback time. `Some((0.0,
    /// 0.0))` distinguishes an explicit `layerall.pos = 0,0` (which does *not*
    /// inherit). [`Scene::effective_layerall_pos`] returns the non-inheriting
    /// fallback for a standalone scene.
    pub layerall_pos: Option<(f32, f32)>,
    /// Per-layer overlay definitions (`layerN.*`), ordered by layer index.
    pub layers: Vec<SceneLayer>,
}

impl Scene {
    /// The scene's own `layerall.pos`, treating an absent value as `(0.0, 0.0)`.
    ///
    /// This is the *standalone* value with **no** carry-over from a previous
    /// scene; carry-over (a scene omitting `layerall.pos` inheriting the prior
    /// scene's value) is resolved during playback by
    /// [`crate::player::StoryboardPlayer`]. Use this when a scene is considered in
    /// isolation.
    #[must_use]
    pub fn effective_layerall_pos(&self) -> (f32, f32) {
        self.layerall_pos.unwrap_or((0.0, 0.0))
    }
}

/// A single overlay layer within a [`Scene`] (`layerN.*` keys).
#[derive(Debug, Clone)]
pub struct SceneLayer {
    /// The layer index `N` from `layerN.*` (MUGEN supports `0`–`9`).
    pub index: u32,
    /// Per-layer offset added to the scene's `layerall.pos` (`layerN.offset`).
    ///
    /// `(0.0, 0.0)` when unspecified.
    pub offset: (f32, f32),
    /// Optional animation action number (`layerN.anim`).
    ///
    /// References an entry in [`Storyboard::animations`].
    pub anim: Option<i32>,
    /// Optional static sprite `(group, image)` (`layerN.spriteno`).
    pub spriteno: Option<(i32, i32)>,
    /// Tick at which this layer becomes visible (`layerN.starttime`).
    ///
    /// `0` (shown from the start of the scene) when unspecified.
    pub starttime: i32,
    /// Optional tick at which this layer stops being shown (`layerN.endtime`).
    pub endtime: Option<i32>,
}

/// A named group of background layers (`[<name>Def]` + `[<name> <layer>]`).
///
/// Referenced by [`Scene::bg_name`].
#[derive(Debug, Clone)]
pub struct BgGroup {
    /// The group name (e.g. `"BG0"`), as referenced by `bg.name`.
    pub name: String,
    /// The background layers belonging to this group, in file order.
    pub layers: Vec<BgLayer>,
}

/// A single background layer within a [`BgGroup`] (`[<group> <layer>]`).
#[derive(Debug, Clone)]
pub struct BgLayer {
    /// The layer's own name (the part after the group prefix, e.g. `"Mountains"`).
    pub name: String,
    /// The element kind (`type`), e.g. `"normal"`. Defaults to `"normal"`.
    pub kind: String,
    /// Sprite `(group, image)` to draw (`spriteno`). `(0, 0)` if unspecified.
    pub spriteno: (i32, i32),
    /// Initial position `(x, y)` (`start`). `(0.0, 0.0)` if unspecified.
    pub start: (f32, f32),
    /// Scroll velocity `(x, y)` (`velocity`).
    ///
    /// MUGEN allows either a single scalar (applied to `x`, `y` left `0.0`) or an
    /// `x,y` pair; both are normalized to a pair here.
    pub velocity: (f32, f32),
    /// Tiling counts `(x, y)` (`tile`). `(0, 0)` (no tiling) if unspecified.
    pub tile: (i32, i32),
    /// Whether palette index 0 is treated as transparent (`mask`).
    ///
    /// `false` when unspecified.
    pub mask: bool,
    /// Optional blend/transparency mode (`trans`), e.g. `"add"`, `"sub"`.
    pub trans: Option<String>,
}

impl Storyboard {
    /// Loads and parses a storyboard `.def` file from `path`.
    ///
    /// Embedded `[Begin Action N]` animations are parsed from the same file.
    ///
    /// # Errors
    ///
    /// Returns [`fp_core::FpError`] only when the file cannot be read. Malformed
    /// or missing keys never error — they degrade to documented safe defaults and
    /// emit a `tracing::warn!`.
    pub fn load(path: &Path) -> FpResult<Self> {
        let text = std::fs::read_to_string(path)?;
        Ok(Self::from_def(&text))
    }

    /// Parses a storyboard from raw `.def` text.
    ///
    /// Tolerates a leading UTF-8 BOM and CRLF line endings (via
    /// [`fp_formats::def::DefFile`]). Never panics; malformed values are replaced
    /// with safe defaults and logged.
    pub fn from_def(text: &str) -> Self {
        // Reuse fp-formats DEF parsing for the section/key-value semantics
        // (BOM, CRLF, comments, quotes). If it somehow fails, fall back to an
        // empty section map rather than panicking.
        let def = match DefFile::from_str(text) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("storyboard: DEF parse failed ({e}); using empty model");
                DefFile {
                    sections: HashMap::new(),
                }
            }
        };

        // Recover section ordering from the raw text, since DefFile uses an
        // unordered map but scenes / bg groups must be returned in file order.
        let ordered = ordered_section_names(text);

        let localcoord = parse_pair_i32(def.get("Info", "localcoord")).unwrap_or((320, 240));
        let sprite_path = def.get("SceneDef", "spr").unwrap_or("").to_string();
        let start_scene = parse_i32(def.get("SceneDef", "startscene"), "SceneDef.startscene", 0);

        let scenes = parse_scenes(&def, &ordered);
        let bg_groups = parse_bg_groups(&def, &ordered);
        let animations = parse_embedded_animations(text);

        if scenes.is_empty() {
            tracing::warn!("storyboard: no [Scene N] sections found");
        }

        Storyboard {
            localcoord,
            sprite_path,
            start_scene,
            scenes,
            bg_groups,
            animations,
        }
    }
}

// ---------------------------------------------------------------------------
// Section-ordering recovery
// ---------------------------------------------------------------------------

/// A section header recovered from the raw text, in file order.
///
/// `key` is the lowercased name used to look the section up in [`DefFile`];
/// `display` preserves the original casing so layer / group names keep their
/// authored spelling (e.g. `"Mountains"`).
#[derive(Debug, Clone)]
struct OrderedSection {
    /// Lowercased header (matches `DefFile`'s map keys).
    key: String,
    /// Original-cased header text (for display names).
    display: String,
}

/// Returns the storyboard's section headers in file order.
///
/// Mirrors [`DefFile`]'s header detection (BOM strip, comment strip, trim) so the
/// recovered `key`s line up with `DefFile`'s lowercased map keys, while the
/// `display` field keeps the original casing.
fn ordered_section_names(text: &str) -> Vec<OrderedSection> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut names = Vec::new();
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if line.starts_with('[') && line.ends_with(']') && line.len() >= 2 {
            let inner = line[1..line.len() - 1].trim();
            names.push(OrderedSection {
                key: inner.to_ascii_lowercase(),
                display: inner.to_string(),
            });
        }
    }
    names
}

/// Strip a trailing `;` comment from a line (matches `DefFile` behavior).
fn strip_comment(line: &str) -> &str {
    match line.find(';') {
        Some(pos) => &line[..pos],
        None => line,
    }
}

// ---------------------------------------------------------------------------
// Scene parsing
// ---------------------------------------------------------------------------

/// Parse all `[Scene N]` sections in file order.
fn parse_scenes(def: &DefFile, ordered: &[OrderedSection]) -> Vec<Scene> {
    let mut scenes = Vec::new();
    for section in ordered {
        // Section keys are already lowercased; scene headers look like "scene 0".
        let Some(rest) = section.key.strip_prefix("scene") else {
            continue;
        };
        let rest = rest.trim();
        // Must be a numeric scene (skip e.g. "scenedef").
        if rest.parse::<i32>().is_err() {
            continue;
        }
        scenes.push(parse_scene(def, &section.key));
    }
    scenes
}

/// Parse a single `[Scene N]` section (already lowercased name).
fn parse_scene(def: &DefFile, section: &str) -> Scene {
    let end_time = parse_i32(def.get(section, "end.time"), "scene end.time", 0);
    let fadein_time = parse_i32(def.get(section, "fadein.time"), "scene fadein.time", 0);
    let fadeout_time = parse_i32(def.get(section, "fadeout.time"), "scene fadeout.time", 0);
    let fadein_col = parse_color(def.get(section, "fadein.col"));
    let fadeout_col = parse_color(def.get(section, "fadeout.col"));
    let clearcolor = parse_color(def.get(section, "clearcolor"));
    let bg_name = def
        .get(section, "bg.name")
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let bgm = def
        .get(section, "bgm")
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    // Stored as `Option`: `None` means the key was absent, so the value is
    // inherited from the previous scene at playback time (MUGEN carry-over). A
    // present-but-malformed value also yields `None` (warned in `parse_pair_f32`)
    // and therefore inherits, matching the "degrade safely" rule.
    let layerall_pos = parse_pair_f32(def.get(section, "layerall.pos"));
    let layers = parse_scene_layers(def, section);

    Scene {
        end_time,
        fadein_time,
        fadeout_time,
        fadein_col,
        fadeout_col,
        clearcolor,
        bg_name,
        bgm,
        layerall_pos,
        layers,
    }
}

/// Parse the `layerN.*` overlay layers within a scene section.
fn parse_scene_layers(def: &DefFile, section: &str) -> Vec<SceneLayer> {
    let keys = match def.sections.get(section) {
        Some(k) => k,
        None => return Vec::new(),
    };

    // Discover which layer indices are present by scanning for `layerN.` keys.
    let mut indices: Vec<u32> = Vec::new();
    for key in keys.keys() {
        if let Some(idx) = layer_index_of(key) {
            if !indices.contains(&idx) {
                indices.push(idx);
            }
        }
    }
    indices.sort_unstable();

    indices
        .into_iter()
        .map(|index| {
            let prefix = format!("layer{index}");
            let offset =
                parse_pair_f32(def.get(section, &format!("{prefix}.offset"))).unwrap_or((0.0, 0.0));
            let anim = def
                .get(section, &format!("{prefix}.anim"))
                .and_then(|v| parse_opt_i32(v, "layer anim"));
            let spriteno = parse_pair_i32(def.get(section, &format!("{prefix}.spriteno")));
            let starttime = parse_i32(
                def.get(section, &format!("{prefix}.starttime")),
                "layer starttime",
                0,
            );
            let endtime = def
                .get(section, &format!("{prefix}.endtime"))
                .and_then(|v| parse_opt_i32(v, "layer endtime"));
            SceneLayer {
                index,
                offset,
                anim,
                spriteno,
                starttime,
                endtime,
            }
        })
        .collect()
}

/// Extract the layer index `N` from a `layerN.<field>` key, if present.
fn layer_index_of(key: &str) -> Option<u32> {
    let rest = key.strip_prefix("layer")?;
    let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if num.is_empty() {
        return None;
    }
    // Require a `.` after the digits so we don't misread `layerall.pos`.
    let after = &rest[num.len()..];
    if !after.starts_with('.') {
        return None;
    }
    num.parse::<u32>().ok()
}

// ---------------------------------------------------------------------------
// Background-group parsing
// ---------------------------------------------------------------------------

/// Parse `[<name>Def]` background-group definitions and their `[<name> <layer>]`
/// layer sections, grouped by their shared name prefix, in file order.
fn parse_bg_groups(def: &DefFile, ordered: &[OrderedSection]) -> Vec<BgGroup> {
    let mut groups: Vec<BgGroup> = Vec::new();

    for section in ordered {
        // A group is introduced by a section whose name ends in "def" and that is
        // NOT the top-level "scenedef"/"info" and NOT a "scene N" header.
        let Some(prefix_key) = section.key.strip_suffix("def") else {
            continue;
        };
        let prefix_key = prefix_key.trim();
        if prefix_key.is_empty() || prefix_key == "scene" {
            // "[Def]" or "[SceneDef]" — not a BG group.
            continue;
        }
        // Skip a numeric "scene N def" should it ever appear (defensive).
        if prefix_key
            .strip_prefix("scene")
            .map(str::trim)
            .is_some_and(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_digit()))
        {
            continue;
        }
        // Skip background CONTROLLER defs ("[BGNCtrlDef]") — these declare BG
        // animation controllers, not background layer groups. Without this a bare
        // "[BG0CtrlDef]" (no trailing layer name) strips "def" to "bg0ctrl" and
        // would otherwise produce a spurious empty group.
        if prefix_key.ends_with("ctrl") {
            continue;
        }

        // Group display name = the def header with the trailing "Def" removed,
        // preserving the authored casing (e.g. "[BG0Def]" -> "BG0").
        let group_name = section
            .display
            .get(..prefix_key.len())
            .unwrap_or(prefix_key)
            .trim()
            .to_string();

        let mut layers = Vec::new();

        // Collect layer sections "<prefix> <layer>" in file order, matching the
        // prefix case-insensitively against each section's key.
        for layer_section in ordered {
            let Some(after) = layer_section.key.strip_prefix(prefix_key) else {
                continue;
            };
            // Must be "<prefix> <something>" (a space then a non-empty name);
            // this excludes the def section itself and unrelated sections.
            let Some(after) = after.strip_prefix(' ') else {
                continue;
            };
            if after.trim().is_empty() {
                continue;
            }
            // Recover the original-cased layer name from the display header by
            // taking everything after the prefix and the separating space.
            let layer_name = layer_section
                .display
                .get(prefix_key.len()..)
                .unwrap_or("")
                .trim()
                .to_string();
            layers.push(parse_bg_layer(def, &layer_section.key, &layer_name));
        }

        groups.push(BgGroup {
            name: group_name,
            layers,
        });
    }

    groups
}

/// Parse a single `[<group> <layer>]` background-layer section.
fn parse_bg_layer(def: &DefFile, section: &str, layer_name: &str) -> BgLayer {
    let kind = def
        .get(section, "type")
        .filter(|s| !s.is_empty())
        .unwrap_or("normal")
        .to_string();
    let spriteno = parse_pair_i32(def.get(section, "spriteno")).unwrap_or((0, 0));
    let start = parse_pair_f32(def.get(section, "start")).unwrap_or((0.0, 0.0));
    let velocity = parse_velocity(def.get(section, "velocity"));
    let tile = parse_pair_i32(def.get(section, "tile")).unwrap_or((0, 0));
    let mask = parse_bool(def.get(section, "mask"));
    let trans = def
        .get(section, "trans")
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    BgLayer {
        name: layer_name.to_string(),
        kind,
        spriteno,
        start,
        velocity,
        tile,
        mask,
        trans,
    }
}

// ---------------------------------------------------------------------------
// Embedded AIR animations
// ---------------------------------------------------------------------------

/// Parse embedded `[Begin Action N]` blocks from the storyboard text.
///
/// Only the action blocks are handed to the AIR parser; all `[Section]` headers
/// that are *not* `[Begin Action N]` (and the key/value lines beneath them) are
/// dropped first so the AIR parser sees a clean animation file and does not emit
/// spurious "unrecognized line" warnings.
fn parse_embedded_animations(text: &str) -> HashMap<i32, AnimAction> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);

    let mut filtered = String::new();
    let mut in_action = false;
    for raw in text.lines() {
        let trimmed = strip_comment(raw).trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            // Any new section header ends the previous action block; only
            // [Begin Action ...] re-opens one.
            in_action = is_begin_action_header(trimmed);
            if in_action {
                filtered.push_str(raw);
                filtered.push('\n');
            }
            continue;
        }
        if in_action {
            filtered.push_str(raw);
            filtered.push('\n');
        }
    }

    if filtered.trim().is_empty() {
        return HashMap::new();
    }

    match AirFile::from_str(&filtered) {
        Ok(air) => air.actions,
        Err(e) => {
            tracing::warn!("storyboard: embedded animation parse failed ({e})");
            HashMap::new()
        }
    }
}

/// Returns true if a `[...]` header is a `[Begin Action N]` header.
fn is_begin_action_header(header: &str) -> bool {
    let lower = header.to_ascii_lowercase();
    let inner = lower
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .map(str::trim)
        .unwrap_or("");
    inner
        .strip_prefix("begin action")
        .map(|rest| rest.trim().parse::<i32>().is_ok())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Value parsing helpers (all degrade safely + warn)
// ---------------------------------------------------------------------------

/// Parse an `i32`, returning `default` (with a warning) on absence/malformation.
fn parse_i32(value: Option<&str>, what: &str, default: i32) -> i32 {
    match value {
        None => default,
        Some(v) => {
            let v = v.trim();
            if v.is_empty() {
                return default;
            }
            match v.parse::<i32>() {
                Ok(n) => n,
                Err(_) => {
                    tracing::warn!("storyboard: malformed {what} value {v:?}; using {default}");
                    default
                }
            }
        }
    }
}

/// Parse an optional `i32`; warns and returns `None` if present but malformed.
fn parse_opt_i32(value: &str, what: &str) -> Option<i32> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    match v.parse::<i32>() {
        Ok(n) => Some(n),
        Err(_) => {
            tracing::warn!("storyboard: malformed {what} value {v:?}; ignoring");
            None
        }
    }
}

/// Parse a `"x,y"` pair of `i32`s. `None` if absent/unparseable.
fn parse_pair_i32(value: Option<&str>) -> Option<(i32, i32)> {
    let v = value?.trim();
    if v.is_empty() {
        return None;
    }
    let mut it = v.split(',');
    // Use `and_then` (not `?`) on the second component so a single-value pair
    // ("320" instead of "320,240") is diagnosed by the warn below rather than
    // returning silently.
    let a = it.next().and_then(|t| t.trim().parse::<i32>().ok());
    let b = it.next().and_then(|t| t.trim().parse::<i32>().ok());
    match (a, b) {
        (Some(a), Some(b)) => Some((a, b)),
        _ => {
            tracing::warn!("storyboard: malformed integer pair {v:?} (expected \"x,y\"); ignoring");
            None
        }
    }
}

/// Parse a `"x,y"` pair of `f32`s. `None` if absent/unparseable.
fn parse_pair_f32(value: Option<&str>) -> Option<(f32, f32)> {
    let v = value?.trim();
    if v.is_empty() {
        return None;
    }
    let mut it = v.split(',');
    let a = it.next().and_then(|t| t.trim().parse::<f32>().ok());
    let b = it.next().and_then(|t| t.trim().parse::<f32>().ok());
    match (a, b) {
        (Some(a), Some(b)) => Some((a, b)),
        _ => {
            tracing::warn!("storyboard: malformed float pair {v:?} (expected \"x,y\"); ignoring");
            None
        }
    }
}

/// Parse an `r,g,b` color triple, clamping each component to `0..=255`.
fn parse_color(value: Option<&str>) -> Option<(u8, u8, u8)> {
    let v = value?.trim();
    if v.is_empty() {
        return None;
    }
    let parts: Vec<&str> = v.split(',').collect();
    if parts.len() < 3 {
        tracing::warn!("storyboard: malformed color {v:?}; ignoring");
        return None;
    }
    let comp = |s: &str| -> Option<u8> {
        s.trim()
            .parse::<i32>()
            .ok()
            .map(|n| n.clamp(0, 255) as u8)
    };
    match (comp(parts[0]), comp(parts[1]), comp(parts[2])) {
        (Some(r), Some(g), Some(b)) => Some((r, g, b)),
        _ => {
            tracing::warn!("storyboard: malformed color {v:?}; ignoring");
            None
        }
    }
}

/// Parse a `velocity` value: either a scalar `x` (with `y = 0`) or an `x,y` pair.
fn parse_velocity(value: Option<&str>) -> (f32, f32) {
    let v = match value {
        Some(v) => v.trim(),
        None => return (0.0, 0.0),
    };
    if v.is_empty() {
        return (0.0, 0.0);
    }
    if v.contains(',') {
        parse_pair_f32(Some(v)).unwrap_or((0.0, 0.0))
    } else {
        match v.parse::<f32>() {
            Ok(x) => (x, 0.0),
            Err(_) => {
                tracing::warn!("storyboard: malformed velocity {v:?}; using (0,0)");
                (0.0, 0.0)
            }
        }
    }
}

/// Parse a MUGEN boolean flag (`1`/`0`, `true`/`false`). Defaults to `false`.
fn parse_bool(value: Option<&str>) -> bool {
    match value {
        None => false,
        Some(v) => {
            let v = v.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYNTH: &str = "\
[Info]
localcoord = 320,240

[SceneDef]
spr = test.sff
startscene = 1

[Scene 0]
fadein.time = 60
fadeout.time = 30
clearcolor = 255,255,255
bg.name = BG0
layerall.pos = 160,120
layer0.anim = 0
layer0.offset = 10,20
layer0.starttime = 5
end.time = 180

[BG0Def]

[BG0 Mountains]
type = normal
spriteno = 0,0
start = 0,48
velocity = 6
tile = 1,0

[BG0 Shadows]
type = normal
spriteno = 5,0
start = -7000,48
velocity = 36
tile = 2,0
mask = 1
trans = sub

[Begin Action 0]
0,0, 0,0, -1
";

    #[test]
    fn parses_header_and_scenedef() {
        let sb = Storyboard::from_def(SYNTH);
        assert_eq!(sb.localcoord, (320, 240));
        assert_eq!(sb.sprite_path, "test.sff");
        assert_eq!(sb.start_scene, 1);
    }

    #[test]
    fn parses_scene_fields() {
        let sb = Storyboard::from_def(SYNTH);
        assert_eq!(sb.scenes.len(), 1);
        let s = &sb.scenes[0];
        assert_eq!(s.end_time, 180);
        assert_eq!(s.fadein_time, 60);
        assert_eq!(s.fadeout_time, 30);
        assert_eq!(s.clearcolor, Some((255, 255, 255)));
        assert_eq!(s.bg_name.as_deref(), Some("BG0"));
        assert_eq!(s.layerall_pos, Some((160.0, 120.0)));
        assert_eq!(s.layers.len(), 1);
        assert_eq!(s.layers[0].index, 0);
        assert_eq!(s.layers[0].anim, Some(0));
        assert_eq!(s.layers[0].offset, (10.0, 20.0));
        assert_eq!(s.layers[0].starttime, 5);
    }

    #[test]
    fn parses_bg_group_with_layers() {
        let sb = Storyboard::from_def(SYNTH);
        assert_eq!(sb.bg_groups.len(), 1);
        let g = &sb.bg_groups[0];
        assert_eq!(g.name, "BG0");
        assert_eq!(g.layers.len(), 2);
        assert_eq!(g.layers[0].name, "Mountains");
        assert_eq!(g.layers[0].spriteno, (0, 0));
        assert_eq!(g.layers[0].velocity, (6.0, 0.0));
        assert_eq!(g.layers[0].tile, (1, 0));
        assert!(!g.layers[0].mask);
        assert_eq!(g.layers[1].name, "Shadows");
        assert!(g.layers[1].mask);
        assert_eq!(g.layers[1].trans.as_deref(), Some("sub"));
    }

    #[test]
    fn parses_embedded_animation() {
        let sb = Storyboard::from_def(SYNTH);
        assert!(sb.animations.contains_key(&0));
        assert_eq!(sb.animations[&0].frames.len(), 1);
    }

    #[test]
    fn bg_controller_def_is_not_a_group() {
        // A bare "[BGNCtrlDef]" declares a BG animation controller, not a layer
        // group; it must NOT yield a spurious BgGroup (regression for the
        // "strip 'def' -> 'bg0ctrl'" false positive), and "[BG0Ctrl N]" controller
        // sections must not be pulled in as BG0 layers.
        let text = "\
[SceneDef]
spr = t.sff

[BG0Def]

[BG0 Sky]
type = normal
spriteno = 0,0

[BG0CtrlDef]

[BG0Ctrl 0]
type = null
";
        let sb = Storyboard::from_def(text);
        assert_eq!(sb.bg_groups.len(), 1, "only BG0 is a group, not BG0Ctrl");
        assert_eq!(sb.bg_groups[0].name, "BG0");
        assert_eq!(sb.bg_groups[0].layers.len(), 1, "[BG0Ctrl 0] is not a BG0 layer");
        assert_eq!(sb.bg_groups[0].layers[0].name, "Sky");
    }

    #[test]
    fn missing_optional_keys_default_safely() {
        let text = "\
[Info]
[SceneDef]
[Scene 0]
end.time = 100
";
        let sb = Storyboard::from_def(text);
        assert_eq!(sb.localcoord, (320, 240)); // default
        assert_eq!(sb.sprite_path, "");
        assert_eq!(sb.start_scene, 0);
        assert_eq!(sb.scenes.len(), 1);
        let s = &sb.scenes[0];
        assert_eq!(s.end_time, 100);
        assert_eq!(s.fadein_time, 0);
        assert_eq!(s.clearcolor, None);
        assert_eq!(s.bg_name, None);
        // Absent `layerall.pos` is `None` (inherits at playback); its standalone
        // fallback is (0,0).
        assert_eq!(s.layerall_pos, None);
        assert_eq!(s.effective_layerall_pos(), (0.0, 0.0));
        assert!(s.layers.is_empty());
    }

    #[test]
    fn malformed_numbers_degrade() {
        let text = "\
[Info]
localcoord = abc,def
[SceneDef]
spr = x.sff
[Scene 0]
end.time = not_a_number
fadein.time = 12
clearcolor = 300,-5,128
[BG0Def]
[BG0 Layer]
spriteno = bad
velocity = oops
";
        let sb = Storyboard::from_def(text);
        assert_eq!(sb.localcoord, (320, 240)); // fell back
        let s = &sb.scenes[0];
        assert_eq!(s.end_time, 0); // malformed -> default
        assert_eq!(s.fadein_time, 12);
        // clamped to 0..=255
        assert_eq!(s.clearcolor, Some((255, 0, 128)));
        let layer = &sb.bg_groups[0].layers[0];
        assert_eq!(layer.spriteno, (0, 0));
        assert_eq!(layer.velocity, (0.0, 0.0));
    }

    #[test]
    fn empty_scene_list_is_safe() {
        let sb = Storyboard::from_def("[Info]\nlocalcoord = 100,200\n");
        assert!(sb.scenes.is_empty());
        assert!(sb.bg_groups.is_empty());
        assert!(sb.animations.is_empty());
        assert_eq!(sb.localcoord, (100, 200));
    }

    #[test]
    fn empty_input_does_not_panic() {
        let sb = Storyboard::from_def("");
        assert!(sb.scenes.is_empty());
        assert_eq!(sb.localcoord, (320, 240));
    }

    #[test]
    fn bom_and_crlf_tolerated() {
        let text = "\u{feff}[Info]\r\nlocalcoord = 640,480\r\n[Scene 0]\r\nend.time = 10\r\n";
        let sb = Storyboard::from_def(text);
        assert_eq!(sb.localcoord, (640, 480));
        assert_eq!(sb.scenes.len(), 1);
        assert_eq!(sb.scenes[0].end_time, 10);
    }

    #[test]
    fn velocity_pair_form() {
        let text = "\
[Scene 0]
end.time = 1
[FGDef]
[FG Cloud]
velocity = 2.5, -1.5
";
        let sb = Storyboard::from_def(text);
        let layer = &sb.bg_groups[0].layers[0];
        assert_eq!(layer.velocity, (2.5, -1.5));
    }

    #[test]
    fn scenes_returned_in_order() {
        let text = "\
[Scene 2]
end.time = 3
[Scene 0]
end.time = 1
[Scene 1]
end.time = 2
";
        // File order is 2, 0, 1; we preserve textual order, not numeric.
        let sb = Storyboard::from_def(text);
        assert_eq!(sb.scenes.len(), 3);
        assert_eq!(sb.scenes[0].end_time, 3);
        assert_eq!(sb.scenes[1].end_time, 1);
        assert_eq!(sb.scenes[2].end_time, 2);
    }

    #[test]
    fn multiple_layers_sorted_by_index() {
        let text = "\
[Scene 0]
end.time = 10
layer2.anim = 102
layer0.anim = 100
layer1.anim = 101
";
        let sb = Storyboard::from_def(text);
        let layers = &sb.scenes[0].layers;
        assert_eq!(layers.len(), 3);
        assert_eq!(layers[0].index, 0);
        assert_eq!(layers[1].index, 1);
        assert_eq!(layers[2].index, 2);
        assert_eq!(layers[0].anim, Some(100));
        assert_eq!(layers[2].anim, Some(102));
    }

    // -----------------------------------------------------------------------
    // Proctor-added tests: edge cases, error paths, and MUGEN semantics.
    // -----------------------------------------------------------------------

    /// `layerall.pos` must never be mistaken for a `layerN.*` overlay layer.
    /// The `layer_index_of` guard requires digits-then-`.`; "all" has no digits.
    #[test]
    fn layerall_pos_is_not_a_layer() {
        let text = "\
[Scene 0]
end.time = 10
layerall.pos = 5,6
";
        let sb = Storyboard::from_def(text);
        assert_eq!(sb.scenes[0].layerall_pos, Some((5.0, 6.0)));
        assert!(
            sb.scenes[0].layers.is_empty(),
            "layerall.* must not create a SceneLayer"
        );
    }

    /// `layerall.pos` is parsed as `Option` so the player can implement MUGEN's
    /// carry-over: an explicit value is `Some`, an omitted value is `None`
    /// (inherit), and an explicit `0,0` is `Some((0,0))` (do NOT inherit).
    #[test]
    fn layerall_pos_absent_vs_explicit_zero() {
        // Scene 0 sets it explicitly; scene 1 omits it; scene 2 sets it to 0,0.
        let text = "\
[Scene 0]
end.time = 10
layerall.pos = 160,0
[Scene 1]
end.time = 10
[Scene 2]
end.time = 10
layerall.pos = 0,0
";
        let sb = Storyboard::from_def(text);
        assert_eq!(sb.scenes[0].layerall_pos, Some((160.0, 0.0)));
        assert_eq!(
            sb.scenes[1].layerall_pos, None,
            "an omitted layerall.pos must be None so the player inherits it"
        );
        assert_eq!(
            sb.scenes[2].layerall_pos,
            Some((0.0, 0.0)),
            "an explicit 0,0 is Some, not None — it must NOT inherit"
        );
        // The standalone fallback collapses None to (0,0).
        assert_eq!(sb.scenes[1].effective_layerall_pos(), (0.0, 0.0));
    }

    /// Two-digit (and higher) layer indices parse correctly. MUGEN documents
    /// layers 0-9, but the parser uses `u32` and should not silently truncate.
    #[test]
    fn multi_digit_layer_index() {
        let text = "\
[Scene 0]
end.time = 10
layer10.anim = 510
layer0.anim = 500
";
        let sb = Storyboard::from_def(text);
        let layers = &sb.scenes[0].layers;
        assert_eq!(layers.len(), 2);
        // Sorted numerically: 0 before 10.
        assert_eq!(layers[0].index, 0);
        assert_eq!(layers[1].index, 10);
        assert_eq!(layers[1].anim, Some(510));
    }

    /// A layer can carry a static `spriteno` instead of an `anim`. Both are
    /// optional and independent.
    #[test]
    fn layer_spriteno_and_endtime() {
        let text = "\
[Scene 0]
end.time = 100
layer0.spriteno = 3,7
layer0.endtime = 90
";
        let sb = Storyboard::from_def(text);
        let l = &sb.scenes[0].layers[0];
        assert_eq!(l.spriteno, Some((3, 7)));
        assert_eq!(l.anim, None);
        assert_eq!(l.endtime, Some(90));
        assert_eq!(l.starttime, 0); // unspecified -> shown from start
    }

    /// `fadein.col` / `fadeout.col` are parsed independently of the times.
    #[test]
    fn fade_colors_parsed() {
        let text = "\
[Scene 0]
end.time = 10
fadein.time = 15
fadein.col = 255,255,255
fadeout.time = 30
fadeout.col = 0,0,0
";
        let sb = Storyboard::from_def(text);
        let s = &sb.scenes[0];
        assert_eq!(s.fadein_col, Some((255, 255, 255)));
        assert_eq!(s.fadeout_col, Some((0, 0, 0)));
    }

    /// `bgm` is captured as an optional string and empty values are dropped.
    #[test]
    fn bgm_optional_and_empty_dropped() {
        let with = Storyboard::from_def("[Scene 0]\nend.time = 1\nbgm = song.mp3\n");
        assert_eq!(with.scenes[0].bgm.as_deref(), Some("song.mp3"));

        let empty = Storyboard::from_def("[Scene 0]\nend.time = 1\nbgm =\n");
        assert_eq!(empty.scenes[0].bgm, None, "empty bgm must be None");
    }

    /// A `bg.name` that is present but empty must collapse to `None`, not
    /// `Some("")` (which would never match a BgGroup).
    #[test]
    fn empty_bg_name_is_none() {
        let sb = Storyboard::from_def("[Scene 0]\nend.time = 1\nbg.name =\n");
        assert_eq!(sb.scenes[0].bg_name, None);
    }

    /// All MUGEN-style truthy boolean spellings map to `true`; everything else
    /// (including garbage and absence) maps to `false`.
    #[test]
    fn mask_boolean_spellings() {
        let case = |v: &str| {
            let text = format!("[BGDef]\n[BG L]\nmask = {v}\n");
            Storyboard::from_def(&text).bg_groups[0].layers[0].mask
        };
        assert!(case("1"));
        assert!(case("true"));
        assert!(case("TRUE"));
        assert!(case("yes"));
        assert!(case("on"));
        assert!(!case("0"));
        assert!(!case("false"));
        assert!(!case("garbage"));
        // Absent entirely -> false.
        let sb = Storyboard::from_def("[BGDef]\n[BG L]\nspriteno = 0,0\n");
        assert!(!sb.bg_groups[0].layers[0].mask);
    }

    /// A BG layer with no `type` key defaults to `"normal"` (MUGEN's default),
    /// and an explicit empty `type` also falls back to `"normal"`.
    #[test]
    fn bg_layer_kind_defaults_to_normal() {
        let sb = Storyboard::from_def("[BGDef]\n[BG L]\nspriteno = 0,0\n");
        assert_eq!(sb.bg_groups[0].layers[0].kind, "normal");

        let sb2 = Storyboard::from_def("[BGDef]\n[BG L]\ntype =\n");
        assert_eq!(sb2.bg_groups[0].layers[0].kind, "normal");
    }

    /// A scalar velocity applies to x with y left at 0; a pair applies to both.
    /// Negative and fractional scalars are honored.
    #[test]
    fn velocity_scalar_and_negative() {
        let scalar = Storyboard::from_def("[BGDef]\n[BG L]\nvelocity = -3.5\n");
        assert_eq!(scalar.bg_groups[0].layers[0].velocity, (-3.5, 0.0));

        let none = Storyboard::from_def("[BGDef]\n[BG L]\nspriteno = 0,0\n");
        assert_eq!(none.bg_groups[0].layers[0].velocity, (0.0, 0.0));
    }

    /// `start` accepts negative coordinates (intro.def uses start = -7000,48).
    #[test]
    fn start_negative_coords() {
        let sb = Storyboard::from_def("[BGDef]\n[BG L]\nstart = -7000,48\n");
        assert_eq!(sb.bg_groups[0].layers[0].start, (-7000.0, 48.0));
    }

    /// Color components are clamped to 0..=255 on both ends and a short triple
    /// (< 3 components) yields `None`.
    #[test]
    fn color_clamp_and_short_triple() {
        let clamp = Storyboard::from_def("[Scene 0]\nend.time = 1\nclearcolor = -10,300,128\n");
        assert_eq!(clamp.scenes[0].clearcolor, Some((0, 255, 128)));

        let short = Storyboard::from_def("[Scene 0]\nend.time = 1\nclearcolor = 10,20\n");
        assert_eq!(short.scenes[0].clearcolor, None);
    }

    /// Unknown / extra keys present in the real motif files (tilespacing, id,
    /// window, bgm.loop) must be silently ignored without disturbing the keys we
    /// do read.
    #[test]
    fn extra_unknown_keys_ignored() {
        let text = "\
[SceneDef]
spr = x.sff
[Scene 0]
end.time = 10
bgm = m.mp3
bgm.loop = 0
[BG0Def]
[BG0 L]
id = 1
type = normal
spriteno = 0,0
start = 0,240
window = 0,24, 319,215
tilespacing = 480
velocity = 6
";
        let sb = Storyboard::from_def(text);
        assert_eq!(sb.sprite_path, "x.sff");
        assert_eq!(sb.scenes[0].end_time, 10);
        assert_eq!(sb.scenes[0].bgm.as_deref(), Some("m.mp3"));
        let l = &sb.bg_groups[0].layers[0];
        assert_eq!(l.spriteno, (0, 0));
        assert_eq!(l.start, (0.0, 240.0));
        assert_eq!(l.velocity, (6.0, 0.0));
    }

    /// `[BG<n>Ctrl ...]` / `[BG<n>CtrlDef ...]` controller sections (present in
    /// credits.def) must NOT be misattributed as background layers of the BG
    /// group, and must NOT spawn a spurious group of their own.
    #[test]
    fn bg_controllers_not_treated_as_layers() {
        let text = "\
[Scene 0]
end.time = 1600
bg.name = BG0
[BG0Def]
[BG0 Credits]
type = normal
spriteno = 0,0
[BG0CtrlDef Credits]
CtrlID = 1
[BG0Ctrl Start scrolling]
type = VelSet
y = -1
[BG0Ctrl Stop scrolling]
type = VelSet
y = 0
";
        let sb = Storyboard::from_def(text);
        // Exactly one BG group from [BG0Def]; the CtrlDef does not end in "Def"
        // as a group prefix the way [BG0Def] does (it is "bg0ctrldef credits").
        let bg0: Vec<&BgGroup> = sb
            .bg_groups
            .iter()
            .filter(|g| g.name.eq_ignore_ascii_case("BG0"))
            .collect();
        assert_eq!(bg0.len(), 1, "only one BG0 group expected");
        // Only the real "Credits" layer — controllers excluded.
        assert_eq!(
            bg0[0].layers.len(),
            1,
            "controllers must not become layers, got {:?}",
            bg0[0].layers.iter().map(|l| &l.name).collect::<Vec<_>>()
        );
        assert_eq!(bg0[0].layers[0].name, "Credits");
    }

    /// Embedded `[Begin Action N]` blocks are extracted even when interleaved
    /// with non-action sections, and the surrounding scene key/values are not
    /// fed to the AIR parser.
    #[test]
    fn embedded_actions_interleaved_with_scenes() {
        let text = "\
[Scene 0]
end.time = 45
layer0.anim = 10
[Begin Action 10]
10,0, 0,0, -1
[Scene 1]
end.time = 30
[Begin Action 11]
11,0, 0,0, -1
";
        let sb = Storyboard::from_def(text);
        assert!(sb.animations.contains_key(&10));
        assert!(sb.animations.contains_key(&11));
        assert_eq!(sb.animations[&10].frames.len(), 1);
        // The layer references an action that was parsed.
        assert_eq!(sb.scenes[0].layers[0].anim, Some(10));
        assert!(sb.animations.contains_key(&sb.scenes[0].layers[0].anim.unwrap()));
    }

    /// A storyboard with no embedded action blocks yields an empty animation map
    /// (and does not invoke the AIR parser on empty text).
    #[test]
    fn no_embedded_actions_yields_empty_map() {
        let sb = Storyboard::from_def("[Scene 0]\nend.time = 10\nlayer0.spriteno = 0,0\n");
        assert!(sb.animations.is_empty());
    }

    /// `[Begin Action]` with no number, or a non-numeric number, is not a valid
    /// action header and must not produce an animation entry.
    #[test]
    fn malformed_begin_action_headers_skipped() {
        let text = "\
[Begin Action]
0,0, 0,0, -1
[Begin Action foo]
0,0, 0,0, -1
";
        let sb = Storyboard::from_def(text);
        assert!(
            sb.animations.is_empty(),
            "headers without a valid number must be ignored"
        );
    }

    /// Section headers are matched case-insensitively, matching DefFile and
    /// MUGEN semantics. A lowercased `[scene 0]` still parses as scene 0.
    #[test]
    fn case_insensitive_headers() {
        let text = "\
[info]
LOCALCOORD = 640,480
[scenedef]
SPR = mix.sff
[scene 0]
END.TIME = 99
[bg0def]
[BG0 lo]
SPRITENO = 2,3
";
        let sb = Storyboard::from_def(text);
        assert_eq!(sb.localcoord, (640, 480));
        assert_eq!(sb.sprite_path, "mix.sff");
        assert_eq!(sb.scenes.len(), 1);
        assert_eq!(sb.scenes[0].end_time, 99);
        assert_eq!(sb.bg_groups.len(), 1);
        assert_eq!(sb.bg_groups[0].layers[0].spriteno, (2, 3));
    }

    /// `[SceneDef]` itself, and the bare `[Def]`/`[Info]` sections, must never be
    /// mistaken for background groups.
    #[test]
    fn scenedef_and_info_not_bg_groups() {
        let text = "\
[Info]
localcoord = 320,240
[SceneDef]
spr = x.sff
[Scene 0]
end.time = 10
";
        let sb = Storyboard::from_def(text);
        assert!(
            sb.bg_groups.is_empty(),
            "no real BG groups, got {:?}",
            sb.bg_groups.iter().map(|g| &g.name).collect::<Vec<_>>()
        );
    }

    /// `startscene` is parsed when present and defaults to 0 when absent or
    /// malformed.
    #[test]
    fn startscene_default_and_malformed() {
        let present = Storyboard::from_def("[SceneDef]\nstartscene = 3\n");
        assert_eq!(present.start_scene, 3);

        let absent = Storyboard::from_def("[SceneDef]\nspr = x.sff\n");
        assert_eq!(absent.start_scene, 0);

        let bad = Storyboard::from_def("[SceneDef]\nstartscene = xyz\n");
        assert_eq!(bad.start_scene, 0);
    }

    /// A BG group declared via `[<name>Def]` but with no following layer sections
    /// is still emitted, with an empty layer list (degrade safely).
    #[test]
    fn bg_group_with_no_layers() {
        let sb = Storyboard::from_def("[Scene 0]\nend.time = 1\nbg.name = Empty\n[EmptyDef]\n");
        let g = sb
            .bg_groups
            .iter()
            .find(|g| g.name.eq_ignore_ascii_case("Empty"));
        assert!(g.is_some(), "group from [EmptyDef] should exist");
        assert!(g.unwrap().layers.is_empty());
    }

    /// Whitespace around values and around the `=` is tolerated (DefFile trims),
    /// including padded integer pairs and colors.
    #[test]
    fn whitespace_tolerance() {
        let text = "\
[Info]
localcoord   =    320 , 240
[Scene 0]
end.time =   240
clearcolor =  10 , 20 , 30
";
        let sb = Storyboard::from_def(text);
        assert_eq!(sb.localcoord, (320, 240));
        assert_eq!(sb.scenes[0].end_time, 240);
        assert_eq!(sb.scenes[0].clearcolor, Some((10, 20, 30)));
    }

    /// `Storyboard::load` returns `Err` (never panics) when the file is absent.
    #[test]
    fn load_missing_file_errors() {
        let res = Storyboard::load(Path::new(
            "/nonexistent/definitely/not/here/storyboard.def",
        ));
        assert!(res.is_err(), "loading a missing file must return Err");
    }

    /// A pathological file consisting only of comments and blank lines yields an
    /// empty, non-panicking model.
    #[test]
    fn only_comments_and_blanks() {
        let text = "; just a comment\n\n  ; another\n\n";
        let sb = Storyboard::from_def(text);
        assert!(sb.scenes.is_empty());
        assert!(sb.bg_groups.is_empty());
        assert!(sb.animations.is_empty());
        assert_eq!(sb.localcoord, (320, 240));
        assert_eq!(sb.sprite_path, "");
    }

    /// Documented behavior: scene ordering follows *textual* order, so a scene
    /// whose layers reference an action defined *later* in the file still has
    /// that action available (animations are gathered globally).
    #[test]
    fn forward_anim_reference_resolves() {
        let text = "\
[Scene 0]
end.time = 45
layer0.anim = 200
[Begin Action 200]
1,0, 0,0, -1
";
        let sb = Storyboard::from_def(text);
        let referenced = sb.scenes[0].layers[0].anim.unwrap();
        assert!(sb.animations.contains_key(&referenced));
    }
}
