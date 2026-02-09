/// Parameters controlling how a sprite is drawn on screen.
pub struct SpriteDrawParams {
    /// Horizontal position in screen pixels.
    pub x: f32,
    /// Vertical position in screen pixels.
    pub y: f32,
    /// Mirror the sprite horizontally.
    pub flip_h: bool,
    /// Mirror the sprite vertically.
    pub flip_v: bool,
    /// Horizontal scale factor (1.0 = original size).
    pub scale_x: f32,
    /// Vertical scale factor (1.0 = original size).
    pub scale_y: f32,
}

impl Default for SpriteDrawParams {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            flip_h: false,
            flip_v: false,
            scale_x: 1.0,
            scale_y: 1.0,
        }
    }
}
