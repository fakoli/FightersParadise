//! # fp-storyboard
//!
//! Storyboard and cutscene playback system for the Fighters Paradise engine.
//! Handles MUGEN storyboard definitions for character intros, endings, and
//! other scripted sequences.
//!
//! This crate currently provides **parsing and a typed scene model only**
//! (no rendering yet). The entry point is [`Storyboard::load`], which reads a
//! storyboard `.def` file into a [`Storyboard`] value built on top of the
//! `fp-formats` DEF and AIR parsers.
//!
//! ```no_run
//! use std::path::Path;
//! use fp_storyboard::Storyboard;
//!
//! let sb = Storyboard::load(Path::new("intro.def"))?;
//! println!("{} scenes, sprite = {}", sb.scenes.len(), sb.sprite_path);
//! # Ok::<(), fp_core::FpError>(())
//! ```

#![warn(missing_docs)]

mod storyboard;

pub use storyboard::{BgGroup, BgLayer, Scene, SceneLayer, Storyboard};
