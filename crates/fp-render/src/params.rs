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

/// A MUGEN-style per-draw color tint (the `PalFX` / `AfterImage` color effect,
/// audit #33).
///
/// Applied to the palette-looked-up RGBA of every pixel, in MUGEN's order:
/// first a grayscale blend controlled by [`color`](Self::color) (`256` = full
/// color, `0` = fully grayscale), then a per-channel multiply
/// ([`mul`](Self::mul)), then a per-channel signed add ([`add`](Self::add)); the
/// result is clamped back into `0.0..=1.0`. The fragment shader
/// (`shaders/palette.wgsl`) does the per-pixel math; this struct is the CPU-side
/// description handed to the renderer.
///
/// The values mirror MUGEN's 0–255 integer convention but are pre-normalized to
/// the shader's `0.0..` float scale by the caller: `add` is a signed fraction
/// (`±1.0` = ±255), `mul` is a plain multiplier (`1.0` = unchanged), and `color`
/// is a `0.0..=1.0` color-retention fraction (`1.0` = full color). The
/// [`IDENTITY`](Self::IDENTITY) effect (also [`Default`]) is a guaranteed no-op:
/// a sprite drawn with it is byte-for-byte identical to one drawn before this
/// feature existed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PalFx {
    /// Signed per-channel add applied last, as a fraction of full scale
    /// (`±1.0` = ±255 in MUGEN units). `[0.0; 3]` adds nothing.
    pub add: [f32; 3],
    /// Per-channel multiply applied after the grayscale blend (`1.0` =
    /// unchanged). `[1.0; 3]` leaves the color as-is.
    pub mul: [f32; 3],
    /// Color-retention fraction in `0.0..=1.0`: `1.0` keeps full color, `0.0`
    /// is fully grayscale (luminance), values between blend the two. Mirrors
    /// MUGEN's `PalFX color = 0..256`.
    pub color: f32,
}

impl PalFx {
    /// The identity (no-op) effect: full color, unit multiply, zero add. A
    /// sprite drawn with this is pixel-identical to one drawn with no effect.
    pub const IDENTITY: Self = Self {
        add: [0.0, 0.0, 0.0],
        mul: [1.0, 1.0, 1.0],
        color: 1.0,
    };

    /// Returns `true` when this effect is the identity (no-op) — every channel
    /// of [`add`](Self::add) is `0`, [`mul`](Self::mul) is `1`, and
    /// [`color`](Self::color) is `1`. The renderer uses this to skip uploading a
    /// tint uniform for the common case, keeping no-op draws on the original
    /// path.
    #[must_use]
    pub fn is_identity(&self) -> bool {
        *self == Self::IDENTITY
    }
}

impl Default for PalFx {
    fn default() -> Self {
        Self::IDENTITY
    }
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
    /// MUGEN-style color tint (the `PalFX` / `AfterImage` color effect, audit
    /// #33). Defaults to [`PalFx::IDENTITY`], the no-op effect, so an existing
    /// `..Default::default()` construction renders byte-identically to before
    /// this field existed.
    pub palfx: PalFx,
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
            palfx: PalFx::IDENTITY,
        }
    }
}

/// Rec. 601 luma weights, used for the [`PalFx::color`] grayscale blend. Matches
/// the constants the fragment shader (`shaders/palette.wgsl`) uses, so the CPU
/// reference and the GPU stay in lockstep.
const LUMA_WEIGHTS: [f32; 3] = [0.299, 0.587, 0.114];

