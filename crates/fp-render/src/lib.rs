//! # fp-render
//!
//! WebGPU-based rendering pipeline for the Fighters Paradise engine.
//!
//! Implements palette-indexed sprite rendering using `wgpu`. MUGEN sprites
//! are 256-color indexed images; the GPU shader performs palette lookup from
//! a 1D texture, enabling cheap palette swaps (costume colors) without
//! re-uploading sprite data.

#![warn(missing_docs)]

mod params;
mod renderer;
mod texture;
mod vertex;

pub use params::SpriteDrawParams;
pub use renderer::{RenderFrame, Renderer};
pub use texture::{PaletteTexture, SpriteTexture};
pub use vertex::SpriteVertex;
