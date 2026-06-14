//! # fp-stage
//!
//! Stage system for the Fighters Paradise engine. Loads MUGEN stage definitions
//! (`.def`), manages background elements (normal, animated, parallax), and a
//! camera that follows the two fighters' midpoint clamped to the stage bounds.
//!
//! A MUGEN stage `.def` is an INI-style file (the same grammar [`fp_formats::def`]
//! parses) with these sections:
//!
//! ```text
//! [Info]            ; name / author
//! [Camera]          ; bound{left,right,top,bottom}, tension, verticalfollow, floortension
//! [PlayerInfo]      ; p1/p2 startx/starty + per-player x/z boundaries
//! [StageInfo]       ; zoffset, localcoord, xscale/yscale
//! [BGdef]           ; spr = the stage's sprite (SFF) file
//! [BG <name>]       ; one per background element: type, spriteno, start, delta, tile, ...
//! ```
//!
//! Layering: `[BG ...]` elements appear **in file order**, and each carries a
//! `layerno` (0 = drawn behind the fighters, 1 = drawn in front). Because order
//! and multiplicity matter — and a stage may legitimately have several `[BG]`
//! sections with the same (or empty) name — the BG list is parsed from an
//! order-preserving section walk rather than from a name-keyed map.
//!
//! ## Never crash on bad content
//!
//! Every parser here follows the workspace contract: bad numbers, unknown keys,
//! and unknown element types are `tracing::warn!`-logged and skipped (substituting
//! a safe default), never panicking. Loading only returns `Err` when the file
//! itself cannot be read.

#![warn(missing_docs)]

use std::path::{Path, PathBuf};

use fp_core::{FpResult, Vec2};

/// Free-text metadata about a stage, from its `[Info]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StageInfoText {
    /// The stage's display name (`name`), empty if unspecified.
    pub name: String,
    /// The stage's author (`author`), empty if unspecified.
    pub author: String,
}

/// Camera configuration from the `[Camera]` section.
///
/// The four `bound*` values are the world-space limits the camera's view may
/// scroll to. `tension` / `floortension` / `verticalfollow` describe how the
/// camera reacts to the fighters; they are parsed and preserved for fidelity even
/// though the current follow model ([`Stage::camera_follow_x`]) only uses the
/// horizontal bounds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    /// Leftmost world X the camera may scroll to (`boundleft`).
    pub bound_left: f32,
    /// Rightmost world X the camera may scroll to (`boundright`).
    pub bound_right: f32,
    /// Topmost world Y the camera may scroll to (`boundhigh`/`boundtop`).
    pub bound_top: f32,
    /// Bottommost world Y the camera may scroll to (`boundlow`/`boundbottom`).
    pub bound_bottom: f32,
    /// Horizontal "tension" margin: how close a fighter gets to the screen edge
    /// before the camera starts scrolling (`tension`).
    pub tension: f32,
    /// Vertical-follow factor in `[0, 1]`: how strongly the camera tracks a
    /// jumping fighter's height (`verticalfollow`).
    pub vertical_follow: f32,
    /// Vertical "tension" near the floor (`floortension`).
    pub floor_tension: f32,
}

impl Default for Camera {
    /// A neutral camera: a symmetric 200px horizontal range, no vertical travel,
    /// and zero tension — a sane fallback when `[Camera]` is absent.
    fn default() -> Self {
        Self {
            bound_left: -200.0,
            bound_right: 200.0,
            bound_top: 0.0,
            bound_bottom: 0.0,
            tension: 0.0,
            vertical_follow: 0.0,
            floor_tension: 0.0,
        }
    }
}

/// Player start positions and boundaries from the `[PlayerInfo]` section.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayerInfo {
    /// Player 1's start position (`p1startx`, `p1starty`).
    pub p1_start: Vec2<f32>,
    /// Player 2's start position (`p2startx`, `p2starty`).
    pub p2_start: Vec2<f32>,
    /// Leftmost world X a fighter may be pushed to (`leftbound`).
    pub left_bound: f32,
    /// Rightmost world X a fighter may be pushed to (`rightbound`).
    pub right_bound: f32,
}