/// Applies a [`PalFx`] tint to a single linear RGB triple, mirroring the math the
/// fragment shader performs per pixel: grayscale blend (`color`) → multiply
/// (`mul`) → signed add (`add`), each channel clamped back to `0.0..=1.0`.
///
/// This is the CPU reference used to unit-test the tint without a GPU; the WGSL
/// `apply_palfx` in `shaders/palette.wgsl` is the exact same sequence. An
/// [identity](PalFx::IDENTITY) effect returns `rgb` unchanged (every step is a
/// no-op), which is what guarantees a no-op draw is byte-identical.
#[must_use]
pub fn apply_palfx(rgb: [f32; 3], fx: &PalFx) -> [f32; 3] {
    let luma = rgb[0] * LUMA_WEIGHTS[0] + rgb[1] * LUMA_WEIGHTS[1] + rgb[2] * LUMA_WEIGHTS[2];
    let mut out = [0.0f32; 3];
    for i in 0..3 {
        // Grayscale blend: lerp(luma, channel, color). color = 1 keeps full
        // color; color = 0 collapses to luminance.
        let blended = luma + (rgb[i] - luma) * fx.color;
        // Multiply then signed add, then clamp.
        out[i] = (blended * fx.mul[i] + fx.add[i]).clamp(0.0, 1.0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: [f32; 3], b: [f32; 3]) -> bool {
        (0..3).all(|i| (a[i] - b[i]).abs() < 1e-5)
    }

    #[test]
    fn default_palfx_is_identity() {
        assert_eq!(PalFx::default(), PalFx::IDENTITY);
        assert!(PalFx::default().is_identity());
    }

    #[test]
    fn default_sprite_params_carry_identity_palfx() {
        assert!(SpriteDrawParams::default().palfx.is_identity());
    }

    #[test]
    fn identity_palfx_leaves_color_unchanged() {
        let c = [0.2, 0.5, 0.9];
        assert!(approx(apply_palfx(c, &PalFx::IDENTITY), c));
        // A handful of arbitrary colors must all round-trip exactly.
        for c in [[0.0, 0.0, 0.0], [1.0, 1.0, 1.0], [0.13, 0.71, 0.42]] {
            assert!(approx(apply_palfx(c, &PalFx::IDENTITY), c));
        }
    }

    #[test]
    fn add_offsets_each_channel_and_clamps() {
        let fx = PalFx {
            add: [0.5, -0.5, 0.0],
            ..PalFx::IDENTITY
        };
        let out = apply_palfx([0.6, 0.6, 0.6], &fx);
        assert!((out[0] - 1.0).abs() < 1e-5, "0.6+0.5 clamps to 1.0");
        assert!((out[1] - 0.1).abs() < 1e-5, "0.6-0.5 = 0.1");
        assert!((out[2] - 0.6).abs() < 1e-5, "no add leaves 0.6");
    }

    #[test]
    fn mul_scales_each_channel_and_clamps_low() {
        let fx = PalFx {
            mul: [0.5, 2.0, 0.0],
            ..PalFx::IDENTITY
        };
        let out = apply_palfx([0.4, 0.4, 0.4], &fx);
        assert!((out[0] - 0.2).abs() < 1e-5);
        assert!((out[1] - 0.8).abs() < 1e-5);
        assert!((out[2] - 0.0).abs() < 1e-5);
    }

    #[test]
    fn color_zero_produces_grayscale_luma() {
        let fx = PalFx {
            color: 0.0,
            ..PalFx::IDENTITY
        };
        let c = [0.2, 0.5, 0.9];
        let luma = c[0] * LUMA_WEIGHTS[0] + c[1] * LUMA_WEIGHTS[1] + c[2] * LUMA_WEIGHTS[2];
        let out = apply_palfx(c, &fx);
        // All three channels collapse to the same luminance value.
        assert!(approx(out, [luma, luma, luma]));
    }

    #[test]
    fn color_one_keeps_full_color() {
        let fx = PalFx {
            color: 1.0,
            ..PalFx::IDENTITY
        };
        let c = [0.2, 0.5, 0.9];
        assert!(approx(apply_palfx(c, &fx), c));
    }

    #[test]
    fn order_is_color_then_mul_then_add() {
        // color=0 → luma, then *2, then +0.1. Verify the documented sequence.
        let fx = PalFx {
            add: [0.1, 0.1, 0.1],
            mul: [2.0, 2.0, 2.0],
            color: 0.0,
        };
        let c = [0.2, 0.2, 0.2];
        let luma = c[0] * LUMA_WEIGHTS[0] + c[1] * LUMA_WEIGHTS[1] + c[2] * LUMA_WEIGHTS[2];
        let expected = (luma * 2.0 + 0.1).clamp(0.0, 1.0);
        let out = apply_palfx(c, &fx);
        assert!(approx(out, [expected, expected, expected]));
    }
}
