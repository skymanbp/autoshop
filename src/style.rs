//! Style similarity-retrieval reference (V2_PLAN §3).
//!
//! For a photo being edited, find the user's edits on the most SIMILAR past
//! photos (by EXIF + histogram features) and feed those to the advisor as SOFT
//! reference. This deliberately replaces the earlier global-bias "distillation":
//! different photo TYPES are edited differently, so we condition on similar
//! context instead of averaging everything. The retrieved edits are reference,
//! not a target to copy.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::decode::{self, Histogram, Meta};
use crate::eval::crs_f32;
use crate::pipeline;
use crate::recipe::EditRecipe;

const NDIM: usize = 14;
/// Unbounded (log / ratio) dims to z-score; the rest are already ~bounded.
const ZSCORE_DIMS: [usize; 4] = [0, 1, 2, 10];
/// Per-dim distance weights (scene-type discriminators heavier).
const WEIGHTS: [f32; NDIM] = [
    1.5, 1.0, 1.0, 0.5, 0.5, 1.5, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.5,
];
/// Slider keys shown as the reference (crs key → label). Tint/Saturation/Dehaze
/// were added in index v2 so the style blend captures the user's colour habits.
const REF_KEYS: [(&str, &str); 12] = [
    ("Exposure2012", "exposure"),
    ("Contrast2012", "contrast"),
    ("Highlights2012", "highlights"),
    ("Shadows2012", "shadows"),
    ("Whites2012", "whites"),
    ("Blacks2012", "blacks"),
    ("Vibrance", "vibrance"),
    ("Clarity2012", "clarity"),
    ("Temperature", "temperature_K"),
    ("Tint", "tint"),
    ("Saturation", "saturation"),
    ("Dehaze", "dehaze"),
];

/// 14-dim feature vector from capture metadata + histogram.
pub fn feature_vector(meta: &Meta, hist: &Histogram) -> [f32; NDIM] {
    let lnpos = |v: f32| if v > 0.0 { v.ln() } else { 0.0 };
    let total: f64 = hist.luma.iter().map(|&v| v as f64).sum::<f64>().max(1.0);
    let mean_of = |b: &[u32]| -> f32 {
        let s: f64 = b.iter().enumerate().map(|(i, &v)| i as f64 * v as f64).sum();
        (s / total) as f32
    };
    let mean_l = mean_of(&hist.luma);
    let var: f64 = hist
        .luma
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let d = i as f64 - mean_l as f64;
            d * d * v as f64
        })
        .sum::<f64>()
        / total;
    let std_l = var.sqrt() as f32;
    let (mr, mg, mb) = (mean_of(&hist.r), mean_of(&hist.g), mean_of(&hist.b));
    let hour = parse_hour(meta.date_time.as_deref());
    let (w, h) = (meta.width.max(1) as f32, meta.height.max(1) as f32);
    let wb = meta.as_shot_wb_coeffs;
    let warmth = if wb[0] > 0.0 && wb[2] > 0.0 { (wb[0] / wb[2]).ln() } else { 0.0 };
    let ang = std::f32::consts::TAU * hour / 24.0;
    [
        lnpos(meta.focal_length_mm.unwrap_or(35.0)),
        lnpos(meta.iso.unwrap_or(100) as f32),
        lnpos(meta.aperture.unwrap_or(5.6)),
        ang.sin(),
        ang.cos(),
        mean_l / 255.0,
        hist.clip_black_pct / 100.0,
        hist.clip_white_pct / 100.0,
        (mr - mg) / 255.0,
        (mb - mg) / 255.0,
        warmth,
        w / h,
        std_l / 255.0,
        if h > w { 1.0 } else { 0.0 },
    ]
}

fn parse_hour(dt: Option<&str>) -> f32 {
    // EXIF "2023:06:01 14:30:00" → 14
    dt.and_then(|s| s.split(' ').nth(1))
        .and_then(|t| t.split(':').next())
        .and_then(|h| h.parse::<f32>().ok())
        .unwrap_or(12.0)
}

fn read_settings(xmp: &str) -> BTreeMap<String, f32> {
    REF_KEYS
        .iter()
        .filter_map(|(k, label)| crs_f32(xmp, k).map(|v| (label.to_string(), v)))
        .collect()
}

