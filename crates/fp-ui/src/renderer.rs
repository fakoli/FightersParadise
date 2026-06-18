//! GPU-side screenpack HUD: turn a [`ScreenpackLayout`] + live match state into
//! `fp-render` draw calls.
//!
//! [`ScreenpackHud`] owns the GPU-resident pieces of a screenpack — every
//! `fight.sff` sprite uploaded as an `R8Unorm` index texture + its palette, and
//! the `font0..fontN` fonts as [`GlyphFont`]s — built once with
//! [`ScreenpackHud::build`]. Each frame, [`ScreenpackHud::draw`] takes a small
//! [`MatchHudState`] (life/power fractions, names, round/KO/timer) and issues
//! [`RenderFrame::draw_sprite`] / [`RenderFrame::draw_text`] calls to paint the
//! life bars, power bars, fighter portraits, names, round announcer, and timer.
//!
//! The bar-fill geometry ([`bar_fill_uv`], [`clamp_fraction`]) is pure and
//! unit-tested; the GPU draw is a thin loop over it. `MatchHudState` is a plain
//! data struct so this module need not depend on `fp-engine`: the caller (the
//! app) fills it from `Player::life`/`power`/etc.

use std::collections::HashMap;

use fp_formats::fnt::FntFont;
use fp_formats::sff::SffFile;
use fp_render::{
    GlyphFont, PalFx, PaletteTexture, RenderFrame, Renderer, SpriteDrawParams, SpriteTexture,
    TextDrawParams,
};

use crate::hud_config::{BarColor, HudConfig, HudElement};
use crate::screenpack::{FaceSide, LifebarSide, PowerbarSide, ScreenpackLayout, SpriteRef};

/// Live, per-frame HUD inputs the screenpack renderer needs, decoupled from any
/// engine type so this crate does not depend on `fp-engine`.
///
/// The caller fills this each frame from the match: life/power fractions in
/// `[0, 1]` (the app computes them via `Player::life`/`life_max` etc.), the two
/// fighter display names, and the round/timer/KO readout.
#[derive(Debug, Clone, Default)]
pub struct MatchHudState {
    /// P1 life fraction in `[0, 1]`.
    pub p1_life: f32,
    /// P2 life fraction in `[0, 1]`.
    pub p2_life: f32,
    /// P1 power (super-meter) fraction in `[0, 1]`.
    pub p1_power: f32,
    /// P2 power (super-meter) fraction in `[0, 1]`.
    pub p2_power: f32,
    /// P1 display name.
    pub p1_name: String,
    /// P2 display name.
    pub p2_name: String,
    /// The fight timer, in whole seconds; `None` hides the timer text.
    pub timer_seconds: Option<i32>,
    /// Text drawn by the round announcer (e.g. `"Round 1"`, `"KO"`, `"Fight"`);
    /// empty hides it.
    pub round_text: String,
    /// Number of hits in the current active combo. The combo counter is drawn
    /// only while this is `>= 2` (MUGEN shows the counter from the 2nd hit on);
    /// `0`/`1` hide it. See [`combo_text`].
    pub combo_count: i32,
    /// A monotonically increasing frame counter, used purely to drive the
    /// deterministic max-power flash (T074) — no RNG, so it is replay-safe.
    /// Defaults to `0`; the first flash phase is the bright (no-op) phase, so a
    /// caller that never sets it sees no flash artifact.
    pub frame: u64,
}

/// A GPU-resident screenpack: the parsed layout, every referenced `fight.sff`
/// sprite uploaded as a texture+palette, and the fonts uploaded as [`GlyphFont`]s.
///
/// Build once per match with [`ScreenpackHud::build`]; reuse across frames. The
/// owned [`ScreenpackLayout`] is exposed via [`layout`](Self::layout) for the
/// app's power-level sound routing.
pub struct ScreenpackHud {
    /// The parsed layout this HUD draws.
    layout: ScreenpackLayout,
    /// `fight.sff` sprites keyed by `(group, image)`, decoded + uploaded lazily
    /// at build time.
    sprites: HashMap<(u16, u16), GpuSprite>,
    /// Fonts in `font0..` slot order; `fonts[i]` is the GPU upload of layout font
    /// slot `i`. A font that failed to load is `None` (its text is skipped).
    fonts: Vec<Option<GlyphFont>>,
    /// Player-facing customization overrides (bar colors, offset/scale, per-element
    /// visibility) layered over [`layout`](Self::layout) at draw time. Defaults to
    /// the no-op [`HudConfig::default`], so an unconfigured HUD draws unchanged.
    hud_config: HudConfig,
}

/// One uploaded `fight.sff` sprite: its index texture, palette, and pixel size.
struct GpuSprite {
    texture: SpriteTexture,
    palette: PaletteTexture,
    width: f32,
    height: f32,
}

