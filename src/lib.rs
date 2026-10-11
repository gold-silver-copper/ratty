//! Ratty terminal runtime and rendering library.
//!
//! This crate provides the terminal runtime, scene integration, protocol handling and widget
//! plumbing for Ratty.

#![recursion_limit = "256"]
#![warn(missing_docs)]
#![warn(clippy::unwrap_used)]

pub mod camera;
pub mod cli;
mod colors;
pub mod config;
pub mod control;
pub mod inline;
pub mod keyboard;
pub mod kitty;
pub mod model;
pub mod mouse;
mod output;
pub mod paths;
pub mod plugin;
pub mod present;
mod pty;
mod pty_input;
mod pty_process;
pub mod rendering;
pub mod rgp;
pub mod runtime;
pub mod scene;
pub mod screen;
pub mod systems;
pub mod terminal;
