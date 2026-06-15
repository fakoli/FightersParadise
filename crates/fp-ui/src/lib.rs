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
//!
//! Character-select and title screens are not yet implemented.

#![warn(missing_docs)]

pub mod renderer;
pub mod screenpack;

pub use renderer::{bar_fill_uv, clamp_fraction, MatchHudState, ScreenpackHud};
pub use screenpack::{
    ComboLayout, FaceSide, LifebarSide, NameSide, Pos, PowerbarSide, RoundLayout, ScreenpackLayout,
    SpriteRef, TextElem, TimeLayout,
};
