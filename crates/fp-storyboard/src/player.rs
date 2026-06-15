//! A tick/render driver over a parsed [`Storyboard`].
//!
//! [`crate::storyboard`] turns a storyboard `.def` into a static, typed scene
//! model. This module adds the *playback* half: a [`StoryboardPlayer`] that walks
//! that model one 60 Hz tick at a time, tracking the current scene, the elapsed
//! time within it, and each visible layer's animation cursor, and exposing — per
//! tick — the flat set of sprites to draw ([`StoryboardDraw`]) and whether the
//! storyboard has finished ([`StoryboardPlayer::is_done`]).
//!
//! The player is **pure and rendering-agnostic**: it resolves *which* sprite goes
//! *where* (in the storyboard's own local-coordinate space, Y-down, origin at the
//! coordinate-space top-left) but never touches a GPU. A consumer (e.g. `fp-app`)
//! maps each [`StoryboardDraw`] onto the screen and uploads the pixels. This keeps
//! the whole driver unit-testable from a parsed [`Storyboard`] alone.
//!
//! # Scene flow
//!
//! Playback starts at [`Storyboard::start_scene`] (clamped into range) and
//! advances through the [`Storyboard::scenes`] list **in file order**. A scene is
//! held for its `end.time` ticks; once elapsed, the player moves to the next
//! scene. After the last scene ends the player is *done* and emits an empty draw
//! list. A scene with `end.time <= 0` is treated as a zero-length scene and is
//! stepped over on the next tick (so a malformed storyboard always terminates
//! rather than hanging).
//!
//! # Layers
//!
//! Each scene's `layerN.*` overlays are resolved against the storyboard's embedded
//! `[Begin Action N]` animations (for `layerN.anim`) or drawn as a single static
//! sprite (`layerN.spriteno`). A layer is visible only within
//! `[starttime, endtime]` of its scene. An animated layer advances its own AIR
//! element cursor each tick, looping at the action's `loopstart` like the in-match
//! animator.

use std::collections::HashMap;

use crate::storyboard::{Scene, SceneLayer, Storyboard};
use fp_core::SpriteId;
use fp_formats::air::{AnimAction, BlendMode};

/// One sprite to draw for the current storyboard tick.
///
/// Positions are in the storyboard's **local coordinate space** (the
/// `[Info] localcoord` frame), Y increasing downward, before any screen mapping.
/// `pos` already folds in the scene's `layerall.pos`, the layer's `offset`, and
/// the current AIR frame's own per-frame offset, so a renderer only has to map
/// local space onto the window and anchor by the sprite's axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StoryboardDraw {
    /// The sprite `(group, image)` to draw, resolved from the layer's current
    /// animation frame or its static `spriteno`.
    pub sprite: SpriteId,
    /// Draw position in storyboard-local coordinates (Y-down).
    pub pos: (f32, f32),
    /// Whether to flip the sprite horizontally (from the AIR frame; always
    /// `false` for a static `spriteno` layer).
    pub flip_h: bool,
    /// Whether to flip the sprite vertically (from the AIR frame; always `false`
    /// for a static `spriteno` layer).
    pub flip_v: bool,
    /// The AIR blend mode for this frame (always [`BlendMode::Normal`] for a
    /// static `spriteno` layer).
    pub blend: BlendMode,
    /// The originating layer index (`layerN`), lowest-first in the draw list.
    /// Exposed so a consumer can apply a stable per-layer draw order if desired;
    /// the list is already returned in ascending layer order.
    pub layer: u32,
}

/// The live animation cursor for one animated storyboard layer.
///
/// Mirrors the in-match animator: `elem` indexes into the action's frame list and
/// `elem_time` counts ticks spent on the current frame; on reaching a frame's
/// `ticks` duration it advances, looping back to `loopstart` past the end. A frame
/// with `ticks <= 0` (MUGEN's `-1` "hold forever") never advances.
#[derive(Debug, Clone, Copy)]
struct LayerCursor {
    /// The `layerN` index this cursor belongs to.
    index: u32,
    /// Current frame index into the action's `frames`.
    elem: usize,
    /// Ticks spent displaying the current frame.
    elem_time: i32,
}

