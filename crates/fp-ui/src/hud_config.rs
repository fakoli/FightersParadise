//! User-facing HUD customization overrides (T046).
//!
//! A [`ScreenpackLayout`](crate::screenpack::ScreenpackLayout) describes where a
//! `fight.def` *authored* the HUD. [`HudConfig`] sits **on top** of that: a small,
//! optional set of overrides a player can set (from a config file or the in-game
//! HUD-customization screen) to retint the life/power bars, nudge or scale a HUD
//! element, or hide individual elements (life, power, name, timer, combo).
//!
//! The model is deliberately *additive* and centred on a guaranteed no-op
//! [default](HudConfig::default): every field is "no override", so a
//! [`ScreenpackHud`](crate::renderer::ScreenpackHud) drawing with
//! [`HudConfig::default`] issues exactly the same draw calls it did before this
//! feature existed (regression-guarded by a renderer test). The renderer reads
//! these overrides each frame; nothing here touches the GPU and nothing panics.
//!
//! Overrides are expressed as deltas/multipliers/toggles rather than absolute
//! replacements so they compose cleanly with whatever the screenpack authored:
//! - a [bar color](BarColor) retints a bar's front-fill via a per-channel
//!   multiply (a `PalFX`-style tint), so `WHITE` is a no-op and any other color
//!   tints toward it;
//! - a position override is an `(dx, dy)` pixel delta added to an element's
//!   anchor;
//! - a scale override multiplies an element's size (`1.0` = unchanged);
//! - a visibility toggle hides an element entirely when `false`.

use fp_render::PalFx;

/// A normalized RGB color in `0.0..=1.0`, used to retint a HUD bar.
///
/// Stored as three linear channels. [`WHITE`](BarColor::WHITE) (also
/// [`Default`]) is the neutral color: tinting a bar to white multiplies every
/// channel by `1.0`, i.e. leaves it exactly as the screenpack drew it. Any other
/// color tints the bar's front-fill toward that color via a per-channel multiply.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BarColor {
    /// Red channel in `0.0..=1.0`.
    pub r: f32,
    /// Green channel in `0.0..=1.0`.
    pub g: f32,
    /// Blue channel in `0.0..=1.0`.
    pub b: f32,
}

impl BarColor {
    /// Neutral white — the no-op tint (every channel `1.0`).
    pub const WHITE: Self = Self {
        r: 1.0,
        g: 1.0,
        b: 1.0,
    };
    /// Pure red.
    pub const RED: Self = Self {
        r: 1.0,
        g: 0.0,
        b: 0.0,
    };
    /// Pure green.
    pub const GREEN: Self = Self {
        r: 0.0,
        g: 1.0,
        b: 0.0,
    };
    /// Pure blue.
    pub const BLUE: Self = Self {
        r: 0.0,
        g: 0.0,
        b: 1.0,
    };
    /// Yellow (red + green).
    pub const YELLOW: Self = Self {
        r: 1.0,
        g: 1.0,
        b: 0.0,
    };
    /// Cyan (green + blue).
    pub const CYAN: Self = Self {
        r: 0.0,
        g: 1.0,
        b: 1.0,
    };

    /// The cycle of preset bar colors the in-game customization screen steps
    /// through, starting at the neutral [`WHITE`](BarColor::WHITE) no-op.
    pub const PRESETS: [BarColor; 6] = [
        BarColor::WHITE,
        BarColor::RED,
        BarColor::GREEN,
        BarColor::BLUE,
        BarColor::YELLOW,
        BarColor::CYAN,
    ];

    /// Builds a color from three `0.0..=1.0` channels (clamped into range so a bad
    /// config value can never produce an out-of-gamut tint).
    #[must_use]
    pub fn new(r: f32, g: f32, b: f32) -> Self {
        Self {
            r: r.clamp(0.0, 1.0),
            g: g.clamp(0.0, 1.0),
            b: b.clamp(0.0, 1.0),
        }
    }

    /// Whether this color is the neutral [`WHITE`](BarColor::WHITE) no-op tint.
    #[must_use]
    pub fn is_neutral(&self) -> bool {
        *self == Self::WHITE
    }

    /// A short uppercase label (matching the HUD font's glyph set) for the
    /// customization screen, or `None` for a non-preset color.
    #[must_use]
    pub fn label(&self) -> Option<&'static str> {
        match *self {
            BarColor::WHITE => Some("WHITE"),
            BarColor::RED => Some("RED"),
            BarColor::GREEN => Some("GREEN"),
            BarColor::BLUE => Some("BLUE"),
            BarColor::YELLOW => Some("YELLOW"),
            BarColor::CYAN => Some("CYAN"),
            _ => None,
        }
    }

    /// The next preset color in [`PRESETS`](BarColor::PRESETS), wrapping at the
    /// end. A non-preset color steps to the first preset.
    #[must_use]
    pub fn next_preset(&self) -> Self {
        match Self::PRESETS.iter().position(|c| c == self) {
            Some(i) => Self::PRESETS[(i + 1) % Self::PRESETS.len()],
            None => Self::PRESETS[0],
        }
    }

    /// This color as a [`PalFx`] front-fill tint: a per-channel multiply, with
    /// full color retention / no inversion / no add. [`WHITE`](BarColor::WHITE)
    /// yields [`PalFx::IDENTITY`] exactly, so a neutral color is a guaranteed
    /// no-op draw (byte-for-byte identical to no tint).
    #[must_use]
    pub fn to_palfx(self) -> PalFx {
        PalFx {
            mul: [self.r, self.g, self.b],
            ..PalFx::IDENTITY
        }
    }
}

