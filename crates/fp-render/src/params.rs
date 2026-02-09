/// Sprite blending mode for rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlendMode {
    /// Standard alpha blending.
    #[default]
    Normal,
    /// Additive blending (sprite colors added to background).
    Additive,
    /// Subtractive blending (sprite colors subtracted from background).
    Subtractive,
}

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
    /// Blending mode for this sprite.
    pub blend: BlendMode,
    /// Rotation angle in radians (clockwise, around sprite center).
    pub angle: f32,
    /// Opacity multiplier (0.0 = fully transparent, 1.0 = fully opaque).
    pub alpha: f32,
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
            blend: BlendMode::Normal,
            angle: 0.0,
            alpha: 1.0,
        }
    }
}
