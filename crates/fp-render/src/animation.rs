//! Animation controller for managing frame playback.
//!
//! The [`AnimController`] drives animation playback by tracking the current frame,
//! managing timing, and handling looping. It consumes [`AnimAction`] data from the
//! AIR parser and provides the current frame's sprite/transform information to
//! the rendering system each tick.
//!
//! MUGEN animations run at 60 ticks/second. Each frame has a `ticks` duration
//! that specifies how many game ticks it stays on screen. A ticks value of -1
//! means the frame is held indefinitely.

use fp_core::Vec2;
use fp_formats::air::{AnimAction, AnimFrame};

/// The resolved per-frame sprite transform (scale + rotation) for a single
/// rendered tick.
///
/// MUGEN's extended AIR lets a frame carry an optional `xscale, yscale` pair and
/// an `angle` (degrees). This is the *resolved* transform — defaults filled in
/// (`scale = (1.0, 1.0)`, `angle_deg = 0.0` when the frame omits them), and any
/// `Interpolate Scale` / `Interpolate Angle` already blended from the previous
/// frame. The renderer maps it onto [`SpriteDrawParams`](crate::SpriteDrawParams)
/// (`scale_x`/`scale_y`, and `angle` converted to radians).
///
/// [`IDENTITY`](Self::IDENTITY) (also [`Default`]) is the no-op transform: a
/// sprite drawn with it is positioned exactly as before this feature existed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameTransform {
    /// Per-axis scale factor (`1.0` = original size). Defaults to `(1.0, 1.0)`.
    pub scale: Vec2<f32>,
    /// Rotation in **degrees** (MUGEN's unit; positive is the AIR `angle`
    /// convention). `0.0` = no rotation. Convert to radians before handing to
    /// the renderer.
    pub angle_deg: f32,
}

impl FrameTransform {
    /// The no-op transform: unit scale, zero rotation.
    pub const IDENTITY: Self = Self {
        scale: Vec2 { x: 1.0, y: 1.0 },
        angle_deg: 0.0,
    };

    /// Rotation in radians, for handing straight to
    /// [`SpriteDrawParams::angle`](crate::SpriteDrawParams::angle).
    #[must_use]
    pub fn angle_rad(&self) -> f32 {
        self.angle_deg.to_radians()
    }
}

impl Default for FrameTransform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// The resolved (defaulted) transform a single AIR frame requests, ignoring any
/// interpolation — `scale` defaults to `(1.0, 1.0)` and `angle` to `0.0` when the
/// frame omits them.
#[must_use]
fn frame_transform(frame: &AnimFrame) -> FrameTransform {
    FrameTransform {
        scale: frame.scale.unwrap_or(Vec2::new(1.0, 1.0)),
        angle_deg: frame.angle.unwrap_or(0.0),
    }
}

/// Linearly interpolates the sprite transform from `prev` into `cur` by fraction
/// `t` (clamped to `0.0..=1.0`), honoring `cur.interpolate`.
///
/// MUGEN's `Interpolate Scale` / `Interpolate Angle` lines request that the named
/// transform smoothly blend from the *previous* frame's value to this frame's
/// value across this frame's on-screen duration. `t` is how far into the current
/// element we are (`0.0` at the element's first tick, approaching `1.0` at its
/// last). A transform whose interpolate flag is `false` snaps to `cur`'s own
/// value (no blend), which keeps a plain AIR (no `Interpolate` lines)
/// byte-for-byte unchanged. When `prev` is `None` (the first element) there is
/// nothing to blend from, so `cur`'s value is used directly.
#[must_use]
pub fn interpolated_transform(prev: Option<&AnimFrame>, cur: &AnimFrame, t: f32) -> FrameTransform {
    let t = t.clamp(0.0, 1.0);
    let cur_tf = frame_transform(cur);
    let Some(prev) = prev else {
        return cur_tf;
    };
    let prev_tf = frame_transform(prev);
    let interp = cur.interpolate;
    FrameTransform {
        scale: Vec2::new(
            if interp.scale {
                lerp(prev_tf.scale.x, cur_tf.scale.x, t)
            } else {
                cur_tf.scale.x
            },
            if interp.scale {
                lerp(prev_tf.scale.y, cur_tf.scale.y, t)
            } else {
                cur_tf.scale.y
            },
        ),
        angle_deg: if interp.angle {
            lerp(prev_tf.angle_deg, cur_tf.angle_deg, t)
        } else {
            cur_tf.angle_deg
        },
    }
}