/// Short human tag like "tele/bright/midday" for the reference block.
fn derive_tag(f: &[f32; NDIM]) -> String {
    let focal = f[0].exp();
    let lens = if focal < 24.0 { "ultrawide" } else if focal < 45.0 { "wide" } else if focal < 90.0 { "normal" } else { "tele" };
    let tone = if f[5] < 0.33 { "dark" } else if f[5] > 0.6 { "bright" } else { "mid" };
    let hour = (f[3].atan2(f[4]) / std::f32::consts::TAU * 24.0 + 24.0) % 24.0;
    let tod = if !(5.0..20.0).contains(&hour) { "night" } else if (5.0..9.0).contains(&hour) || (17.0..20.0).contains(&hour) { "goldenish" } else { "midday" };
    let orient = if f[13] > 0.5 { "portrait" } else { "landscape" };
    format!("{lens}/{tone}/{tod}/{orient}")
}

#[derive(Serialize, Deserialize, Clone)]
pub struct StyleExemplar {
    pub stem: String,
    pub feat: Vec<f32>,
    pub tag: String,
    pub settings: BTreeMap<String, f32>,
    /// The user's master tone-curve shape `[black_lift, s_strength]` (0..255
    /// scale), if they drew one — the curve "habit" the flat sliders can't carry.
    /// `#[serde(default)]` keeps v1 index files loadable.
    #[serde(default)]
    pub curve: Option<[f32; 2]>,
}

#[derive(Serialize, Deserialize)]
pub struct StyleIndex {
    pub version: u32,
    pub mean: Vec<f32>,
    pub std: Vec<f32>,
    pub exemplars: Vec<StyleExemplar>,
    /// Absolute folder this index was built from (the user's edited-RAW library),
    /// so the UI can show its provenance. `#[serde(default)]` keeps old index
    /// files (written before this field) loadable.
    #[serde(default)]
    pub source_dir: Option<String>,
}

impl StyleIndex {
    /// Scan a folder for RAW+.xmp pairs (the user's own edits) and build the index.
    pub fn build(dir: &Path) -> Result<StyleIndex> {
        let raws = pipeline::find_raws(dir)?;
        let pairs: Vec<_> = raws.iter().filter(|r| r.with_extension("xmp").exists()).collect();
        println!("building style index from {} RAW+.xmp pairs ...", pairs.len());
        let mut exemplars = Vec::new();
        for (i, raw) in pairs.iter().enumerate() {
            let decoded = match decode::decode_raw(raw) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("  skip {}: {e}", pipeline::stem(raw));
                    continue;
                }
            };
            let feat = feature_vector(&decoded.meta, &decoded.histogram);
            let xmp = std::fs::read_to_string(raw.with_extension("xmp")).unwrap_or_default();
            exemplars.push(StyleExemplar {
                stem: pipeline::stem(raw).to_string(),
                tag: derive_tag(&feat),
                feat: feat.to_vec(),
                settings: read_settings(&xmp),
                curve: crate::eval::user_curve_shape(&xmp).map(|(b, s)| [b, s]),
            });
            if (i + 1) % 20 == 0 {
                println!("  {} / {}", i + 1, pairs.len());
            }
        }
        let (mean, std) = compute_norm(&exemplars);
        // Record where this index was built from, for UI provenance / other users.
        let source_dir = std::path::absolute(dir).map(|p| p.display().to_string()).ok();
        // v2: exemplars now carry tint/saturation/dehaze + tone-curve shape.
        Ok(StyleIndex { version: 2, mean, std, exemplars, source_dir })
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        pipeline::ensure_parent(path)?;
        std::fs::write(path, serde_json::to_string(self)?)
            .with_context(|| format!("write style index {}", path.display()))?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<StyleIndex> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read style index {}", path.display()))?;
        Ok(serde_json::from_str(&text)?)
    }

    /// k nearest exemplars to (meta,hist), excluding `exclude_stem` (the query
    /// itself when it's a corpus member).
    pub fn retrieve(&self, meta: &Meta, hist: &Histogram, k: usize, exclude_stem: &str) -> Vec<&StyleExemplar> {
        let q = normalize(feature_vector(meta, hist), &self.mean, &self.std);
        let mut scored: Vec<(f32, &StyleExemplar)> = self
            .exemplars
            .iter()
            .filter(|e| e.stem != exclude_stem && e.feat.len() == NDIM)
            .map(|e| {
                let mut ef = [0.0f32; NDIM];
                ef.copy_from_slice(&e.feat);
                let en = normalize(ef, &self.mean, &self.std);
                let d2: f32 = (0..NDIM).map(|i| WEIGHTS[i] * (q[i] - en[i]).powi(2)).sum();
                (d2, e)
            })
            .collect();
        scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.into_iter().take(k).map(|(_, e)| e).collect()
    }

    /// Render retrieved exemplars as a SOFT reference block for the advisor prompt.
    pub fn render_reference(&self, ex: &[&StyleExemplar]) -> Option<String> {
        if ex.is_empty() {
            return None;
        }
        let lines: Vec<String> = ex
            .iter()
            .map(|e| {
                let s: Vec<String> = e
                    .settings
                    .iter()
                    .map(|(k, v)| format!("{k} {v:+.0}"))
                    .collect();
                format!("[{}] {}", e.tag, s.join(", "))
            })
            .collect();
        // Average the retrieved exemplars' tone-curve SHAPE (those who drew one),
        // so the AI shapes its tone_curve the way this user habitually does.
        let curves: Vec<[f32; 2]> = ex.iter().filter_map(|e| e.curve).collect();
        let curve_note = if !curves.is_empty() {
            let n = curves.len() as f32;
            let bl = curves.iter().map(|c| c[0]).sum::<f32>() / n;
            let ss = curves.iter().map(|c| c[1]).sum::<f32>() / n;
            format!(
                "  THEIR TYPICAL MASTER TONE CURVE: black-lift {bl:+.0}, S-strength {ss:+.0} \
(0..255 scale) — shape your `tone_curve` to a similar gentleness, not stronger."
            )
        } else {
            String::new()
        };
        Some(format!(
            "STYLE REFERENCE — how this user edited SIMILAR past shots (for consistency with their \
taste; reference, do NOT copy verbatim, the scene differs): {}{}",
            lines.join("  |  "),
            curve_note
        ))
    }
}

