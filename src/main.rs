//! Autoshop — AI-assisted automatic development of RAW photographs.
//!
//! Architecture in one line: the AI advisor looks at a RAW preview + metadata
//! and emits an [`recipe::EditRecipe`]; a deterministic render engine applies
//! that recipe. See `docs/ARCHITECTURE.md` for the full design. Shared advise +
//! output logic lives in [`pipeline`]; the CLI here and the web UI ([`serve`])
//! both call it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use image::GenericImageView;

// The engine modules now live in the `autoshop` library crate (src/lib.rs),
// shared with the native GUI binary (src/bin/gui.rs).
use autoshop::{decode, denoise, eval, fit, generative, pipeline, render, retouch, serve};
use autoshop::advisor::Verdict;
use autoshop::config::Config;
use autoshop::pipeline::{default_out, ensure_parent, find_raws, produce_recipe, stem, write_recipe, write_xmp, xmp_target};
use autoshop::recipe::EditRecipe;
use autoshop::style::StyleIndex;

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
        /// Optional direction for the AI, e.g. "warmer and moodier, lift the
        /// shadows, keep skin natural".
        #[arg(long)]
        guidance: Option<String>,
        /// How strongly to follow your historical edit style, 0..1 (needs a built
        /// `style-index`). Omit to use AUTOSHOP_STYLE_STRENGTH (default 0.3).
        #[arg(long)]
        style: Option<f32>,
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
        /// Output image path (default: ./out/<stem>.developed.tif, 16-bit).
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Optional direction for the AI (e.g. "warmer and moodier").
        #[arg(long)]
        guidance: Option<String>,
        /// How strongly to follow your historical edit style, 0..1 (needs a built
        /// `style-index`). Omit for AUTOSHOP_STYLE_STRENGTH (default 0.3).
        #[arg(long)]
        style: Option<f32>,
        /// Run AI denoise (SCUNet, GPU) before developing — for high-ISO/astro.
        #[arg(long)]
        denoise: bool,
        /// Denoise strength 0..1 (blend with original); default 1.0.
        #[arg(long)]
        denoise_strength: Option<f32>,
        /// SCUNet model: color_real_psnr (default) / color_real_gan / color_15|25|50.
        #[arg(long)]
        denoise_model: Option<String>,
    },
    /// AI-denoise a RAW or an already-baked image (PNG/TIFF/JPEG) into a clean
    /// 16-bit master in ./out. Manual, GPU-accelerated (SCUNet sidecar). Default
    /// off everywhere else — this is the explicit "denoise now" command.
    Denoise {
        /// RAW (.arw/.dng/...) or image (.png/.tif/.jpg) to denoise.
        input: PathBuf,
        /// Output path (default: ./out/<stem>.denoised.tif).
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Strength 0..1 (blend with original); default 1.0.
        #[arg(long)]
        strength: Option<f32>,
        /// SCUNet model tier (see `auto --denoise-model`).
        #[arg(long)]
        model: Option<String>,
    },
    /// Batch-process every RAW under a folder (resumes by skipping done .xmp).
    /// Outputs go to ./out — the photo library stays read-only.
    Batch {
        /// Folder to scan recursively for .ARW files.
        dir: PathBuf,
        /// Also render a 16-bit developed TIFF per RAW (slower, large files).
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
    /// Build the style index from your edited library (RAW+.xmp pairs) → the
    /// advisor then references your edits on similar shots. Run once / on update.
    StyleIndex {
        /// Folder to scan recursively for RAW + .xmp pairs (your edits).
        dir: PathBuf,
    },
    /// EXPERIMENTAL: full-frame generative restyle via OpenAI Images (low-res,
    /// lossy re-render — NOT a master; the XMP/render path is the real workflow).
    Reimagine {
        /// Path to the RAW file.
        raw: PathBuf,
        /// What to do (e.g. "moody cinematic, deepen shadows, warm highlights").
        #[arg(long)]
        prompt: String,
        /// "high" keeps it recognizably the same photo; "low" = free rein.
        #[arg(long, default_value = "high")]
        fidelity: String,
        /// Output quality tier: low | medium | high | auto (higher = more detail,
        /// higher cost). Defaults to AUTOSHOP_IMAGE_QUALITY (config default: high).
        #[arg(long)]
        quality: Option<String>,
        /// Output PNG (default: ./out/<stem>.reimagine.png).
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Reverse-fit a LOOK into an editable recipe: given the SAME shot twice —
    /// the source and a target rendition (e.g. the `reimagine` output, or any
    /// finished reference of this frame) — solve for the EditRecipe that
    /// reproduces the target through the deterministic engine, and write the
    /// recipe JSON + Lightroom XMP. No pixels are copied, so the result applies
    /// at FULL sensor resolution. Deterministic; no API key needed.
    Match {
        /// Source RAW (or baked image) the look should be fitted onto.
        raw: PathBuf,
        /// The look to match — e.g. ./out/<stem>.reimagine.png.
        target: PathBuf,
        /// Also render the fitted recipe at full resolution
        /// (./out/<stem>.matched.tif, 16-bit).
        #[arg(long)]
        render: bool,
        /// Also extract a reusable style PROMPT from the pair via the vision
        /// model (./out/<stem>.style.txt; needs OPENAI_API_KEY).
        #[arg(long)]
        style_prompt: bool,
        /// Recipe JSON output (default: ./out/<stem>.matched.json).
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// EXPERIMENTAL: generative object removal via OpenAI Images. The mask is an
    /// RGBA PNG; transparent pixels mark the region to regenerate.
    Retouch {
        /// Path to the RAW file.
        raw: PathBuf,
        /// RGBA PNG mask (transparent = region to edit).
        #[arg(long)]
        mask: PathBuf,
        /// What to do (e.g. "remove the trash can, fill with pavement").
        #[arg(long)]
        prompt: String,
        /// Output quality tier: low | medium | high | auto (higher = more detail,
        /// higher cost). Defaults to AUTOSHOP_IMAGE_QUALITY (config default: high).
        #[arg(long)]
        quality: Option<String>,
        /// Composite onto the full-sensor develop (e.g. 61 MP) instead of the
        /// embedded preview — the untouched area keeps native resolution. Slow;
        /// the regenerated patch is upscaled. No effect on baked PNG/TIFF sources.
        #[arg(long)]
        full_res: bool,
        /// Output PNG (default: ./out/<stem>.retouch.png).
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// OPTIONAL pixel-retouch mode — AI-driven HEAL (spot / blemish / dust
    /// removal) that edits pixels directly by sampling SURROUNDING REAL pixels.
    /// Retouching, NOT generation: no gpt-image, no invented content. Non-XMP;
    /// writes a pixel master to ./out. Targeting is hybrid (AI auto-detect and/or
    /// a painted mask).
    Heal {
        /// RAW or baked image (.png/.tif/.jpg) to retouch.
        src: PathBuf,
        /// Optional painted RGBA mask (transparent = heal here) to ADD manual targets.
        #[arg(long)]
        mask: Option<PathBuf>,
        /// Skip AI auto-detection (heal only the painted mask).
        #[arg(long)]
        no_auto: bool,
        /// Heal the full-sensor develop (e.g. 61 MP) instead of the embedded
        /// preview. Slow; RAW only.
        #[arg(long)]
        full_res: bool,
        /// Output image (default: ./out/<stem>.heal.png).
        #[arg(short, long)]
        out: Option<PathBuf>,
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
        Command::Analyze { raw, out, guidance, style } => analyze_cmd(&raw, out, guidance, style),
        Command::Apply { raw, recipe, out } => apply_cmd(&raw, &recipe, &out),
        Command::Auto { raw, out, guidance, style, denoise, denoise_strength, denoise_model } => {
            auto_cmd(&raw, out, guidance, style, denoise, denoise_strength, denoise_model)
        }
        Command::Denoise { input, out, strength, model } => denoise_cmd(&input, out, strength, model),
        Command::Batch { dir, render, limit } => batch_cmd(&dir, render, limit),
        Command::Eval { dir, limit } => eval::run(&dir, limit),
        Command::StyleIndex { dir } => style_index_cmd(&dir),
        Command::Reimagine { raw, prompt, fidelity, quality, out } => {
            let cfg = Config::load();
            let out = out.unwrap_or_else(|| default_out(&raw, "reimagine", "png"));
            pipeline::guard_readonly(&out, &raw)?; // never write into the source library
            let q = quality.unwrap_or_else(|| cfg.openai_image_quality.clone());
            generative::reimagine(&cfg, &raw, &prompt, &fidelity, &q, &out)
        }
        Command::Match { raw, target, render, style_prompt, out } => {
            match_cmd(&raw, &target, render, style_prompt, out)
        }
        Command::Retouch { raw, mask, prompt, quality, full_res, out } => {
            let cfg = Config::load();
            let out = out.unwrap_or_else(|| default_out(&raw, "retouch", "png"));
            pipeline::guard_readonly(&out, &raw)?; // never write into the source library
            let q = quality.unwrap_or_else(|| cfg.openai_image_quality.clone());
            generative::retouch(&cfg, &raw, &mask, &prompt, &q, full_res, &out)
        }
        Command::Heal { src, mask, no_auto, full_res, out } => heal_cmd(&src, mask, no_auto, full_res, out),
        Command::Serve { dir, port } => serve::serve(&dir, port),
        Command::RecipeSchema => {
            let template = EditRecipe::default();
            println!("{}", serde_json::to_string_pretty(&template)?);
            Ok(())
        }
    }
}

fn style_index_cmd(dir: &Path) -> Result<()> {
    let index = StyleIndex::build(dir)?;
    let out = PathBuf::from("out/style-index.json");
    index.save(&out)?;
    println!(
        "style index → {} ({} exemplars). The advisor will now reference your edits on similar shots.",
        out.display(),
        index.exemplars.len()
    );
    Ok(())
}

fn decode_cmd(raw: &Path, out: Option<PathBuf>) -> Result<()> {
    let decoded = decode::decode_any(raw)?;

    let out = out.unwrap_or_else(|| default_out(raw, "preview", "jpg"));
    pipeline::guard_readonly(&out, raw)?;
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

fn analyze_cmd(raw: &Path, out: Option<PathBuf>, guidance: Option<String>, style: Option<f32>) -> Result<()> {
    let cfg = Config::load();
    if let Some(o) = &out {
        pipeline::guard_readonly(o, raw)?;
    }
    // CLI analyze always proposes from the original (base = None); the refine /
    // "adjust current edit" path is a web-UI affordance.
    let style = style.unwrap_or(cfg.style_strength);
    let (recipe, verdict) = produce_recipe(raw, &cfg, true, guidance.as_deref(), None, style)?;
    let recipe_path = write_recipe(raw, &recipe, out)?;

    println!("\n--- proposed recipe ---");
    println!("{}", serde_json::to_string_pretty(&recipe)?);
    println!("\n--- verdict: {:?} ---", verdict.decision);
    for reason in &verdict.reasons {
        println!("  - {reason}");
    }
    println!("\nrecipe -> {}", recipe_path.display());
    // XMP only for a RAW; a baked source (PNG/TIFF) gets the recipe JSON only.
    if decode::is_raw(raw) {
        let xmp_path = write_xmp(raw, &recipe)?;
        println!("xmp    -> {}", xmp_path.display());
        let s = stem(raw);
        println!("  (the library is read-only — copy {s}.xmp next to {s}.ARW to open it in Lightroom)");
    } else {
        println!("  (baked source — recipe JSON only; XMP applies to RAW in Lightroom)");
    }
    Ok(())
}

fn apply_cmd(raw: &Path, recipe_path: &Path, out: &Path) -> Result<()> {
    let text = std::fs::read_to_string(recipe_path)
        .with_context(|| format!("read recipe {}", recipe_path.display()))?;
    let recipe: EditRecipe =
        serde_json::from_str(&text).with_context(|| format!("parse recipe {}", recipe_path.display()))?;
    pipeline::guard_readonly(out, raw)?;
    ensure_parent(out)?;
    println!("rendering {} with {} ...", raw.display(), recipe_path.display());
    let (w, h) = render::render_to_file(raw, &recipe, out, None)?;
    println!("render -> {} ({} x {})", out.display(), w, h);
    Ok(())
}

fn auto_cmd(
    raw: &Path,
    out: Option<PathBuf>,
    guidance: Option<String>,
    style: Option<f32>,
    denoise: bool,
    denoise_strength: Option<f32>,
    denoise_model: Option<String>,
) -> Result<()> {
    let cfg = Config::load();
    let style = style.unwrap_or(cfg.style_strength);
    let (recipe, verdict) = produce_recipe(raw, &cfg, true, guidance.as_deref(), None, style)?;
    write_recipe(raw, &recipe, None)?;

    // Default to a 16-bit TIFF master (highest fidelity); pass -o foo.jpg for a
    // smaller 8-bit file.
    let out = out.unwrap_or_else(|| default_out(raw, "developed", "tif"));
    pipeline::guard_readonly(&out, raw)?;
    ensure_parent(&out)?;
    // Opt-in AI denoise runs inside the render, before tone/sharpen.
    let dn = denoise
        .then(|| denoise::DenoiseOpts::from_config(&cfg, denoise_model, denoise_strength.unwrap_or(1.0)));
    println!(
        "verdict: {:?}; rendering full-resolution (16-bit){} ...",
        verdict.decision,
        if denoise { " with AI denoise" } else { "" }
    );
    let (w, h) = render::render_to_file(raw, &recipe, &out, dn.as_ref())?;
    println!("render -> {} ({} x {})", out.display(), w, h);
    // XMP only for a RAW (Lightroom reads it beside the RAW); a baked source
    // (PNG/TIFF) gets the recipe JSON only.
    if decode::is_raw(raw) {
        let xmp_path = write_xmp(raw, &recipe)?;
        println!("xmp    -> {}", xmp_path.display());
    } else {
        println!("(baked source — recipe.json only, no XMP)");
    }
    Ok(())
}

/// Standalone AI denoise: RAW → neutral-developed denoised master, or a baked
/// PNG/TIFF/JPEG → denoised copy. Always writes to ./out (library read-only).
fn denoise_cmd(
    input: &Path,
    out: Option<PathBuf>,
    strength: Option<f32>,
    model: Option<String>,
) -> Result<()> {
    let cfg = Config::load();
    let out = out.unwrap_or_else(|| default_out(input, "denoised", "tif"));
    pipeline::guard_readonly(&out, input)?;
    ensure_parent(&out)?;
    let opts = denoise::DenoiseOpts::from_config(&cfg, model, strength.unwrap_or(1.0));
    if decode::is_raw(input) {
        println!("denoising RAW {} (neutral develop) ...", input.display());
        let (w, h) = render::render_to_file(input, &EditRecipe::default(), &out, Some(&opts))?;
        println!("denoised -> {} ({} x {})", out.display(), w, h);
    } else {
        println!("denoising image {} ...", input.display());
        denoise::denoise_file(&opts, input, &out)?;
        println!("denoised -> {}", out.display());
    }
    Ok(())
}

/// Reverse-fit: solve for the EditRecipe that maps `raw`'s look onto `target`'s
/// (the same frame, differently developed — e.g. the reimagine output). The
/// deliverables are parametric (recipe JSON + XMP + optional full-res render),
/// so the low-res generative experiment becomes a real, adjustable develop.
fn match_cmd(
    raw: &Path,
    target: &Path,
    render_full: bool,
    style_prompt: bool,
    out: Option<PathBuf>,
) -> Result<()> {
    let src = decode::preview_only(raw)?;
    let tgt = decode::load_image(target)?;
    println!("reverse-fitting {} onto the look of {} …", raw.display(), target.display());
    let rep = fit::fit_recipe(&src, &tgt);
    println!(
        "  look error {:.3} → {:.3}  (0 = identical distributions; masks/local edits are not recoverable)",
        rep.err_before, rep.err_after
    );
    println!("--- fitted recipe ---");
    println!("{}", serde_json::to_string_pretty(&rep.recipe)?);

    let out = out.unwrap_or_else(|| default_out(raw, "matched", "json"));
    pipeline::guard_readonly(&out, raw)?;
    let recipe_path = write_recipe(raw, &rep.recipe, Some(out))?;
    println!("recipe -> {}", recipe_path.display());
    if decode::is_raw(raw) {
        let xmp_path = write_xmp(raw, &rep.recipe)?;
        let s = stem(raw);
        println!("xmp    -> {} (copy {s}.xmp beside {s}.ARW for Lightroom)", xmp_path.display());
    }
    if render_full {
        let img_out = default_out(raw, "matched", "tif");
        pipeline::guard_readonly(&img_out, raw)?;
        ensure_parent(&img_out)?;
        println!("rendering the fitted recipe at full resolution …");
        let (w, h) = render::render_to_file(raw, &rep.recipe, &img_out, None)?;
        println!("render -> {} ({w} x {h})", img_out.display());
    }
    if style_prompt {
        let cfg = Config::load();
        // Small uploads are plenty for a style read (and cheap): ~0.5 MP each.
        let jpg = |img: &image::DynamicImage| -> Result<Vec<u8>> {
            let mut buf = Vec::new();
            image::DynamicImage::ImageRgb8(img.thumbnail(768, 768).to_rgb8())
                .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)
                .context("encode style-prompt jpeg")?;
            Ok(buf)
        };
        println!("extracting a reusable style prompt ({}) …", cfg.openai_model);
        let prompt = autoshop::advisor::describe_style(&cfg, &jpg(&src)?, &jpg(&tgt)?)?;
        let p_out = default_out(raw, "style", "txt");
        ensure_parent(&p_out)?;
        std::fs::write(&p_out, &prompt).with_context(|| format!("write {}", p_out.display()))?;
        println!("--- style prompt (reusable as a reimagine Direction) ---");
        println!("{prompt}");
        println!("style  -> {}", p_out.display());
    }
    Ok(())
}

/// OPTIONAL pixel-retouch (heal) mode: AI auto-detects and/or a painted mask, and
/// the deterministic engine heals each spot from surrounding real pixels. Writes
/// a pixel master to ./out — non-XMP (pixel edits don't serialise to ACR).
fn heal_cmd(
    src: &Path,
    mask: Option<PathBuf>,
    no_auto: bool,
    full_res: bool,
    out: Option<PathBuf>,
) -> Result<()> {
    let cfg = Config::load();
    let out = out.unwrap_or_else(|| default_out(src, "heal", "png"));
    pipeline::guard_readonly(&out, src)?;
    println!(
        "pixel retouch (heal) — {}{} ...",
        if no_auto { "painted mask only" } else { "AI auto-detect" },
        if mask.is_some() && !no_auto { " + painted mask" } else { "" }
    );
    let report = retouch::heal(&cfg, src, mask.as_deref(), !no_auto, full_res, &out)?;
    if !report.rationale.is_empty() {
        println!("  {}", report.rationale);
    }
    println!(
        "healed {} spot(s) -> {} ({} x {})",
        report.spots, out.display(), report.dims.0, report.dims.1
    );
    Ok(())
}

fn batch_cmd(dir: &Path, render: bool, limit: usize) -> Result<()> {
    let cfg = Config::load();
    let raws = find_raws(dir)?;
    println!("found {} RAW(s) under {}", raws.len(), dir.display());

    let pending: Vec<&PathBuf> = raws.iter().filter(|r| !xmp_target(r).exists()).collect();
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
        match process_one(raw, &cfg, render) {
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

fn process_one(raw: &Path, cfg: &Config, render_master: bool) -> Result<Verdict> {
    // Batch uses the configured style strength (AUTOSHOP_STYLE_STRENGTH).
    let (recipe, verdict) = produce_recipe(raw, cfg, false, None, None, cfg.style_strength)?;
    write_recipe(raw, &recipe, None)?;
    write_xmp(raw, &recipe)?;
    if render_master {
        let out = default_out(raw, "developed", "tif"); // 16-bit master
        ensure_parent(&out)?;
        render::render_to_file(raw, &recipe, &out, None)?;
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
