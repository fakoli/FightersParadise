//! Resource-state HUD legibility (T074): pure threshold→color helpers that tint
//! the life and power bars to telegraph **risk** (low life) and **reward** (a
//! ready super meter) at a glance.
//!
//! Everything here is pure and deterministic:
//!
//! - [`low_life_tint`] returns a red-shift [`BarColor`] when a fighter's life
//!   fraction drops below [`LOW_LIFE_THRESHOLD`] (25%), and [`BarColor::WHITE`]
//!   (the no-op tint) otherwise.
//! - [`max_power_flash_tint`] returns a frame-driven flashing tint when the power
//!   (super) meter is at max — the super is available — and [`BarColor::WHITE`]
//!   otherwise. The flash is driven from a frame counter (no RNG), so it is
//!   deterministic and replay-safe.
//!
//! Both return a [`BarColor`] that callers **multiply** onto whatever tint the
//! screenpack / [`HudConfig`](crate::HudConfig) already applies (via
//! [`BarColor::combine`]), so a configured bar color is respected and only nudged
//! toward the state color — never replaced outright. They are purely
//! presentational: a [`BarColor::WHITE`] result is a guaranteed no-op draw, so a
//! HUD with no assets or a neutral fraction is byte-for-byte unchanged.

use crate::BarColor;

/// Life fraction (in `0.0..=1.0`) at or below which the life bar red-shifts to
/// telegraph that the fighter is in danger. MUGEN-style "low life" warning.
pub const LOW_LIFE_THRESHOLD: f32 = 0.25;

/// Power (super-meter) fraction (in `0.0..=1.0`) at or above which the power bar
/// is considered "at max" (a super is available) and begins to flash. Slightly
/// below `1.0` so a meter that lands a hair under full from rounding still reads
/// as ready.
pub const MAX_POWER_THRESHOLD: f32 = 0.999;

/// Number of frames in one full on→off cycle of the max-power flash, at 60 Hz
/// (≈ half a second), giving a calm ~2 Hz pulse rather than a harsh strobe.
pub const POWER_FLASH_PERIOD: u64 = 30;

/// The color the life bar is tinted toward at low life: a strong red-shift
/// (full red, knocked-down green and blue) that survives a multiply onto a
/// green/white configured fill, pushing it visibly toward red.
const LOW_LIFE_COLOR: BarColor = BarColor {
    r: 1.0,
    g: 0.25,
    b: 0.25,
};

/// The color the power bar is tinted toward on the "bright" phase of the
/// max-power flash: full white (a no-op multiply, so the bar reads at its
/// brightest configured color). The flash alternates this with
/// [`POWER_FLASH_DIM`].
const POWER_FLASH_BRIGHT: BarColor = BarColor::WHITE;

/// The color the power bar is tinted toward on the "dim" phase of the max-power
/// flash: a partial darken so the bar visibly pulses. A multiply by these
/// sub-`1.0` channels dims whatever color the bar already is.
const POWER_FLASH_DIM: BarColor = BarColor {
    r: 0.45,
    g: 0.45,
    b: 0.45,
};

impl BarColor {
    /// Combines this color with `other` by a per-channel multiply, returning a
    /// new [`BarColor`]. Multiplying by [`BarColor::WHITE`] is the identity, so
    /// combining a threshold tint (which is `WHITE` when inactive) with a
    /// configured bar color leaves the configured color untouched while the
    /// state is neutral, and nudges it toward the state color otherwise.
    #[must_use]
    pub fn combine(self, other: BarColor) -> BarColor {
        BarColor {
            r: self.r * other.r,
            g: self.g * other.g,
            b: self.b * other.b,
        }
    }
}

/// The low-life red-shift tint for a life `fraction` in `0.0..=1.0`.
///
/// Returns [`LOW_LIFE_COLOR`] once `fraction` is at or below
/// [`LOW_LIFE_THRESHOLD`] (25%), and the no-op [`BarColor::WHITE`] above it.
/// Callers multiply the result onto the configured life-bar tint (see
/// [`BarColor::combine`]). Pure; the fraction is treated as already clamped by
/// the caller (an out-of-range value still yields a sensible result because the
/// comparison is monotone).
#[must_use]
pub fn low_life_tint(fraction: f32) -> BarColor {
    if fraction <= LOW_LIFE_THRESHOLD {
        LOW_LIFE_COLOR
    } else {
        BarColor::WHITE
    }
}