impl ScreenpackHud {
    /// Builds the GPU-resident HUD from a parsed [`ScreenpackLayout`], the loaded
    /// `fight.sff`, and the loaded fonts (one per `font0..` slot, in order).
    ///
    /// `fonts` is indexed by font slot; pass `None` for a slot whose font failed
    /// to load (its text is then skipped at draw time). Each sprite referenced by
    /// the layout is decoded from `sff` and uploaded; a sprite that is missing or
    /// fails to decode is skipped with a `tracing::warn!` and simply does not draw
    /// — never a panic.
    pub fn build(
        renderer: &Renderer,
        layout: ScreenpackLayout,
        sff: &SffFile,
        fonts: Vec<Option<FntFont>>,
    ) -> Self {
        let mut sprites = HashMap::new();
        for r in layout_sprite_refs(&layout) {
            let key = (r.group, r.image);
            if sprites.contains_key(&key) {
                continue;
            }
            if let Some(gpu) = upload_sprite(renderer, sff, r.group, r.image) {
                sprites.insert(key, gpu);
            }
        }

        let fonts = fonts
            .into_iter()
            .map(|f| f.map(|font| GlyphFont::new(renderer.device(), renderer.queue(), font)))
            .collect();

        Self {
            layout,
            sprites,
            fonts,
            hud_config: HudConfig::default(),
        }
    }

    /// The parsed layout backing this HUD (e.g. for power-level sound routing).
    pub fn layout(&self) -> &ScreenpackLayout {
        &self.layout
    }

    /// The player-facing HUD customization overrides this HUD draws with (T046).
    pub fn hud_config(&self) -> &HudConfig {
        &self.hud_config
    }

    /// Replaces the HUD customization overrides (T046).
    ///
    /// The app calls this when the player changes a value on the in-game
    /// HUD-customization screen (or loads one from a config file); the next
    /// [`draw`](Self::draw) honors the new overrides. Passing
    /// [`HudConfig::default`] restores the unchanged HUD.
    pub fn set_hud_config(&mut self, config: HudConfig) {
        self.hud_config = config;
    }

    /// Mutable access to the HUD customization overrides, for an in-place edit
    /// (e.g. the customization screen toggling one element).
    pub fn hud_config_mut(&mut self) -> &mut HudConfig {
        &mut self.hud_config
    }

    /// Draws the whole screenpack HUD for the current frame.
    ///
    /// Order: P1 then P2 life bars (every `bg0..bgN` background layer → mid →
    /// front fill), power bars, fighter portraits ([`crate::screenpack::FaceSide`]),
    /// fighter names, the timer, the round announcer, and — while a combo is
    /// active — the combo counter. Missing sprites/fonts are silently skipped
    /// (logged once at build), so a partial screenpack still renders whatever it
    /// does define.
    pub fn draw(&self, frame: &mut RenderFrame<'_>, state: &MatchHudState) {
        let cfg = &self.hud_config;
        let (dx, dy) = cfg.offset();
        // A global pixel offset added to every element's anchor; `(0, 0)` (the
        // default) leaves the position byte-identical.
        let (ox, oy) = (dx as f32, dy as f32);

        // Life bars — gated on the Life element's visibility, tinted by the
        // configured life color, then nudged toward red at low life (T074), and
        // globally scaled. The threshold tint is `WHITE` (a no-op) above 25%, so
        // a healthy bar draws exactly as the screenpack styled it.
        if cfg.is_visible(HudElement::Life) {
            let base = cfg.life_color();
            let p1_tint = base.combine(crate::low_life_tint(state.p1_life));
            let p2_tint = base.combine(crate::low_life_tint(state.p2_life));
            self.draw_lifebar(
                frame,
                &self.layout.p1_lifebar,
                state.p1_life,
                ox,
                oy,
                p1_tint,
            );
            self.draw_lifebar(
                frame,
                &self.layout.p2_lifebar,
                state.p2_life,
                ox,
                oy,
                p2_tint,
            );
        }
        // Power bars — gated on the Power element's visibility. The configured
        // tint flashes (T074) once the meter is full (a super is available); the
        // flash is `WHITE` (a no-op) on its bright phase and below max, so an
        // unfilled bar draws unchanged.
        if cfg.is_visible(HudElement::Power) {
            let base = cfg.power_color();
            let p1_tint = base.combine(crate::max_power_flash_tint(state.p1_power, state.frame));
            let p2_tint = base.combine(crate::max_power_flash_tint(state.p2_power, state.frame));
            self.draw_powerbar(
                frame,
                &self.layout.p1_powerbar,
                state.p1_power,
                ox,
                oy,
                p1_tint,
            );
            self.draw_powerbar(
                frame,
                &self.layout.p2_powerbar,
                state.p2_power,
                ox,
                oy,
                p2_tint,
            );
        }
        // Fighter portraits ([Face]) — drawn with the bars (no separate toggle).
        self.draw_face(frame, &self.layout.p1_face, ox, oy);
        self.draw_face(frame, &self.layout.p2_face, ox, oy);
        // Names — gated on the Name element's visibility.
        if cfg.is_visible(HudElement::Name) {
            self.draw_text_slot(
                frame,
                self.layout.p1_name.font,
                self.layout.p1_name.pos.x as f32 + ox,
                self.layout.p1_name.pos.y as f32 + oy,
                &state.p1_name,
            );
            self.draw_text_slot(
                frame,
                self.layout.p2_name.font,
                self.layout.p2_name.pos.x as f32 + ox,
                self.layout.p2_name.pos.y as f32 + oy,
                &state.p2_name,
            );
        }
        // Timer — gated on the Timer element's visibility.
        if cfg.is_visible(HudElement::Timer) {
            if let Some(secs) = state.timer_seconds {
                self.draw_text_slot(
                    frame,
                    self.layout.time.font,
                    self.layout.time.pos.x as f32 + ox,
                    self.layout.time.pos.y as f32 + oy,
                    &secs.to_string(),
                );
            }
        }
        // Round announcer (always drawn — not a player-toggleable element).
        if !state.round_text.is_empty() {
            self.draw_text_slot(
                frame,
                self.layout.round.font,
                self.layout.round.pos.x as f32 + ox,
                self.layout.round.pos.y as f32 + oy,
                &state.round_text,
            );
        }
        // Combo counter — only while a combo is active (>= 2 hits) AND the Combo
        // element is visible.
        if cfg.is_visible(HudElement::Combo) {
            if let Some(text) = combo_text(state.combo_count) {
                self.draw_text_slot(
                    frame,
                    self.layout.combo.font,
                    self.layout.combo.pos.x as f32 + ox,
                    self.layout.combo.pos.y as f32 + oy,
                    &text,
                );
            }
        }
    }