/// Linear interpolation `a + (b - a) * t`.
#[inline]
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Controls playback of a single animation action.
///
/// Created from an [`AnimAction`] (parsed from an AIR file) and advanced one
/// tick at a time via [`tick()`](Self::tick). Query the current frame with
/// [`current_frame()`](Self::current_frame) to get sprite and transform data.
///
/// # Examples
///
/// ```ignore
/// let controller = AnimController::new(action);
/// loop {
///     controller.tick();
///     let frame = controller.current_frame();
///     // render frame.sprite at frame.offset
/// }
/// ```
pub struct AnimController {
    action: AnimAction,
    current_frame_index: usize,
    frame_timer: i32,
    finished: bool,
    total_time: i32,
    loop_count: u32,
}

impl AnimController {
    /// Creates a new controller starting at frame 0 of the given action.
    pub fn new(action: AnimAction) -> Self {
        let initial_ticks = action.frames.first().map_or(0, |f| f.ticks);
        Self {
            action,
            current_frame_index: 0,
            frame_timer: initial_ticks,
            finished: false,
            total_time: 0,
            loop_count: 0,
        }
    }

    /// Advance the animation by one tick (called once per 60Hz game tick).
    ///
    /// Returns `true` if the frame changed during this tick.
    pub fn tick(&mut self) -> bool {
        if self.action.frames.is_empty() {
            return false;
        }

        self.total_time += 1;

        // Infinite-duration frame — never advance
        if self.frame_timer == -1 {
            self.finished = true;
            return false;
        }

        self.frame_timer -= 1;
        if self.frame_timer <= 0 {
            // Advance to next frame
            self.current_frame_index += 1;

            if self.current_frame_index >= self.action.frames.len() {
                // Loop back to loopstart
                self.current_frame_index = self.action.loopstart;
                self.loop_count += 1;
            }

            self.frame_timer = self.action.frames[self.current_frame_index].ticks;
            return true;
        }

        false
    }

    /// Returns the current animation frame data.
    ///
    /// Panics if the action has no frames (should never happen with valid AIR data).
    pub fn current_frame(&self) -> &AnimFrame {
        &self.action.frames[self.current_frame_index]
    }

    /// Returns the current frame index (0-based).
    pub fn frame_index(&self) -> usize {
        self.current_frame_index
    }

    /// Returns the interpolated [`FrameTransform`] (scale + angle) for the
    /// current tick.
    ///
    /// Reads the current frame's `scale`/`angle` (defaulting unset values to
    /// unit scale / no rotation) and, when the current frame requests
    /// `Interpolate Scale` / `Interpolate Angle`, linearly blends from the
    /// *previous* frame's transform across the current element's duration. The
    /// blend fraction is [`anim_elem_time()`](Self::anim_elem_time)`/ ticks`
    /// (`0.0` at the element's first tick, approaching `1.0` at its last); a
    /// hold-forever frame (`ticks <= 0`) uses the destination value directly. A
    /// frame without an interpolate flag (the common case) snaps to its own
    /// value, so a plain AIR renders byte-identically to before this feature.
    /// Returns [`FrameTransform::IDENTITY`] for an empty action.
    pub fn current_transform(&self) -> FrameTransform {
        let frames = &self.action.frames;
        let Some(cur) = frames.get(self.current_frame_index) else {
            return FrameTransform::IDENTITY;
        };
        // Previous element in the action (None on element 0 — nothing to blend
        // from). Interpolation only ever blends from the immediately preceding
        // element of the same action.
        let prev = self
            .current_frame_index
            .checked_sub(1)
            .and_then(|i| frames.get(i));
        let t = if cur.ticks > 0 {
            self.anim_elem_time() as f32 / cur.ticks as f32
        } else {
            // Hold-forever / zero-duration element: no meaningful progress, so
            // use the destination value directly.
            0.0
        };
        interpolated_transform(prev, cur, t)
    }

    /// Change to a different animation action, resetting to frame 0.
    pub fn set_action(&mut self, action: AnimAction) {
        let initial_ticks = action.frames.first().map_or(0, |f| f.ticks);
        self.action = action;
        self.current_frame_index = 0;
        self.frame_timer = initial_ticks;
        self.finished = false;
        self.total_time = 0;
        self.loop_count = 0;
    }

