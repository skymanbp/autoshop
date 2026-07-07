//! AI segmentation bridge — Rust side of the sidecar (`python/segment.py`).
//!
//! Same shell-out pattern as [`crate::denoise`] (SCUNet): a local Python
//! process does the model inference and writes an 8-bit grayscale mask PNG
//! (white = selected, soft edges = the model's own alpha), which the app then
//! attaches to the recipe as a [`crate::recipe::MaskGeometry::Bitmap`] local
//! adjustment. The AI picks *where*; every actual edit stays a deterministic
//! recipe slider. Models auto-download to the user's home caches on first run
//! (`~/.u2net`, `~/.cache/huggingface`) — no weights in the repo.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::config::Config;

/// Everything one segmentation run needs; built from [`Config`] like
/// [`crate::denoise::DenoiseOpts`].
pub struct SegmentOpts {
    pub python_bin: String,
    pub script: PathBuf,
    /// `"subject"` (U²-Net salient object) or `"sky"` (SegFormer ADE20K).
    pub target: String,
}

impl SegmentOpts {
    pub fn from_config(cfg: &Config, target: &str) -> Self {
        SegmentOpts {
            python_bin: cfg.python_bin.clone(),
            script: PathBuf::from(&cfg.segment_script),
            target: target.to_string(),
        }
    }
}

/// Run the sidecar: `input` (any image file) → `output` (8-bit grayscale PNG).
/// The mask is in the INPUT's frame — feed it the original-frame preview so it
/// lands in the same space recipe masks live in.
pub fn segment_file(opts: &SegmentOpts, input: &Path, output: &Path) -> Result<()> {
    if !opts.script.exists() {
        bail!(
            "segmentation sidecar not found at {} — run from the project dir or set \
             AUTOSHOP_SEGMENT_SCRIPT.",
            opts.script.display()
        );
    }
    crate::pipeline::ensure_parent(output)?;
    let mut cmd = Command::new(&opts.python_bin);
    cmd.arg(&opts.script)
        .arg("--input")
        .arg(input)
        .arg("--output")
        .arg(output)
        .arg("--target")
        .arg(&opts.target)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    // Don't flash a console window when the windowed GUI spawns the sidecar.
    crate::hide_child_console(&mut cmd);
    let status = cmd.status().with_context(|| {
        format!(
            "launch segmentation sidecar ({} {}) — is Python on PATH / AUTOSHOP_PYTHON set?",
            opts.python_bin,
            opts.script.display()
        )
    })?;
    if !status.success() {
        bail!(
            "segmentation sidecar exited with {} (see its log above — a missing \
             dependency prints the exact pip install line)",
            status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into())
        );
    }
    if !output.exists() {
        bail!("sidecar reported success but wrote no mask at {}", output.display());
    }
    Ok(())
}
