//! Autoshop engine library — the shared core behind both front-ends.
//!
//! The AI advisor looks at a RAW preview + metadata and emits a
//! [`recipe::EditRecipe`]; the deterministic [`render`] engine applies it.
//! Both the CLI (`bin/autoshop`, i.e. `main.rs`) and the native GUI
//! (`bin/gui.rs`) link this library and call the engine directly — the GUI has
//! NO HTTP server; it invokes `render`/`pipeline`/`decode` in-process.
//!
//! See `docs/ARCHITECTURE.md` for the full design.

pub mod advisor;
pub mod config;
pub mod decode;
pub mod denoise;
pub mod eval;
pub mod generative;
pub mod pipeline;
pub mod recipe;
pub mod render;
pub mod retouch;
pub mod serve;
pub mod style;
pub mod xmp;