impl Default for PlayerInfo {
    /// Fighters facing each other 60px either side of the origin, with a
    /// symmetric 200px push range — the same defaults the app already uses.
    fn default() -> Self {
        Self {
            p1_start: Vec2::new(-60.0, 0.0),
            p2_start: Vec2::new(60.0, 0.0),
            left_bound: -200.0,
            right_bound: 200.0,
        }
    }
}

/// Stage geometry from the `[StageInfo]` section.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StageInfo {
    /// Vertical offset of the floor in world pixels (`zoffset`).
    pub z_offset: f32,
    /// The coordinate space the stage was authored in (`localcoord = w, h`).
    pub local_coord: Vec2<f32>,
    /// Horizontal scale applied to the whole stage (`xscale`).
    pub x_scale: f32,
    /// Vertical scale applied to the whole stage (`yscale`).
    pub y_scale: f32,
}

impl Default for StageInfo {
    /// MUGEN's classic 320x240 authoring space, no floor offset, unit scale.
    fn default() -> Self {
        Self {
            z_offset: 0.0,
            local_coord: Vec2::new(320.0, 240.0),
            x_scale: 1.0,
            y_scale: 1.0,
        }
    }
}

/// The kind of a `[BG ...]` element (`type = ...`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BgType {
    /// A single static sprite (`type = normal`). The default for an unspecified
    /// or unknown type.
    #[default]
    Normal,
    /// A parallax element (`type = parallax`) — scrolls at a fraction of the
    /// camera given by its `delta`.
    Parallax,
    /// An animated element (`type = anim`) cycling through an AIR action.
    Anim,
}

/// Which draw layer a `[BG ...]` element belongs to (`layerno = ...`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BgLayer {
    /// Drawn behind the fighters (`layerno = 0`). The default.
    #[default]
    Back,
    /// Drawn in front of the fighters (`layerno = 1`).
    Front,
}

/// A single background element parsed from a `[BG <name>]` section.
#[derive(Debug, Clone, PartialEq)]
pub struct BgElement {
    /// The element name (the text after `BG` in the section header), possibly
    /// empty.
    pub name: String,
    /// The element kind (`type`).
    pub kind: BgType,
    /// `spriteno = group, image` — the sprite to draw (for `normal`/`parallax`).
    /// The start sprite of the AIR action for `anim` types in real MUGEN; here it
    /// is the directly-referenced sprite.
    pub sprite: Vec2<i32>,
    /// AIR action number for `type = anim` (`actionno`), `None` otherwise.
    pub action_no: Option<i32>,
    /// `start = x, y` — the element's world position at camera origin.
    pub start: Vec2<f32>,
    /// `delta = dx, dy` — the parallax scroll factor. `1.0` scrolls with the
    /// camera; `0.5` scrolls half as fast; `0.0` is pinned to the screen.
    pub delta: Vec2<f32>,
    /// `tile = x, y` — number of times the element tiles (`0` = no tiling /
    /// single draw on that axis).
    pub tile: Vec2<i32>,
    /// `velocity = x, y` — auto-scroll velocity in world px/tick.
    pub velocity: Vec2<f32>,
    /// The draw layer (`layerno`).
    pub layer: BgLayer,
    /// `mask = 0/1` — whether palette index 0 is treated as transparent. MUGEN
    /// defaults this on; preserved for fidelity.
    pub mask: bool,
}

impl Default for BgElement {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: BgType::Normal,
            sprite: Vec2::new(0, 0),
            action_no: None,
            start: Vec2::new(0.0, 0.0),
            // Delta defaults to (1, 1): scroll 1:1 with the camera (no parallax).
            delta: Vec2::new(1.0, 1.0),
            tile: Vec2::new(0, 0),
            velocity: Vec2::new(0.0, 0.0),
            layer: BgLayer::Back,
            mask: true,
        }
    }
}

/// The `[BGdef]` section: the sprite (SFF) file every `[BG]` element draws from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BgDef {
    /// `spr = <path>` — the stage's SFF, resolved relative to the `.def`'s
    /// directory. `None` if unspecified.
    pub sprite_path: Option<PathBuf>,
}

/// A fully-parsed MUGEN stage: metadata, camera, player info, geometry, the
/// background SFF reference, and the ordered list of background elements.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stage {
    /// `[Info]` metadata.
    pub info: StageInfoText,
    /// `[Camera]` configuration.
    pub camera: Camera,
    /// `[PlayerInfo]` start positions and boundaries.
    pub player_info: PlayerInfo,
    /// `[StageInfo]` geometry.
    pub stage_info: StageInfo,
    /// `[BGdef]` sprite-file reference.
    pub bgdef: BgDef,
    /// `[BG ...]` elements, **in file order**.
    pub backgrounds: Vec<BgElement>,
}

