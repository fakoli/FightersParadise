//! # AIR — Animation file format parser
//!
//! Parses MUGEN `.air` files which define animation sequences for characters
//! and stages. Each animation action is identified by a number and contains
//! a sequence of frames with sprite references, timing, optional flip/blend
//! settings, and collision box definitions.
//!
//! # Format Overview
//!
//! AIR files are text-based with the following structure:
//! ```text
//! ; comment
//! [Begin Action 0]
//! Clsn2Default: 1
//!  Clsn2[0] = -10, -80, 10, 0
//! Loopstart
//! 0,0, 0,0, 7
//! 0,1, 0,0, 7
//! 0,2, 0,0, 7, H
//! ```

use std::collections::HashMap;
use std::path::Path;

use fp_core::{FpError, FpResult, Rect, SpriteId, Vec2};

/// A complete parsed AIR file containing all animation actions.
#[derive(Debug, Clone)]
pub struct AirFile {
    /// Map from action number to animation action.
    pub actions: HashMap<i32, AnimAction>,
}

/// A single animation action (sequence of frames).
#[derive(Debug, Clone)]
pub struct AnimAction {
    /// The action number identifying this animation.
    pub action_number: i32,
    /// The sequence of animation frames.
    pub frames: Vec<AnimFrame>,
    /// Frame index where looping restarts (default: 0).
    pub loopstart: usize,
}

/// A single animation frame within an action.
///
/// Implements [`Default`] (an empty sprite-0 frame with no transforms) so
/// callers can build a frame and override only the fields they care about via
/// `AnimFrame { ..Default::default() }`.
#[derive(Debug, Clone, Default)]
pub struct AnimFrame {
    /// Which sprite to display (group, image).
    pub sprite: SpriteId,
    /// Pixel offset from character position.
    pub offset: Vec2<i16>,
    /// Duration in game ticks (1/60th second). -1 = infinite (hold forever).
    pub ticks: i32,
    /// Mirror the sprite horizontally.
    pub flip_h: bool,
    /// Mirror the sprite vertically.
    pub flip_v: bool,
    /// Sprite blending mode.
    pub blend: BlendMode,
    /// Optional per-frame scale `(xscale, yscale)` from the extended AIR
    /// `... , xscale, yscale` columns. `None` when the frame omits them
    /// (the common case — MUGEN then uses the default scale of `1.0`).
    pub scale: Option<Vec2<f32>>,
    /// Optional per-frame rotation in degrees from the extended AIR `angle`
    /// column. `None` when the frame omits it (default: no rotation).
    pub angle: Option<f32>,
    /// Which transforms interpolate from the *previous* frame into this one,
    /// as declared by the `Interpolate ...` lines preceding this frame.
    pub interpolate: Interpolate,
    /// Attack collision boxes (Clsn1) for this frame.
    pub clsn1: Vec<Rect>,
    /// Hurtbox collision boxes (Clsn2) for this frame.
    pub clsn2: Vec<Rect>,
}

/// Which transforms a frame interpolates from the previous frame.
///
/// MUGEN AIR allows standalone `Interpolate Offset` / `Interpolate Scale` /
/// `Interpolate Angle` / `Interpolate Blend` lines between two frame lines; each
/// requests smooth interpolation of that transform across the *preceding*
/// frame's duration into the frame that follows the line. All fields default to
/// `false`, so a plain AIR with no `Interpolate` lines is byte-for-byte
/// unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Interpolate {
    /// Interpolate the position offset (`Interpolate Offset`).
    pub offset: bool,
    /// Interpolate the scale (`Interpolate Scale`).
    pub scale: bool,
    /// Interpolate the rotation angle (`Interpolate Angle`).
    pub angle: bool,
    /// Interpolate the blend parameters (`Interpolate Blend`).
    pub blend: bool,
}

impl Interpolate {
    /// Returns `true` if no interpolation flag is set (the default).
    pub fn is_none(&self) -> bool {
        !self.offset && !self.scale && !self.angle && !self.blend
    }
}

/// Sprite blending mode for rendering.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum BlendMode {
    /// Standard rendering (no blending).
    #[default]
    Normal,
    /// Additive blending — sprite colors are added to the background.
    Additive,
    /// Additive blending with custom source alpha (0–255).
    AdditiveAlpha(u8),
    /// Subtractive blending — sprite colors are subtracted from the background.
    Subtractive,
}

impl AirFile {
    /// Loads and parses an AIR file from the given path.
    pub fn load(path: &Path) -> FpResult<Self> {
        let text = std::fs::read_to_string(path)?;
        Self::from_str(&text)
    }

    /// Parses an AIR file from a string.
    ///
    /// Tolerates a leading UTF-8 BOM (real MUGEN `.air` files are commonly saved
    /// UTF-8-with-BOM) and CRLF line endings.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(text: &str) -> FpResult<Self> {
        let mut actions = HashMap::new();
        let mut current_action: Option<ActionBuilder> = None;