/// Mean of the retrieved exemplars' slider settings, keyed by the matching
/// [`EditRecipe`] field name. This is the "distill toward my historical style"
/// target — applied as a *gentle, capped* pull by [`blend_toward`], never a full
/// override (per the user's "use as reference, not a target" decision).
pub fn style_targets(ex: &[&StyleExemplar]) -> BTreeMap<&'static str, f32> {
    // (xmp settings label from REF_KEYS) → (EditRecipe field name)
    const MAP: [(&str, &str); 12] = [
        ("exposure", "exposure_ev"),
        ("contrast", "contrast"),
        ("highlights", "highlights"),
        ("shadows", "shadows"),
        ("whites", "whites"),
        ("blacks", "blacks"),
        ("vibrance", "vibrance"),
        ("clarity", "clarity"),
        ("temperature_K", "temperature_k"),
        ("tint", "tint"),
        ("saturation", "saturation"),
        ("dehaze", "dehaze"),
    ];
    let mut out = BTreeMap::new();
    for (label, field) in MAP {
        let vals: Vec<f32> = ex.iter().filter_map(|e| e.settings.get(label).copied()).collect();
        if !vals.is_empty() {
            out.insert(field, vals.iter().sum::<f32>() / vals.len() as f32);
        }
    }
    out
}

/// Pull `recipe`'s global sliders a fraction `t` (0..1) toward `targets` (your
/// historical style means). `t = 0` is a no-op; the caller caps `t` so even
/// "100% style" never fully overrides the AI's scene-specific proposal.
pub fn blend_toward(recipe: &mut EditRecipe, targets: &BTreeMap<&'static str, f32>, t: f32) {
    let t = t.clamp(0.0, 1.0);
    if t <= 0.0 || targets.is_empty() {
        return;
    }
    let lerp = |a: f32, b: f32| a + (b - a) * t;
    for (field, &target) in targets {
        match *field {
            "exposure_ev" => recipe.exposure_ev = lerp(recipe.exposure_ev, target),
            "contrast" => recipe.contrast = lerp(recipe.contrast, target),
            "highlights" => recipe.highlights = lerp(recipe.highlights, target),
            "shadows" => recipe.shadows = lerp(recipe.shadows, target),
            "whites" => recipe.whites = lerp(recipe.whites, target),
            "blacks" => recipe.blacks = lerp(recipe.blacks, target),
            "vibrance" => recipe.vibrance = lerp(recipe.vibrance, target),
            "clarity" => recipe.clarity = lerp(recipe.clarity, target),
            "tint" => recipe.tint = lerp(recipe.tint, target),
            "saturation" => recipe.saturation = lerp(recipe.saturation, target),
            "dehaze" => recipe.dehaze = lerp(recipe.dehaze, target),
            "temperature_k" => {
                let cur = recipe.temperature_k.unwrap_or(target);
                recipe.temperature_k = Some(lerp(cur, target));
            }
            _ => {}
        }
    }
}