impl Stage {
    /// Loads and parses a stage `.def` from `path`.
    ///
    /// Returns `Err` only when the file cannot be read; a syntactically odd or
    /// partial stage parses to a [`Stage`] with safe defaults (every malformed
    /// value is warned and skipped).
    pub fn load(path: &Path) -> FpResult<Self> {
        let text = std::fs::read_to_string(path)?;
        Ok(Self::parse(&text, path.parent()))
    }

    /// Parses a stage from raw `.def` text. `base_dir`, when given, is the
    /// directory the `[BGdef] spr` path is resolved against (the `.def`'s own
    /// directory); pass `None` to leave `spr` as a bare relative path.
    ///
    /// Never fails: this is the in-memory counterpart of [`Stage::load`] used by
    /// tests and any caller that already holds the file text.
    pub fn parse(text: &str, base_dir: Option<&Path>) -> Self {
        let sections = parse_sections(text);
        let mut stage = Stage::default();

        for section in &sections {
            let lname = section.name.to_ascii_lowercase();
            // A `[BG <name>]` header begins with "bg " (or is exactly "bg").
            if lname == "bg" || lname.starts_with("bg ") {
                if let Some(bg) = parse_bg(section) {
                    stage.backgrounds.push(bg);
                }
                continue;
            }
            match lname.as_str() {
                "info" => parse_info(section, &mut stage.info),
                "camera" => parse_camera(section, &mut stage.camera),
                "playerinfo" => parse_player_info(section, &mut stage.player_info),
                "stageinfo" => parse_stage_info(section, &mut stage.stage_info),
                "bgdef" => parse_bgdef(section, base_dir, &mut stage.bgdef),
                other => {
                    tracing::warn!("stage: ignoring unknown section [{other}]");
                }
            }
        }

        tracing::info!(
            "stage parsed: name={:?}, {} background element(s)",
            stage.info.name,
            stage.backgrounds.len(),
        );
        stage
    }

    /// Computes the camera's world X so its view follows the midpoint of the two
    /// fighters, clamped to the camera's horizontal bounds.
    ///
    /// `p1_x`/`p2_x` are the fighters' world X positions. The returned value is
    /// the world X the camera centers on; it is clamped to
    /// `[bound_left, bound_right]` so the view never scrolls past the authored
    /// stage edges. Robust against an inverted bound pair (`left > right`): it
    /// clamps to the normalized `[min, max]`.
    pub fn camera_follow_x(&self, p1_x: f32, p2_x: f32) -> f32 {
        let midpoint = (p1_x + p2_x) * 0.5;
        let lo = self.camera.bound_left.min(self.camera.bound_right);
        let hi = self.camera.bound_left.max(self.camera.bound_right);
        midpoint.clamp(lo, hi)
    }
}

/// Computes the on-screen X of a background element given the camera's world X.
///
/// The element's `delta.x` is its parallax factor: a `delta` of `1.0` scrolls the
/// element 1:1 with the camera (it stays fixed in the world), `0.5` scrolls it
/// half as fast (distant background), and `0.0` pins it to the screen. The screen
/// X is the element's authored `start.x` minus the camera offset scaled by
/// `delta.x`:
///
/// ```text
/// screen_x = start.x - camera_x * delta.x
/// ```
///
/// This is a pure function (no GPU state) so the parallax math is unit-testable
/// independently of rendering.
pub fn parallax_screen_x(start_x: f32, delta_x: f32, camera_x: f32) -> f32 {
    start_x - camera_x * delta_x
}

/// The vertical counterpart of [`parallax_screen_x`] for `delta.y`.
pub fn parallax_screen_y(start_y: f32, delta_y: f32, camera_y: f32) -> f32 {
    start_y - camera_y * delta_y
}

// ---------------------------------------------------------------------------
// Ordered section parsing
// ---------------------------------------------------------------------------