/// A tick driver over a parsed [`Storyboard`].
///
/// Built with [`StoryboardPlayer::new`]; advanced one tick at a time with
/// [`StoryboardPlayer::tick`]; queried with [`StoryboardPlayer::draw_list`] and
/// [`StoryboardPlayer::is_done`]. Holds the storyboard by value so the player is
/// self-contained and `'static`.
#[derive(Debug, Clone)]
pub struct StoryboardPlayer {
    /// The parsed storyboard being played.
    storyboard: Storyboard,
    /// Index into [`Storyboard::scenes`] of the scene currently showing. Once it
    /// reaches `scenes.len()` the player is done.
    scene_index: usize,
    /// Ticks elapsed within the current scene.
    scene_time: i32,
    /// Per-animated-layer cursors for the current scene, in ascending layer order.
    cursors: Vec<LayerCursor>,
    /// Latches `true` once playback runs past the final scene (or starts with no
    /// scenes at all), so [`is_done`](Self::is_done) is stable.
    done: bool,
}

impl StoryboardPlayer {
    /// Creates a player positioned at the storyboard's `start_scene` (clamped into
    /// `0..scenes.len()`), with `scene_time` at `0` and every animated layer's
    /// cursor reset to its first frame.
    ///
    /// An empty storyboard (no scenes) starts already [`done`](Self::is_done).
    #[must_use]
    pub fn new(storyboard: Storyboard) -> Self {
        let scene_count = storyboard.scenes.len();
        // `start_scene` is authored 0-based; clamp negatives and overshoots so a
        // malformed value can never index out of range.
        let start = if scene_count == 0 {
            0
        } else {
            storyboard
                .start_scene
                .clamp(0, scene_count as i32 - 1) as usize
        };
        let done = scene_count == 0;
        let cursors = if done {
            Vec::new()
        } else {
            Self::cursors_for_scene(&storyboard, start)
        };
        Self {
            storyboard,
            scene_index: start,
            scene_time: 0,
            cursors,
            done,
        }
    }

    /// The storyboard being played (read-only).
    #[must_use]
    pub fn storyboard(&self) -> &Storyboard {
        &self.storyboard
    }

    /// Whether playback has run past the final scene (or there were no scenes).
    ///
    /// Once `true` this stays `true` and [`draw_list`](Self::draw_list) is empty.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// The 0-based index of the scene currently showing, or the scene count once
    /// the player is [`done`](Self::is_done).
    #[must_use]
    pub fn scene_index(&self) -> usize {
        self.scene_index
    }

    /// Ticks elapsed within the current scene.
    #[must_use]
    pub fn scene_time(&self) -> i32 {
        self.scene_time
    }

    /// Advances playback by one 60 Hz tick.
    ///
    /// Increments the current scene's elapsed time and every visible animated
    /// layer's cursor, rolling over to the next scene once `scene.end_time` is
    /// reached and latching [`done`](Self::is_done) past the last scene. A no-op
    /// once already done. Never panics.
    pub fn tick(&mut self) {
        if self.done {
            return;
        }

        // Advance the per-layer animation cursors for the scene we are showing.
        // (A static-spriteno layer has no cursor, so it is simply not present.)
        // Borrow the scene and animation map directly here (not through a `&self`
        // helper) so the `&mut self.cursors` iteration does not alias `&self`.
        if let Some(scene) = self.storyboard.scenes.get(self.scene_index) {
            let animations = &self.storyboard.animations;
            for cursor in &mut self.cursors {
                if let Some(action) =
                    layer_action(scene, animations, cursor.index)
                {
                    advance_cursor(cursor, action);
                }
            }
        }

        self.scene_time += 1;

        // Roll over any number of zero/finished scenes in this tick so a chain of
        // `end.time <= 0` scenes (or an exactly-elapsed one) always terminates.
        loop {
            let Some(scene) = self.storyboard.scenes.get(self.scene_index) else {
                self.done = true;
                self.cursors.clear();
                return;
            };
            // `end.time` is the scene's total duration in ticks; a non-positive
            // duration is a degenerate zero-length scene we step straight over.
            let end = scene.end_time.max(0);
            if self.scene_time < end {
                break;
            }
            // This scene is finished; advance to the next.
            self.scene_index += 1;
            self.scene_time = 0;
            if self.scene_index >= self.storyboard.scenes.len() {
                self.done = true;
                self.cursors.clear();
                return;
            }
            self.cursors = Self::cursors_for_scene(&self.storyboard, self.scene_index);
        }
    }