fn normalize(mut v: [f32; NDIM], mean: &[f32], std: &[f32]) -> [f32; NDIM] {
    for &d in &ZSCORE_DIMS {
        let s = std.get(d).copied().unwrap_or(1.0).max(1e-4);
        v[d] = (v[d] - mean.get(d).copied().unwrap_or(0.0)) / s;
    }
    v
}

fn compute_norm(ex: &[StyleExemplar]) -> (Vec<f32>, Vec<f32>) {
    let mut mean = vec![0.0f32; NDIM];
    let mut std = vec![1.0f32; NDIM];
    if ex.is_empty() {
        return (mean, std);
    }
    let n = ex.len() as f32;
    for &d in &ZSCORE_DIMS {
        let m: f32 = ex.iter().map(|e| e.feat[d]).sum::<f32>() / n;
        let var: f32 = ex.iter().map(|e| (e.feat[d] - m).powi(2)).sum::<f32>() / n;
        mean[d] = m;
        std[d] = var.sqrt().max(1e-4);
    }
    (mean, std)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hour_reads_exif() {
        assert_eq!(parse_hour(Some("2023:06:01 14:30:00")), 14.0);
        assert_eq!(parse_hour(None), 12.0);
    }

    #[test]
    fn tag_describes_a_bright_tele_landscape() {
        let mut f = [0.0f32; NDIM];
        f[0] = 120.0_f32.ln(); // tele
        f[5] = 0.7; // bright
        f[3] = 0.0;
        f[4] = 1.0; // hour ~ 0/12 region → midday-ish via atan2(0,1)=0 → hour 0 = night
        f[13] = 0.0; // landscape
        let tag = derive_tag(&f);
        assert!(tag.starts_with("tele/bright/"), "got {tag}");
        assert!(tag.ends_with("/landscape"), "got {tag}");
    }

    #[test]
    fn style_blend_pulls_toward_historical_mean() {
        let mk = |exp: f32, con: f32, sat: f32| StyleExemplar {
            stem: "x".into(),
            feat: vec![0.0; NDIM],
            tag: "t".into(),
            settings: BTreeMap::from([
                ("exposure".to_string(), exp),
                ("contrast".to_string(), con),
                ("saturation".to_string(), sat),
                ("dehaze".to_string(), 8.0),
            ]),
            curve: Some([5.0, 12.0]),
        };
        let (a, b) = (mk(0.4, 20.0, 10.0), mk(0.6, 40.0, 30.0));
        let targets = style_targets(&[&a, &b]);
        assert_eq!(targets.get("exposure_ev").copied(), Some(0.5)); // mean(0.4,0.6)
        assert_eq!(targets.get("contrast").copied(), Some(30.0)); // mean(20,40)
        assert_eq!(targets.get("saturation").copied(), Some(20.0)); // mean(10,30) — v2 field
        assert_eq!(targets.get("dehaze").copied(), Some(8.0)); // v2 field

        let mut r = EditRecipe::default();
        blend_toward(&mut r, &targets, 0.5); // pull halfway from 0
        assert!((r.exposure_ev - 0.25).abs() < 1e-5, "{}", r.exposure_ev);
        assert!((r.contrast - 15.0).abs() < 1e-4, "{}", r.contrast);
        assert!((r.saturation - 10.0).abs() < 1e-4, "{}", r.saturation); // halfway to 20
        assert!((r.dehaze - 4.0).abs() < 1e-4, "{}", r.dehaze); // halfway to 8

        let before = r.clone();
        blend_toward(&mut r, &targets, 0.0); // strength 0 = no-op
        assert_eq!(r, before);
    }

    #[test]
    fn reference_surfaces_the_users_curve_habit() {
        let ex = StyleExemplar {
            stem: "x".into(),
            feat: vec![0.0; NDIM],
            tag: "wide/mid/midday/landscape".into(),
            settings: BTreeMap::from([("contrast".to_string(), 15.0)]),
            curve: Some([6.0, 20.0]),
        };
        let idx = StyleIndex {
            version: 2,
            mean: vec![0.0; NDIM],
            std: vec![1.0; NDIM],
            exemplars: vec![],
            source_dir: None,
        };
        let r = idx.render_reference(&[&ex]).unwrap();
        assert!(r.contains("TYPICAL MASTER TONE CURVE"), "{r}");
        assert!(r.contains("S-strength +20"), "{r}");
    }
}