/// One parsed `.def` section: its header name plus its key/value pairs, **in
/// order**. Unlike a name-keyed map this preserves both the order of `[BG]`
/// sections and any duplicate section names a stage may carry.
struct Section {
    /// The raw section name (the text between `[` and `]`, trimmed), preserving
    /// its original case so a `[BG <name>]`'s name survives.
    name: String,
    /// The key/value pairs, lowercased keys, in file order.
    entries: Vec<(String, String)>,
}

impl Section {
    /// Returns the first value for `key` (case-insensitive), if present.
    fn get(&self, key: &str) -> Option<&str> {
        let key = key.to_ascii_lowercase();
        self.entries
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// Splits stage `.def` text into ordered [`Section`]s, applying the same
/// tolerances as [`fp_formats::def`]: a leading UTF-8 BOM, CRLF endings,
/// `;`/`//`/`#` comments, surrounding quotes, and case-insensitive keys. Splits
/// each entry on the **first** `=` only so values survive verbatim. Lines before
/// the first `[Section]` header are ignored.
fn parse_sections(text: &str) -> Vec<Section> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut sections: Vec<Section> = Vec::new();

    for raw_line in text.lines() {
        let line = strip_comment(raw_line).trim();
        if line.is_empty() {
            continue;
        }

        if line.starts_with('[') && line.ends_with(']') {
            let name = line[1..line.len() - 1].trim().to_string();
            sections.push(Section {
                name,
                entries: Vec::new(),
            });
            continue;
        }

        if let Some(eq) = line.find('=') {
            if let Some(section) = sections.last_mut() {
                let key = line[..eq].trim().to_ascii_lowercase();
                let value = strip_quotes(line[eq + 1..].trim());
                section.entries.push((key, value));
            }
        }
    }

    sections
}

/// Strips `;`, `//`, and `#` comments from a line (whichever appears first).
fn strip_comment(line: &str) -> &str {
    let mut end = line.len();
    for marker in [";", "//", "#"] {
        if let Some(pos) = line.find(marker) {
            end = end.min(pos);
        }
    }
    &line[..end]
}

/// Strips a single pair of surrounding double quotes, if present.
fn strip_quotes(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// Per-section parsers
// ---------------------------------------------------------------------------

/// Parses a single number from `section[key]`, warning and returning `None` on a
/// malformed value so the caller can keep its default.
fn num<T: std::str::FromStr>(section: &Section, key: &str) -> Option<T> {
    let raw = section.get(key)?;
    match raw.trim().parse::<T>() {
        Ok(v) => Some(v),
        Err(_) => {
            tracing::warn!(
                "stage [{}]: ignoring malformed value for `{key}` = {raw:?}",
                section.name
            );
            None
        }
    }
}

/// Parses the `n`th (0-based) comma-separated component of `section[key]` as a
/// number, warning on a malformed component. A missing key or too-few components
/// yields `None` (the caller keeps its default).
fn comp<T: std::str::FromStr>(section: &Section, key: &str, n: usize) -> Option<T> {
    let raw = section.get(key)?;
    let part = raw.split(',').nth(n)?.trim();
    if part.is_empty() {
        return None;
    }
    match part.parse::<T>() {
        Ok(v) => Some(v),
        Err(_) => {
            tracing::warn!(
                "stage [{}]: ignoring malformed component {n} of `{key}` = {raw:?}",
                section.name
            );
            None
        }
    }
}

fn parse_info(section: &Section, info: &mut StageInfoText) {
    if let Some(name) = section.get("name") {
        info.name = name.to_string();
    }
    if let Some(author) = section.get("author") {
        info.author = author.to_string();
    }
}

fn parse_camera(section: &Section, camera: &mut Camera) {
    if let Some(v) = num(section, "boundleft") {
        camera.bound_left = v;
    }
    if let Some(v) = num(section, "boundright") {
        camera.bound_right = v;
    }
    // MUGEN spells the vertical bounds `boundhigh`/`boundlow`; accept the more
    // intuitive `boundtop`/`boundbottom` aliases too.
    if let Some(v) = num(section, "boundhigh").or_else(|| num(section, "boundtop")) {
        camera.bound_top = v;
    }
    if let Some(v) = num(section, "boundlow").or_else(|| num(section, "boundbottom")) {
        camera.bound_bottom = v;
    }
    if let Some(v) = num(section, "tension") {
        camera.tension = v;
    }
    if let Some(v) = num(section, "verticalfollow") {
        camera.vertical_follow = v;
    }
    if let Some(v) = num(section, "floortension") {
        camera.floor_tension = v;
    }
}

fn parse_player_info(section: &Section, pi: &mut PlayerInfo) {
    if let Some(v) = num(section, "p1startx") {
        pi.p1_start.x = v;
    }
    if let Some(v) = num(section, "p1starty") {
        pi.p1_start.y = v;
    }
    if let Some(v) = num(section, "p2startx") {
        pi.p2_start.x = v;
    }
    if let Some(v) = num(section, "p2starty") {
        pi.p2_start.y = v;
    }
    if let Some(v) = num(section, "leftbound") {
        pi.left_bound = v;
    }
    if let Some(v) = num(section, "rightbound") {
        pi.right_bound = v;
    }
}

fn parse_stage_info(section: &Section, si: &mut StageInfo) {
    if let Some(v) = num(section, "zoffset") {
        si.z_offset = v;
    }
    if let Some(v) = comp(section, "localcoord", 0) {
        si.local_coord.x = v;
    }
    if let Some(v) = comp(section, "localcoord", 1) {
        si.local_coord.y = v;
    }
    if let Some(v) = num(section, "xscale") {
        si.x_scale = v;
    }
    if let Some(v) = num(section, "yscale") {
        si.y_scale = v;
    }
}

fn parse_bgdef(section: &Section, base_dir: Option<&Path>, bgdef: &mut BgDef) {
    if let Some(spr) = section.get("spr") {
        let spr = spr.trim();
        if !spr.is_empty() {
            let resolved = match base_dir {
                Some(dir) => dir.join(spr),
                None => PathBuf::from(spr),
            };
            bgdef.sprite_path = Some(resolved);
        }
    }
}

/// Parses one `[BG <name>]` section into a [`BgElement`]. Always succeeds
/// (returning `Some`) once it has a header — every field falls back to a safe
/// default, and malformed numbers are warned and skipped — so a partial element
/// still renders rather than dropping the whole stage.
fn parse_bg(section: &Section) -> Option<BgElement> {
    // The element name is the header text after the leading "BG".
    let name = section
        .name
        .get(2..)
        .map(|rest| rest.trim().to_string())
        .unwrap_or_default();

    let mut bg = BgElement {
        name,
        ..Default::default()
    };

    if let Some(t) = section.get("type") {
        bg.kind = match t.trim().to_ascii_lowercase().as_str() {
            "normal" => BgType::Normal,
            "parallax" => BgType::Parallax,
            "anim" => BgType::Anim,
            other => {
                tracing::warn!(
                    "stage [{}]: unknown BG type {other:?}; treating as normal",
                    section.name
                );
                BgType::Normal
            }
        };
    }

    if let Some(g) = comp(section, "spriteno", 0) {
        bg.sprite.x = g;
    }
    if let Some(i) = comp(section, "spriteno", 1) {
        bg.sprite.y = i;
    }

    bg.action_no = num(section, "actionno");

    if let Some(v) = comp(section, "start", 0) {
        bg.start.x = v;
    }
    if let Some(v) = comp(section, "start", 1) {
        bg.start.y = v;
    }

    if let Some(v) = comp(section, "delta", 0) {
        bg.delta.x = v;
    }
    if let Some(v) = comp(section, "delta", 1) {
        bg.delta.y = v;
    }

    if let Some(v) = comp(section, "tile", 0) {
        bg.tile.x = v;
    }
    if let Some(v) = comp(section, "tile", 1) {
        bg.tile.y = v;
    }

    if let Some(v) = comp(section, "velocity", 0) {
        bg.velocity.x = v;
    }
    if let Some(v) = comp(section, "velocity", 1) {
        bg.velocity.y = v;
    }

    if let Some(layer) = num::<i32>(section, "layerno") {
        bg.layer = if layer >= 1 {
            BgLayer::Front
        } else {
            BgLayer::Back
        };
    }

    if let Some(mask) = num::<i32>(section, "mask") {
        bg.mask = mask != 0;
    }

    Some(bg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small but representative synthetic stage `.def` exercising every section
    /// plus an unknown section and an unknown key (which must be tolerated), two
    /// `[BG]` elements with distinct deltas/layers, and one with a bad number.
    const SYNTHETIC: &str = r#"
; a synthetic test stage
[Info]
name = "Test Stage"
author = "FP"
unknownkey = whatever        ; must be ignored, not fatal

[Camera]
boundleft = -300
boundright = 300
boundhigh = -100
boundlow = 0
tension = 50
verticalfollow = 0.2
floortension = 30

[PlayerInfo]
p1startx = -70
p1starty = 0
p2startx = 70
p2starty = 0
leftbound = -320
rightbound = 320

[StageInfo]
zoffset = 192
localcoord = 320, 240
xscale = 1
yscale = 1

[BGdef]
spr = stage.sff

[BG Sky]
type = normal
spriteno = 0, 0
start = 0, 0
delta = 0.5, 1.0
layerno = 0

[BG Floor]
type = parallax
spriteno = 1, 0
start = 0, 200
delta = 1.0, 1.0
tile = 1, 0
velocity = -2, 0
layerno = 1
mask = 0

[Foobar]            ; an unknown section — must be skipped
key = value

[BG Bad]
type = normal
spriteno = 2, 0
delta = notanumber, 1.0     ; malformed → keep default delta.x
"#;

    #[test]
    fn parses_info_camera_playerinfo_stageinfo() {
        let stage = Stage::parse(SYNTHETIC, None);

        assert_eq!(stage.info.name, "Test Stage");
        assert_eq!(stage.info.author, "FP");

        assert_eq!(stage.camera.bound_left, -300.0);
        assert_eq!(stage.camera.bound_right, 300.0);
        assert_eq!(stage.camera.bound_top, -100.0);
        assert_eq!(stage.camera.bound_bottom, 0.0);
        assert_eq!(stage.camera.tension, 50.0);
        assert!((stage.camera.vertical_follow - 0.2).abs() < 1e-6);
        assert_eq!(stage.camera.floor_tension, 30.0);

        assert_eq!(stage.player_info.p1_start, Vec2::new(-70.0, 0.0));
        assert_eq!(stage.player_info.p2_start, Vec2::new(70.0, 0.0));
        assert_eq!(stage.player_info.left_bound, -320.0);
        assert_eq!(stage.player_info.right_bound, 320.0);

        assert_eq!(stage.stage_info.z_offset, 192.0);
        assert_eq!(stage.stage_info.local_coord, Vec2::new(320.0, 240.0));
        assert_eq!(stage.stage_info.x_scale, 1.0);
        assert_eq!(stage.stage_info.y_scale, 1.0);
    }

    #[test]
    fn parses_bgdef_spr_relative_to_base_dir() {
        let base = Path::new("/stages/mystage");
        let stage = Stage::parse(SYNTHETIC, Some(base));
        assert_eq!(
            stage.bgdef.sprite_path.as_deref(),
            Some(Path::new("/stages/mystage/stage.sff"))
        );

        // Without a base dir the path stays relative.
        let stage_rel = Stage::parse(SYNTHETIC, None);
        assert_eq!(
            stage_rel.bgdef.sprite_path.as_deref(),
            Some(Path::new("stage.sff"))
        );
    }

    #[test]
    fn parses_bg_elements_in_order_with_deltas_and_layers() {
        let stage = Stage::parse(SYNTHETIC, None);
        // Three [BG ...] sections (Sky, Floor, Bad) — in file order.
        assert_eq!(stage.backgrounds.len(), 3);

        let sky = &stage.backgrounds[0];
        assert_eq!(sky.name, "Sky");
        assert_eq!(sky.kind, BgType::Normal);
        assert_eq!(sky.sprite, Vec2::new(0, 0));
        assert_eq!(sky.start, Vec2::new(0.0, 0.0));
        assert_eq!(sky.delta, Vec2::new(0.5, 1.0));
        assert_eq!(sky.layer, BgLayer::Back);
        assert!(sky.mask, "mask defaults on when unspecified");

        let floor = &stage.backgrounds[1];
        assert_eq!(floor.name, "Floor");
        assert_eq!(floor.kind, BgType::Parallax);
        assert_eq!(floor.sprite, Vec2::new(1, 0));
        assert_eq!(floor.start, Vec2::new(0.0, 200.0));
        assert_eq!(floor.delta, Vec2::new(1.0, 1.0));
        assert_eq!(floor.tile, Vec2::new(1, 0));
        assert_eq!(floor.velocity, Vec2::new(-2.0, 0.0));
        assert_eq!(floor.layer, BgLayer::Front);
        assert!(!floor.mask, "mask = 0 disables transparency");
    }

    #[test]
    fn malformed_bg_number_keeps_default() {
        let stage = Stage::parse(SYNTHETIC, None);
        let bad = &stage.backgrounds[2];
        assert_eq!(bad.name, "Bad");
        // `delta = notanumber, 1.0` → delta.x keeps the (1.0) default, delta.y = 1.0.
        assert_eq!(bad.delta, Vec2::new(1.0, 1.0));
    }

    #[test]
    fn unknown_section_and_key_are_tolerated_not_fatal() {
        // The presence of [Foobar] and `unknownkey` must not have dropped anything.
        let stage = Stage::parse(SYNTHETIC, None);
        assert_eq!(stage.info.name, "Test Stage");
        assert_eq!(stage.backgrounds.len(), 3);
    }

    #[test]
    fn empty_stage_yields_all_defaults() {
        let stage = Stage::parse("", None);
        assert_eq!(stage, Stage::default());
        assert!(stage.backgrounds.is_empty());
    }

    #[test]
    fn bom_and_crlf_and_comment_styles_tolerated() {
        let text = "\u{feff}[Info]\r\nname = \"Crlf\" // trailing\r\n# hash comment\r\n[BG]\r\ntype = normal\r\n";
        let stage = Stage::parse(text, None);
        assert_eq!(stage.info.name, "Crlf");
        assert_eq!(stage.backgrounds.len(), 1);
        assert_eq!(stage.backgrounds[0].name, "", "bare [BG] has empty name");
    }

    #[test]
    fn parallax_delta_half_moves_half_as_fast() {
        // delta = 0.5 → element shifts half a camera step.
        let at0 = parallax_screen_x(100.0, 0.5, 0.0);
        let at100 = parallax_screen_x(100.0, 0.5, 100.0);
        assert_eq!(at0, 100.0, "at camera 0 the element sits at its start");
        assert_eq!(at100, 50.0, "camera +100 moves a delta=0.5 element only -50");
        assert_eq!(at0 - at100, 50.0, "half the camera travel");
    }

    #[test]
    fn parallax_delta_one_moves_one_to_one() {
        // delta = 1.0 → element is world-fixed: it shifts exactly with the camera.
        let at0 = parallax_screen_x(0.0, 1.0, 0.0);
        let at100 = parallax_screen_x(0.0, 1.0, 100.0);
        assert_eq!(at0 - at100, 100.0, "full camera travel");
    }

    #[test]
    fn parallax_delta_zero_is_screen_fixed() {
        // delta = 0.0 → pinned to the screen regardless of the camera.
        assert_eq!(parallax_screen_x(40.0, 0.0, 0.0), 40.0);
        assert_eq!(parallax_screen_x(40.0, 0.0, 999.0), 40.0);
    }

    #[test]
    fn parallax_screen_y_matches_x_formula() {
        assert_eq!(parallax_screen_y(20.0, 0.5, 80.0), 20.0 - 40.0);
    }

    #[test]
    fn camera_follows_midpoint_clamped_to_bounds() {
        let mut stage = Stage::default();
        stage.camera.bound_left = -100.0;
        stage.camera.bound_right = 100.0;

        // Centered fighters → camera at the midpoint (0).
        assert_eq!(stage.camera_follow_x(-60.0, 60.0), 0.0);
        // Midpoint follows: (-20 + 80)/2 = 30.
        assert_eq!(stage.camera_follow_x(-20.0, 80.0), 30.0);
        // Both far right → midpoint clamps to the right bound.
        assert_eq!(stage.camera_follow_x(500.0, 600.0), 100.0);
        // Both far left → clamps to the left bound.
        assert_eq!(stage.camera_follow_x(-500.0, -600.0), -100.0);
    }

    #[test]
    fn camera_follow_handles_inverted_bounds() {
        // A stage that authored left > right must not produce NaN/empty clamp.
        let mut stage = Stage::default();
        stage.camera.bound_left = 100.0;
        stage.camera.bound_right = -100.0;
        let x = stage.camera_follow_x(0.0, 0.0);
        assert!((-100.0..=100.0).contains(&x));
    }
}