        // Strip a leading UTF-8 BOM if present so the first line parses cleanly.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);

        for raw_line in text.lines() {
            // `lines()` already strips the trailing `\r` of CRLF endings.
            let line = strip_comment(raw_line).trim();
            if line.is_empty() {
                continue;
            }

            // Check for [Begin Action N]
            if let Some(action_num) = parse_begin_action(line) {
                // Finalize previous action
                if let Some(builder) = current_action.take() {
                    let action = builder.build();
                    actions.insert(action.action_number, action);
                }
                current_action = Some(ActionBuilder::new(action_num));
                continue;
            }

            let Some(builder) = current_action.as_mut() else {
                // Lines before the first [Begin Action] are ignored
                continue;
            };

            // Check for Loopstart
            if line.eq_ignore_ascii_case("loopstart") {
                builder.loopstart_pending = true;
                continue;
            }

            // Check for `Interpolate <Offset|Scale|Angle|Blend>` lines. These
            // accumulate onto the next frame's `interpolate` field.
            if let Some(kind) = parse_interpolate(line) {
                match kind {
                    InterpolateKind::Offset => builder.interpolate_pending.offset = true,
                    InterpolateKind::Scale => builder.interpolate_pending.scale = true,
                    InterpolateKind::Angle => builder.interpolate_pending.angle = true,
                    InterpolateKind::Blend => builder.interpolate_pending.blend = true,
                }
                continue;
            }

            // Check for collision box declarations
            if let Some(count) = parse_clsn_default(line, "Clsn2Default") {
                builder.clsn2_default_count = count;
                builder.collecting_clsn = Some(ClsnTarget::Clsn2Default);
                builder.clsn2_default.clear();
                continue;
            }
            if let Some(count) = parse_clsn_default(line, "Clsn1Default") {
                builder.clsn1_default_count = count;
                builder.collecting_clsn = Some(ClsnTarget::Clsn1Default);
                builder.clsn1_default.clear();
                continue;
            }
            if let Some(count) = parse_clsn_header(line, "Clsn2") {
                builder.clsn2_frame_count = count;
                builder.collecting_clsn = Some(ClsnTarget::Clsn2Frame);
                builder.clsn2_frame.clear();
                continue;
            }
            if let Some(count) = parse_clsn_header(line, "Clsn1") {
                builder.clsn1_frame_count = count;
                builder.collecting_clsn = Some(ClsnTarget::Clsn1Frame);
                builder.clsn1_frame.clear();
                continue;
            }

            // Check for Clsn[i] = left, top, right, bottom
            if let Some(rect) = parse_clsn_entry(line) {
                match builder.collecting_clsn {
                    Some(ClsnTarget::Clsn2Default) => builder.clsn2_default.push(rect),
                    Some(ClsnTarget::Clsn1Default) => builder.clsn1_default.push(rect),
                    Some(ClsnTarget::Clsn2Frame) => builder.clsn2_frame.push(rect),
                    Some(ClsnTarget::Clsn1Frame) => builder.clsn1_frame.push(rect),
                    None => {
                        tracing::warn!("Clsn entry outside of Clsn block: {line}");
                    }
                }
                continue;
            }

            // Try to parse as a frame line
            if let Some(frame) = parse_frame_line(line, builder) {
                if builder.loopstart_pending {
                    builder.loopstart = builder.frames.len();
                    builder.loopstart_pending = false;
                }
                builder.frames.push(frame);
                // Reset per-frame clsn after adding the frame
                builder.clsn1_frame.clear();
                builder.clsn2_frame.clear();
                builder.clsn1_frame_count = 0;
                builder.clsn2_frame_count = 0;
                builder.collecting_clsn = None;
                // Pending interpolation flags were consumed by this frame.
                builder.interpolate_pending = Interpolate::default();
                continue;
            }

            // Unknown line — log and skip
            tracing::warn!(
                action = builder.action_number,
                "AIR: unrecognized line: {line}"
            );
        }

        // Finalize last action
        if let Some(builder) = current_action {
            let action = builder.build();
            actions.insert(action.action_number, action);
        }

        if actions.is_empty() {
            return Err(FpError::parse("AIR", "no animation actions found"));
        }

        tracing::info!("AIR: loaded {} animation actions", actions.len());
        Ok(Self { actions })
    }

    /// Looks up an animation action by number.
    pub fn action(&self, number: i32) -> Option<&AnimAction> {
        self.actions.get(&number)
    }
}

// ---------------------------------------------------------------------------
// Internal builder and parser helpers
// ---------------------------------------------------------------------------

/// Which collision box collection we're currently reading into.
#[derive(Debug, Clone, Copy)]
enum ClsnTarget {
    Clsn1Default,
    Clsn2Default,
    Clsn1Frame,
    Clsn2Frame,
}

