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

use fp_core::{FpResult, Rect, Vec2};
use fp_formats::air::AnimAction;

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
    /// Runtime scroll offset, in world px, accumulated from [`BgElement::velocity`]
    /// one tick at a time by [`BgElement::advance_scroll`]. **Not parsed** — it
    /// starts at `(0, 0)` and is the engine's running auto-scroll position, added
    /// to the element's draw position on top of parallax. Kept on the element so a
    /// caller can advance the whole stage's scroll with [`Stage::advance_scroll`]
    /// once per tick and read it back when drawing.
    pub scroll: Vec2<f32>,
    /// Runtime animation clock, in game ticks since this element's AIR action
    /// started, advanced one tick at a time by [`BgElement::advance_anim`].
    /// **Not parsed** — it starts at `0` and is only meaningful for a
    /// [`BgType::Anim`] element (one with an [`action_no`](BgElement::action_no));
    /// a static element ignores it. The currently-displayed AIR element is read
    /// back from it via [`BgElement::current_anim_elem`] / [`anim_elem_at_tick`].
    pub anim_tick: i32,
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
            scroll: Vec2::new(0.0, 0.0),
            anim_tick: 0,
        }
    }
}

impl BgElement {
    /// Advances this element's runtime [`scroll`](BgElement::scroll) offset by one
    /// tick of its [`velocity`](BgElement::velocity).
    ///
    /// A zero-velocity element never moves (the offset stays put); a non-zero
    /// velocity accumulates linearly — after `n` calls the offset is `velocity * n`
    /// (plus its starting value). To keep the accumulator from growing without
    /// bound during long matches it is wrapped back into one tile period per axis
    /// once a tile size is known: pass the element's drawn sprite size as
    /// `tile_size`. Tiling repeats every `tile_size` px, so wrapping the offset
    /// modulo the tile size is visually identical while keeping the float small and
    /// precise. Pass a non-positive size on an axis (e.g. `0.0`) to disable
    /// wrapping on that axis (the raw accumulation is kept).
    pub fn advance_scroll(&mut self, tile_size: Vec2<f32>) {
        self.scroll.x = advance_axis(self.scroll.x, self.velocity.x, tile_size.x);
        self.scroll.y = advance_axis(self.scroll.y, self.velocity.y, tile_size.y);
    }

    /// Advances this element's [`anim_tick`](BgElement::anim_tick) clock by one
    /// game tick.
    ///
    /// `action` is the AIR action driving this element (resolved by the caller
    /// from the element's [`action_no`](BgElement::action_no) against the stage's
    /// own `.air`). The clock is only bumped while the action is *still looping or
    /// running*: once it has played out a non-looping action (its `loopstart`
    /// equals the frame count — i.e. there is no loop region) the clock is held at
    /// the action's total duration so the last frame stays on screen instead of
    /// the modulo wrapping it back to the start. A looping action (the usual case)
    /// advances forever; the wrap is applied by [`anim_elem_at_tick`] when the
    /// frame is selected. The clock saturates rather than overflowing.
    ///
    /// Only meaningful for a [`BgType::Anim`] element; a caller may skip calling it
    /// for static elements (their `anim_tick` simply stays `0`).
    pub fn advance_anim(&mut self, action: &AnimAction) {
        self.anim_tick = self.anim_tick.saturating_add(1);
        // Guard the clock against unbounded growth on a looping action by wrapping
        // it back into one loop period once it has cleared the action's total
        // duration. A non-looping action (no real loop region) is held at its end
        // instead so the final frame persists.
        let total = action_total_ticks(action);
        if total > 0 {
            let loop_ticks = action_loop_ticks(action);
            if loop_ticks > 0 {
                // Looping action: keep the clock in `[0, total)` ∪ the loop region
                // by subtracting whole loop periods once past the end. This is
                // visually identical (the loop region repeats) while bounding the
                // value.
                while self.anim_tick >= total {
                    self.anim_tick -= loop_ticks;
                }
            } else {
                // Non-looping (loopstart == frame count): pin to the last frame.
                self.anim_tick = self.anim_tick.min(total);
            }
        }
    }

    /// Returns the index of the AIR frame this element currently displays, given
    /// the AIR `action` it is bound to and its running
    /// [`anim_tick`](BgElement::anim_tick) clock. A convenience wrapper over
    /// [`anim_elem_at_tick`].
    pub fn current_anim_elem(&self, action: &AnimAction) -> usize {
        anim_elem_at_tick(action, self.anim_tick)
    }