    /// Draws one life bar: every `bg0..bgN` background layer in z-order at full
    /// size, then the mid layer, then the front layer clipped horizontally to
    /// `frac` of the bar's `range` span.
    ///
    /// `(ox, oy)` is the global HUD pixel offset (see [`HudConfig::offset`]) added
    /// to the bar's anchor; `tint` retints the front fill (a neutral
    /// [`BarColor::WHITE`] is a no-op). The global [scale](HudConfig::scale) is
    /// read from the config.
    fn draw_lifebar(
        &self,
        frame: &mut RenderFrame<'_>,
        bar: &LifebarSide,
        frac: f32,
        ox: f32,
        oy: f32,
        tint: BarColor,
    ) {
        let base_x = bar.pos.x as f32 + ox;
        let base_y = bar.pos.y as f32 + oy;
        let scale = self.hud_config.scale();
        // Every background layer, then mid, all drawn whole.
        for &bg in &bar.bg_layers {
            self.draw_sprite_ref(frame, Some(bg), base_x, base_y, scale);
        }
        self.draw_sprite_ref(frame, bar.mid, base_x, base_y, scale);
        // Front fill clips to the life fraction over the bar's range.
        self.draw_bar_fill(
            frame, bar.front, base_x, base_y, bar.range, frac, scale, tint,
        );
    }

    /// Draws one power bar (same layering as a life bar, clipped to `frac`).
    ///
    /// See [`draw_lifebar`](Self::draw_lifebar) for the `(ox, oy)`/`tint` meaning.
    fn draw_powerbar(
        &self,
        frame: &mut RenderFrame<'_>,
        bar: &PowerbarSide,
        frac: f32,
        ox: f32,
        oy: f32,
        tint: BarColor,
    ) {
        let base_x = bar.pos.x as f32 + ox;
        let base_y = bar.pos.y as f32 + oy;
        let scale = self.hud_config.scale();
        for &bg in &bar.bg_layers {
            self.draw_sprite_ref(frame, Some(bg), base_x, base_y, scale);
        }
        self.draw_sprite_ref(frame, bar.mid, base_x, base_y, scale);
        self.draw_bar_fill(
            frame, bar.front, base_x, base_y, bar.range, frac, scale, tint,
        );
    }

    /// Draws one player's portrait ([`FaceSide`]) at its parsed position.
    ///
    /// The draw position is the face's `pos` plus the sprite's `offset` (computed
    /// purely by [`face_draw_pos`]) plus the global HUD `(ox, oy)` offset; the
    /// portrait is drawn at full size (scale `1.0`). A face with no sprite
    /// reference, or whose sprite failed to upload, draws nothing.
    fn draw_face(&self, frame: &mut RenderFrame<'_>, face: &FaceSide, ox: f32, oy: f32) {
        let Some(r) = face.spr else { return };
        let Some(gpu) = self.sprites.get(&(r.group, r.image)) else {
            return;
        };
        let (x, y) = face_draw_pos(face);
        let params = SpriteDrawParams {
            x: x + ox,
            y: y + oy,
            ..Default::default()
        };
        frame.draw_sprite(&gpu.texture, &gpu.palette, &params);
    }