    /// The sprites to draw for the current tick, in ascending layer order.
    ///
    /// Returns an empty list once the player is [`done`](Self::is_done). Each
    /// visible layer contributes at most one [`StoryboardDraw`]: an animated layer
    /// resolves its current AIR frame's sprite/offset/flip/blend; a static layer
    /// emits its `spriteno`. A layer outside its `[starttime, endtime]` window, or
    /// one whose animation/sprite cannot be resolved, contributes nothing.
    #[must_use]
    pub fn draw_list(&self) -> Vec<StoryboardDraw> {
        let mut out = Vec::new();
        if self.done {
            return out;
        }
        let Some(scene) = self.storyboard.scenes.get(self.scene_index) else {
            return out;
        };

        for layer in &scene.layers {
            if !self.layer_visible(layer) {
                continue;
            }
            if let Some(draw) = self.layer_draw(layer) {
                out.push(draw);
            }
        }
        out
    }

    // -- internals ----------------------------------------------------------

    /// Builds the fresh per-layer cursors for `scene_index`: one [`LayerCursor`]
    /// per layer that references a resolvable embedded animation (`layerN.anim`),
    /// in ascending layer order. Static-spriteno layers get no cursor.
    fn cursors_for_scene(storyboard: &Storyboard, scene_index: usize) -> Vec<LayerCursor> {
        let Some(scene) = storyboard.scenes.get(scene_index) else {
            return Vec::new();
        };
        scene
            .layers
            .iter()
            .filter(|layer| {
                layer
                    .anim
                    .is_some_and(|n| storyboard.animations.contains_key(&n))
            })
            .map(|layer| LayerCursor {
                index: layer.index,
                elem: 0,
                elem_time: 0,
            })
            .collect()
    }

    /// Whether `layer` is within its `[starttime, endtime]` visibility window for
    /// the current `scene_time`. `endtime` is inclusive; an absent `endtime` means
    /// "visible to the end of the scene".
    fn layer_visible(&self, layer: &SceneLayer) -> bool {
        if self.scene_time < layer.starttime {
            return false;
        }
        match layer.endtime {
            Some(end) => self.scene_time <= end,
            None => true,
        }
    }

    /// The effective `layerall.pos` for the scene at `scene_index`, applying
    /// MUGEN's carry-over rule: a scene that **omits** `layerall.pos`
    /// ([`Scene::layerall_pos`] is `None`) inherits the value from the nearest
    /// preceding scene that set it explicitly; an explicit `0,0` does not inherit.
    /// Falls back to `(0.0, 0.0)` when no scene up to here ever set it.
    ///
    /// This walks scenes in file order, which is the order the player advances
    /// through them, so the inherited value matches what playback has "seen".
    #[must_use]
    pub fn effective_layerall_pos(&self, scene_index: usize) -> (f32, f32) {
        // Scan backward from `scene_index` for the first scene that set the key.
        for idx in (0..=scene_index).rev() {
            if let Some(scene) = self.storyboard.scenes.get(idx) {
                if let Some(pos) = scene.layerall_pos {
                    return pos;
                }
            }
        }
        (0.0, 0.0)
    }

    /// Builds the single [`StoryboardDraw`] for one visible layer, or `None` if it
    /// resolves to no sprite (missing animation, empty action, or no `spriteno`).
    fn layer_draw(&self, layer: &SceneLayer) -> Option<StoryboardDraw> {
        // Base position: scene default + this layer's offset, in local coords.
        // The scene default carries over from a prior scene when this scene omits
        // `layerall.pos` (MUGEN's documented inheritance).
        let layerall = self.effective_layerall_pos(self.scene_index);
        let base_x = layerall.0 + layer.offset.0;
        let base_y = layerall.1 + layer.offset.1;

        if let Some(anim) = layer.anim {
            let action = self.storyboard.animations.get(&anim)?;
            if action.frames.is_empty() {
                return None;
            }
            // Find this layer's live cursor; absent (shouldn't happen for an
            // animated, resolvable layer) falls back to the first frame.
            let elem = self
                .cursors
                .iter()
                .find(|c| c.index == layer.index)
                .map(|c| c.elem.min(action.frames.len() - 1))
                .unwrap_or(0);
            let frame = action.frames.get(elem)?;
            Some(StoryboardDraw {
                sprite: frame.sprite,
                pos: (
                    base_x + frame.offset.x as f32,
                    base_y + frame.offset.y as f32,
                ),
                flip_h: frame.flip_h,
                flip_v: frame.flip_v,
                blend: frame.blend,
                layer: layer.index,
            })
        } else if let Some((group, image)) = layer.spriteno {
            let sprite = sprite_id_from_pair(group, image)?;
            Some(StoryboardDraw {
                sprite,
                pos: (base_x, base_y),
                flip_h: false,
                flip_v: false,
                blend: BlendMode::Normal,
                layer: layer.index,
            })
        } else {
            None
        }
    }
}