    /// Returns the total ticks elapsed since the animation started.
    pub fn time(&self) -> i32 {
        self.total_time
    }

    /// Returns the MUGEN `AnimTime` trigger value.
    ///
    /// If the animation is still playing, returns total time. If on an
    /// infinite frame, returns total time (always increasing).
    pub fn anim_time(&self) -> i32 {
        self.total_time
    }

    /// Returns the current animation element number (1-based).
    ///
    /// This is the MUGEN `AnimElem` trigger — element 1 is the first frame.
    pub fn anim_elem(&self) -> i32 {
        self.current_frame_index as i32 + 1
    }

    /// Returns ticks elapsed in the current frame element.
    ///
    /// This is the MUGEN `AnimElemTime` trigger value. It equals
    /// `frame.ticks - frame_timer` (how many ticks into this frame we are).
    pub fn anim_elem_time(&self) -> i32 {
        let frame_ticks = self.action.frames[self.current_frame_index].ticks;
        if frame_ticks == -1 {
            // Infinite frame — just return time since we entered it
            // (We don't track this separately, approximate from total_time)
            0
        } else {
            frame_ticks - self.frame_timer
        }
    }

    /// Returns `true` if the animation has reached a terminal state.
    ///
    /// This happens when the current frame has `ticks == -1` (infinite hold).
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Returns the number of times the animation has looped.
    pub fn loop_count(&self) -> u32 {
        self.loop_count
    }

    /// Force the controller to a specific frame index.
    ///
    /// Clamps to valid range. Resets the frame timer.
    pub fn set_frame(&mut self, index: usize) {
        if self.action.frames.is_empty() {
            return;
        }
        self.current_frame_index = index.min(self.action.frames.len() - 1);
        self.frame_timer = self.action.frames[self.current_frame_index].ticks;
    }

    /// Returns a reference to the current action.
    pub fn action(&self) -> &AnimAction {
        &self.action
    }