impl Default for BarColor {
    fn default() -> Self {
        Self::WHITE
    }
}

/// Which individual HUD elements are toggleable for visibility.
///
/// Each [`HudElement`] maps to a part of the screenpack HUD the player can hide;
/// [`HudConfig::is_visible`] consults the config for any of them. The renderer
/// gates each element's draw on its visibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HudElement {
    /// The life bars (both players).
    Life,
    /// The power (super-meter) bars (both players).
    Power,
    /// The fighter name texts (both players).
    Name,
    /// The fight timer / clock text.
    Timer,
    /// The combo counter text.
    Combo,
}

impl HudElement {
    /// Every toggleable element, in the order the customization screen lists them.
    pub const ALL: [HudElement; 5] = [
        HudElement::Life,
        HudElement::Power,
        HudElement::Name,
        HudElement::Timer,
        HudElement::Combo,
    ];

    /// A short uppercase label (matching the HUD font's glyph set) for the
    /// customization screen.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            HudElement::Life => "LIFE",
            HudElement::Power => "POWER",
            HudElement::Name => "NAME",
            HudElement::Timer => "TIMER",
            HudElement::Combo => "COMBO",
        }
    }
}

/// The player-facing HUD customization overrides (T046), layered over the
/// screenpack-authored [`ScreenpackLayout`](crate::screenpack::ScreenpackLayout).
///
/// Built with [`HudConfig::default`] (a guaranteed no-op: every element visible,
/// no tint, no offset, unit scale) and then mutated through the small setters
/// (`set_*`) the in-game HUD-customization screen and any config loader call. The
/// renderer reads it each frame; with the default config the HUD renders exactly
/// as it did before this type existed.
///
/// The visibility set stores only the *hidden* elements (an element absent from
/// the set is visible), so the default — nothing hidden — is the all-visible
/// no-op and equality with [`HudConfig::default`] is a cheap "no overrides" check.
#[derive(Debug, Clone, PartialEq)]
pub struct HudConfig {
    /// Front-fill tint for both life bars (default [`BarColor::WHITE`] = no tint).
    life_color: BarColor,
    /// Front-fill tint for both power bars (default [`BarColor::WHITE`]).
    power_color: BarColor,
    /// A global `(dx, dy)` pixel offset added to every HUD element's anchor
    /// (default `(0, 0)` = no shift). Lets a player nudge the whole HUD.
    offset: (i32, i32),
    /// A global size multiplier for HUD bars (default `1.0` = unchanged).
    scale: f32,
    /// The set of elements the player has hidden; an element NOT listed is
    /// visible. Empty by default (everything visible).
    hidden: Vec<HudElement>,
}

impl Default for HudConfig {
    fn default() -> Self {
        Self {
            life_color: BarColor::WHITE,
            power_color: BarColor::WHITE,
            offset: (0, 0),
            scale: 1.0,
            hidden: Vec::new(),
        }
    }
}

impl HudConfig {
    /// A fresh no-op config (every element visible, no tint, no offset, unit
    /// scale). Same as [`HudConfig::default`]; provided for call-site clarity.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this config carries no overrides at all — i.e. it is the default
    /// no-op. The renderer can use this to stay on the original draw path.
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The life-bar front-fill tint color.
    #[must_use]
    pub fn life_color(&self) -> BarColor {
        self.life_color
    }

    /// Sets the life-bar front-fill tint color.
    pub fn set_life_color(&mut self, color: BarColor) {
        self.life_color = color;
    }

    /// The power-bar front-fill tint color.
    #[must_use]
    pub fn power_color(&self) -> BarColor {
        self.power_color
    }

    /// Sets the power-bar front-fill tint color.
    pub fn set_power_color(&mut self, color: BarColor) {
        self.power_color = color;
    }

    /// The global `(dx, dy)` HUD-anchor pixel offset.
    #[must_use]
    pub fn offset(&self) -> (i32, i32) {
        self.offset
    }

    /// Sets the global `(dx, dy)` HUD-anchor pixel offset.
    pub fn set_offset(&mut self, dx: i32, dy: i32) {
        self.offset = (dx, dy);
    }

    /// The global HUD-bar size multiplier (`1.0` = unchanged).
    #[must_use]
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Sets the global HUD-bar size multiplier, clamped to a sane positive range
    /// (`0.1..=4.0`) so a bad value can never collapse or explode the HUD.
    pub fn set_scale(&mut self, scale: f32) {
        self.scale = if scale.is_finite() {
            scale.clamp(0.1, 4.0)
        } else {
            1.0
        };
    }