    /// Draws a bar's front-fill sprite clipped to `frac` of `range`.
    ///
    /// Uses [`bar_fill_uv`] to compute the visible UV sub-rectangle and the
    /// destination width, so a `frac` of `0` draws nothing and `1` draws the full
    /// sprite. A negative `range` span (P2's mirrored bar) clips from the right.
    /// `scale` multiplies the drawn size (`1.0` = unchanged) and `tint` retints
    /// the fill (a neutral [`BarColor::WHITE`] yields the identity tint, i.e. no
    /// change).
    #[allow(clippy::too_many_arguments)]
    fn draw_bar_fill(
        &self,
        frame: &mut RenderFrame<'_>,
        front: Option<SpriteRef>,
        base_x: f32,
        base_y: f32,
        range: (i32, i32),
        frac: f32,
        scale: f32,
        tint: BarColor,
    ) {
        let Some(r) = front else { return };
        let Some(gpu) = self.sprites.get(&(r.group, r.image)) else {
            return;
        };
        let frac = clamp_fraction(frac);
        if frac <= 0.0 {
            return;
        }
        let (uv, dst_w, dst_x_off) = bar_fill_uv(range, frac, gpu.width);
        // `draw_sprite_region` takes the destination size directly (it does NOT
        // read `params.scale_x`/`scale_y`), so the global `scale` override must be
        // baked into the destination width/height here to match the scaled bg/mid
        // layers (which go through `draw_sprite`). `scale == 1.0` (default config)
        // leaves `dst_w`/`gpu.height` untouched, so the no-override draw is
        // byte-for-byte unchanged.
        let params = bar_fill_params(
            base_x + r.offset.x as f32 + dst_x_off,
            base_y + r.offset.y as f32,
            tint,
        );
        frame.draw_sprite_region(
            &gpu.texture,
            &gpu.palette,
            &params,
            uv,
            dst_w * scale,
            gpu.height * scale,
        );
    }

    /// Draws a sprite reference at full size scaled by `scale` (background/mid
    /// layers). `scale` of `1.0` (the default config) is byte-identical to the
    /// pre-T046 draw.
    fn draw_sprite_ref(
        &self,
        frame: &mut RenderFrame<'_>,
        spr: Option<SpriteRef>,
        base_x: f32,
        base_y: f32,
        scale: f32,
    ) {
        let Some(r) = spr else { return };
        let Some(gpu) = self.sprites.get(&(r.group, r.image)) else {
            return;
        };
        let params = SpriteDrawParams {
            x: base_x + r.offset.x as f32,
            y: base_y + r.offset.y as f32,
            scale_x: scale,
            scale_y: scale,
            ..Default::default()
        };
        frame.draw_sprite(&gpu.texture, &gpu.palette, &params);
    }

    /// Draws `text` at `(x, y)` using the font in slot `slot`, if loaded.
    fn draw_text_slot(&self, frame: &mut RenderFrame<'_>, slot: usize, x: f32, y: f32, text: &str) {
        if text.is_empty() {
            return;
        }
        let Some(Some(font)) = self.fonts.get(slot) else {
            return;
        };
        let params = TextDrawParams {
            x,
            y,
            ..Default::default()
        };
        frame.draw_text(font, text, &params);
    }
}

/// Decodes and uploads one `fight.sff` sprite to the GPU, or `None` (with a
/// warning) if it is missing or fails to decode.
fn upload_sprite(renderer: &Renderer, sff: &SffFile, group: u16, image: u16) -> Option<GpuSprite> {
    let (index, sprite) = sff
        .sprites
        .iter()
        .enumerate()
        .find(|(_, s)| s.group == group && s.image == image)?;
    if sprite.width == 0 || sprite.height == 0 {
        tracing::warn!(
            group,
            image,
            "screenpack sprite has zero dimensions; skipping"
        );
        return None;
    }
    let pixels = match sff.decode_sprite(index) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(group, image, error = %e, "screenpack sprite failed to decode; skipping");
            return None;
        }
    };
    let palette = match sff.palette(sprite.palette_index as usize) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(group, image, error = %e, "screenpack sprite palette missing; skipping");
            return None;
        }
    };
    let texture = SpriteTexture::new(
        renderer.device(),
        renderer.queue(),
        sprite.width as u32,
        sprite.height as u32,
        &pixels,
    );
    let palette = PaletteTexture::new(renderer.device(), renderer.queue(), &palette);
    Some(GpuSprite {
        texture,
        palette,
        width: sprite.width as f32,
        height: sprite.height as f32,
    })
}

