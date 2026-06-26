//! Autoshop — AI-assisted automatic development of RAW photographs.
//!
//! Architecture in one line: the AI advisor looks at a RAW preview + metadata
//! and emits an [`recipe::EditRecipe`]; a deterministic render engine applies
//! that recipe. See `docs/ARCHITECTURE.md` for the full design. Shared advise +
//! output logic lives in [`pipeline`]; the CLI here and the web UI ([`serve`])
//! both call it.

mod advisor;
mod config;
mod decode;
mod eval;
mod pipeline;
mod recipe;
mod render;
mod serve;
mod xmp;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use image::GenericImageView;

use advisor::Verdict;
use config::Config;
use pipeline::{default_out, ensure_parent, find_raws, produce_recipe, stem, write_recipe, write_xmp, xmp_target};
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
    /// Decode a RAW, ask the AI advisor to propose an edit, have Claude verify
    /// it, and write the recipe JSON + a Lightroom .xmp sidecar (no render).
    Analyze {
        /// Path to the RAW file.
        raw: PathBuf,
        /// Where to write the recipe JSON (default: ./out/<stem>.recipe.json).
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Also write the .xmp next to the source RAW. WRITES INTO THE LIBRARY;
        /// default writes only to ./out.
        #[arg(long)]
        beside: bool,
    },
    /// Render an existing EditRecipe onto a RAW and save the developed image.
    Apply {
        /// Path to the RAW file.
        raw: PathBuf,
        /// Path to the recipe JSON produced by `analyze`.
        recipe: PathBuf,
        /// Output image path (extension selects format: .jpg / .png / .tif).
        #[arg(short, long)]
        out: PathBuf,
    },
    /// End-to-end for one RAW: analyze (recipe + xmp) then render an image.
    Auto {
        /// Path to the RAW file.
        raw: PathBuf,
        /// Output image path (default: ./out/<stem>.developed.jpg).
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Batch-process every RAW under a folder (resumes by skipping done .xmp).
    Batch {
        /// Folder to scan recursively for .ARW files.
        dir: PathBuf,
        /// Write each .xmp next to its RAW (into the library) instead of ./out.
        #[arg(long)]
        beside: bool,
        /// Also render a developed JPEG per RAW (slower).
        #[arg(long)]
        render: bool,
        /// Max RAWs to process this run (cost guard; raise to do more).
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Evaluate AI edits against your own: for RAWs that have a sibling .xmp
    /// (your Lightroom/ACR edit), run the AI and report per-field error + bias.
    Eval {
        /// Folder to scan recursively for RAW + .xmp pairs.
        dir: PathBuf,
        /// Max photos to evaluate (cost guard; each one runs the AI).
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Start the local web UI (open the printed URL in a browser).
    Serve {
        /// Photo library folder to browse (scanned recursively for .ARW).
        dir: PathBuf,
        /// Port to listen on.
        #[arg(short, long, default_value_t = 8080)]
        port: u16,
    },
    /// Print the default EditRecipe as JSON — the exact shape the AI must emit.
    RecipeSchema,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Decode { raw, out } => decode_cmd(&raw, out),
        Command::Analyze { raw, out, beside } => analyze_cmd(&raw, out, beside),
        Command::Apply { raw, recipe, out } => apply_cmd(&raw, &recipe, &out),
        Command::Auto { raw, out } => auto_cmd(&raw, out),
        Command::Batch { dir, beside, render, limit } => batch_cmd(&dir, beside, render, limit),
        Command::Eval { dir, limit } => eval::run(&dir, limit),
        Command::Serve { dir, port } => serve::serve(&dir, port),
        Command::RecipeSchema => {
            let template = EditRecipe::default();
            println!("{}", serde_json::to_string_pretty(&template)?);
            Ok(())
        }
    }
}

fn decode_cmd(raw: &Path, out: Option<PathBuf>) -> Result<()> {
    let decoded = decode::decode_raw(raw)?;

    let out = out.unwrap_or_else(|| default_out(raw, "preview", "jpg"));
    ensure_parent(&out)?;
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

fn analyze_cmd(raw: &Path, out: Option<PathBuf>, beside: bool) -> Result<()> {
    let cfg = Config::load();
    let (recipe, verdict) = produce_recipe(raw, &cfg, true)?;
    let recipe_path = write_recipe(raw, &recipe, out)?;
    let xmp_path = write_xmp(raw, &recipe, beside)?;

    println!("\n--- proposed recipe ---");
    println!("{}", serde_json::to_string_pretty(&recipe)?);
    println!("\n--- verdict: {:?} ---", verdict.decision);
    for reason in &verdict.reasons {
        println!("  - {reason}");
    }
    println!("\nrecipe -> {}", recipe_path.display());
    println!("xmp    -> {}", xmp_path.display());
    if !beside {
        let s = stem(raw);
        println!("  (copy {s}.xmp next to {s}.ARW, or rerun with --beside, to open in Lightroom)");
    }
    Ok(())
}

fn apply_cmd(raw: &Path, recipe_path: &Path, out: &Path) -> Result<()> {
    let text = std::fs::read_to_string(recipe_path)
        .with_context(|| format!("read recipe {}", recipe_path.display()))?;
    let recipe: EditRecipe =
        serde_json::from_str(&text).with_context(|| format!("parse recipe {}", recipe_path.display()))?;
    ensure_parent(out)?;
    println!("rendering {} with {} ...", raw.display(), recipe_path.display());
    let (w, h) = render::render_to_file(raw, &recipe, out)?;
    println!("render -> {} ({} x {})", out.display(), w, h);
    Ok(())
}

fn auto_cmd(raw: &Path, out: Option<PathBuf>) -> Result<()> {
    let cfg = Config::load();
    let (recipe, verdict) = produce_recipe(raw, &cfg, true)?;
    write_recipe(raw, &recipe, None)?;
    let xmp_path = write_xmp(raw, &recipe, false)?;

    let out = out.unwrap_or_else(|| default_out(raw, "developed", "jpg"));
    ensure_parent(&out)?;
    println!("verdict: {:?}; rendering full-resolution ...", verdict.decision);
    let (w, h) = render::render_to_file(raw, &recipe, &out)?;
    println!("render -> {} ({} x {})", out.display(), w, h);
    println!("xmp    -> {}", xmp_path.display());
    Ok(())
}

fn batch_cmd(dir: &Path, beside: bool, render: bool, limit: usize) -> Result<()> {
    let cfg = Config::load();
    let raws = find_raws(dir)?;
    println!("found {} RAW(s) under {}", raws.len(), dir.display());

    let pending: Vec<&PathBuf> = raws.iter().filter(|r| !xmp_target(r, beside).exists()).collect();
    let todo = pending.len();
    let n = todo.min(limit);
    println!("{todo} pending; processing {n} this run (--limit {limit}).");
    if todo > n {
        println!("  {} more remain — raise --limit to process them.", todo - n);
    }

    let (mut ok, mut fail) = (0usize, 0usize);
    for (i, raw) in pending.iter().take(n).enumerate() {
        print!("[{}/{}] {} ... ", i + 1, n, stem(raw));
        use std::io::Write;
        let _ = std::io::stdout().flush();
        match process_one(raw, &cfg, beside, render) {
            Ok(v) => {
                println!("{:?}", v.decision);
                ok += 1;
            }
            Err(e) => {
                println!("FAILED: {e}");
                fail += 1;
            }
        }
    }
    println!("\nbatch done: {ok} ok, {fail} failed, {} still pending.", todo.saturating_sub(n));
    Ok(())
}

fn process_one(raw: &Path, cfg: &Config, beside: bool, render_jpeg: bool) -> Result<Verdict> {
    let (recipe, verdict) = produce_recipe(raw, cfg, false)?;
    write_recipe(raw, &recipe, None)?;
    write_xmp(raw, &recipe, beside)?;
    if render_jpeg {
        let out = default_out(raw, "developed", "jpg");
        ensure_parent(&out)?;
        render::render_to_file(raw, &recipe, &out)?;
    }
    Ok(verdict)
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
