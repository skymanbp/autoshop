//! Autoshop — AI-assisted automatic development of RAW photographs.
//!
//! Architecture in one line: the AI advisor looks at a RAW preview + metadata
//! and emits an [`recipe::EditRecipe`]; a deterministic render engine applies
//! that recipe. See `docs/ARCHITECTURE.md` for the full design.
//!
//! Milestone status: M0 (data model + CLI) done. M1 is live: `decode` really
//! decodes a RAW (preview + EXIF + histogram); `analyze` runs the advise chain
//! (propose → Claude verify) — GPT vision is used when `OPENAI_API_KEY` is set,
//! otherwise a heuristic baseline proposer stands in. `apply`/`auto` (render)
//! are still stubbed and bail explicitly so nothing lies about being done.

mod advisor;
mod config;
mod decode;
mod recipe;
mod xmp;

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use image::GenericImageView;

use advisor::{Advisor, ClaudeProvider, Decision, HeuristicProposer, OpenAiProvider, Preview};
use config::Config;
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
    /// it, and write the resulting EditRecipe as JSON (no pixels rendered).
    Analyze {
        /// Path to the RAW file.
        raw: PathBuf,
        /// Where to write the recipe JSON (default: ./out/<stem>.recipe.json).
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Also write the .xmp sidecar next to the source RAW so Lightroom picks
        /// it up directly. WRITES INTO THE LIBRARY; default writes only to ./out.
        #[arg(long)]
        beside: bool,
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
        Command::Analyze { raw, out, beside } => analyze_cmd(&raw, out, beside),
        Command::RecipeSchema => {
            // Genuinely useful today: this is the schema we hand the AI as the
            // required output format.
            let template = EditRecipe::default();
            println!("{}", serde_json::to_string_pretty(&template)?);
            Ok(())
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
    let decoded = decode::decode_raw(raw)?;

    // Encode the (downscaled) preview to JPEG bytes for the vision advisor.
    let preview_img = decoded.preview_resized(1568);
    let mut jpeg = Vec::new();
    preview_img
        .write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
        .context("encode preview JPEG for advisor")?;
    let preview = Preview { jpeg };

    // Proposer: GPT vision when a key is set, else the heuristic baseline. If
    // the GPT call fails (e.g. no billing quota, network), warn loudly and fall
    // back to the heuristic so the tool still produces a recipe rather than
    // dying — disclosure, not silent masking.
    let openai = OpenAiProvider::new(&cfg);
    let heuristic = HeuristicProposer;
    let (mut recipe, can_revise) = if cfg.openai_api_key.is_some() {
        println!("proposer : OpenAI ({})", cfg.openai_model);
        match openai.propose(&preview, &decoded.meta, &decoded.histogram, None) {
            Ok(r) => (r, true),
            Err(e) => {
                eprintln!("⚠ GPT proposer failed ({e})\n  → falling back to the heuristic baseline.");
                (heuristic.propose(&preview, &decoded.meta, &decoded.histogram, None)?, false)
            }
        }
    } else {
        println!("proposer : heuristic baseline (set OPENAI_API_KEY to use GPT vision)");
        (heuristic.propose(&preview, &decoded.meta, &decoded.histogram, None)?, false)
    };

    // Verify with Claude (live, free via Claude Code OAuth).
    let claude = ClaudeProvider::new(&cfg);
    println!("verifier : Claude ({})", cfg.claude_model);
    let mut verdict = claude.verify(&recipe, &decoded.meta, &decoded.histogram)?;

    // One revision round — only if GPT actually produced the recipe (the
    // heuristic ignores hints, so revising it would just repeat the same recipe).
    if verdict.decision != Decision::Accept && can_revise {
        if let Some(hint) = verdict.revised_hint.clone() {
            println!("verdict was {:?} → one revision round (hint: {hint})", verdict.decision);
            recipe = openai.propose(&preview, &decoded.meta, &decoded.histogram, Some(&hint))?;
            verdict = claude.verify(&recipe, &decoded.meta, &decoded.histogram)?;
        }
    }

    // Persist the recipe JSON (to ./out, never beside the read-only source).
    let out = out.unwrap_or_else(|| default_out(raw, "recipe", "json"));
    ensure_parent(&out)?;
    std::fs::write(&out, serde_json::to_string_pretty(&recipe)?)
        .with_context(|| format!("write recipe {}", out.display()))?;

    // Emit the Lightroom/ACR XMP sidecar — the primary deliverable. By default
    // it goes to ./out; `--beside` writes it next to the RAW (into the library).
    let stem = raw.file_stem().and_then(|s| s.to_str()).unwrap_or("recipe");
    let xmp_out = if beside {
        raw.with_extension("xmp")
    } else {
        PathBuf::from("out").join(format!("{stem}.xmp"))
    };
    ensure_parent(&xmp_out)?;
    std::fs::write(&xmp_out, xmp::recipe_to_xmp(&recipe))
        .with_context(|| format!("write xmp {}", xmp_out.display()))?;

    println!("\n--- proposed recipe ---");
    println!("{}", serde_json::to_string_pretty(&recipe)?);
    println!("\n--- verdict: {:?} ---", verdict.decision);
    for reason in &verdict.reasons {
        println!("  - {reason}");
    }
    println!("\nrecipe -> {}", out.display());
    println!("xmp    -> {}", xmp_out.display());
    if !beside {
        println!("  (copy {stem}.xmp next to {stem}.ARW, or rerun with --beside, to open in Lightroom)");
    }
    Ok(())
}

/// `./out/<stem>.<kind>.<ext>` — outputs never go beside the source RAW.
fn default_out(raw: &Path, kind: &str, ext: &str) -> PathBuf {
    let stem = raw.file_stem().and_then(|s| s.to_str()).unwrap_or(kind);
    PathBuf::from("out").join(format!("{stem}.{kind}.{ext}"))
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create output dir {}", parent.display()))?;
        }
    }
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
