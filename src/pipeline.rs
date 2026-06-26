//! Shared pipeline core used by both the CLI (`main.rs`) and the web UI
//! (`serve.rs`): run the advise chain for one RAW and write its outputs to the
//! right place. Keeping this in one module means the CLI and the server can
//! never drift apart.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::advisor::{
    Advisor, ClaudeProvider, Decision, HeuristicProposer, OpenAiProvider, Preview, Verdict,
};
use crate::config::Config;
use crate::decode;
use crate::recipe::EditRecipe;
use crate::xmp;

/// Run the full advise chain for one RAW: decode → propose (GPT or heuristic
/// fallback) → Claude verify → optional one revision round. `verbose` prints the
/// proposer/verifier lines (CLI uses true, the server uses false).
pub fn produce_recipe(raw: &Path, cfg: &Config, verbose: bool) -> Result<(EditRecipe, Verdict)> {
    let decoded = decode::decode_raw(raw)?;

    let preview_img = decoded.preview_resized(1568);
    let mut jpeg = Vec::new();
    preview_img
        .write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
        .context("encode preview JPEG for advisor")?;
    let preview = Preview { jpeg };

    // GPT vision when a key is set; on failure (quota/network) warn and fall back
    // to the heuristic so we still produce a recipe (disclosure, not masking).
    let openai = OpenAiProvider::new(cfg);
    let heuristic = HeuristicProposer;
    let (mut recipe, can_revise) = if cfg.openai_api_key.is_some() {
        if verbose {
            println!("proposer : OpenAI ({})", cfg.openai_model);
        }
        match openai.propose(&preview, &decoded.meta, &decoded.histogram, None) {
            Ok(r) => (r, true),
            Err(e) => {
                eprintln!("⚠ GPT proposer failed ({e})\n  → falling back to the heuristic baseline.");
                (heuristic.propose(&preview, &decoded.meta, &decoded.histogram, None)?, false)
            }
        }
    } else {
        if verbose {
            println!("proposer : heuristic baseline (set OPENAI_API_KEY to use GPT vision)");
        }
        (heuristic.propose(&preview, &decoded.meta, &decoded.histogram, None)?, false)
    };

    let claude = ClaudeProvider::new(cfg);
    if verbose {
        println!("verifier : Claude ({})", cfg.claude_model);
    }
    let mut verdict = claude.verify(&recipe, &decoded.meta, &decoded.histogram)?;

    // One revision round — only if GPT actually produced the recipe.
    if verdict.decision != Decision::Accept && can_revise {
        if let Some(hint) = verdict.revised_hint.clone() {
            if verbose {
                println!("verdict was {:?} → one revision round (hint: {hint})", verdict.decision);
            }
            recipe = openai.propose(&preview, &decoded.meta, &decoded.histogram, Some(&hint))?;
            verdict = claude.verify(&recipe, &decoded.meta, &decoded.histogram)?;
        }
    }
    Ok((recipe, verdict))
}

pub fn write_recipe(raw: &Path, recipe: &EditRecipe, out: Option<PathBuf>) -> Result<PathBuf> {
    let out = out.unwrap_or_else(|| default_out(raw, "recipe", "json"));
    ensure_parent(&out)?;
    std::fs::write(&out, serde_json::to_string_pretty(recipe)?)
        .with_context(|| format!("write recipe {}", out.display()))?;
    Ok(out)
}

pub fn write_xmp(raw: &Path, recipe: &EditRecipe, beside: bool) -> Result<PathBuf> {
    let out = xmp_target(raw, beside);
    ensure_parent(&out)?;
    std::fs::write(&out, xmp::recipe_to_xmp(recipe))
        .with_context(|| format!("write xmp {}", out.display()))?;
    Ok(out)
}

/// Where the .xmp for `raw` goes: next to the RAW (`beside`) or ./out/<stem>.xmp.
pub fn xmp_target(raw: &Path, beside: bool) -> PathBuf {
    if beside {
        raw.with_extension("xmp")
    } else {
        PathBuf::from("out").join(format!("{}.xmp", stem(raw)))
    }
}

/// `./out/<stem>.<kind>.<ext>` — outputs never go beside the source RAW.
pub fn default_out(raw: &Path, kind: &str, ext: &str) -> PathBuf {
    PathBuf::from("out").join(format!("{}.{kind}.{ext}", stem(raw)))
}

pub fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create output dir {}", parent.display()))?;
        }
    }
    Ok(())
}

pub fn stem(p: &Path) -> &str {
    p.file_stem().and_then(|s| s.to_str()).unwrap_or("out")
}

/// Recursively collect every `.arw` (case-insensitive) under `dir`, sorted.
pub fn find_raws(dir: &Path) -> Result<Vec<PathBuf>> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let p = entry?.path();
            if p.is_dir() {
                walk(&p, out)?;
            } else if p
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| x.eq_ignore_ascii_case("arw"))
            {
                out.push(p);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, &mut out).with_context(|| format!("scan {}", dir.display()))?;
    out.sort();
    Ok(out)
}