/// Accumulates data while parsing a single animation action.
struct ActionBuilder {
    action_number: i32,
    frames: Vec<AnimFrame>,
    loopstart: usize,
    loopstart_pending: bool,
    // Default collision boxes (apply to frames that don't override)
    clsn1_default: Vec<Rect>,
    clsn2_default: Vec<Rect>,
    clsn1_default_count: usize,
    clsn2_default_count: usize,
    // Per-frame collision boxes
    clsn1_frame: Vec<Rect>,
    clsn2_frame: Vec<Rect>,
    clsn1_frame_count: usize,
    clsn2_frame_count: usize,
    // What we're currently collecting
    collecting_clsn: Option<ClsnTarget>,
    // Interpolation flags accumulated from `Interpolate ...` lines seen since
    // the previous frame; applied to the next frame, then reset.
    interpolate_pending: Interpolate,
}

impl ActionBuilder {
    fn new(action_number: i32) -> Self {
        Self {
            action_number,
            frames: Vec::new(),
            loopstart: 0,
            loopstart_pending: false,
            clsn1_default: Vec::new(),
            clsn2_default: Vec::new(),
            clsn1_default_count: 0,
            clsn2_default_count: 0,
            clsn1_frame: Vec::new(),
            clsn2_frame: Vec::new(),
            clsn1_frame_count: 0,
            clsn2_frame_count: 0,
            collecting_clsn: None,
            interpolate_pending: Interpolate::default(),
        }
    }

    fn build(self) -> AnimAction {
        AnimAction {
            action_number: self.action_number,
            loopstart: self.loopstart,
            frames: self.frames,
        }
    }
}

/// Strip `;` comments from a line.
fn strip_comment(line: &str) -> &str {
    match line.find(';') {
        Some(pos) => &line[..pos],
        None => line,
    }
}

/// Parse `[Begin Action N]` header, returning the action number.
///
/// MUGEN allows an optional trailing comment after the number, e.g.
/// `[Begin Action 12010, Tornado Whirlwind]`. Only the **leading integer**
/// (the first comma-separated token) is parsed; any trailing `, <free-text>`
/// label is ignored. Negative action numbers are tolerated.
fn parse_begin_action(line: &str) -> Option<i32> {
    let line_lower = line.to_ascii_lowercase();
    let trimmed = line_lower.trim();
    if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
        return None;
    }
    let inner = &trimmed[1..trimmed.len() - 1].trim();
    let rest = inner.strip_prefix("begin action")?;
    // Take only the leading number token, ignoring any `, <label>` comment.
    let num_token = rest.trim().split(',').next()?.trim();
    num_token.parse::<i32>().ok()
}

/// Parse `ClsnNDefault: count` header.
///
/// Tolerates stray junk characters between the `ClsnNDefault` prefix and the
/// colon, as seen in real-world content like `Clsn2Defaultf: 1` (a stray `f`).
/// Everything from the prefix up to the first `:` is ignored, so the common
/// `Clsn1Default`/`Clsn2Default` headers are still recognized when malformed.
fn parse_clsn_default(line: &str, prefix: &str) -> Option<usize> {
    let lower = line.to_ascii_lowercase();
    let p = prefix.to_ascii_lowercase();
    if !lower.starts_with(&p) {
        return None;
    }
    // Skip everything after the prefix up to the first colon, ignoring any
    // stray characters (e.g. the trailing `f` in `Clsn2Defaultf:`).
    let after_prefix = &line[prefix.len()..];
    let colon_pos = after_prefix.find(':')?;
    let rest = &after_prefix[colon_pos + 1..];
    rest.trim().parse::<usize>().ok()
}

/// Parse `Clsn1: count` or `Clsn2: count` (per-frame, non-default).
fn parse_clsn_header(line: &str, prefix: &str) -> Option<usize> {
    let lower = line.to_ascii_lowercase();
    let p = prefix.to_ascii_lowercase();
    // Must match exactly "clsn1:" or "clsn2:" (not "clsn1default:")
    if !lower.starts_with(&p) {
        return None;
    }
    // Make sure it's not a "default" variant
    let after_prefix = &lower[p.len()..];
    if after_prefix.trim_start().starts_with('d') {
        return None; // It's ClsnNDefault, not ClsnN
    }
    let rest = &line[prefix.len()..];
    let rest = rest.trim().strip_prefix(':')?;
    rest.trim().parse::<usize>().ok()
}

/// Parse `Clsn1[i] = left, top, right, bottom` or `Clsn2[i] = ...`.
fn parse_clsn_entry(line: &str) -> Option<Rect> {
    let lower = line.to_ascii_lowercase();
    let trimmed = lower.trim();
    if !trimmed.starts_with("clsn") {
        return None;
    }
    // Find the '=' separator
    let eq_pos = line.find('=')?;
    let coords_str = line[eq_pos + 1..].trim();

    let nums: Vec<f32> = coords_str
        .split(',')
        .filter_map(|s| s.trim().parse::<f32>().ok())
        .collect();

    if nums.len() < 4 {
        tracing::warn!("AIR: Clsn entry has fewer than 4 coordinates: {line}");
        return None;
    }

    let left = nums[0].min(nums[2]);
    let top = nums[1].min(nums[3]);
    let right = nums[0].max(nums[2]);
    let bottom = nums[1].max(nums[3]);

    Some(Rect::new(left, top, right - left, bottom - top))
}