/// Resolves the embedded [`AnimAction`] a scene layer's `layerN.anim` points at,
/// if the layer with `layer_index` exists, is animated, and the action is present.
fn layer_action<'a>(
    scene: &Scene,
    animations: &'a HashMap<i32, AnimAction>,
    layer_index: u32,
) -> Option<&'a AnimAction> {
    let layer = scene.layers.iter().find(|l| l.index == layer_index)?;
    let anim = layer.anim?;
    animations.get(&anim)
}

/// Advances one animated layer's cursor by one tick, looping at `loopstart` past
/// the action's end. A frame whose `ticks <= 0` (MUGEN's `-1` hold) never
/// advances; an empty action is a no-op.
fn advance_cursor(cursor: &mut LayerCursor, action: &AnimAction) {
    if action.frames.is_empty() {
        return;
    }
    // Guard a cursor that somehow points past the (possibly shorter) action.
    if cursor.elem >= action.frames.len() {
        cursor.elem = action.loopstart.min(action.frames.len() - 1);
        cursor.elem_time = 0;
    }
    cursor.elem_time += 1;
    while let Some(frame) = action.frames.get(cursor.elem) {
        // `ticks <= 0` holds the frame indefinitely (MUGEN `-1`).
        if frame.ticks <= 0 || cursor.elem_time < frame.ticks {
            break;
        }
        cursor.elem_time = 0;
        cursor.elem += 1;
        if cursor.elem >= action.frames.len() {
            cursor.elem = action.loopstart.min(action.frames.len() - 1);
        }
    }
}