    /// Returns the action number of the current animation.
    pub fn action_number(&self) -> i32 {
        self.action.action_number
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fp_core::{SpriteId, Vec2};
    use fp_formats::air::{AnimFrame, BlendMode, Interpolate};

    fn make_frame(group: u16, image: u16, ticks: i32) -> AnimFrame {
        AnimFrame {
            sprite: SpriteId::new(group, image),
            offset: Vec2::new(0, 0),
            ticks,
            flip_h: false,
            flip_v: false,
            blend: BlendMode::Normal,
            clsn1: vec![],
            clsn2: vec![],
            ..Default::default()
        }
    }

    fn make_action(frames: Vec<AnimFrame>, loopstart: usize) -> AnimAction {
        AnimAction {
            action_number: 0,
            frames,
            loopstart,
        }
    }

    #[test]
    fn basic_frame_advancement() {
        let action = make_action(vec![make_frame(0, 0, 3), make_frame(0, 1, 3)], 0);
        let mut ctrl = AnimController::new(action);

        assert_eq!(ctrl.frame_index(), 0);
        assert_eq!(ctrl.anim_elem(), 1);

        // Tick 3 times — frame 0 has 3 ticks
        assert!(!ctrl.tick()); // tick 1: timer 3->2
        assert!(!ctrl.tick()); // tick 2: timer 2->1
        assert!(ctrl.tick()); // tick 3: timer 1->0, advance to frame 1

        assert_eq!(ctrl.frame_index(), 1);
        assert_eq!(ctrl.anim_elem(), 2);
    }

    #[test]
    fn looping() {
        let action = make_action(vec![make_frame(0, 0, 2), make_frame(0, 1, 2)], 0);
        let mut ctrl = AnimController::new(action);

        // Advance past both frames (4 ticks) to loop back
        ctrl.tick(); // t1
        ctrl.tick(); // t2 -> frame 1
        ctrl.tick(); // t3
        ctrl.tick(); // t4 -> loops to frame 0

        assert_eq!(ctrl.frame_index(), 0);
        assert_eq!(ctrl.loop_count(), 1);
    }

    #[test]
    fn loopstart_nonzero() {
        let action = make_action(
            vec![
                make_frame(0, 0, 2), // intro frame
                make_frame(0, 1, 2), // loopstart here
                make_frame(0, 2, 2),
            ],
            1, // loopstart at frame 1
        );
        let mut ctrl = AnimController::new(action);

        // Play through all 3 frames (6 ticks)
        for _ in 0..6 {
            ctrl.tick();
        }

        // Should loop back to frame 1, not frame 0
        assert_eq!(ctrl.frame_index(), 1);
        assert_eq!(ctrl.loop_count(), 1);
    }

    #[test]
    fn infinite_tick_stops() {
        let action = make_action(vec![make_frame(0, 0, -1)], 0);
        let mut ctrl = AnimController::new(action);

        // Tick many times — should never advance
        for _ in 0..100 {
            assert!(!ctrl.tick());
        }

        assert_eq!(ctrl.frame_index(), 0);
        assert!(ctrl.is_finished());
    }

    #[test]
    fn set_action_resets() {
        let action1 = make_action(vec![make_frame(0, 0, 2), make_frame(0, 1, 2)], 0);
        let action2 = make_action(vec![make_frame(5, 0, 3)], 0);

        let mut ctrl = AnimController::new(action1);
        ctrl.tick();
        ctrl.tick(); // advance to frame 1

        ctrl.set_action(action2);
        assert_eq!(ctrl.frame_index(), 0);
        assert_eq!(ctrl.time(), 0);
        assert_eq!(ctrl.action_number(), 0);
    }

    #[test]
    fn anim_elem_time() {
        let action = make_action(vec![make_frame(0, 0, 5)], 0);
        let mut ctrl = AnimController::new(action);

        assert_eq!(ctrl.anim_elem_time(), 0);
        ctrl.tick();
        assert_eq!(ctrl.anim_elem_time(), 1);
        ctrl.tick();
        assert_eq!(ctrl.anim_elem_time(), 2);
    }

    #[test]
    fn total_time_increments() {
        let action = make_action(vec![make_frame(0, 0, 2), make_frame(0, 1, 2)], 0);
        let mut ctrl = AnimController::new(action);

        assert_eq!(ctrl.time(), 0);
        ctrl.tick();
        assert_eq!(ctrl.time(), 1);
        ctrl.tick();
        assert_eq!(ctrl.time(), 2);
        ctrl.tick();
        assert_eq!(ctrl.time(), 3);
    }

    #[test]
    fn set_frame() {
        let action = make_action(
            vec![
                make_frame(0, 0, 5),
                make_frame(0, 1, 5),
                make_frame(0, 2, 5),
            ],
            0,
        );
        let mut ctrl = AnimController::new(action);

        ctrl.set_frame(2);
        assert_eq!(ctrl.frame_index(), 2);

        // Clamp to valid range
        ctrl.set_frame(100);
        assert_eq!(ctrl.frame_index(), 2); // clamped to last
    }

    // ---- T009: per-frame scale/angle + Interpolate at render time ----------

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    /// A frame that carries an explicit scale + angle, and may request scale /
    /// angle interpolation from its predecessor.
    fn xform_frame(
        ticks: i32,
        scale: (f32, f32),
        angle: f32,
        interp_scale: bool,
        interp_angle: bool,
    ) -> AnimFrame {
        AnimFrame {
            ticks,
            scale: Some(Vec2::new(scale.0, scale.1)),
            angle: Some(angle),
            interpolate: Interpolate {
                scale: interp_scale,
                angle: interp_angle,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn frame_scale_angle_produces_transform() {
        // A frame with scale/angle (and no interpolation) yields exactly that
        // transform — the resolved value the renderer maps onto SpriteDrawParams.
        let action = make_action(vec![xform_frame(4, (2.0, 0.5), 90.0, false, false)], 0);
        let ctrl = AnimController::new(action);
        let tf = ctrl.current_transform();
        assert!(approx(tf.scale.x, 2.0));
        assert!(approx(tf.scale.y, 0.5));
        assert!(approx(tf.angle_deg, 90.0));
        // angle_rad converts to radians for the renderer.
        assert!(approx(tf.angle_rad(), 90.0_f32.to_radians()));
    }

    #[test]
    fn frame_without_transform_is_identity() {
        // A plain frame (no scale/angle columns) resolves to the no-op transform,
        // keeping a vanilla AIR byte-identical to before this feature.
        let action = make_action(vec![make_frame(0, 0, 4)], 0);
        let ctrl = AnimController::new(action);
        assert_eq!(ctrl.current_transform(), FrameTransform::IDENTITY);
    }

    #[test]
    fn interpolate_blends_scale_and_angle_at_mid_keyframe() {
        // Two keyframes: frame 0 = (scale 1.0, angle 0), frame 1 requests
        // Interpolate Scale + Angle to (scale 2.0, angle 90) over 4 ticks. At a
        // tick midway through element 1, the transform is the linear blend.
        let action = make_action(
            vec![
                xform_frame(4, (1.0, 1.0), 0.0, false, false),
                xform_frame(4, (2.0, 4.0), 90.0, true, true),
            ],
            0,
        );
        let mut ctrl = AnimController::new(action);

        // Advance into element 1: 4 ticks finishes element 0 and lands on the
        // first tick of element 1 (anim_elem_time 0). Then tick twice more so
        // anim_elem_time == 2, i.e. t = 2/4 = 0.5 — the midpoint between the two
        // keyframes.
        for _ in 0..4 {
            ctrl.tick();
        }
        assert_eq!(ctrl.frame_index(), 1, "should be on the second element");
        ctrl.tick();
        ctrl.tick();
        assert_eq!(
            ctrl.anim_elem_time(),
            2,
            "halfway through the 4-tick element"
        );

        let tf = ctrl.current_transform();
        // scale.x: lerp(1.0, 2.0, 0.5) = 1.5; scale.y: lerp(1.0, 4.0, 0.5) = 2.5
        assert!(approx(tf.scale.x, 1.5), "scale.x = {}", tf.scale.x);
        assert!(approx(tf.scale.y, 2.5), "scale.y = {}", tf.scale.y);
        // angle: lerp(0, 90, 0.5) = 45
        assert!(approx(tf.angle_deg, 45.0), "angle = {}", tf.angle_deg);
    }

    #[test]
    fn no_interpolate_flag_snaps_to_destination() {
        // Same two keyframes but WITHOUT the Interpolate flags: the transform
        // snaps to element 1's own value with no blend, regardless of how far
        // into the element we are.
        let action = make_action(
            vec![
                xform_frame(4, (1.0, 1.0), 0.0, false, false),
                xform_frame(4, (2.0, 4.0), 90.0, false, false),
            ],
            0,
        );
        let mut ctrl = AnimController::new(action);
        for _ in 0..6 {
            ctrl.tick();
        }
        let tf = ctrl.current_transform();
        assert!(approx(tf.scale.x, 2.0));
        assert!(approx(tf.scale.y, 4.0));
        assert!(approx(tf.angle_deg, 90.0));
    }

    #[test]
    fn interpolated_transform_helper_endpoints_and_clamp() {
        let prev = xform_frame(4, (1.0, 1.0), 10.0, false, false);
        let cur = xform_frame(4, (3.0, 3.0), 50.0, true, true);

        // t = 0 → previous values (start of the blend).
        let at0 = interpolated_transform(Some(&prev), &cur, 0.0);
        assert!(approx(at0.scale.x, 1.0));
        assert!(approx(at0.angle_deg, 10.0));

        // t = 1 → destination values (end of the blend).
        let at1 = interpolated_transform(Some(&prev), &cur, 1.0);
        assert!(approx(at1.scale.x, 3.0));
        assert!(approx(at1.angle_deg, 50.0));

        // t out of range is clamped to [0, 1].
        let over = interpolated_transform(Some(&prev), &cur, 2.5);
        assert!(approx(over.scale.x, 3.0));
        let under = interpolated_transform(Some(&prev), &cur, -1.0);
        assert!(approx(under.scale.x, 1.0));

        // No previous frame → destination value directly (nothing to blend).
        let no_prev = interpolated_transform(None, &cur, 0.5);
        assert!(approx(no_prev.scale.x, 3.0));
        assert!(approx(no_prev.angle_deg, 50.0));
    }

    #[test]
    fn interpolate_scale_only_leaves_angle_snapped() {
        // Only Interpolate Scale is set: scale blends, angle snaps to cur.
        let prev = xform_frame(4, (1.0, 1.0), 0.0, false, false);
        let cur = xform_frame(4, (5.0, 5.0), 90.0, true, false);
        let tf = interpolated_transform(Some(&prev), &cur, 0.5);
        // scale midway: lerp(1, 5, 0.5) = 3
        assert!(approx(tf.scale.x, 3.0));
        assert!(approx(tf.scale.y, 3.0));
        // angle NOT interpolated → snaps to cur's 90.
        assert!(approx(tf.angle_deg, 90.0));
    }
}
