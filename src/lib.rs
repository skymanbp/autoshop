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
pub mod openai_models;
pub mod pipeline;
pub mod recipe;
pub mod render;
pub mod retouch;
pub mod serve;
pub mod style;
pub mod xmp;

/// Stop a spawned **console** child (the `claude` CLI, the python denoise sidecar)
/// from popping its own console window when the parent is the windowed desktop GUI
/// (built with `windows_subsystem = "windows"`, so it has no console of its own).
///
/// Sets Windows' `CREATE_NO_WINDOW` flag. It does NOT detach stdio: when launched
/// from the CLI (which has a console) the child still inherits those handles, so
/// `Stdio::inherit()` output keeps showing — it only suppresses a *new* window.
/// A no-op on non-Windows targets.
pub fn hide_child_console(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}
