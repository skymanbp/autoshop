//! Shared pipeline core used by both the CLI (`main.rs`) and the web UI
//! (`serve.rs`): run the advise chain for one RAW and write its outputs to the
//! right place. Keeping this in one module means the CLI and the server can
//! never drift apart.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::advisor::{
    Advisor, ClaudeProvider, Decision, HeuristicProposer, OpenAiProvider, OpenAiVerifier, Preview,
    Verdict,
};
use crate::config::Config;
use crate::decode;
use crate::recipe::EditRecipe;
use crate::xmp;

/// Run the full advise chain for one RAW: decode → propose (GPT or heuristic
/// fallback) → Claude verify → optional one revision round. `verbose` prints the
/// proposer/verifier lines (CLI uses true, the server uses false).
/// Run the advise chain for one RAW. `guidance` is an optional user direction
/// (a prompt steering the edit, e.g. "warmer and moodier") woven into the GPT
/// prompt.
pub fn produce_recipe(
    raw: &Path,
    cfg: &Config,
    verbose: bool,
    guidance: Option<&str>,
    base: Option<&EditRecipe>,
    style_strength: f32,
) -> Result<(EditRecipe, Verdict)> {
    // decode_any: a camera RAW, or an already-baked PNG/TIFF/JPEG (PNG-source mode).
    let decoded = decode::decode_any(raw)?;

    // Refine mode: when `base` (the user's CURRENT edit) is given, fold it into
    // the direction so GPT adjusts that edit rather than proposing from scratch.
    // Absent a base, behaviour is unchanged — a fresh proposal from the original.
    let refine_owned: Option<String> = base.map(|b| {
        let base_json = serde_json::to_string(b).unwrap_or_default();
        format!(
            "REFINE the photographer's CURRENT edit instead of starting over — keep its choices and \
             change only what this direction implies. CURRENT EDIT (EditRecipe JSON): {base_json}. \
             Direction: {}",
            guidance.unwrap_or("make a small, tasteful improvement")
        )
    });
    let guidance = refine_owned.as_deref().or(guidance);

    let preview_img = decoded.preview_resized(1568);
    let mut jpeg = Vec::new();
    preview_img
        .write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
        .context("encode preview JPEG for advisor")?;
    let preview = Preview { jpeg };

    // Style influence: retrieve the user's edits on the most SIMILAR past shots
    // (needs `autoshop style-index`). style_strength == 0 disables it entirely;
    // otherwise we inject a soft text reference AND, at higher strength, gently
    // pull the FINAL recipe toward those historical means (the blend below).
    let style = (style_strength > 0.0)
        .then(|| crate::style::StyleIndex::load(std::path::Path::new("out/style-index.json")).ok())
        .flatten()
        .map(|ix| {
            let ex = ix.retrieve(&decoded.meta, &decoded.histogram, 4, stem(raw));
            (ix.render_reference(&ex), crate::style::style_targets(&ex))
        });
    let reference: Option<String> = style.as_ref().and_then(|(r, _)| r.clone());
    let ref_str = reference.as_deref();
    if verbose && ref_str.is_some() {
        println!("style    : reference from similar past edits (strength {:.0}%)", style_strength * 100.0);
    }
    if verbose
        && let Some(g) = guidance {
            println!("direction: {g}");
        }

    let (meta, hist) = (&decoded.meta, &decoded.histogram);

    // GPT vision when a key is set; on failure (quota/network) warn and fall back
    // to the heuristic so we still produce a recipe (disclosure, not masking).
    let openai = OpenAiProvider::new(cfg);
    let heuristic = HeuristicProposer;
    let (mut recipe, can_revise) = if cfg.openai_api_key.is_some() {
        if verbose {
            println!("proposer : OpenAI ({})", cfg.openai_model);
        }
        match openai.propose(&preview, meta, hist, ref_str, guidance, None) {
            Ok(r) => (r, true),
            Err(e) => {
                eprintln!("⚠ GPT proposer failed ({e})\n  → falling back to the heuristic baseline.");
                (heuristic.propose(&preview, meta, hist, None, None, None)?, false)
            }
        }
    } else {
        if verbose {
            println!("proposer : heuristic baseline (set OPENAI_API_KEY to use GPT vision)");
        }
        (heuristic.propose(&preview, meta, hist, None, None, None)?, false)
    };

    // Verifier (analysis role): OAuth `claude` CLI by default, or an
    // OpenAI-compatible API when the analysis provider is set to `api`.
    let verifier: Box<dyn Advisor> = if cfg.analysis_is_api() {
        Box::new(OpenAiVerifier::new(cfg))
    } else {
        Box::new(ClaudeProvider::new(cfg))
    };
    if verbose {
        let who = if cfg.analysis_is_api() { "OpenAI-API" } else { "Claude (OAuth)" };
        println!("verifier : {who} ({})", cfg.analysis_model);
    }
    let mut verdict = verifier.verify(&recipe, meta, hist)?;

    // Bounded verify→revise loop (only if GPT actually produced the recipe). With
    // the now-symmetric verifier — which pushes a too-flat edit to commit AND a
    // too-cooked one to ease — a few rounds converge toward a finished look instead
    // of just ratcheting down. Capped at MAX_REVISIONS to bound cost/latency; we
    // stop early on Accept or when the verifier stops giving a revision hint.
    const MAX_REVISIONS: usize = 2;
    let mut round = 0;
    while can_revise && round < MAX_REVISIONS && verdict.decision != Decision::Accept {
        let Some(hint) = verdict.revised_hint.clone() else { break };
        round += 1;
        if verbose {
            println!("verdict {:?} → revision {round}/{MAX_REVISIONS} (hint: {hint})", verdict.decision);
        }
        recipe = openai.propose(&preview, meta, hist, ref_str, guidance, Some(&hint))?;
        verdict = verifier.verify(&recipe, meta, hist)?;
    }

    // Distill toward the user's historical style: a gentle, capped pull of the
    // global sliders toward similar past edits. Capped at 60% so even max
    // strength never fully overrides the AI's scene-specific proposal.
    if let Some((_, targets)) = &style {
        let blended = style_strength > 0.0 && !targets.is_empty();
        crate::style::blend_toward(&mut recipe, targets, style_strength.clamp(0.0, 1.0) * 0.6);
        recipe.clamp();
        // The blend mutated the recipe AFTER the verdict, so the verdict above is
        // stale. Re-verify the FINAL recipe so the returned verdict honestly
        // reflects what will actually be applied (not the pre-blend proposal).
        if blended {
            verdict = verifier.verify(&recipe, meta, hist)?;
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

pub fn write_xmp(raw: &Path, recipe: &EditRecipe) -> Result<PathBuf> {
    let out = xmp_target(raw);
    ensure_parent(&out)?;
    std::fs::write(&out, xmp::recipe_to_xmp(recipe))
        .with_context(|| format!("write xmp {}", out.display()))?;
    Ok(out)
}

/// Where the .xmp for `raw` goes — always ./out (the photo library is read-only).
pub fn xmp_target(raw: &Path) -> PathBuf {
    PathBuf::from("out").join(format!("{}.xmp", stem(raw)))
}

/// Guarantee the read-only library: refuse to write `out` if it lands inside the
/// source RAW's own folder (or below it). Outputs belong in ./out.
pub fn guard_readonly(out: &Path, raw: &Path) -> Result<()> {
    use std::path::absolute;
    let (Ok(out_abs), Ok(raw_abs)) = (absolute(out), absolute(raw)) else {
        return Ok(());
    };
    if let Some(raw_dir) = raw_abs.parent()
        && out_abs.starts_with(raw_dir) {
            anyhow::bail!(
                "refusing to write into the source RAW's folder ({}) — the photo library is \
                 read-only. Write outputs to ./out (the default) instead.",
                raw_dir.display()
            );
        }
    Ok(())
}

/// `./out/<stem>.<kind>.<ext>` — outputs never go beside the source RAW.
pub fn default_out(raw: &Path, kind: &str, ext: &str) -> PathBuf {
    PathBuf::from("out").join(format!("{}.{kind}.{ext}", stem(raw)))
}

pub fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create output dir {}", parent.display()))?;
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

/// Like [`find_raws`] but also includes already-baked images (PNG/TIFF/JPEG), so
/// the web UI can browse and edit LR/PS-denoised exports alongside RAWs. Sorted.
pub fn find_sources(dir: &Path) -> Result<Vec<PathBuf>> {
    fn is_source(p: &Path) -> bool {
        crate::decode::is_raw(p)
            || p.extension().and_then(|x| x.to_str()).is_some_and(|x| {
                matches!(x.to_ascii_lowercase().as_str(), "png" | "tif" | "tiff" | "jpg" | "jpeg")
            })
    }
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let p = entry?.path();
            if p.is_dir() {
                walk(&p, out)?;
            } else if is_source(&p) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_refuses_writes_into_the_source_library() {
        // A RAW living in the (read-only) photo library.
        let raw = Path::new("D:/Photography/Raw/2024/Trip/DSC0001.ARW");
        // Writing a sibling INTO that folder must be refused.
        let sibling = Path::new("D:/Photography/Raw/2024/Trip/DSC0001.developed.tif");
        assert!(guard_readonly(sibling, raw).is_err(), "must refuse a sibling write");
        // A subfolder under the RAW's folder is refused too.
        let under = Path::new("D:/Photography/Raw/2024/Trip/out/DSC0001.tif");
        assert!(guard_readonly(under, raw).is_err(), "must refuse a subfolder write");
        // The default ./out (outside the library) is allowed.
        let safe = default_out(raw, "developed", "tif");
        assert!(guard_readonly(&safe, raw).is_ok(), "./out must be allowed");
    }

    #[test]
    fn outputs_always_default_outside_the_library() {
        let raw = Path::new("D:/Photography/Raw/2024/Trip/DSC0001.ARW");
        // Every default output + the XMP sidecar land under ./out, never beside
        // the RAW — the library stays read-only by construction.
        assert!(default_out(raw, "developed", "tif").starts_with("out"));
        assert!(default_out(raw, "recipe", "json").starts_with("out"));
        assert!(xmp_target(raw).starts_with("out"));
        assert_eq!(xmp_target(raw), Path::new("out/DSC0001.xmp")); // stem preserved
    }
}
