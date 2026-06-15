//! # fp-storyboard
//!
//! Storyboard and cutscene playback system for the Fighters Paradise engine.
//! Handles MUGEN storyboard definitions for character intros, endings, and
//! other scripted sequences.
//!
//! This crate provides storyboard **parsing** (a typed scene model via
//! [`Storyboard::load`]) plus a **playback driver** ([`StoryboardPlayer`]) that
//! walks that model one 60 Hz tick at a time and exposes the set of sprites to
//! draw ([`StoryboardDraw`]) and when the storyboard is finished. The player is
//! pure and rendering-agnostic; a consumer (e.g. `fp-app`) maps each
//! [`StoryboardDraw`] onto the screen. The parser is built on top of the
//! `fp-formats` DEF and AIR parsers.
//!
//! ```no_run
//! use std::path::Path;
//! use fp_storyboard::{Storyboard, StoryboardPlayer};
//!
//! let sb = Storyboard::load(Path::new("intro.def"))?;
//! println!("{} scenes, sprite = {}", sb.scenes.len(), sb.sprite_path);
//!
//! let mut player = StoryboardPlayer::new(sb);
//! while !player.is_done() {
//!     for draw in player.draw_list() {
//!         // map `draw.pos` onto the screen and blit `draw.sprite`...
//!     }
//!     player.tick();
//! }
//! # Ok::<(), fp_core::FpError>(())
//! ```

#![warn(missing_docs)]

mod player;
mod storyboard;

pub use player::{StoryboardDraw, StoryboardPlayer};
pub use storyboard::{BgGroup, BgLayer, Scene, SceneLayer, Storyboard};
