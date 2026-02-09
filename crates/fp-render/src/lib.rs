//! # fp-render
//!
//! WebGPU-based rendering pipeline for the Fighters Paradise engine.
//!
//! Implements palette-indexed sprite rendering using `wgpu`. MUGEN sprites
//! are 256-color indexed images; the GPU shader performs palette lookup from
//! a 1D texture, enabling cheap palette swaps (costume colors) without
//! re-uploading sprite data.

#![warn(missing_docs)]

mod animation;
pub mod atlas;
mod params;
mod renderer;
mod texture;
mod vertex;

pub use animation::AnimController;
pub use atlas::{AtlasRegion, TextureAtlas};
pub use params::{BlendMode, SpriteDrawParams};
pub use renderer::{RenderFrame, Renderer};
pub use texture::{PaletteTexture, SpriteTexture};
pub use vertex::SpriteVertex;
