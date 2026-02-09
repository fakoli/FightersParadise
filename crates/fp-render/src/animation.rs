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

use fp_formats::air::{AnimAction, AnimFrame};

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
    use fp_formats::air::{AnimFrame, BlendMode};

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
        let action = make_action(
            vec![make_frame(0, 0, 3), make_frame(0, 1, 3)],
            0,
        );
        let mut ctrl = AnimController::new(action);

        assert_eq!(ctrl.frame_index(), 0);
        assert_eq!(ctrl.anim_elem(), 1);

        // Tick 3 times — frame 0 has 3 ticks
        assert!(!ctrl.tick()); // tick 1: timer 3->2
        assert!(!ctrl.tick()); // tick 2: timer 2->1
        assert!(ctrl.tick());  // tick 3: timer 1->0, advance to frame 1

        assert_eq!(ctrl.frame_index(), 1);
        assert_eq!(ctrl.anim_elem(), 2);
    }

    #[test]
    fn looping() {
        let action = make_action(
            vec![make_frame(0, 0, 2), make_frame(0, 1, 2)],
            0,
        );
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
        let action = make_action(
            vec![make_frame(0, 0, -1)],
            0,
        );
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
        let action1 = make_action(
            vec![make_frame(0, 0, 2), make_frame(0, 1, 2)],
            0,
        );
        let action2 = make_action(
            vec![make_frame(5, 0, 3)],
            0,
        );

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
        let action = make_action(
            vec![make_frame(0, 0, 5)],
            0,
        );
        let mut ctrl = AnimController::new(action);

        assert_eq!(ctrl.anim_elem_time(), 0);
        ctrl.tick();
        assert_eq!(ctrl.anim_elem_time(), 1);
        ctrl.tick();
        assert_eq!(ctrl.anim_elem_time(), 2);
    }

    #[test]
    fn total_time_increments() {
        let action = make_action(
            vec![make_frame(0, 0, 2), make_frame(0, 1, 2)],
            0,
        );
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
            vec![make_frame(0, 0, 5), make_frame(0, 1, 5), make_frame(0, 2, 5)],
            0,
        );
        let mut ctrl = AnimController::new(action);

        ctrl.set_frame(2);
        assert_eq!(ctrl.frame_index(), 2);

        // Clamp to valid range
        ctrl.set_frame(100);
        assert_eq!(ctrl.frame_index(), 2); // clamped to last
    }
}