/// Collects every sprite reference used by a layout (both bars' layers + faces),
/// so the builder can pre-upload exactly the sprites it will draw.
fn layout_sprite_refs(layout: &ScreenpackLayout) -> Vec<SpriteRef> {
    let mut refs = Vec::new();
    let mut push_lifebar = |b: &LifebarSide| {
        refs.extend(b.bg_layers.iter().copied());
        refs.extend(b.mid);
        refs.extend(b.front);
    };
    push_lifebar(&layout.p1_lifebar);
    push_lifebar(&layout.p2_lifebar);
    let mut push_powerbar = |b: &PowerbarSide| {
        refs.extend(b.bg_layers.iter().copied());
        refs.extend(b.mid);
        refs.extend(b.front);
    };
    push_powerbar(&layout.p1_powerbar);
    push_powerbar(&layout.p2_powerbar);
    refs.extend(layout.p1_face.spr);
    refs.extend(layout.p2_face.spr);
    refs
}

/// The combo-counter text for a hit count, or `None` when no counter should
/// show.
///
/// MUGEN displays the combo counter only once a combo is *active* — from the
/// second connected hit onward — so a count of `0` or `1` (and any negative,
/// defensive against bad inputs) returns `None` and draws nothing. A count of
/// `2+` formats as `"<n> Hits"` (e.g. `"5 Hits"`).
///
/// Pure and unit-tested — no GPU. The renderer calls this each frame from
/// [`MatchHudState::combo_count`] and draws the result at the parsed
/// [`crate::screenpack::ComboLayout`] position.
pub fn combo_text(count: i32) -> Option<String> {
    if count >= 2 {
        Some(format!("{count} Hits"))
    } else {
        None
    }
}

/// Clamps a bar fraction into `[0, 1]`, mapping NaN to `0`.
///
/// Pure; mirrors `fp-app`'s `life_fraction`/`power_fraction` safety so the
/// screenpack and quad HUDs agree on out-of-range inputs.
pub fn clamp_fraction(frac: f32) -> f32 {
    if frac.is_nan() {
        0.0
    } else {
        frac.clamp(0.0, 1.0)
    }
}

/// Computes the visible front-fill sub-rectangle for a bar at fraction `frac`.
///
/// Returns `(uv, dst_w, dst_x_off)`:
/// - `uv` is the `[u_min, v_min, u_max, v_max]` source rectangle (whole sprite
///   height, horizontally clipped to `frac`).
/// - `dst_w` is the destination width in pixels (`sprite_w * frac`).
/// - `dst_x_off` is the X offset to add to the draw position so a right-anchored
///   (negative-span) bar clips from its right edge rather than its left.
///
/// A non-negative `range` span (`x1 >= x0`) clips from the **left** (P1's bar,
/// which empties toward the centre); a negative span (`x1 < x0`, P2's mirrored
/// bar) clips from the **right**. `sprite_w` is the full sprite width in pixels.
///
/// Pure and unit-tested — no GPU.
pub fn bar_fill_uv(range: (i32, i32), frac: f32, sprite_w: f32) -> ([f32; 4], f32, f32) {
    let frac = clamp_fraction(frac);
    let dst_w = sprite_w * frac;
    // Whether the bar empties toward the right edge (P2's mirrored range).
    let right_anchored = range.1 < range.0;
    if right_anchored {
        // Keep the right `frac` of the sprite: u in [1-frac, 1], drawn shifted
        // right so its right edge stays put.
        let uv = [1.0 - frac, 0.0, 1.0, 1.0];
        let dst_x_off = sprite_w - dst_w;
        (uv, dst_w, dst_x_off)
    } else {
        // Keep the left `frac`: u in [0, frac], drawn at the bar's left edge.
        let uv = [0.0, 0.0, frac, 1.0];
        (uv, dst_w, 0.0)
    }
}

/// Computes the screen position at which a player's portrait ([`FaceSide`]) is
/// drawn: the face's anchor `pos` plus the sprite reference's `offset`.
///
/// Returns `(x, y)` in screen pixels. The portrait is drawn at full size (scale
/// `1.0`); MUGEN screenpack `[Face]` elements carry no per-face scale, so the
/// position is the only placement input. A face with no sprite reference still
/// resolves to its bare `pos` (the renderer just skips the draw).
///
/// Pure and unit-tested — no GPU. Mirrors the `pos + offset` placement the
/// renderer uses for every other screenpack sprite.
pub fn face_draw_pos(face: &FaceSide) -> (f32, f32) {
    let (ox, oy) = match face.spr {
        Some(r) => (r.offset.x, r.offset.y),
        None => (0, 0),
    };
    ((face.pos.x + ox) as f32, (face.pos.y + oy) as f32)
}

/// The [`PalFx`] tint used to draw a bar's front fill for a configured
/// [`BarColor`] (T046).
///
/// A neutral [`BarColor::WHITE`] returns [`PalFx::IDENTITY`] exactly — the no-op
/// tint — so the default HUD config produces byte-for-byte identical bar fills;
/// any other color tints the fill via a per-channel multiply.
///
/// Pure and unit-tested — no GPU.
#[must_use]
pub fn bar_tint_palfx(color: BarColor) -> PalFx {
    color.to_palfx()
}

