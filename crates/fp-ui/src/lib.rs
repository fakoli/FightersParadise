//! # fp-ui
//!
//! UI and motif system for the Fighters Paradise engine: the in-fight HUD
//! rendered from a MUGEN `fight.def`/`fight.sff` **screenpack** (life bars, power
//! bars, fighter names, round announcer, timer), instead of hand-rolled quads.
//!
//! Two layers:
//!
//! - [`screenpack`] — a pure parser turning a `fight.def` ([`fp_formats::def::DefFile`])
//!   into a typed [`ScreenpackLayout`] (which sprites/fonts to load and where every
//!   HUD element sits). No GPU, fully unit-tested, never panics on bad content.
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

#![warn(missing_docs)]

pub mod renderer;
pub mod screenpack;
pub mod select_def;
pub mod system_def;

pub use renderer::{bar_fill_uv, clamp_fraction, combo_text, MatchHudState, ScreenpackHud};
pub use screenpack::{
    ComboLayout, FaceSide, LifebarSide, NameSide, Pos, PowerbarSide, RoundLayout, ScreenpackLayout,
    SpriteRef, TextElem, TimeLayout,
};
pub use select_def::{RosterEntry, SelectDef, SelectSlot};
pub use system_def::{
    CursorSide, MenuItem, MenuItemKind, SelectInfo, SystemDef, TitleInfo, VsScreen, VsSide,
};