/// Converts a storyboard `(group, image)` pair (stored as `i32`) into a
/// [`SpriteId`], returning `None` when either falls outside the SFF `u16` range
/// rather than wrapping to a wrong sprite.
fn sprite_id_from_pair(group: i32, image: i32) -> Option<SpriteId> {
    match (u16::try_from(group), u16::try_from(image)) {
        (Ok(g), Ok(i)) => Some(SpriteId::new(g, i)),
        _ => {
            tracing::warn!("storyboard: spriteno ({group}, {image}) out of SFF range; skipping");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storyboard::Storyboard;

    /// A storyboard with two scenes, each with one animated overlay layer that
    /// references a single-frame embedded action. Scene 0 lasts 3 ticks, scene 1
    /// lasts 2 ticks.
    const TWO_SCENE: &str = "\
[SceneDef]
spr = t.sff
startscene = 0

[Scene 0]
layerall.pos = 160,0
layer0.anim = 0
layer0.offset = 0,32
end.time = 3

[Scene 1]
layer0.anim = 1
layer0.offset = 0,176
end.time = 2

[Begin Action 0]
10,0, 0,0, -1

[Begin Action 1]
11,3, 0,0, -1
";

    #[test]
    fn starts_on_start_scene_and_draws() {
        let sb = Storyboard::from_def(TWO_SCENE);
        let player = StoryboardPlayer::new(sb);
        assert!(!player.is_done());
        assert_eq!(player.scene_index(), 0);
        let draws = player.draw_list();
        assert_eq!(draws.len(), 1, "scene 0 has one visible layer");
        // Action 0 frame 0 is sprite (10, 0); pos = layerall(160,0)+offset(0,32).
        assert_eq!(draws[0].sprite, SpriteId::new(10, 0));
        assert_eq!(draws[0].pos, (160.0, 32.0));
        assert_eq!(draws[0].layer, 0);
    }

    #[test]
    fn advances_to_next_scene_after_end_time() {
        let sb = Storyboard::from_def(TWO_SCENE);
        let mut player = StoryboardPlayer::new(sb);
        // Scene 0 ends after 3 ticks.
        player.tick(); // t=1
        player.tick(); // t=2
        assert_eq!(player.scene_index(), 0, "still in scene 0 before end");
        player.tick(); // t=3 -> rolls to scene 1
        assert_eq!(player.scene_index(), 1, "rolled into scene 1 at end.time");
        assert_eq!(player.scene_time(), 0, "scene time reset on roll-over");
        assert!(!player.is_done());
        // Now drawing scene 1's layer: action 1 frame 0 = sprite (11, 3).
        // Scene 1 OMITS layerall.pos, so it inherits scene 0's (160,0) per MUGEN's
        // carry-over rule; pos = carried (160,0) + offset (0,176).
        let draws = player.draw_list();
        assert_eq!(draws.len(), 1);
        assert_eq!(draws[0].sprite, SpriteId::new(11, 3));
        assert_eq!(draws[0].pos, (160.0, 176.0));
    }

    /// MUGEN carry-over: a scene that omits `layerall.pos` inherits the previous
    /// scene's value (the real KFM intro relies on this — only scene 0 declares
    /// `layerall.pos = 160,0`). An explicit `0,0` later breaks the chain.
    #[test]
    fn layerall_pos_carries_over_across_scenes() {
        let text = "\
[Scene 0]
layerall.pos = 160,0
layer0.spriteno = 1,0
end.time = 1
[Scene 1]
layer0.spriteno = 2,0
end.time = 1
[Scene 2]
layerall.pos = 0,0
layer0.spriteno = 3,0
end.time = 1
[Scene 3]
layer0.spriteno = 4,0
end.time = 1
";
        let sb = Storyboard::from_def(text);
        let player = StoryboardPlayer::new(sb);

        // Scene 0 explicitly sets 160,0.
        assert_eq!(player.effective_layerall_pos(0), (160.0, 0.0));
        // Scene 1 omits it -> inherits scene 0's 160,0.
        assert_eq!(
            player.effective_layerall_pos(1),
            (160.0, 0.0),
            "scene 1 must inherit scene 0's layerall.pos"
        );
        // Scene 2 sets it explicitly to 0,0 -> NOT inherited from scene 0.
        assert_eq!(player.effective_layerall_pos(2), (0.0, 0.0));
        // Scene 3 omits it -> inherits scene 2's explicit 0,0.
        assert_eq!(player.effective_layerall_pos(3), (0.0, 0.0));

        // And the carried value reaches the actual draw: scene 0's static layer is
        // at 160,0 (carried pos + zero offset).
        let draws = player.draw_list();
        assert_eq!(draws[0].pos, (160.0, 0.0));
    }

    /// When the very first scene omits `layerall.pos`, there is nothing to inherit,
    /// so it falls back to the (0,0) MUGEN default rather than panicking.
    #[test]
    fn layerall_pos_absent_from_start_defaults_to_zero() {
        let text = "\
[Scene 0]
layer0.spriteno = 1,0
end.time = 1
";
        let sb = Storyboard::from_def(text);
        let player = StoryboardPlayer::new(sb);
        assert_eq!(player.effective_layerall_pos(0), (0.0, 0.0));
        assert_eq!(player.draw_list()[0].pos, (0.0, 0.0));
    }

    #[test]
    fn finishes_after_last_scene() {
        let sb = Storyboard::from_def(TWO_SCENE);
        let mut player = StoryboardPlayer::new(sb);
        // 3 ticks to finish scene 0, then 2 to finish scene 1 = 5 ticks total.
        for _ in 0..5 {
            player.tick();
        }
        assert!(player.is_done(), "both scenes elapsed");
        assert!(player.draw_list().is_empty(), "done -> empty draw list");
        // Ticking past the end is a stable no-op.
        player.tick();
        assert!(player.is_done());
    }

    #[test]
    fn empty_storyboard_starts_done() {
        let sb = Storyboard::from_def("[Info]\nlocalcoord = 320,240\n");
        let mut player = StoryboardPlayer::new(sb);
        assert!(player.is_done(), "no scenes -> immediately done");
        assert!(player.draw_list().is_empty());
        player.tick();
        assert!(player.is_done());
    }

    #[test]
    fn start_scene_clamped_into_range() {
        // startscene = 9 but only 2 scenes -> clamp to the last (index 1).
        let text = "\
[SceneDef]
startscene = 9
[Scene 0]
end.time = 5
[Scene 1]
end.time = 5
";
        let sb = Storyboard::from_def(text);
        let player = StoryboardPlayer::new(sb);
        assert_eq!(player.scene_index(), 1, "overshooting startscene clamps");
    }

    #[test]
    fn negative_start_scene_clamped_to_zero() {
        let text = "\
[SceneDef]
startscene = -3
[Scene 0]
end.time = 5
";
        let sb = Storyboard::from_def(text);
        let player = StoryboardPlayer::new(sb);
        assert_eq!(player.scene_index(), 0);
    }

    #[test]
    fn zero_length_scenes_are_stepped_over() {
        // Two zero-length scenes followed by a real one: a single tick must roll
        // through both degenerate scenes without hanging.
        let text = "\
[Scene 0]
layer0.spriteno = 1,0
end.time = 0
[Scene 1]
layer0.spriteno = 2,0
end.time = 0
[Scene 2]
layer0.spriteno = 3,0
end.time = 4
";
        let sb = Storyboard::from_def(text);
        let mut player = StoryboardPlayer::new(sb);
        // Scene 0 has end.time 0; first tick should roll all the way to scene 2.
        player.tick();
        assert_eq!(player.scene_index(), 2, "rolled past both zero-length scenes");
        assert!(!player.is_done());
        let draws = player.draw_list();
        assert_eq!(draws.len(), 1);
        assert_eq!(draws[0].sprite, SpriteId::new(3, 0));
    }

    #[test]
    fn static_spriteno_layer_draws() {
        let text = "\
[Scene 0]
layerall.pos = 10,20
layer0.spriteno = 7,2
layer0.offset = 3,4
end.time = 5
";
        let sb = Storyboard::from_def(text);
        let player = StoryboardPlayer::new(sb);
        let draws = player.draw_list();
        assert_eq!(draws.len(), 1);
        assert_eq!(draws[0].sprite, SpriteId::new(7, 2));
        // pos = layerall(10,20) + offset(3,4); a static sprite has no frame offset.
        assert_eq!(draws[0].pos, (13.0, 24.0));
        assert_eq!(draws[0].blend, BlendMode::Normal);
    }

    #[test]
    fn layer_visibility_window_respected() {
        // layer0 shows from tick 2 to 4 inclusive.
        let text = "\
[Scene 0]
layer0.spriteno = 1,0
layer0.starttime = 2
layer0.endtime = 4
end.time = 10
";
        let sb = Storyboard::from_def(text);
        let mut player = StoryboardPlayer::new(sb);
        assert!(player.draw_list().is_empty(), "t=0 before starttime");
        player.tick(); // t=1
        assert!(player.draw_list().is_empty(), "t=1 before starttime");
        player.tick(); // t=2
        assert_eq!(player.draw_list().len(), 1, "t=2 visible (starttime)");
        player.tick(); // t=3
        assert_eq!(player.draw_list().len(), 1, "t=3 visible");
        player.tick(); // t=4
        assert_eq!(player.draw_list().len(), 1, "t=4 visible (endtime inclusive)");
        player.tick(); // t=5
        assert!(player.draw_list().is_empty(), "t=5 past endtime");
    }

    #[test]
    fn animation_cursor_advances_and_loops() {
        // A two-frame looping action: frame 0 for 2 ticks, frame 1 for 2 ticks,
        // looping. Layer references it; track the drawn sprite over ticks.
        let text = "\
[Scene 0]
layer0.anim = 5
end.time = 100
[Begin Action 5]
100,0, 0,0, 2
100,1, 0,0, 2
";
        let sb = Storyboard::from_def(text);
        let mut player = StoryboardPlayer::new(sb);
        // t=0: frame 0 -> (100,0)
        assert_eq!(player.draw_list()[0].sprite, SpriteId::new(100, 0));
        player.tick(); // t=1: still frame 0 (2-tick duration)
        assert_eq!(player.draw_list()[0].sprite, SpriteId::new(100, 0));
        player.tick(); // t=2: advanced to frame 1
        assert_eq!(player.draw_list()[0].sprite, SpriteId::new(100, 1));
        player.tick(); // t=3: still frame 1
        assert_eq!(player.draw_list()[0].sprite, SpriteId::new(100, 1));
        player.tick(); // t=4: looped back to frame 0
        assert_eq!(player.draw_list()[0].sprite, SpriteId::new(100, 0));
    }

    #[test]
    fn missing_animation_reference_draws_nothing() {
        // layer references action 99 which does not exist -> no draw, no cursor,
        // no panic, scene still advances normally.
        let text = "\
[Scene 0]
layer0.anim = 99
end.time = 3
";
        let sb = Storyboard::from_def(text);
        let mut player = StoryboardPlayer::new(sb);
        assert!(player.draw_list().is_empty(), "unresolved anim draws nothing");
        for _ in 0..3 {
            player.tick();
        }
        assert!(player.is_done(), "scene still advances and finishes");
    }

    #[test]
    fn multiple_layers_in_ascending_order() {
        let text = "\
[Scene 0]
layer2.spriteno = 3,0
layer0.spriteno = 1,0
layer1.spriteno = 2,0
end.time = 5
";
        let sb = Storyboard::from_def(text);
        let player = StoryboardPlayer::new(sb);
        let draws = player.draw_list();
        assert_eq!(draws.len(), 3);
        // SceneLayer list is sorted by index in the parser, so the draw list is too.
        assert_eq!(draws[0].layer, 0);
        assert_eq!(draws[1].layer, 1);
        assert_eq!(draws[2].layer, 2);
        assert_eq!(draws[0].sprite, SpriteId::new(1, 0));
        assert_eq!(draws[2].sprite, SpriteId::new(3, 0));
    }

    #[test]
    fn out_of_range_spriteno_skipped() {
        // A negative group has no valid SpriteId; the layer is skipped, not a panic.
        let text = "\
[Scene 0]
layer0.spriteno = -1,0
end.time = 5
";
        let sb = Storyboard::from_def(text);
        let player = StoryboardPlayer::new(sb);
        assert!(player.draw_list().is_empty(), "out-of-range spriteno skipped");
    }

    #[test]
    fn hold_forever_frame_does_not_advance() {
        // Single frame with ticks = -1 (hold forever): the cursor never moves.
        let text = "\
[Scene 0]
layer0.anim = 0
end.time = 100
[Begin Action 0]
5,0, 0,0, -1
";
        let sb = Storyboard::from_def(text);
        let mut player = StoryboardPlayer::new(sb);
        for _ in 0..10 {
            player.tick();
        }
        // Still in scene 0 (end.time 100), still showing the single held frame.
        assert_eq!(player.scene_index(), 0);
        assert_eq!(player.draw_list()[0].sprite, SpriteId::new(5, 0));
    }

    #[test]
    fn real_kfm_intro_plays_to_completion() {
        // Asset-gated: exercises the real KFM intro storyboard end to end when the
        // test-assets fixture is present, and skips cleanly when it is absent.
        // `CARGO_MANIFEST_DIR` points at `crates/fp-storyboard`; the workspace
        // `test-assets/` symlink is two levels up (matching tests/real_fixtures.rs).
        // A bare relative path would resolve against the crate dir and always skip.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-assets/kfm/intro.def");
        let Ok(sb) = Storyboard::load(&path) else {
            eprintln!("skipping: {} not present", path.display());
            return;
        };
        assert!(!sb.scenes.is_empty(), "KFM intro has scenes");
        let total: i32 = sb.scenes.iter().map(|s| s.end_time.max(0)).sum();
        let mut player = StoryboardPlayer::new(sb);
        assert!(!player.is_done());
        // The first scene draws its overlay layers.
        assert!(
            !player.draw_list().is_empty(),
            "KFM intro scene 0 draws overlay anims"
        );
        // Ticking through the summed scene durations finishes the storyboard.
        for _ in 0..=total {
            player.tick();
        }
        assert!(player.is_done(), "KFM intro plays to completion");
        assert!(player.draw_list().is_empty());
    }
}
