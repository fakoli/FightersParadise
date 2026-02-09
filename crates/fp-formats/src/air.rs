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
#[derive(Debug, Clone)]
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
    /// Attack collision boxes (Clsn1) for this frame.
    pub clsn1: Vec<Rect>,
    /// Hurtbox collision boxes (Clsn2) for this frame.
    pub clsn2: Vec<Rect>,
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
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(text: &str) -> FpResult<Self> {
        let mut actions = HashMap::new();
        let mut current_action: Option<ActionBuilder> = None;

        for raw_line in text.lines() {
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
fn parse_begin_action(line: &str) -> Option<i32> {
    let line_lower = line.to_ascii_lowercase();
    let trimmed = line_lower.trim();
    if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
        return None;
    }
    let inner = &trimmed[1..trimmed.len() - 1].trim();
    let rest = inner.strip_prefix("begin action")?;
    rest.trim().parse::<i32>().ok()
}

/// Parse `ClsnNDefault: count` header.
fn parse_clsn_default(line: &str, prefix: &str) -> Option<usize> {
    let lower = line.to_ascii_lowercase();
    let p = prefix.to_ascii_lowercase();
    if !lower.starts_with(&p) {
        return None;
    }
    let rest = &line[prefix.len()..];
    let rest = rest.trim().strip_prefix(':')?;
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

/// Parse a frame line: `group, image, x_offset, y_offset, ticks[, flip[, blend]]`
fn parse_frame_line(line: &str, builder: &ActionBuilder) -> Option<AnimFrame> {
    let parts: Vec<&str> = line.split(',').collect();
    if parts.len() < 5 {
        return None;
    }

    let group = parts[0].trim().parse::<i32>().ok()? as u16;
    let image = parts[1].trim().parse::<i32>().ok()? as u16;
    let x_offset = parts[2].trim().parse::<i16>().ok()?;
    let y_offset = parts[3].trim().parse::<i16>().ok()?;
    let ticks = parts[4].trim().parse::<i32>().ok()?;

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
        clsn1,
        clsn2,
    })
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
}