    /// Returns the `(group, image)` sprite this element currently displays.
    ///
    /// For a [`BgType::Anim`] element this is the sprite of the AIR frame selected
    /// by [`current_anim_elem`](BgElement::current_anim_elem); the element's own
    /// [`sprite`](BgElement::sprite) field is the fallback used when the action has
    /// no usable frame (and for non-animated elements, whose drawn sprite is
    /// always their static `sprite`). The caller resolves the action from this
    /// element's [`action_no`](BgElement::action_no) against the stage's `.air`.
    pub fn current_anim_sprite(&self, action: &AnimAction) -> Vec2<i32> {
        match action.frames.get(self.current_anim_elem(action)) {
            Some(frame) => Vec2::new(frame.sprite.group() as i32, frame.sprite.image() as i32),
            None => self.sprite,
        }
    }
}

/// Sum of the `ticks` of every frame in `action`, treating a hold-forever frame
/// (`ticks <= 0`) as contributing `0` (it never ends, so nothing after it ever
/// begins). Saturates rather than overflowing. `0` for an empty action.
fn action_total_ticks(action: &AnimAction) -> i32 {
    action
        .frames
        .iter()
        .fold(0i32, |acc, f| acc.saturating_add(f.ticks.max(0)))
}

/// Sum of the `ticks` of the frames in `action`'s loop region (from `loopstart`
/// to the end), i.e. the period the animation repeats with. `0` when there is no
/// loop region (`loopstart` is at or past the last frame) or every looped frame
/// is hold-forever — in which case the action does not cycle.
fn action_loop_ticks(action: &AnimAction) -> i32 {
    action
        .frames
        .iter()
        .skip(action.loopstart)
        .fold(0i32, |acc, f| acc.saturating_add(f.ticks.max(0)))
}

/// Selects the index of the AIR frame an animated background element displays at
/// `tick` ticks into its action.
///
/// This is the pure core of stage-layer animation (no GPU/SFF state), so the
/// frame-selection rule can be unit-tested independently of rendering. It walks
/// the action's frames accumulating each frame's `ticks` duration; the frame
/// whose `[start, start + ticks)` window contains `tick` is the one shown. The
/// rules match the character executor's [`advance_animation`] semantics:
///
/// - A frame with `ticks <= 0` is **hold-forever** (MUGEN's `-1`): the animation
///   never advances past it, so any `tick` from its start onward shows it.
/// - At the end of the action the cursor loops back to `loopstart` (the frame
///   index marked by AIR's `Loopstart`), so a long-running `tick` wraps through
///   the loop region rather than running off the end.
/// - A negative `tick` clamps to `0` (the first frame); an empty action yields
///   `0` (the caller treats an out-of-range index as "no frame").
///
/// The returned index is always a valid index into `action.frames` for a
/// non-empty action.
pub fn anim_elem_at_tick(action: &AnimAction, tick: i32) -> usize {
    let n = action.frames.len();
    if n == 0 {
        return 0;
    }

    // Negative ticks (defensive) show the first frame.
    let mut remaining = tick.max(0);

    // The intro region `[0, loopstart)` plays once; the loop region
    // `[loopstart, n)` repeats. Clamp a malformed loopstart into range.
    let loopstart = action.loopstart.min(n.saturating_sub(1));

    // Walk forward, looping at the end, until `remaining` lands inside a frame's
    // window or we hit a hold-forever frame. Bound the walk so a pathological
    // all-zero-duration loop region can never spin forever.
    const MAX_STEPS: usize = 1 << 20;
    let mut elem = 0usize;
    for _ in 0..MAX_STEPS {
        // SAFETY of indexing: `elem` is always kept in `0..n` below.
        let dur = action.frames[elem].ticks;
        if dur <= 0 {
            // Hold-forever frame: the animation parks here.
            return elem;
        }
        if remaining < dur {
            // `tick` falls within this frame's window.
            return elem;
        }
        remaining -= dur;
        elem += 1;
        if elem >= n {
            elem = loopstart;
            // A loop region of zero total duration would never consume
            // `remaining`; stop on the loop's first frame rather than spin.
            if action_loop_ticks(action) <= 0 {
                return elem;
            }
        }
    }
    // Bound hit (only reachable for a degenerate action): a safe, valid index.
    loopstart
}