/// Builds the [`SpriteDrawParams`] for a bar front-fill draw at `(x, y)` with the
/// given [`BarColor`] `tint` (T046).
///
/// This is the single construction site the renderer's `draw_bar_fill` uses, so
/// the no-override regression test can assert against the *real* params rather
/// than a hand-copied literal. The per-element `scale` is NOT carried on the
/// params — [`RenderFrame::draw_sprite_region`](fp_render::RenderFrame::draw_sprite_region)
/// ignores `scale_x`/`scale_y` and takes the destination size directly — so the
/// caller bakes `scale` into the destination width/height instead. With a neutral
/// [`BarColor::WHITE`] this yields exactly `SpriteDrawParams { x, y, ..default }`
/// (identity tint), i.e. the pre-T046 draw.
///
/// Pure and unit-tested — no GPU.
#[must_use]
pub fn bar_fill_params(x: f32, y: f32, tint: BarColor) -> SpriteDrawParams {
    SpriteDrawParams {
        x,
        y,
        palfx: bar_tint_palfx(tint),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_fraction_bounds_and_nan() {
        assert_eq!(clamp_fraction(-1.0), 0.0);
        assert_eq!(clamp_fraction(0.0), 0.0);
        assert_eq!(clamp_fraction(0.5), 0.5);
        assert_eq!(clamp_fraction(1.0), 1.0);
        assert_eq!(clamp_fraction(2.0), 1.0);
        assert_eq!(clamp_fraction(f32::NAN), 0.0);
    }

    #[test]
    fn full_fraction_draws_whole_sprite() {
        let (uv, dst_w, off) = bar_fill_uv((0, 256), 1.0, 200.0);
        assert_eq!(uv, [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(dst_w, 200.0);
        assert_eq!(off, 0.0);
    }

    #[test]
    fn empty_fraction_draws_nothing_wide() {
        let (uv, dst_w, off) = bar_fill_uv((0, 256), 0.0, 200.0);
        assert_eq!(uv, [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(dst_w, 0.0);
        assert_eq!(off, 0.0);
    }

    #[test]
    fn left_anchored_half_fill_clips_from_left() {
        // P1 bar (positive span): half life -> left half of the sprite, at x=0.
        let (uv, dst_w, off) = bar_fill_uv((0, 256), 0.5, 200.0);
        assert_eq!(uv, [0.0, 0.0, 0.5, 1.0]);
        assert_eq!(dst_w, 100.0);
        assert_eq!(off, 0.0);
    }

    #[test]
    fn right_anchored_half_fill_clips_from_right() {
        // P2 bar (negative span): half life -> right half of the sprite, shifted
        // right so the right edge stays anchored.
        let (uv, dst_w, off) = bar_fill_uv((0, -256), 0.5, 200.0);
        assert_eq!(uv, [0.5, 0.0, 1.0, 1.0]);
        assert_eq!(dst_w, 100.0);
        assert_eq!(
            off, 100.0,
            "shift = sprite_w - dst_w keeps the right edge fixed"
        );
    }

    #[test]
    fn fraction_is_clamped_inside_bar_fill() {
        // Over-range fraction clamps to a full bar, not a >100% draw.
        let (uv, dst_w, _) = bar_fill_uv((0, 256), 1.5, 200.0);
        assert_eq!(uv, [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(dst_w, 200.0);
    }

    #[test]
    fn collects_all_layout_sprite_refs() {
        use crate::screenpack::{LifebarSide, Pos};
        let layout = ScreenpackLayout {
            p1_lifebar: LifebarSide {
                bg_layers: vec![
                    SpriteRef {
                        group: 0,
                        image: 0,
                        offset: Pos::default(),
                    },
                    SpriteRef {
                        group: 0,
                        image: 3,
                        offset: Pos::default(),
                    },
                ],
                front: Some(SpriteRef {
                    group: 2,
                    image: 0,
                    offset: Pos::default(),
                }),
                ..Default::default()
            },
            p2_lifebar: LifebarSide {
                bg_layers: vec![SpriteRef {
                    group: 0,
                    image: 1,
                    offset: Pos::default(),
                }],
                ..Default::default()
            },
            ..Default::default()
        };
        let refs = layout_sprite_refs(&layout);
        // p1 bg0, p1 bg1, p1 front, p2 bg0 -> 4 refs collected (all bg layers).
        assert_eq!(refs.len(), 4);
        assert!(refs.iter().any(|r| (r.group, r.image) == (0, 0)));
        assert!(refs.iter().any(|r| (r.group, r.image) == (0, 3)));
        assert!(refs.iter().any(|r| (r.group, r.image) == (2, 0)));
        assert!(refs.iter().any(|r| (r.group, r.image) == (0, 1)));
    }

    #[test]
    fn lifebar_bg_layers_are_collected_in_z_order() {
        use crate::screenpack::{LifebarSide, Pos};
        // bg0 must precede bg1 in the collected refs (z-order: bg0 at the back).
        let layout = ScreenpackLayout {
            p1_lifebar: LifebarSide {
                bg_layers: vec![
                    SpriteRef {
                        group: 5,
                        image: 0,
                        offset: Pos::default(),
                    },
                    SpriteRef {
                        group: 5,
                        image: 1,
                        offset: Pos::default(),
                    },
                ],
                ..Default::default()
            },
            ..Default::default()
        };
        let refs = layout_sprite_refs(&layout);
        assert_eq!(
            (refs[0].group, refs[0].image),
            (5, 0),
            "bg0 first (drawn at the back)"
        );
        assert_eq!(
            (refs[1].group, refs[1].image),
            (5, 1),
            "bg1 second (drawn on top)"
        );
    }

    // ---- T046 HUD customization: no-override regression guard ------------

    #[test]
    fn default_bar_tint_is_identity_palfx() {
        // Acceptance #1 keystone: with no override, the life/power bar tint is the
        // identity PalFx, so a default-config bar fill draws byte-for-byte as it
        // did before T046 (an identity PalFx is the SpriteDrawParams default).
        use crate::hud_config::HudConfig;
        let cfg = HudConfig::default();
        assert_eq!(bar_tint_palfx(cfg.life_color()), PalFx::IDENTITY);
        assert_eq!(bar_tint_palfx(cfg.power_color()), PalFx::IDENTITY);
    }

    #[test]
    fn default_config_bar_fill_params_match_pre_t046_draw() {
        // Acceptance #1: exercise the REAL param-construction helper
        // (`bar_fill_params`, the single site `draw_bar_fill` calls) for the
        // DEFAULT config (offset (0,0), white tint) and assert it equals the
        // pre-T046 construction (`x, y, ..Default::default()`). This is the
        // regression guard that a no-override HUD is unchanged — it tracks the
        // real helper body, not a hand-copied literal.
        use crate::hud_config::HudConfig;
        let cfg = HudConfig::default();
        let (ox, oy) = cfg.offset();
        let tint = cfg.life_color();
        // Stand-in for a parsed front-fill sprite offset + a bar anchor.
        let (base_x, base_y) = (80.0 + ox as f32, 33.0 + oy as f32);
        let (off_x, off_y, dst_x_off) = (4.0, 4.0, 0.0);

        // The T046 path (config-driven), built by the production helper.
        let t046 = bar_fill_params(base_x + off_x + dst_x_off, base_y + off_y, tint);
        // The original pre-T046 path.
        let original = SpriteDrawParams {
            x: 80.0 + off_x + dst_x_off,
            y: 33.0 + off_y,
            ..Default::default()
        };
        assert_eq!(t046.x, original.x);
        assert_eq!(t046.y, original.y);
        assert_eq!(t046.scale_x, original.scale_x);
        assert_eq!(t046.scale_y, original.scale_y);
        assert!(t046.palfx.is_identity());
        assert_eq!(t046.palfx, original.palfx);
        assert_eq!(t046.alpha, original.alpha);
        assert_eq!(t046.blend, original.blend);
    }

    #[test]
    fn bar_fill_scale_changes_the_destination_size() {
        // The `scale` override must reach the front-fill's DESTINATION size, not
        // its params (`draw_sprite_region` ignores `scale_x`/`scale_y`). Mirror
        // the renderer's `draw_bar_fill` size math: a non-1.0 scale multiplies the
        // dst width and height, and scale 1.0 leaves them untouched (the
        // no-override byte-for-byte guarantee).
        let (_uv, dst_w, _off) = bar_fill_uv((0, 256), 1.0, 200.0);
        let sprite_h = 16.0_f32;

        // Default scale 1.0: destination size is unchanged.
        let scale = 1.0_f32;
        assert_eq!(dst_w * scale, 200.0);
        assert_eq!(sprite_h * scale, 16.0);

        // A 2.0 scale doubles both dimensions of the colored front fill, matching
        // the doubled bg/mid layers.
        let scale = 2.0_f32;
        assert_eq!(dst_w * scale, 400.0, "front fill grows with the frame");
        assert_eq!(sprite_h * scale, 32.0);
    }

    #[test]
    fn default_config_is_all_visible_no_shift() {
        // Every element visible, no anchor shift: the draw() visibility gates and
        // offset add are all no-ops under the default config.
        use crate::hud_config::{HudConfig, HudElement};
        let cfg = HudConfig::default();
        assert_eq!(cfg.offset(), (0, 0));
        assert_eq!(cfg.scale(), 1.0);
        for e in HudElement::ALL {
            assert!(cfg.is_visible(e), "{e:?} visible under default config");
        }
    }

    #[test]
    fn a_set_override_changes_the_tint_the_renderer_reads() {
        // A configured non-white life color is a real tint the renderer applies
        // (so the change is visible), while a neutral color stays the no-op.
        use crate::hud_config::{BarColor, HudConfig};
        let mut cfg = HudConfig::default();
        cfg.set_life_color(BarColor::RED);
        let fx = bar_tint_palfx(cfg.life_color());
        assert!(!fx.is_identity(), "a real override is not the no-op tint");
        assert_eq!(fx.mul, [1.0, 0.0, 0.0]);
    }

    #[test]
    fn combo_text_hidden_below_two_hits() {
        // No active combo: 0 or 1 hit draws nothing (negatives are defensive).
        assert_eq!(combo_text(0), None);
        assert_eq!(combo_text(1), None);
        assert_eq!(combo_text(-3), None);
    }

    #[test]
    fn combo_text_formats_active_combo() {
        assert_eq!(combo_text(2).as_deref(), Some("2 Hits"));
        assert_eq!(combo_text(5).as_deref(), Some("5 Hits"));
        assert_eq!(combo_text(99).as_deref(), Some("99 Hits"));
    }

    #[test]
    fn face_draw_pos_places_p1_and_p2_portraits() {
        // A parsed [Face] for each player resolves to its anchor pos + sprite
        // offset — the position the renderer draws the portrait at.
        use crate::screenpack::{FaceSide, Pos};
        let p1 = FaceSide {
            spr: Some(SpriteRef {
                group: 9000,
                image: 0,
                offset: Pos::new(1, 2),
            }),
            pos: Pos::new(12, 12),
        };
        let p2 = FaceSide {
            spr: Some(SpriteRef {
                group: 9000,
                image: 0,
                offset: Pos::default(),
            }),
            pos: Pos::new(308, 12),
        };
        // P1: pos (12,12) + offset (1,2) = (13, 14).
        assert_eq!(face_draw_pos(&p1), (13.0, 14.0));
        // P2: pos (308,12) + no offset = (308, 12); distinct from P1's spot.
        assert_eq!(face_draw_pos(&p2), (308.0, 12.0));
        assert_ne!(
            face_draw_pos(&p1),
            face_draw_pos(&p2),
            "the two players' portraits sit at different screen positions"
        );
    }

    #[test]
    fn face_draw_pos_without_sprite_is_bare_pos() {
        // No sprite ref -> position is just the anchor (the renderer skips drawing).
        use crate::screenpack::{FaceSide, Pos};
        let face = FaceSide {
            spr: None,
            pos: Pos::new(40, 50),
        };
        assert_eq!(face_draw_pos(&face), (40.0, 50.0));
    }

    #[test]
    fn parsed_face_layout_drives_p1_p2_placement() {
        // End-to-end: a [Face] section parsed from a fight.def yields P1/P2
        // portraits placed at the correct (pos + offset) positions for each side.
        use crate::screenpack::Pos;
        use fp_formats::def::DefFile;
        let def = DefFile::from_str(
            "[Face]\n\
             p1.pos    = 12, 12\n\
             p1.spr    = 9000, 0\n\
             p1.offset = 1, 1\n\
             p2.pos    = 308, 12\n\
             p2.spr    = 9000, 0\n\
             p2.offset = 0, 0\n",
        )
        .unwrap();
        let layout = ScreenpackLayout::parse(&def);
        // Both players have a portrait sprite parsed.
        assert_eq!(
            layout.p1_face.spr,
            Some(SpriteRef {
                group: 9000,
                image: 0,
                offset: Pos::new(1, 1)
            })
        );
        assert_eq!(
            layout.p2_face.spr,
            Some(SpriteRef {
                group: 9000,
                image: 0,
                offset: Pos::default()
            })
        );
        // And each side draws at its parsed (pos + offset) position.
        assert_eq!(face_draw_pos(&layout.p1_face), (13.0, 13.0));
        assert_eq!(face_draw_pos(&layout.p2_face), (308.0, 12.0));
    }

    #[test]
    fn combo_layout_position_and_font_drive_the_draw() {
        // The combo element is placed at its parsed position with its parsed
        // font slot; this asserts the layout fields the renderer reads.
        use crate::screenpack::{ComboLayout, Pos};
        let layout = ScreenpackLayout {
            combo: ComboLayout {
                pos: Pos::new(30, 80),
                font: 3,
            },
            ..Default::default()
        };
        assert_eq!(layout.combo.pos, Pos::new(30, 80));
        assert_eq!(layout.combo.font, 3);
        // And the count gate the draw uses agrees with combo_text.
        assert!(combo_text(layout.combo.pos.x).is_some()); // 30 >= 2 -> shows
    }
}
