//! AfterImage trail compositing helpers (T007).
//!
//! MUGEN's `AfterImage` draws a fading trail of a character's recent frames
//! behind the live sprite. The per-frame *geometry* (which sprite, where, which
//! way it faced) is captured in `fp-character`'s frame-history ring; this module
//! owns the *presentation* side the renderer needs: how each trailing ghost is
//! tinted and composited.
//!
//! The math is renderer-agnostic and GPU-free so it can be unit-tested on the CPU
//! (the windowed app feeds the resulting [`PalFx`] / [`BlendMode`] / alpha into a
//! `SpriteDrawParams` per ghost). It deliberately does **not** depend on
//! `fp-character`: callers translate that crate's `TrailBlend` / `AfterImageState`
//! into the small value types here.
//!
//! ## Per-ghost progressive modulation
//!
//! MUGEN applies `PalBright` and `PalContrast` *cumulatively* down the trail: the
//! newest ghost gets the base tint, and each older ghost gets one more application
//! of the brightness add and the contrast multiply. [`ghost_palfx`] reproduces
//! that — ghost `n` (0 = newest) receives `base.add + n × palbright` on the add
//! channel and `base.mul × palcontrast^n` on the multiply channel.

use crate::params::{BlendMode, PalFx};

/// How an `AfterImage` trail is composited over the background (MUGEN
/// `AfterImage trans`).
///
/// A renderer-side mirror of `fp_character::TrailBlend` (this crate does not
/// depend on `fp-character`); callers map one onto the other. Each variant
/// resolves to a [`BlendMode`] plus a base opacity via [`Self::blend_mode`] /
/// [`Self::base_alpha`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrailTrans {
    /// `trans = none` — ordinary alpha blending (the MUGEN `AfterImage` default).
    #[default]
    None,
    /// `trans = add` (or `addalpha`) — additive blending.
    Add,
    /// `trans = add1` — half-strength additive blending.
    Add1,
    /// `trans = sub` — subtractive blending.
    Sub,
}

impl TrailTrans {
    /// The [`BlendMode`] the renderer should composite the trail with.
    #[must_use]
    pub fn blend_mode(self) -> BlendMode {
        match self {
            TrailTrans::None => BlendMode::Normal,
            TrailTrans::Add | TrailTrans::Add1 => BlendMode::Additive,
            TrailTrans::Sub => BlendMode::Subtractive,
        }
    }

    /// The base opacity multiplier for the newest ghost. `Add1` (half-additive)
    /// halves the contribution; the others draw at full strength before the
    /// trailing fade in [`ghost_alpha`] is applied.
    #[must_use]
    pub fn base_alpha(self) -> f32 {
        match self {
            TrailTrans::Add1 => 0.5,
            _ => 1.0,
        }
    }
}

/// The per-ghost color modulation of an `AfterImage` trail (T007).
///
/// Bundles the base ghost tint ([`base`](Self::base) — the controller's
/// `PalAdd`/`PalMul`) with the cumulative ramps applied one step per ghost:
/// [`palbright`](Self::palbright) (signed add ramp, `±1.0` = ±255) and
/// [`palcontrast`](Self::palcontrast) (multiply ramp, `1.0` = ×1). Feed it to
/// [`ghost_palfx`] with a ghost index to get that ghost's [`PalFx`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AfterImageModulation {
    /// The base tint applied to the newest (index `0`) ghost.
    pub base: PalFx,
    /// Per-step signed brightness add (MUGEN `PalBright`, normalized to `±1.0`).
    /// Ghost `n` gets `base.add[c] + n × palbright[c]` on channel `c`.
    pub palbright: [f32; 3],
    /// Per-step contrast multiply (MUGEN `PalContrast`, normalized so `255` →
    /// `1.0`). Ghost `n` gets `base.mul[c] × palcontrast[c]^n` on channel `c`.
    pub palcontrast: [f32; 3],
}

impl AfterImageModulation {
    /// The identity modulation: identity base tint, no per-ghost ramp (every
    /// ghost renders with the base color unchanged).
    pub const IDENTITY: Self = Self {
        base: PalFx::IDENTITY,
        palbright: [0.0; 3],
        palcontrast: [1.0; 3],
    };
}

impl Default for AfterImageModulation {
    fn default() -> Self {
        Self::IDENTITY
    }
}

/// The [`PalFx`] tint for the `ghost_index`-th trail ghost (`0` = newest), applying
/// `palbright`/`palcontrast` cumulatively as MUGEN does (T007).
///
/// Starting from the base tint, the add channel accumulates `ghost_index ×
/// palbright` and the multiply channel accumulates `palcontrast^ghost_index`, so
/// each older ghost is progressively dimmer / lower-contrast. The grayscale
/// [`color`](PalFx::color) factor is carried through from the base unchanged. The
/// returned multiply channel is clamped non-negative (a negative multiplier is
/// meaningless); the shader clamps the final color to `0.0..=1.0`.
#[must_use]
pub fn ghost_palfx(modulation: &AfterImageModulation, ghost_index: usize) -> PalFx {
    let n = ghost_index as f32;
    let mut add = [0.0f32; 3];
    let mut mul = [0.0f32; 3];
    for c in 0..3 {
        add[c] = modulation.base.add[c] + n * modulation.palbright[c];
        mul[c] = (modulation.base.mul[c] * modulation.palcontrast[c].powf(n)).max(0.0);
    }
    PalFx {
        add,
        mul,
        color: modulation.base.color,
        invertall: modulation.base.invertall,
    }
}