/// Parse a frame line. The base form is
/// `group, image, x_offset, y_offset, ticks[, flip[, blend]]`; MUGEN also allows
/// extended trailing columns `[, xscale, yscale[, angle]]`:
/// `group, image, x, y, ticks, flip, blend, xscale, yscale, angle`.
fn parse_frame_line(line: &str, builder: &ActionBuilder) -> Option<AnimFrame> {
    let parts: Vec<&str> = line.split(',').collect();
    if parts.len() < 5 {
        return None;
    }

    let group = parse_leading_i32(parts[0])? as u16;
    let image = parse_leading_i32(parts[1])? as u16;
    let x_offset = parse_leading_i32(parts[2])? as i16;
    let y_offset = parse_leading_i32(parts[3])? as i16;
    let ticks = parse_leading_i32(parts[4])?;

    // Parse optional flip flags
    let mut flip_h = false;
    let mut flip_v = false;
    let mut blend = BlendMode::Normal;

    if parts.len() > 5 {
        let flags = parts[5].trim().to_ascii_uppercase();
        flip_h = flags.contains('H');
        flip_v = flags.contains('V');
    }

    // Parse optional blend mode
    if parts.len() > 6 {
        blend = parse_blend_mode(parts[6].trim());
    }

    // Parse optional extended scale/angle columns. `xscale` and `yscale` (cols 7
    // and 8) come as a pair; `angle` (col 9) is the rotation in degrees. A column
    // that is missing, empty, or unparseable leaves the corresponding field
    // `None` so plain AIR frames are unaffected.
    let xscale = parts.get(7).and_then(|s| parse_opt_f32(s));
    let yscale = parts.get(8).and_then(|s| parse_opt_f32(s));
    let scale = match (xscale, yscale) {
        (Some(x), Some(y)) => Some(Vec2::new(x, y)),
        // A lone xscale with no yscale falls back to a uniform scale.
        (Some(x), None) => Some(Vec2::new(x, x)),
        _ => None,
    };
    let angle = parts.get(9).and_then(|s| parse_opt_f32(s));

    // Determine collision boxes: per-frame overrides take priority over defaults
    let clsn1 = if !builder.clsn1_frame.is_empty() {
        builder.clsn1_frame.clone()
    } else {
        builder.clsn1_default.clone()
    };

    let clsn2 = if !builder.clsn2_frame.is_empty() {
        builder.clsn2_frame.clone()
    } else {
        builder.clsn2_default.clone()
    };

    Some(AnimFrame {
        sprite: SpriteId::new(group, image),
        offset: Vec2::new(x_offset, y_offset),
        ticks,
        flip_h,
        flip_v,
        blend,
        scale,
        angle,
        interpolate: builder.interpolate_pending,
        clsn1,
        clsn2,
    })
}

/// Parse a frame column as an `i32`, tolerating trailing junk.
///
/// First tries a strict parse so well-formed columns are unchanged. If that
/// fails, falls back to scanning the **leading integer** (optional sign + one or
/// more digits) and ignores any unparseable tail, as seen in real content like
/// `2..A` (parsed as `2`). Returns `None` only when there is no leading integer
/// at all (so a fully-invalid column still drops the frame as before), or when a
/// well-formed leading integer is out of `i32` range — the latter is warn-logged
/// so a vanished frame stays diagnosable rather than dropping silently.
fn parse_leading_i32(s: &str) -> Option<i32> {
    let t = s.trim();
    if let Ok(n) = t.parse::<i32>() {
        return Some(n);
    }
    let bytes = t.as_bytes();
    let mut end = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        end = 1;
    }
    let digits_start = end;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == digits_start {
        return None; // no digits found
    }
    match t[..end].parse::<i32>() {
        Ok(n) => {
            tracing::warn!(
                "AIR: frame column had trailing junk, parsed leading integer: {t:?} -> {n}"
            );
            Some(n)
        }
        Err(_) => {
            // The leading run of digits overflows `i32`; drop the column rather
            // than guess. Warn-logged so the dropped frame is diagnosable.
            tracing::warn!("AIR: frame column leading integer out of i32 range, dropping: {t:?}");
            None
        }
    }
}

/// Parse a possibly-empty trimmed column as an `f32`, returning `None` for an
/// empty or unparseable column (so a placeholder like `... , , ...` is skipped).
fn parse_opt_f32(s: &str) -> Option<f32> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        t.parse::<f32>().ok()
    }
}