    /// Whether `element` is currently visible (the default for every element).
    #[must_use]
    pub fn is_visible(&self, element: HudElement) -> bool {
        !self.hidden.contains(&element)
    }

    /// Sets whether `element` is visible.
    pub fn set_visible(&mut self, element: HudElement, visible: bool) {
        let present = self.hidden.iter().position(|&e| e == element);
        match (visible, present) {
            // Make visible: drop it from the hidden set if present.
            (true, Some(i)) => {
                self.hidden.remove(i);
            }
            // Hide: add it to the hidden set if not already there.
            (false, None) => self.hidden.push(element),
            _ => {}
        }
    }

    /// Toggles `element`'s visibility and returns its new visible state.
    pub fn toggle_visible(&mut self, element: HudElement) -> bool {
        let now = !self.is_visible(element);
        self.set_visible(element, now);
        now
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_a_no_op_config() {
        let c = HudConfig::default();
        assert!(c.is_default());
        assert!(c.life_color().is_neutral());
        assert!(c.power_color().is_neutral());
        assert_eq!(c.offset(), (0, 0));
        assert_eq!(c.scale(), 1.0);
        for e in HudElement::ALL {
            assert!(c.is_visible(e), "{e:?} visible by default");
        }
    }

    #[test]
    fn white_bar_color_maps_to_identity_palfx() {
        // The keystone of the regression guarantee: a neutral color is exactly the
        // no-op tint, so a default-config bar draws byte-identically.
        assert_eq!(BarColor::WHITE.to_palfx(), PalFx::IDENTITY);
        assert!(BarColor::WHITE.to_palfx().is_identity());
        assert!(BarColor::default().is_neutral());
    }

    #[test]
    fn non_white_color_is_a_real_tint() {
        let fx = BarColor::RED.to_palfx();
        assert!(!fx.is_identity());
        assert_eq!(fx.mul, [1.0, 0.0, 0.0]);
    }

    #[test]
    fn bar_color_new_clamps_into_gamut() {
        let c = BarColor::new(2.0, -1.0, 0.5);
        assert_eq!(c, BarColor::new(1.0, 0.0, 0.5));
        assert_eq!((c.r, c.g, c.b), (1.0, 0.0, 0.5));
    }

    #[test]
    fn bar_color_next_preset_cycles_and_wraps() {
        // Starts white -> red -> ... -> cyan -> back to white.
        let mut c = BarColor::WHITE;
        let order = [
            BarColor::RED,
            BarColor::GREEN,
            BarColor::BLUE,
            BarColor::YELLOW,
            BarColor::CYAN,
            BarColor::WHITE,
        ];
        for expected in order {
            c = c.next_preset();
            assert_eq!(c, expected);
        }
    }

    #[test]
    fn preset_labels_are_present_and_unique() {
        let mut seen = Vec::new();
        for c in BarColor::PRESETS {
            let l = c.label().expect("preset has a label");
            assert!(!seen.contains(&l), "labels unique: {l}");
            seen.push(l);
        }
    }

    #[test]
    fn set_life_color_changes_only_life() {
        let mut c = HudConfig::default();
        c.set_life_color(BarColor::GREEN);
        assert_eq!(c.life_color(), BarColor::GREEN);
        assert!(c.power_color().is_neutral(), "power untouched");
        assert!(!c.is_default(), "an override makes it non-default");
    }

    #[test]
    fn visibility_toggle_round_trips() {
        let mut c = HudConfig::default();
        assert!(c.is_visible(HudElement::Power));
        let now = c.toggle_visible(HudElement::Power);
        assert!(!now);
        assert!(!c.is_visible(HudElement::Power), "hidden after toggle");
        // Other elements stay visible.
        assert!(c.is_visible(HudElement::Life));
        // Toggling back makes it visible and restores the default config.
        let now = c.toggle_visible(HudElement::Power);
        assert!(now);
        assert!(c.is_visible(HudElement::Power));
        assert!(c.is_default(), "round-trip restores the no-op config");
    }

    #[test]
    fn set_visible_is_idempotent() {
        let mut c = HudConfig::default();
        c.set_visible(HudElement::Timer, false);
        c.set_visible(HudElement::Timer, false);
        assert!(!c.is_visible(HudElement::Timer));
        // Only one entry recorded despite two hides.
        c.set_visible(HudElement::Timer, true);
        assert!(c.is_visible(HudElement::Timer));
        assert!(c.is_default());
    }

    #[test]
    fn set_scale_clamps_and_rejects_nonfinite() {
        let mut c = HudConfig::default();
        c.set_scale(100.0);
        assert_eq!(c.scale(), 4.0, "clamped to the max");
        c.set_scale(0.0);
        assert_eq!(c.scale(), 0.1, "clamped to the min");
        c.set_scale(f32::NAN);
        assert_eq!(c.scale(), 1.0, "NaN falls back to unit scale");
        c.set_scale(1.5);
        assert_eq!(c.scale(), 1.5);
    }

    #[test]
    fn set_offset_is_a_plain_delta() {
        let mut c = HudConfig::default();
        c.set_offset(10, -4);
        assert_eq!(c.offset(), (10, -4));
        assert!(!c.is_default());
    }
}