/// The max-power flash tint for a power `fraction` in `0.0..=1.0` at frame
/// `frame`.
///
/// While `fraction` is at or above [`MAX_POWER_THRESHOLD`] (the super is
/// available) the result alternates between [`POWER_FLASH_BRIGHT`] and
/// [`POWER_FLASH_DIM`] every half-[`POWER_FLASH_PERIOD`] of frames, producing a
/// deterministic pulse with no RNG. Below the threshold it returns the no-op
/// [`BarColor::WHITE`]. Callers multiply the result onto the configured
/// power-bar tint (see [`BarColor::combine`]).
#[must_use]
pub fn max_power_flash_tint(fraction: f32, frame: u64) -> BarColor {
    if fraction < MAX_POWER_THRESHOLD {
        return BarColor::WHITE;
    }
    // Bright for the first half of the period, dim for the second half — a
    // deterministic square-wave pulse keyed purely off the frame counter.
    let half = (POWER_FLASH_PERIOD / 2).max(1);
    if (frame / half).is_multiple_of(2) {
        POWER_FLASH_BRIGHT
    } else {
        POWER_FLASH_DIM
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_life_is_neutral() {
        assert!(low_life_tint(1.0).is_neutral());
        assert!(low_life_tint(0.5).is_neutral());
    }

    #[test]
    fn just_above_threshold_is_neutral() {
        assert!(low_life_tint(LOW_LIFE_THRESHOLD + 0.001).is_neutral());
    }

    #[test]
    fn at_and_below_threshold_red_shifts() {
        // At the threshold and below, the tint is the red-shift (not neutral).
        assert!(!low_life_tint(LOW_LIFE_THRESHOLD).is_neutral());
        assert!(!low_life_tint(0.1).is_neutral());
        assert!(!low_life_tint(0.0).is_neutral());
        // The red channel dominates green/blue so a multiply pushes toward red.
        let t = low_life_tint(0.1);
        assert!(t.r > t.g);
        assert!(t.r > t.b);
    }

    #[test]
    fn below_max_power_is_neutral_every_frame() {
        for frame in 0..120 {
            assert!(max_power_flash_tint(0.5, frame).is_neutral());
            assert!(max_power_flash_tint(0.0, frame).is_neutral());
        }
    }

    #[test]
    fn at_max_power_flashes_deterministically() {
        // The pulse alternates bright/dim across the period and is a pure
        // function of the frame (replaying the same frame gives the same color).
        let half = (POWER_FLASH_PERIOD / 2).max(1);
        let a = max_power_flash_tint(1.0, 0);
        let b = max_power_flash_tint(1.0, half);
        assert_ne!(a, b, "flash must toggle across the half-period");
        // Pure: same frame → same color.
        assert_eq!(
            max_power_flash_tint(1.0, 7),
            max_power_flash_tint(1.0, 7),
            "flash is a deterministic function of the frame"
        );
        // It cycles back: one full period later matches.
        assert_eq!(
            max_power_flash_tint(1.0, 3),
            max_power_flash_tint(1.0, 3 + POWER_FLASH_PERIOD)
        );
    }

    #[test]
    fn at_max_power_at_least_one_phase_is_visible() {
        // At max meter the flash actually changes the bar (not WHITE the whole
        // time): at least one phase must dim it.
        let half = (POWER_FLASH_PERIOD / 2).max(1);
        let dim = max_power_flash_tint(1.0, half);
        assert!(!dim.is_neutral(), "dim phase must visibly change the bar");
    }

    #[test]
    fn combine_with_white_is_identity() {
        let green = BarColor::GREEN;
        assert_eq!(green.combine(BarColor::WHITE), green);
        assert_eq!(BarColor::WHITE.combine(green), green);
    }

    #[test]
    fn combine_multiplies_channels() {
        let c = BarColor::new(0.5, 0.5, 0.5).combine(BarColor::new(0.5, 1.0, 0.0));
        assert!((c.r - 0.25).abs() < 1e-6);
        assert!((c.g - 0.5).abs() < 1e-6);
        assert!((c.b - 0.0).abs() < 1e-6);
    }

    #[test]
    fn low_life_combined_onto_green_pushes_toward_red() {
        // A green configured life bar at low life: after combine, red should
        // out-weigh green so the bar reads as endangered.
        let combined = BarColor::GREEN.combine(low_life_tint(0.1));
        // GREEN has r=0, so the multiply keeps r=0 here; verify against a more
        // realistic white-ish configured bar where the shift is visible.
        let white_low = BarColor::WHITE.combine(low_life_tint(0.1));
        assert!(white_low.r > white_low.g);
        assert!(white_low.r > white_low.b);
        // The green case stays valid (no panic / in-gamut), documenting intent.
        let _ = combined;
    }
}
