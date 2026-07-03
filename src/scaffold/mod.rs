//! Scaffold generator for `pacto-bot-admin`.
//!
//! Generates complete, operationally-ready bot handler projects by resolving a
//! compatible contract/SDK/template triple, rendering a `cargo-generate`
//! template, and merging the output into the project while respecting protected
//! files and user edits.

pub mod cache;
pub mod diff;
pub mod generate;
pub mod lock;
pub mod merge;
pub mod render;
pub mod resolve;
pub mod safety;
pub mod template;
pub mod update;