/// Advances one scroll axis by `velocity`, wrapping into `[0, period)` when
/// `period > 0` (and finite). A non-positive or non-finite `period` leaves the
/// value un-wrapped (raw accumulation). The result is always finite: a non-finite
/// intermediate collapses to `0.0` rather than poisoning the offset.
fn advance_axis(offset: f32, velocity: f32, period: f32) -> f32 {
    let next = offset + velocity;
    if !next.is_finite() {
        return 0.0;
    }
    if period.is_finite() && period > 0.0 {
        // `rem_euclid` keeps the result in `[0, period)` even for negative
        // velocities, matching a seamless repeating tile.
        next.rem_euclid(period)
    } else {
        next
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

    /// Computes the camera's vertical offset so its view tracks how high the
    /// fighters are, clamped to the camera's vertical bounds.
    ///
    /// `p1_y`/`p2_y` are the fighters' world Y positions, in MUGEN's convention
    /// where `0` is the ground plane and **negative Y is up** (a jumping fighter
    /// has a negative Y). The camera follows the higher (more negative) reaches of
    /// the fighters' midpoint, scaled by the `[Camera] verticalfollow` factor in
    /// `[0, 1]`: `0` disables vertical travel entirely (the view stays put even
    /// while fighters jump), `1` tracks the midpoint one-to-one. The result is the
    /// world Y the camera offsets its view by, clamped to
    /// `[bound_top, bound_bottom]` so it never scrolls past the authored stage
    /// edges. Robust against an inverted bound pair (`top > bottom`): it clamps to
    /// the normalized `[min, max]`. A non-finite `verticalfollow` (defensive)
    /// disables follow rather than poisoning the offset.
    ///
    /// This mirrors [`camera_follow_x`](Stage::camera_follow_x) for the vertical
    /// axis; it is a pure function so the clamp/scale math is unit-testable
    /// independently of rendering.
    pub fn camera_follow_y(&self, p1_y: f32, p2_y: f32) -> f32 {
        let midpoint = (p1_y + p2_y) * 0.5;
        let follow = if self.camera.vertical_follow.is_finite() {
            // Negative/over-unit factors are clamped into the documented [0, 1].
            self.camera.vertical_follow.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let offset = midpoint * follow;
        let lo = self.camera.bound_top.min(self.camera.bound_bottom);
        let hi = self.camera.bound_top.max(self.camera.bound_bottom);
        offset.clamp(lo, hi)
    }

    /// Advances every background element's auto-scroll offset by one tick,
    /// wrapping each within its tile period.
    ///
    /// Call this once per fixed tick. `tile_size` resolves the per-element drawn
    /// sprite size used to wrap the accumulator (see [`BgElement::advance_scroll`]):
    /// it is called with each element's `(group, image)` sprite reference and must
    /// return that sprite's `(width, height)` in px, or `None` when the size is
    /// unknown (which disables wrapping for that element, keeping raw accumulation).
    /// This indirection keeps `fp-stage` free of any SFF/GPU dependency — the
    /// caller (which already owns the decoded sprites) supplies the sizes.
    pub fn advance_scroll<F>(&mut self, mut tile_size: F)
    where
        F: FnMut(Vec2<i32>) -> Option<Vec2<f32>>,
    {
        for bg in &mut self.backgrounds {
            let size = tile_size(bg.sprite).unwrap_or(Vec2::new(0.0, 0.0));
            bg.advance_scroll(size);
        }
    }

    /// Advances every animated ([`BgType::Anim`]) background element's animation
    /// clock by one tick, leaving static elements untouched.
    ///
    /// Call this once per fixed tick. `action_for` resolves an element's parsed
    /// [`action_no`](BgElement::action_no) to its AIR [`AnimAction`] (looked up in
    /// the stage's own `.air` by the caller, which owns it) — or `None` when the
    /// action is missing, in which case the element is skipped (its clock holds).
    /// This indirection keeps `fp-stage` free of any concrete AIR-ownership /
    /// file-IO concern: the caller supplies the actions.
    ///
    /// An element with no [`action_no`](BgElement::action_no), or whose `kind` is
    /// not [`BgType::Anim`], is never advanced.
    pub fn advance_anim<'a, F>(&mut self, mut action_for: F)
    where
        F: FnMut(i32) -> Option<&'a AnimAction>,
    {
        for bg in &mut self.backgrounds {
            if bg.kind != BgType::Anim {
                continue;
            }
            let Some(action_no) = bg.action_no else {
                continue;
            };
            if let Some(action) = action_for(action_no) {
                bg.advance_anim(action);
            }
        }
    }
}

/// Computes the destination rectangles that tile a single background sprite across
/// a camera viewport.
///
/// MUGEN's `tile = x, y` is a *repeat count* per axis: `0` repeats the sprite
/// enough times to fill the viewport, `1` draws it once (no tiling on that axis),
/// and `n > 1` draws exactly `n` copies. `anchor` is the screen-space top-left of
/// the element's **first** tile (after parallax + auto-scroll have been applied by
/// the caller); `sprite` is the drawn sprite's `(width, height)` in px; `tile` is
/// the parsed repeat count `(x, y)`; and `viewport` is the visible screen size
/// `(width, height)`.
///
/// The returned rects are ordered row-major (all X tiles of the first row, then the
/// next row) and are exactly `sprite`-sized. For an *infinite* (`tile = 0`) axis the
/// span is back-filled so a tile that has scrolled partly off the left/top edge is
/// still drawn — the first rect can start at a negative coordinate — and forward to
/// the first tile fully past the right/bottom edge, so the viewport is always
/// covered with no gap.
///
/// This is a pure function (no GPU state): it lets the tile geometry be unit-tested
/// independently of rendering. A degenerate sprite (zero/negative/non-finite
/// dimension on an axis) yields a single un-tiled rect at the anchor on that axis,
/// never an unbounded loop.
pub fn tile_rects(
    anchor: Vec2<f32>,
    sprite: Vec2<f32>,
    tile: Vec2<i32>,
    viewport: Vec2<f32>,
) -> Vec<Rect> {
    let xs = tile_offsets_1d(anchor.x, sprite.x, tile.x, viewport.x);
    let ys = tile_offsets_1d(anchor.y, sprite.y, tile.y, viewport.y);

    let mut rects = Vec::with_capacity(xs.len().saturating_mul(ys.len()));
    for &y in &ys {
        for &x in &xs {
            rects.push(Rect::new(x, y, sprite.x, sprite.y));
        }
    }
    rects
}

/// The 1-D core of [`tile_rects`]: the list of tile top-left coordinates on one
/// axis. `anchor` is the first tile's coordinate, `size` the tile size on that
/// axis, `count` the parsed repeat count (`0` = infinite, `1` = single, `n` =
/// exactly `n`), and `view` the viewport extent on that axis.
fn tile_offsets_1d(anchor: f32, size: f32, count: i32, view: f32) -> Vec<f32> {
    // A non-drawable tile size can't tile; emit the single anchor tile.
    if !size.is_finite() || size <= 0.0 || !anchor.is_finite() {
        return vec![anchor];
    }

    match count {
        // Single copy (or a malformed negative count → treat as one).
        c if c == 1 || c < 0 => vec![anchor],
        // Exactly `n` copies, marching forward from the anchor.
        c if c > 1 => (0..c).map(|i| anchor + i as f32 * size).collect(),
        // count == 0 → infinite: cover the whole viewport with no gaps.
        _ => {
            // Back-fill from the first tile still touching the left/top edge.
            // The k-th tile spans [anchor + k*size, anchor + (k+1)*size); it is
            // visible when its right edge > 0 and its left edge < view.
            let view = if view.is_finite() && view > 0.0 {
                view
            } else {
                // Without a real viewport, fall back to the single anchor tile
                // rather than looping unbounded.
                return vec![anchor];
            };
            let first_k = (-anchor / size).floor() as i64;
            let last_k = ((view - anchor) / size).ceil() as i64;
            // Guard the span: clamp to a generous cap so a pathological
            // anchor/size never allocates without bound.
            const MAX_TILES: i64 = 4096;
            let span = (last_k - first_k).clamp(0, MAX_TILES);
            (0..=span)
                .map(|i| anchor + (first_k + i) as f32 * size)
                .collect()
        }
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
    use fp_core::SpriteId;
    use fp_formats::air::AnimFrame;

    /// Builds a synthetic AIR action from `(group, image, ticks)` frame tuples and
    /// a `loopstart` index — enough to exercise frame selection without parsing a
    /// real `.air`.
    fn anim_action(number: i32, loopstart: usize, frames: &[(u16, u16, i32)]) -> AnimAction {
        AnimAction {
            action_number: number,
            loopstart,
            frames: frames
                .iter()
                .map(|&(g, i, t)| AnimFrame {
                    sprite: SpriteId::new(g, i),
                    ticks: t,
                    ..Default::default()
                })
                .collect(),
        }
    }

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

    // -----------------------------------------------------------------------
    // T003 — tile-rect generation
    // -----------------------------------------------------------------------

    #[test]
    fn tile_count_one_draws_a_single_rect() {
        // tile = 1, 1 → exactly one rect at the anchor, sprite-sized.
        let rects = tile_rects(
            Vec2::new(10.0, 20.0),
            Vec2::new(64.0, 32.0),
            Vec2::new(1, 1),
            Vec2::new(640.0, 480.0),
        );
        assert_eq!(rects, vec![Rect::new(10.0, 20.0, 64.0, 32.0)]);
    }

    #[test]
    fn tile_count_zero_zero_fills_both_axes() {
        // 0 on an axis means "fill that axis"; with both axes at 0 the rect set is
        // the product of each axis's fill. X = 100px tiles over a 250px viewport,
        // Y = 1000px tiles over a 480px viewport.
        let rects = tile_rects(
            Vec2::new(0.0, 0.0),
            Vec2::new(100.0, 1000.0),
            Vec2::new(0, 0),
            Vec2::new(250.0, 480.0),
        );
        // Distinct X coordinates must cover [0, 250): 0,100,200 alone reach 300, and
        // the fill marches to the first tile fully past the right edge (300, right
        // edge 400). Distinct Y must cover [0, 480): 0 (right edge 1000) suffices,
        // plus the one extra row the ceil() fill adds at 1000.
        let mut xs: Vec<f32> = rects.iter().map(|r| r.x).collect();
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        xs.dedup();
        assert_eq!(*xs.first().unwrap(), 0.0);
        assert!(xs.last().unwrap() + 100.0 >= 250.0, "X must cover the viewport");
        for pair in xs.windows(2) {
            assert!((pair[1] - pair[0] - 100.0).abs() < 1e-4, "X gap at {pair:?}");
        }
        // Every rect is sprite-sized.
        assert!(rects.iter().all(|r| r.w == 100.0 && r.h == 1000.0));
    }

    #[test]
    fn tile_count_n_draws_exactly_n_copies_spaced_by_sprite_size() {
        // tile.x = 3 → three copies at anchor, anchor+w, anchor+2w on the X axis.
        let rects = tile_rects(
            Vec2::new(5.0, 0.0),
            Vec2::new(40.0, 30.0),
            Vec2::new(3, 1),
            Vec2::new(1000.0, 1000.0),
        );
        let xs: Vec<f32> = rects.iter().map(|r| r.x).collect();
        assert_eq!(xs, vec![5.0, 45.0, 85.0]);
        assert!(rects.iter().all(|r| r.w == 40.0 && r.h == 30.0));
    }

    #[test]
    fn infinite_tiling_covers_the_whole_viewport_with_no_gap() {
        // A small 50px-wide tile, infinite, must cover a 320px viewport: the union
        // of the rects spans from <=0 to >=320 with contiguous 50px steps.
        let rects = tile_rects(
            Vec2::new(0.0, 0.0),
            Vec2::new(50.0, 50.0),
            Vec2::new(0, 1),
            Vec2::new(320.0, 50.0),
        );
        let mut xs: Vec<f32> = rects.iter().map(|r| r.x).collect();
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        // First tile starts at or before 0, last tile's right edge is at or past 320.
        assert!(*xs.first().unwrap() <= 0.0);
        assert!(xs.last().unwrap() + 50.0 >= 320.0);
        // Contiguous: each step is exactly one sprite width.
        for pair in xs.windows(2) {
            assert!((pair[1] - pair[0] - 50.0).abs() < 1e-4, "gap at {pair:?}");
        }
    }

    #[test]
    fn infinite_tiling_backfills_a_partly_scrolled_off_tile() {
        // Anchor scrolled 30px to the left of the screen origin: the first visible
        // tile starts at a negative X so its right edge still covers x=0.
        let rects = tile_rects(
            Vec2::new(-30.0, 0.0),
            Vec2::new(50.0, 50.0),
            Vec2::new(0, 1),
            Vec2::new(200.0, 50.0),
        );
        let min_x = rects
            .iter()
            .map(|r| r.x)
            .fold(f32::INFINITY, f32::min);
        // The left-most tile starts at -30 (its right edge = 20 > 0, so it's drawn).
        assert_eq!(min_x, -30.0);
    }

    #[test]
    fn degenerate_zero_sprite_size_yields_single_rect_not_infinite_loop() {
        // A zero-width sprite cannot tile; we must get exactly one rect, never hang.
        let rects = tile_rects(
            Vec2::new(7.0, 9.0),
            Vec2::new(0.0, 0.0),
            Vec2::new(0, 0),
            Vec2::new(640.0, 480.0),
        );
        assert_eq!(rects, vec![Rect::new(7.0, 9.0, 0.0, 0.0)]);
    }

    #[test]
    fn nonfinite_anchor_or_viewport_does_not_panic_or_loop() {
        // Defensive: NaN/inf inputs collapse to the single-anchor fallback.
        let rects = tile_rects(
            Vec2::new(f32::NAN, 0.0),
            Vec2::new(10.0, 10.0),
            Vec2::new(0, 1),
            Vec2::new(100.0, 100.0),
        );
        // X axis: NaN anchor → single fallback tile. Y axis: single tile (size 10
        // covers [0,10) within the band). Product = 1 rect.
        assert_eq!(rects.len(), 1);
        assert!(rects[0].x.is_nan());
    }

    // -----------------------------------------------------------------------
    // T003 — per-layer velocity scrolling
    // -----------------------------------------------------------------------

    #[test]
    fn zero_velocity_never_moves_the_scroll_offset() {
        let mut bg = BgElement {
            velocity: Vec2::new(0.0, 0.0),
            ..Default::default()
        };
        for _ in 0..10 {
            bg.advance_scroll(Vec2::new(64.0, 64.0));
        }
        assert_eq!(bg.scroll, Vec2::new(0.0, 0.0));
    }

    #[test]
    fn velocity_accumulates_one_tick_at_a_time() {
        // velocity = (3, -2), wrapping disabled (size 0) → raw accumulation.
        let mut bg = BgElement {
            velocity: Vec2::new(3.0, -2.0),
            ..Default::default()
        };
        bg.advance_scroll(Vec2::new(0.0, 0.0));
        assert_eq!(bg.scroll, Vec2::new(3.0, -2.0));
        bg.advance_scroll(Vec2::new(0.0, 0.0));
        assert_eq!(bg.scroll, Vec2::new(6.0, -4.0));
        // After n ticks the offset is velocity * n.
        for _ in 0..3 {
            bg.advance_scroll(Vec2::new(0.0, 0.0));
        }
        assert_eq!(bg.scroll, Vec2::new(15.0, -10.0));
    }

    #[test]
    fn scroll_wraps_within_one_tile_period() {
        // velocity 30, tile size 50 → after two ticks offset is 60, wrapped to 10.
        let mut bg = BgElement {
            velocity: Vec2::new(30.0, 0.0),
            ..Default::default()
        };
        bg.advance_scroll(Vec2::new(50.0, 50.0)); // 30
        assert_eq!(bg.scroll.x, 30.0);
        bg.advance_scroll(Vec2::new(50.0, 50.0)); // 60 → 10
        assert!((bg.scroll.x - 10.0).abs() < 1e-4, "got {}", bg.scroll.x);
        // Wrapping keeps it bounded over many ticks.
        for _ in 0..1000 {
            bg.advance_scroll(Vec2::new(50.0, 50.0));
        }
        assert!((0.0..50.0).contains(&bg.scroll.x));
    }

    #[test]
    fn negative_velocity_wraps_to_positive_range() {
        // rem_euclid keeps a leftward scroll inside [0, period).
        let mut bg = BgElement {
            velocity: Vec2::new(-30.0, 0.0),
            ..Default::default()
        };
        bg.advance_scroll(Vec2::new(50.0, 50.0)); // -30 → 20
        assert!((bg.scroll.x - 20.0).abs() < 1e-4, "got {}", bg.scroll.x);
        assert!((0.0..50.0).contains(&bg.scroll.x));
    }

    #[test]
    fn nonfinite_intermediate_scroll_collapses_to_zero() {
        // A huge velocity that overflows to inf must not poison the offset.
        let mut bg = BgElement {
            velocity: Vec2::new(f32::INFINITY, 0.0),
            ..Default::default()
        };
        bg.advance_scroll(Vec2::new(0.0, 0.0));
        assert_eq!(bg.scroll.x, 0.0, "non-finite offset resets to 0");
    }

    #[test]
    fn stage_advance_scroll_drives_every_background() {
        let mut stage = Stage::parse(SYNTHETIC, None);
        // The "Floor" element has velocity = (-2, 0); the others have none.
        // Disable wrapping by reporting no sprite size so accumulation is exact.
        for _ in 0..5 {
            stage.advance_scroll(|_sprite| None);
        }
        let floor = stage
            .backgrounds
            .iter()
            .find(|b| b.name == "Floor")
            .expect("Floor BG present");
        assert_eq!(floor.scroll, Vec2::new(-10.0, 0.0));
        // A no-velocity element stays at the origin.
        let sky = stage
            .backgrounds
            .iter()
            .find(|b| b.name == "Sky")
            .expect("Sky BG present");
        assert_eq!(sky.scroll, Vec2::new(0.0, 0.0));
    }

    #[test]
    fn stage_advance_scroll_wraps_with_reported_tile_size() {
        let mut stage = Stage::parse(SYNTHETIC, None);
        // Report a 3px-wide tile for every sprite: Floor's -2/tick offset wraps in
        // [0, 3). After 5 ticks raw = -10, rem_euclid(3) = 2.
        for _ in 0..5 {
            stage.advance_scroll(|_sprite| Some(Vec2::new(3.0, 3.0)));
        }
        let floor = stage
            .backgrounds
            .iter()
            .find(|b| b.name == "Floor")
            .expect("Floor BG present");
        assert!((floor.scroll.x - 2.0).abs() < 1e-4, "got {}", floor.scroll.x);
        assert!((0.0..3.0).contains(&floor.scroll.x));
    }

    // -----------------------------------------------------------------------
    // T004 — animated-layer frame selection
    // -----------------------------------------------------------------------

    #[test]
    fn anim_frame_selection_picks_the_right_frame_per_tick() {
        // Three frames of 7 ticks each (sprites 0,1,2), no loop region offset.
        // Window layout: frame 0 = [0,7), frame 1 = [7,14), frame 2 = [14,21).
        let action = anim_action(10, 0, &[(0, 0, 7), (0, 1, 7), (0, 2, 7)]);

        // Within frame 0's window.
        assert_eq!(anim_elem_at_tick(&action, 0), 0);
        assert_eq!(anim_elem_at_tick(&action, 6), 0);
        // Boundary: tick 7 begins frame 1.
        assert_eq!(anim_elem_at_tick(&action, 7), 1);
        assert_eq!(anim_elem_at_tick(&action, 13), 1);
        // Frame 2's window.
        assert_eq!(anim_elem_at_tick(&action, 14), 2);
        assert_eq!(anim_elem_at_tick(&action, 20), 2);
    }

    #[test]
    fn anim_frame_selection_wraps_at_loopstart() {
        // loopstart = 1: the action plays frame 0 once (the intro), then loops
        // frames 1 and 2 forever. Total = 21 ticks, loop region = 14 ticks.
        let action = anim_action(10, 1, &[(0, 0, 7), (0, 1, 7), (0, 2, 7)]);

        // First pass through all three frames.
        assert_eq!(anim_elem_at_tick(&action, 0), 0);
        assert_eq!(anim_elem_at_tick(&action, 7), 1);
        assert_eq!(anim_elem_at_tick(&action, 14), 2);
        // At tick 21 the cursor wraps to loopstart (frame 1), NOT back to frame 0.
        assert_eq!(anim_elem_at_tick(&action, 21), 1);
        assert_eq!(anim_elem_at_tick(&action, 28), 2);
        // And keeps looping the [1, 2] region.
        assert_eq!(anim_elem_at_tick(&action, 35), 1);
    }

    #[test]
    fn anim_frame_selection_holds_forever_on_nonpositive_ticks() {
        // A -1 (hold-forever) middle frame parks the animation there for any tick
        // from its start onward; nothing after it ever shows.
        let action = anim_action(10, 0, &[(0, 0, 5), (0, 1, -1), (0, 2, 5)]);
        assert_eq!(anim_elem_at_tick(&action, 0), 0);
        assert_eq!(anim_elem_at_tick(&action, 5), 1);
        assert_eq!(anim_elem_at_tick(&action, 999), 1, "hold-forever parks here");
    }

    #[test]
    fn anim_frame_selection_clamps_negative_and_handles_empty() {
        let action = anim_action(10, 0, &[(0, 0, 7), (0, 1, 7)]);
        // Negative tick clamps to the first frame.
        assert_eq!(anim_elem_at_tick(&action, -100), 0);
        // An empty action never panics and yields index 0.
        let empty = anim_action(11, 0, &[]);
        assert_eq!(anim_elem_at_tick(&empty, 50), 0);
    }

    #[test]
    fn anim_frame_selection_does_not_spin_on_zero_duration_loop() {
        // A loop region whose frames are all hold-forever (ticks <= 0) must not
        // spin: selection stops on the loop's first frame after the intro.
        let action = anim_action(10, 1, &[(0, 0, 7), (0, 1, -1), (0, 2, -1)]);
        // Within the intro frame.
        assert_eq!(anim_elem_at_tick(&action, 3), 0);
        // After the intro, the hold-forever loop parks on its first frame.
        assert_eq!(anim_elem_at_tick(&action, 1_000_000), 1);
    }

    #[test]
    fn bg_advance_anim_loops_and_resolves_current_sprite() {
        // A looping 2-frame action: sprites (5,0) then (5,1), 3 ticks each.
        let action = anim_action(1, 0, &[(5, 0, 3), (5, 1, 3)]);
        let mut bg = BgElement {
            kind: BgType::Anim,
            action_no: Some(1),
            ..Default::default()
        };

        // Tick 0: frame 0, sprite (5, 0).
        assert_eq!(bg.current_anim_elem(&action), 0);
        assert_eq!(bg.current_anim_sprite(&action), Vec2::new(5, 0));

        // Advance into frame 1.
        for _ in 0..3 {
            bg.advance_anim(&action);
        }
        assert_eq!(bg.anim_tick, 3);
        assert_eq!(bg.current_anim_elem(&action), 1);
        assert_eq!(bg.current_anim_sprite(&action), Vec2::new(5, 1));

        // Keep advancing for many ticks: the clock stays bounded (wraps within one
        // loop period) and the element keeps cycling 0,1,0,1...
        for _ in 0..1000 {
            bg.advance_anim(&action);
        }
        let total = action_total_ticks(&action);
        assert!(bg.anim_tick < total, "clock stayed bounded: {}", bg.anim_tick);
        // The selected element is always a valid frame index.
        assert!(bg.current_anim_elem(&action) < action.frames.len());
    }

    #[test]
    fn bg_advance_anim_non_looping_pins_to_last_frame() {
        // loopstart == frame count → no loop region: the clock pins at the total
        // duration and the last frame stays on screen.
        let action = anim_action(2, 2, &[(7, 0, 4), (7, 1, 4)]);
        let mut bg = BgElement {
            kind: BgType::Anim,
            action_no: Some(2),
            ..Default::default()
        };
        for _ in 0..100 {
            bg.advance_anim(&action);
        }
        let total = action_total_ticks(&action);
        assert_eq!(bg.anim_tick, total, "non-looping clock pins at total duration");
        assert_eq!(bg.current_anim_elem(&action), 1, "last frame held");
    }

    #[test]
    fn stage_advance_anim_only_drives_anim_layers() {
        let mut stage = Stage::default();
        stage.backgrounds.push(BgElement {
            name: "static".into(),
            kind: BgType::Normal,
            ..Default::default()
        });
        stage.backgrounds.push(BgElement {
            name: "animated".into(),
            kind: BgType::Anim,
            action_no: Some(1),
            ..Default::default()
        });
        // An anim element whose action is missing must hold (skipped, no panic).
        stage.backgrounds.push(BgElement {
            name: "anim_missing_action".into(),
            kind: BgType::Anim,
            action_no: Some(99),
            ..Default::default()
        });

        let action = anim_action(1, 0, &[(0, 0, 1), (0, 1, 1)]);
        for _ in 0..5 {
            stage.advance_anim(|n| if n == 1 { Some(&action) } else { None });
        }

        // Static element never animates.
        assert_eq!(stage.backgrounds[0].anim_tick, 0);
        // The animated element with a resolvable action advanced 5 ticks (wrapped
        // within the 2-tick loop period → 1).
        assert_eq!(stage.backgrounds[1].anim_tick, 1);
        // The anim element whose action did not resolve stayed put.
        assert_eq!(stage.backgrounds[2].anim_tick, 0);
    }

    // -----------------------------------------------------------------------
    // T004 — vertical camera follow
    // -----------------------------------------------------------------------

    #[test]
    fn camera_follow_y_tracks_midpoint_scaled_by_verticalfollow() {
        let mut stage = Stage::default();
        stage.camera.bound_top = -100.0; // up
        stage.camera.bound_bottom = 0.0; // ground
        stage.camera.vertical_follow = 0.5;

        // Both grounded → no vertical offset.
        assert_eq!(stage.camera_follow_y(0.0, 0.0), 0.0);
        // Both at y = -40 (in the air): midpoint -40, scaled by 0.5 → -20.
        assert_eq!(stage.camera_follow_y(-40.0, -40.0), -20.0);
        // Asymmetric: midpoint (-80 + -20)/2 = -50, * 0.5 → -25.
        assert_eq!(stage.camera_follow_y(-80.0, -20.0), -25.0);
    }

    #[test]
    fn camera_follow_y_clamps_to_vertical_bounds() {
        let mut stage = Stage::default();
        stage.camera.bound_top = -100.0;
        stage.camera.bound_bottom = 0.0;
        stage.camera.vertical_follow = 1.0;

        // A huge jump (very negative Y) clamps to the top bound.
        assert_eq!(stage.camera_follow_y(-500.0, -600.0), -100.0);
        // A downward (positive Y) midpoint clamps to the bottom bound.
        assert_eq!(stage.camera_follow_y(50.0, 90.0), 0.0);
    }

    #[test]
    fn camera_follow_y_zero_factor_disables_vertical_travel() {
        let mut stage = Stage::default();
        stage.camera.bound_top = -100.0;
        stage.camera.bound_bottom = 0.0;
        stage.camera.vertical_follow = 0.0;
        // Even a big jump produces no offset when verticalfollow is 0.
        assert_eq!(stage.camera_follow_y(-90.0, -90.0), 0.0);
    }

    #[test]
    fn camera_follow_y_handles_inverted_bounds_and_bad_factor() {
        // Inverted vertical bounds must not produce NaN/empty clamp.
        let mut stage = Stage::default();
        stage.camera.bound_top = 0.0;
        stage.camera.bound_bottom = -100.0; // inverted (top > bottom)
        stage.camera.vertical_follow = 0.5;
        let y = stage.camera_follow_y(-40.0, -40.0);
        assert!((-100.0..=0.0).contains(&y), "clamped into normalized range: {y}");

        // A non-finite verticalfollow disables follow (offset 0) rather than NaN.
        stage.camera.vertical_follow = f32::NAN;
        assert_eq!(stage.camera_follow_y(-40.0, -40.0), 0.0);
    }
}