/// The opacity for the `ghost_index`-th trail ghost (`0` = newest) out of `count`
/// total ghosts, composited with `trans` (T007).
///
/// The newest ghost is the most opaque and the trail fades linearly to nearly
/// transparent at the tail, scaled by the blend mode's [`base_alpha`](TrailTrans::base_alpha)
/// (half for `add1`). Returns `0.0` for an out-of-range index or empty trail.
#[must_use]
pub fn ghost_alpha(ghost_index: usize, count: usize, trans: TrailTrans) -> f32 {
    if count == 0 || ghost_index >= count {
        return 0.0;
    }
    // Linear fade: newest ghost ≈ full, tail ≈ 1/(count+1). Index 0 is newest.
    let t = (count - ghost_index) as f32 / (count as f32 + 1.0);
    trans.base_alpha() * t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    #[test]
    fn default_modulation_is_identity_for_every_ghost() {
        let m = AfterImageModulation::IDENTITY;
        for i in 0..5 {
            assert!(ghost_palfx(&m, i).is_identity(), "ghost {i} is a no-op");
        }
    }

    #[test]
    fn palbright_adds_cumulatively_down_the_trail() {
        let m = AfterImageModulation {
            base: PalFx::IDENTITY,
            palbright: [0.1, -0.2, 0.0],
            palcontrast: [1.0; 3],
        };
        // Ghost 0 = base (no ramp yet).
        let g0 = ghost_palfx(&m, 0);
        assert!(approx(g0.add[0], 0.0) && approx(g0.add[1], 0.0));
        // Ghost 3 = base + 3 × palbright.
        let g3 = ghost_palfx(&m, 3);
        assert!(approx(g3.add[0], 0.3), "0 + 3×0.1");
        assert!(approx(g3.add[1], -0.6), "0 + 3×-0.2");
        assert!(approx(g3.add[2], 0.0), "no ramp on B");
    }

    #[test]
    fn palcontrast_multiplies_cumulatively_down_the_trail() {
        let m = AfterImageModulation {
            base: PalFx::IDENTITY,
            palbright: [0.0; 3],
            palcontrast: [0.5, 1.0, 0.8],
        };
        // Ghost 0 = base.mul (contrast^0 = 1).
        let g0 = ghost_palfx(&m, 0);
        assert!(approx(g0.mul[0], 1.0));
        // Ghost 2 = base.mul × contrast^2.
        let g2 = ghost_palfx(&m, 2);
        assert!(approx(g2.mul[0], 0.25), "1 × 0.5^2");
        assert!(approx(g2.mul[1], 1.0), "1 × 1^2");
        assert!(approx(g2.mul[2], 0.64), "1 × 0.8^2");
    }

    #[test]
    fn base_tint_carries_into_ghosts() {
        let base = PalFx {
            add: [0.2, 0.0, 0.0],
            mul: [0.9, 0.9, 0.9],
            color: 0.5,
            invertall: false,
        };
        let m = AfterImageModulation {
            base,
            palbright: [0.0; 3],
            palcontrast: [1.0; 3],
        };
        let g0 = ghost_palfx(&m, 0);
        assert!(approx(g0.add[0], 0.2), "base add carried");
        assert!(approx(g0.mul[0], 0.9), "base mul carried");
        assert!(approx(g0.color, 0.5), "base color carried");
    }

    #[test]
    fn negative_multiply_clamps_to_zero() {
        // A pathological contrast that would drive the multiply negative is clamped.
        let m = AfterImageModulation {
            base: PalFx {
                mul: [-1.0; 3],
                ..PalFx::IDENTITY
            },
            palbright: [0.0; 3],
            palcontrast: [1.0; 3],
        };
        let g = ghost_palfx(&m, 0);
        assert!(approx(g.mul[0], 0.0), "negative multiply clamped to 0");
    }

    #[test]
    fn trans_maps_to_blend_mode_and_alpha() {
        assert_eq!(TrailTrans::None.blend_mode(), BlendMode::Normal);
        assert_eq!(TrailTrans::Add.blend_mode(), BlendMode::Additive);
        assert_eq!(TrailTrans::Add1.blend_mode(), BlendMode::Additive);
        assert_eq!(TrailTrans::Sub.blend_mode(), BlendMode::Subtractive);
        assert!(approx(TrailTrans::Add.base_alpha(), 1.0));
        assert!(
            approx(TrailTrans::Add1.base_alpha(), 0.5),
            "add1 is half-strength"
        );
    }

    #[test]
    fn ghost_alpha_fades_newest_to_oldest() {
        let count = 4;
        let a0 = ghost_alpha(0, count, TrailTrans::None);
        let a3 = ghost_alpha(3, count, TrailTrans::None);
        assert!(a0 > a3, "newest ghost is more opaque than the tail");
        assert!(a0 > 0.0 && a3 > 0.0, "all ghosts are visible");
        // Out-of-range / empty: 0.
        assert!(approx(ghost_alpha(4, count, TrailTrans::None), 0.0));
        assert!(approx(ghost_alpha(0, 0, TrailTrans::None), 0.0));
    }

    #[test]
    fn add1_halves_the_ghost_alpha() {
        let normal = ghost_alpha(1, 4, TrailTrans::Add);
        let half = ghost_alpha(1, 4, TrailTrans::Add1);
        assert!(
            approx(half, normal * 0.5),
            "add1 halves the per-ghost alpha"
        );
    }
}
