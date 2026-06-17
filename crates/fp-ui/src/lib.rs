//! # fp-ui
//!
//! UI and motif system for the Fighters Paradise engine: the in-fight HUD
//! rendered from a MUGEN `fight.def`/`fight.sff` **screenpack** (life bars, power
//! bars, fighter portraits, names, round announcer, timer), instead of
//! hand-rolled quads.
//!
//! Two layers:
//!
//! - [`screenpack`] — a pure parser turning a `fight.def` ([`fp_formats::def::DefFile`])
//!   into a typed [`ScreenpackLayout`] (which sprites/fonts to load and where every
//!   HUD element sits). No GPU, fully unit-tested, never panics on bad content.
//! - [`hud_config`] — [`HudConfig`], the player-facing HUD customization overrides
//!   (T046): bar colors, a global offset/scale, and per-element visibility, layered
//!   over the screenpack-authored layout. The [default](HudConfig::default) is a
//!   guaranteed no-op (the HUD renders unchanged); the in-game customization screen
//!   and any config loader mutate it. Pure, no GPU.
//! - [`renderer`] — [`ScreenpackHud`], the GPU-resident HUD: it uploads the
//!   layout's `fight.sff` sprites + fonts once, then each frame draws the bars and
//!   text via `fp-render`'s existing `draw_sprite`/`draw_text`, clipping each bar's
//!   front fill to a live life/power fraction. The fill geometry ([`bar_fill_uv`])
//!   is pure and unit-tested.
//! - [`system_def`] — a pure parser turning a motif `system.def` into a typed
//!   [`SystemDef`] (the title-menu items, the character-select grid geometry, and
//!   the versus-screen placements). Parser + model only; no render integration.
//! - [`select_def`] — a pure parser turning a motif `select.def` into a typed
//!   [`SelectDef`] roster (character slots, extra stages, options).
//! - [`discovery`] — pure directory scanners: a `chars/` directory into a
//!   character roster ([`discover_chars`]) and a `data/` directory into a list of
//!   motif/screenpack sets ([`discover_motifs`]). Filesystem-only, never panics.

#![warn(missing_docs)]

pub mod discovery;
pub mod hud_config;
pub mod renderer;
pub mod resource_state;
pub mod screenpack;
pub mod select_def;
pub mod system_def;

pub use discovery::{discover_chars, discover_motifs, CharEntry, MotifEntry};
pub use hud_config::{BarColor, HudConfig, HudElement};
pub use renderer::{
    bar_fill_uv, bar_tint_palfx, clamp_fraction, combo_text, face_draw_pos, MatchHudState,
    ScreenpackHud,
};
pub use resource_state::{
    low_life_tint, max_power_flash_tint, LOW_LIFE_THRESHOLD, MAX_POWER_THRESHOLD,
    POWER_FLASH_PERIOD,
};
pub use screenpack::{
    ComboLayout, FaceSide, LifebarSide, NameSide, Pos, PowerbarSide, RoundLayout, ScreenpackLayout,
    SpriteRef, TextElem, TimeLayout,
};
pub use select_def::{RosterEntry, SelectDef, SelectSlot};
pub use system_def::{
    CursorSide, MenuItem, MenuItemKind, SelectInfo, SystemDef, TitleInfo, VsScreen, VsSide,
};
