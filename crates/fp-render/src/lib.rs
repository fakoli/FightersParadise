//! # fp-render
//!
//! WebGPU-based rendering pipeline for the Fighters Paradise engine.
//!
//! Implements palette-indexed sprite rendering using `wgpu`. MUGEN sprites
//! are 256-color indexed images; the GPU shader performs palette lookup from
//! a 1D texture, enabling cheap palette swaps (costume colors) without
//! re-uploading sprite data.

#![warn(missing_docs)]

mod afterimage;
mod animation;
pub mod atlas;
mod params;
mod renderer;
mod text;
mod texture;
mod vertex;

pub use afterimage::{ghost_alpha, ghost_palfx, AfterImageModulation, TrailTrans};
pub use animation::AnimController;
pub use atlas::{AtlasRegion, TextureAtlas};
pub use params::{apply_palfx, BlendMode, PalFx, SpriteDrawParams};
pub use renderer::{DebugBox, RenderFrame, Renderer};
pub use text::{layout_text, GlyphFont, PlacedGlyph, TextDrawParams};
pub use texture::{ImageTexture, PaletteTexture, SpriteTexture};
pub use vertex::{DebugVertex, SpriteVertex};