/// Which transform an `Interpolate ...` line requests.
#[derive(Debug, Clone, Copy)]
enum InterpolateKind {
    Offset,
    Scale,
    Angle,
    Blend,
}

/// Parse an `Interpolate <Offset|Scale|Angle|Blend>` line.
///
/// Returns the requested transform, or `None` if the line is not an
/// `Interpolate` directive (case-insensitive, whitespace-tolerant).
fn parse_interpolate(line: &str) -> Option<InterpolateKind> {
    let lower = line.to_ascii_lowercase();
    let rest = lower.trim().strip_prefix("interpolate")?;
    match rest.trim() {
        "offset" => Some(InterpolateKind::Offset),
        "scale" => Some(InterpolateKind::Scale),
        "angle" => Some(InterpolateKind::Angle),
        "blend" => Some(InterpolateKind::Blend),
        other => {
            tracing::warn!("AIR: unknown Interpolate target: {other}");
            None
        }
    }
}

/// Parse a blend mode string.
fn parse_blend_mode(s: &str) -> BlendMode {
    let upper = s.trim().to_ascii_uppercase();
    if upper == "A" || upper == "A1" {
        BlendMode::Additive
    } else if upper == "S" {
        BlendMode::Subtractive
    } else if upper.starts_with("AS") {
        // AS###D### format — extract source alpha
        if let Some(rest) = upper.strip_prefix("AS") {
            let alpha_str = rest.split('D').next().unwrap_or("256");
            let alpha = alpha_str.parse::<u16>().unwrap_or(256).min(256) as u8;
            BlendMode::AdditiveAlpha(alpha)
        } else {
            BlendMode::Additive
        }
    } else {
        BlendMode::Normal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE_AIR: &str = "\
; KFM idle animation
[Begin Action 0]
0,0, 0,0, 7
0,1, 0,0, 7
0,2, 0,0, 7
0,3, 0,0, 40
";

    #[test]
    fn parse_simple_action() {
        let air = AirFile::from_str(SIMPLE_AIR).unwrap();
        assert_eq!(air.actions.len(), 1);

        let action = air.action(0).unwrap();
        assert_eq!(action.action_number, 0);
        assert_eq!(action.frames.len(), 4);
        assert_eq!(action.loopstart, 0);

        assert_eq!(action.frames[0].sprite, SpriteId::new(0, 0));
        assert_eq!(action.frames[0].ticks, 7);
        assert_eq!(action.frames[3].ticks, 40);
    }

    #[test]
    fn parse_loopstart() {
        let air_text = "\
[Begin Action 10]
0,0, 0,0, 5
Loopstart
0,1, 0,0, 5
0,2, 0,0, 5
";
        let air = AirFile::from_str(air_text).unwrap();
        let action = air.action(10).unwrap();
        assert_eq!(action.frames.len(), 3);
        assert_eq!(action.loopstart, 1); // loopstart before 2nd frame
    }

    #[test]
    fn parse_flip_flags() {
        let air_text = "\
[Begin Action 5]
0,0, 0,0, 3, H
0,1, 0,0, 3, V
0,2, 0,0, 3, HV
0,3, 0,0, 3
";
        let air = AirFile::from_str(air_text).unwrap();
        let action = air.action(5).unwrap();

        assert!(action.frames[0].flip_h);
        assert!(!action.frames[0].flip_v);

        assert!(!action.frames[1].flip_h);
        assert!(action.frames[1].flip_v);

        assert!(action.frames[2].flip_h);
        assert!(action.frames[2].flip_v);

        assert!(!action.frames[3].flip_h);
        assert!(!action.frames[3].flip_v);
    }

    #[test]
    fn parse_blend_modes() {
        let air_text = "\
[Begin Action 20]
0,0, 0,0, 3, , A
0,1, 0,0, 3, , S
0,2, 0,0, 3, , A1
0,3, 0,0, 3, , AS128D256
";
        let air = AirFile::from_str(air_text).unwrap();
        let action = air.action(20).unwrap();

        assert_eq!(action.frames[0].blend, BlendMode::Additive);
        assert_eq!(action.frames[1].blend, BlendMode::Subtractive);
        assert_eq!(action.frames[2].blend, BlendMode::Additive);
        assert_eq!(action.frames[3].blend, BlendMode::AdditiveAlpha(128));
    }

    #[test]
    fn parse_negative_action() {
        let air_text = "\
[Begin Action -1]
0,0, 0,0, -1
";
        let air = AirFile::from_str(air_text).unwrap();
        let action = air.action(-1).unwrap();
        assert_eq!(action.frames[0].ticks, -1);
    }

    #[test]
    fn parse_collision_boxes() {
        let air_text = "\
[Begin Action 200]
Clsn2Default: 1
 Clsn2[0] = -10, -80, 10, 0
Clsn1: 1
 Clsn1[0] = 5, -70, 40, -20
0,0, 0,0, 3
0,1, 0,0, 3
";
        let air = AirFile::from_str(air_text).unwrap();
        let action = air.action(200).unwrap();

        // Frame 0 has per-frame clsn1 and default clsn2
        assert_eq!(action.frames[0].clsn1.len(), 1);
        assert_eq!(action.frames[0].clsn2.len(), 1);

        // Frame 1 has no per-frame clsn1 (override cleared), but still has default clsn2
        assert_eq!(action.frames[1].clsn1.len(), 0);
        assert_eq!(action.frames[1].clsn2.len(), 1);

        // Verify clsn2 default box coordinates
        let hurtbox = &action.frames[0].clsn2[0];
        assert_eq!(hurtbox.x, -10.0);
        assert_eq!(hurtbox.y, -80.0);
        assert_eq!(hurtbox.w, 20.0); // right - left = 10 - (-10) = 20
        assert_eq!(hurtbox.h, 80.0); // bottom - top = 0 - (-80) = 80
    }

    #[test]
    fn parse_multiple_actions() {
        let air_text = "\
[Begin Action 0]
0,0, 0,0, 7
0,1, 0,0, 7

[Begin Action 5]
5,0, 0,0, 3
5,1, 0,0, 3

[Begin Action 200]
200,0, 0,0, 4
";
        let air = AirFile::from_str(air_text).unwrap();
        assert_eq!(air.actions.len(), 3);
        assert!(air.action(0).is_some());
        assert!(air.action(5).is_some());
        assert!(air.action(200).is_some());
    }

    #[test]
    fn comments_and_blank_lines_ignored() {
        let air_text = "\
; This is a comment
[Begin Action 0]

; Another comment
0,0, 0,0, 7
; Yet another
0,1, 0,0, 7

";
        let air = AirFile::from_str(air_text).unwrap();
        let action = air.action(0).unwrap();
        assert_eq!(action.frames.len(), 2);
    }

    #[test]
    fn empty_file_returns_error() {
        let result = AirFile::from_str("");
        assert!(result.is_err());
    }

    #[test]
    fn frame_offsets_parsed() {
        let air_text = "\
[Begin Action 0]
0,0, -5, 10, 7
";
        let air = AirFile::from_str(air_text).unwrap();
        let frame = &air.action(0).unwrap().frames[0];
        assert_eq!(frame.offset.x, -5);
        assert_eq!(frame.offset.y, 10);
    }

    #[test]
    fn plain_frames_have_no_extended_params() {
        // Regression: a plain KFM-style AIR must leave every extended field
        // at its default (None / no interpolation).
        let air = AirFile::from_str(SIMPLE_AIR).unwrap();
        let action = air.action(0).unwrap();
        for frame in &action.frames {
            assert!(frame.scale.is_none());
            assert!(frame.angle.is_none());
            assert!(frame.interpolate.is_none());
        }
    }

    #[test]
    fn parse_extended_scale_and_angle() {
        // group, image, x, y, ticks, flip, blend, xscale, yscale, angle
        let air_text = "\
[Begin Action 30]
0,0, 0,0, 5, , , 2.0, 3.0, 45
0,1, 0,0, 5, H, A, 1.5, 1.5, -90
0,2, 0,0, 5
";
        let air = AirFile::from_str(air_text).unwrap();
        let action = air.action(30).unwrap();

        let f0 = &action.frames[0];
        let scale = f0.scale.expect("frame 0 has scale");
        assert_eq!(scale.x, 2.0);
        assert_eq!(scale.y, 3.0);
        assert_eq!(f0.angle, Some(45.0));

        let f1 = &action.frames[1];
        assert!(f1.flip_h);
        assert_eq!(f1.blend, BlendMode::Additive);
        let scale = f1.scale.expect("frame 1 has scale");
        assert_eq!(scale.x, 1.5);
        assert_eq!(scale.y, 1.5);
        assert_eq!(f1.angle, Some(-90.0));

        // Frame without extended columns stays None.
        let f2 = &action.frames[2];
        assert!(f2.scale.is_none());
        assert!(f2.angle.is_none());
    }

    #[test]
    fn extended_scale_without_angle() {
        let air_text = "\
[Begin Action 31]
0,0, 0,0, 5, , , 2.0, 4.0
";
        let air = AirFile::from_str(air_text).unwrap();
        let f = &air.action(31).unwrap().frames[0];
        let scale = f.scale.expect("has scale");
        assert_eq!(scale.x, 2.0);
        assert_eq!(scale.y, 4.0);
        assert!(f.angle.is_none());
    }

    #[test]
    fn parse_interpolate_lines() {
        let air_text = "\
[Begin Action 40]
0,0, 0,0, 5
Interpolate Offset
Interpolate Scale
0,1, 0,0, 5, , , 2.0, 2.0
Interpolate Angle
Interpolate Blend
0,2, 0,0, 5, , , 1.0, 1.0, 90
0,3, 0,0, 5
";
        let air = AirFile::from_str(air_text).unwrap();
        let action = air.action(40).unwrap();

        // Frame 0: no interpolation declared before it.
        assert!(action.frames[0].interpolate.is_none());

        // Frame 1: Offset + Scale interpolate into it.
        let i1 = action.frames[1].interpolate;
        assert!(i1.offset);
        assert!(i1.scale);
        assert!(!i1.angle);
        assert!(!i1.blend);

        // Frame 2: Angle + Blend interpolate into it (flags were reset after f1).
        let i2 = action.frames[2].interpolate;
        assert!(!i2.offset);
        assert!(!i2.scale);
        assert!(i2.angle);
        assert!(i2.blend);

        // Frame 3: flags reset again, none pending.
        assert!(action.frames[3].interpolate.is_none());
    }

    #[test]
    fn interpolate_is_case_insensitive() {
        let air_text = "\
[Begin Action 41]
0,0, 0,0, 5
interpolate offset
INTERPOLATE Scale
0,1, 0,0, 5
";
        let air = AirFile::from_str(air_text).unwrap();
        let i = air.action(41).unwrap().frames[1].interpolate;
        assert!(i.offset);
        assert!(i.scale);
    }

    #[test]
    fn extended_params_coexist_with_collision_boxes() {
        let air_text = "\
[Begin Action 42]
Clsn2Default: 1
 Clsn2[0] = -10, -80, 10, 0
Interpolate Scale
0,0, 0,0, 3, , , 2.0, 2.0, 30
";
        let air = AirFile::from_str(air_text).unwrap();
        let f = &air.action(42).unwrap().frames[0];
        assert_eq!(f.clsn2.len(), 1);
        assert!(f.interpolate.scale);
        assert_eq!(f.scale.map(|s| s.x), Some(2.0));
        assert_eq!(f.angle, Some(30.0));
    }

    #[test]
    fn parse_labeled_begin_action_header() {
        // MUGEN allows `[Begin Action <n>, <free-text comment>]`; only the
        // leading number is significant.
        assert_eq!(
            parse_begin_action("[Begin Action 12010, Tornado Whirlwind]"),
            Some(12010)
        );
        assert_eq!(parse_begin_action("[Begin Action -3, x]"), Some(-3));
        assert_eq!(
            parse_begin_action("[Begin Action 10900, new charge up]"),
            Some(10900)
        );
        // Plain headers (no label) still work.
        assert_eq!(parse_begin_action("[Begin Action 0]"), Some(0));
        assert_eq!(parse_begin_action("[Begin Action 200]"), Some(200));
        // A header whose first token is not a number is rejected.
        assert_eq!(parse_begin_action("[Begin Action foo, bar]"), None);
        // An empty leading token (label present but no number) is rejected. This
        // locks the behavior against a future refactor of the token extraction.
        assert_eq!(parse_begin_action("[Begin Action , foo]"), None);
        assert_eq!(parse_begin_action("[Begin Action ,]"), None);
    }

    #[test]
    fn labeled_action_headers_not_folded() {
        // Regression: a header with a trailing `, <label>` used to fail to parse
        // its number, so the WHOLE header was missed and its frames were folded
        // into the previous action — losing the labeled action entirely.
        let air_text = "\
[Begin Action 10]
10,0, 0,0, 5
[Begin Action 12010, Tornado Whirlwind]
12010,0, 0,0, 7
12010,1, 0,0, 6
[Begin Action 12011, Tornado Whirlwind]
12011,0, 0,0, 4
[Begin Action 10900, new charge up]
6020,0, 4,0, 4
";
        let air = AirFile::from_str(air_text).unwrap();
        // All four actions must be present and distinct.
        assert_eq!(air.actions.len(), 4);
        assert_eq!(air.action(10).unwrap().frames.len(), 1);

        let a12010 = air.action(12010).expect("action 12010 must be present");
        assert_eq!(a12010.frames.len(), 2);
        // Its frames must NOT have been folded into action 10.
        assert_eq!(a12010.frames[0].sprite, SpriteId::new(12010, 0));

        assert!(air.action(12011).is_some());
        assert!(air.action(10900).is_some());
    }

    #[test]
    fn clsn_default_with_trailing_typo_recognized() {
        // Real content contains `Clsn2Defaultf: 1` (a stray `f` before the
        // colon). It must be recognized as a Clsn2Default header so the box is
        // applied as the frame's default hurtbox, not dropped.
        assert_eq!(
            parse_clsn_default("Clsn2Defaultf: 1", "Clsn2Default"),
            Some(1)
        );
        assert_eq!(
            parse_clsn_default("Clsn1Defaultx: 2", "Clsn1Default"),
            Some(2)
        );
        // Well-formed headers still parse.
        assert_eq!(
            parse_clsn_default("Clsn2Default: 3", "Clsn2Default"),
            Some(3)
        );

        let air_text = "\
[Begin Action 300]
Clsn2Defaultf: 1
 Clsn2[0] = -11, -87, 29, -5
230, 0, 0,0, 2
230, 1, 0,0, 2
";
        let air = AirFile::from_str(air_text).unwrap();
        let action = air.action(300).expect("action 300 must parse");
        // The default hurtbox must be applied to both frames.
        assert_eq!(action.frames.len(), 2);
        assert_eq!(action.frames[0].clsn2.len(), 1);
        assert_eq!(action.frames[1].clsn2.len(), 1);
        let box0 = &action.frames[0].clsn2[0];
        assert_eq!(box0.x, -11.0);
        assert_eq!(box0.y, -87.0);
    }

    #[test]
    fn frame_line_with_trailing_junk_ticks() {
        // Real content: `2650, 1, 0,0, 2..A` — the ticks column has trailing
        // junk. The leading integer must be parsed and the frame kept rather
        // than dropped.
        let air_text = "\
[Begin Action 2650]
2650, 0, 0,0, 2,,A
2650, 1, 0,0, 2..A
2650, 2, 0,0, 2,,A
";
        let air = AirFile::from_str(air_text).unwrap();
        let action = air.action(2650).expect("action 2650 must parse");
        // All three frames must be present (the junk frame is not dropped).
        assert_eq!(action.frames.len(), 3);
        assert_eq!(action.frames[1].sprite, SpriteId::new(2650, 1));
        assert_eq!(action.frames[1].ticks, 2);
    }

    #[test]
    fn parse_leading_i32_behavior() {
        // Strict values are unchanged.
        assert_eq!(parse_leading_i32("2650"), Some(2650));
        assert_eq!(parse_leading_i32(" -1 "), Some(-1));
        assert_eq!(parse_leading_i32("+7"), Some(7));
        // Trailing junk is tolerated, leading integer extracted.
        assert_eq!(parse_leading_i32("2..A"), Some(2));
        assert_eq!(parse_leading_i32("12x"), Some(12));
        assert_eq!(parse_leading_i32("-3foo"), Some(-3));
        // No leading integer -> None.
        assert_eq!(parse_leading_i32("A"), None);
        assert_eq!(parse_leading_i32(""), None);
        assert_eq!(parse_leading_i32("+"), None);
        // A well-formed leading integer that overflows i32 is dropped (warn-logged,
        // not silently): both a bare overflow and overflow-with-junk return None.
        assert_eq!(parse_leading_i32("99999999999"), None);
        assert_eq!(parse_leading_i32("99999999999xx"), None);
    }

    // --- Real-fixture tests (skipped when test-assets/ is absent) ---

    /// Resolves a path under the workspace's `test-assets/` directory.
    fn test_asset(rel: &str) -> std::path::PathBuf {
        // CARGO_MANIFEST_DIR points at crates/fp-formats; go up two levels.
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-assets")
            .join(rel)
    }

    #[test]
    fn real_fixture_evilken_labeled_actions_present() {
        // evilken's special-move animations live under labeled `[Begin Action N,
        // <comment>]` headers that the old parser dropped, folding their frames
        // into the previous action. After the fix they must all be present.
        let path = test_asset("evilken/evilken.air");
        if !path.exists() {
            eprintln!("skipping: {} not present", path.display());
            return;
        }
        let air = AirFile::load(&path).expect("evilken.air should parse");

        // Tornado Whirlwind + charge-up special moves (all labeled headers).
        for n in [12010, 12011, 12012, 12013, 12030, 12031, 12032, 12033] {
            assert!(
                air.action(n).is_some(),
                "evilken action {n} (labeled header) must be present"
            );
        }
        // "new charge up" / "new charge up end".
        assert!(
            air.action(10900).is_some(),
            "evilken action 10900 must be present"
        );
        assert!(
            air.action(10901).is_some(),
            "evilken action 10901 must be present"
        );

        // Action 2650 holds the `2..A` junk frame; it must still load with all
        // its frames intact (the junk frame is kept, not dropped).
        let a2650 = air
            .action(2650)
            .expect("evilken action 2650 must be present");
        assert!(
            !a2650.frames.is_empty(),
            "evilken action 2650 must retain its frames"
        );
    }

    #[test]
    fn leading_bom_and_crlf_tolerated() {
        // Real MUGEN `.air` files are UTF-8-with-BOM and CRLF-terminated, and
        // the BOM can land directly on a `[Begin Action]` header.
        let air_text = "\u{feff}[Begin Action 0]\r\n0,0, 0,0, 7\r\n0,1, 0,0, 7\r\n";
        let air = AirFile::from_str(air_text).unwrap();
        let action = air.action(0).expect("action 0 must parse despite BOM/CRLF");
        assert_eq!(action.frames.len(), 2);
        assert_eq!(action.frames[0].ticks, 7);
    }
}
