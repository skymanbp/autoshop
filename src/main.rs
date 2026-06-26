//! Autoshop — AI-assisted automatic development of RAW photographs.
//!
//! Architecture in one line: the AI advisor looks at a RAW preview + metadata
//! and emits an [`recipe::EditRecipe`]; a deterministic render engine applies
//! that recipe. See `docs/ARCHITECTURE.md` for the full design.
//!
//! Milestone status: M0 (data model + CLI) done. M1 decode half (`decode`) is
//! live — it really decodes a RAW, extracts the embedded preview + EXIF +
//! histogram. The advise → render pipeline (`analyze`/`apply`/`auto`) is still
//! stubbed and bails explicitly so nothing lies about being done.

mod decode;
mod recipe;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use image::GenericImageView;

use recipe::EditRecipe;

#[derive(Parser)]
#[command(
    name = "autoshop",
    version,
    about = "AI-assisted automatic development of RAW photographs",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Decode a RAW: extract its embedded preview, EXIF, and histogram.
    /// Reads the RAW only; writes the preview to ./out (never beside the source).
    Decode {
        /// Path to the RAW file (e.g. .ARW, .DNG).
        raw: PathBuf,
        /// Preview output path (default: ./out/<stem>.preview.jpg).
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Decode a RAW file, extract a preview + metadata, ask the AI advisor,
    /// and write the resulting EditRecipe as JSON (no pixels rendered).
    Analyze {
        /// Path to the RAW file.
        raw: PathBuf,
        /// Where to write the recipe JSON (default: <raw>.recipe.json).
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Apply an existing EditRecipe to a RAW file and render an output image.
    Apply {
        /// Path to the RAW file.
        raw: PathBuf,
        /// Path to the recipe JSON produced by `analyze`.
        recipe: PathBuf,
        /// Output image path (extension selects format: .tif / .jpg / .png).
        #[arg(short, long)]
        out: PathBuf,
    },
    /// End-to-end: analyze then apply in one shot.
    Auto {
        /// Path to the RAW file.
        raw: PathBuf,
        /// Output image path.
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Print the default EditRecipe as JSON — the exact shape the AI must emit.
    RecipeSchema,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Decode { raw, out } => decode_cmd(&raw, out),
        Command::RecipeSchema => {
            // Genuinely useful today: this is the schema we hand the AI as the
            // required output format.
            let template = EditRecipe::default();
            println!("{}", serde_json::to_string_pretty(&template)?);
            Ok(())
        }
        Command::Analyze { raw, out } => {
            let _ = (raw, out);
            bail!("`analyze` is not yet implemented — see docs/ARCHITECTURE.md, Milestone M1 (advise half)");
        }
        Command::Apply { raw, recipe, out } => {
            let _ = (raw, recipe, out);
            bail!("`apply` is not yet implemented — see docs/ARCHITECTURE.md, Milestone M2");
        }
        Command::Auto { raw, out } => {
            let _ = (raw, out);
            bail!("`auto` is not yet implemented — see docs/ARCHITECTURE.md, Milestone M3");
        }
    }
}

fn decode_cmd(raw: &Path, out: Option<PathBuf>) -> Result<()> {
    let decoded = decode::decode_raw(raw)?;

    // Default output goes to ./out, NEVER next to the source RAW (the library is
    // read-only).
    let out = out.unwrap_or_else(|| {
        let stem = raw.file_stem().and_then(|s| s.to_str()).unwrap_or("preview");
        PathBuf::from("out").join(format!("{stem}.preview.jpg"))
    });
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create output dir {}", parent.display()))?;
        }
    }
    let preview = decoded.preview_resized(1536);
    preview
        .save(&out)
        .with_context(|| format!("save preview {}", out.display()))?;
    let (pw, ph) = preview.dimensions();

    let m = &decoded.meta;
    let dash = || "-".to_string();
    println!("RAW: {}", raw.display());
    println!("  camera : {} {}", m.make, m.model);
    println!("  lens   : {}", m.lens.as_deref().unwrap_or("-"));
    println!(
        "  expo   : ISO {}  {}  f/{}  {}mm  EV{:+.1}",
        m.iso.map(|v| v.to_string()).unwrap_or_else(dash),
        m.shutter.as_deref().unwrap_or("-"),
        m.aperture.map(|v| format!("{v:.1}")).unwrap_or_else(dash),
        m.focal_length_mm.map(|v| format!("{v:.0}")).unwrap_or_else(dash),
        m.exposure_bias_ev.unwrap_or(0.0),
    );
    println!("  sensor : {} x {}", m.width, m.height);
    println!(
        "  wb     : [{:.3}, {:.3}, {:.3}, {:.3}]",
        m.as_shot_wb_coeffs[0], m.as_shot_wb_coeffs[1], m.as_shot_wb_coeffs[2], m.as_shot_wb_coeffs[3],
    );
    println!("  date   : {}", m.date_time.as_deref().unwrap_or("-"));

    let h = &decoded.histogram;
    println!(
        "  clip   : black {:.2}%   white {:.2}%   ({} px sampled)",
        h.clip_black_pct, h.clip_white_pct, h.sample_pixels,
    );
    println!("  luma   : {}", sparkline(&h.luma));
    println!(
        "  xmp    : {}",
        if decoded.embedded_xmp.is_some() { "embedded packet present" } else { "none" },
    );
    println!("  preview -> {} ({} x {})", out.display(), pw, ph);
    Ok(())
}

/// Render a 256-bin histogram as a compact Unicode block sparkline.
fn sparkline(bins: &[u32]) -> String {
    const BLOCKS: [char; 8] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇'];
    let groups = 48usize;
    let per = bins.len().div_ceil(groups);
    let sums: Vec<u32> = bins.chunks(per).map(|c| c.iter().copied().sum()).collect();
    let max = sums.iter().copied().max().unwrap_or(1).max(1);
    sums.iter()
        .map(|&v| {
            let idx = ((v as f64 / max as f64) * (BLOCKS.len() - 1) as f64).round() as usize;
            BLOCKS[idx.min(BLOCKS.len() - 1)]
        })
        .collect()
}
