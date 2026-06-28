//! Pixel-level RETOUCH (heal) — an OPTIONAL mode, distinct from BOTH other paths:
//!
//!   * the **parametric develop** path (`EditRecipe` → XMP / render): the AI only
//!     turns sliders, "never touches a pixel"; output is reproducible + a sidecar.
//!   * the **generative** path (`generative.rs`, gpt-image): SYNTHESISES new pixels.
//!
//! This mode is traditional spot-healing: it REMOVES small defects (dust, sensor
//! spots, blemishes, tiny distractions) by sampling SURROUNDING REAL pixels and
//! blending them over the defect — exactly a retoucher's heal tool. By
//! construction it only ever copies / shifts / averages pixels that ALREADY exist
//! in the photo; it never invents content. That is the architectural guarantee
//! that this is *retouching, not generation* (the user's hard constraint).
//!
//! Targeting is hybrid: a vision model can auto-detect spots ([`detect_spots`])
//! AND the user can paint regions in the UI ([`plan_from_mask`]); both feed the
//! same deterministic [`heal_image`] engine. Output is a pixel master in ./out
//! (no XMP — pixel edits aren't ACR-serialisable).

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use image::{RgbImage, RgbaImage};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::Config;
use crate::decode;

/// One heal target: a normalised circular region to repair by sampling nearby
/// real pixels. `cx`/`cy` are 0..1 of the frame; `radius` is a fraction of the
/// SHORT side. `source` is an optional explicit donor offset (normalised frame
/// units) — `None` auto-searches the best clean donor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HealSpot {
    pub cx: f32,
    pub cy: f32,
    pub radius: f32,
    pub feather: f32,
    pub source: Option<[f32; 2]>,
    pub label: String,
}

impl Default for HealSpot {
    fn default() -> Self {
        Self { cx: 0.5, cy: 0.5, radius: 0.02, feather: 0.4, source: None, label: String::new() }
    }
}

/// A retouch plan: the spots to heal plus provenance.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RetouchPlan {
    pub spots: Vec<HealSpot>,
    pub rationale: String,
    pub confidence: f32,
}

/// Outcome of a heal run, for the CLI / UI to report.
pub struct HealReport {
    pub spots: usize,
    pub rationale: String,
    pub dims: (u32, u32),
}

// --- the deterministic heal engine -----------------------------------------

/// Apply every spot to `img` in place, healing from surrounding real pixels.
/// Donors are read from a snapshot of the ORIGINAL image, so spots are
/// order-independent and never read a half-written region.
pub fn heal_image(img: &mut RgbImage, spots: &[HealSpot]) {
    if spots.is_empty() {
        return;
    }
    let src = img.clone();
    let (w, h) = img.dimensions();
    let short = w.min(h) as f32;
    for s in spots {
        let cx = s.cx.clamp(0.0, 1.0) * w as f32;
        let cy = s.cy.clamp(0.0, 1.0) * h as f32;
        let r = (s.radius.clamp(0.0, 0.5) * short).max(2.0);
        let off = match s.source {
            Some([sx, sy]) => ((sx * w as f32).round() as i32, (sy * h as f32).round() as i32),
            None => find_donor(&src, cx, cy, r),
        };
        heal_one(&src, img, cx, cy, r, s.feather.clamp(0.0, 1.0), off);
    }
}

/// Read a pixel as f32 RGB, clamping coordinates to the image edge.
#[inline]
fn px(src: &RgbImage, x: i32, y: i32) -> [f32; 3] {
    let xx = x.clamp(0, src.width() as i32 - 1) as u32;
    let yy = y.clamp(0, src.height() as i32 - 1) as u32;
    let p = src.get_pixel(xx, yy).0;
    [p[0] as f32, p[1] as f32, p[2] as f32]
}

/// Search candidate donor offsets on rings around the spot; pick the one whose
/// surroundings best match the spot's border (so the patch is seamless) and that
/// stays in-bounds. Returns a pixel offset (dx, dy) from the spot centre, or
/// (0,0) if no in-bounds donor exists (caller then leaves the spot untouched).
fn find_donor(src: &RgbImage, cx: f32, cy: f32, r: f32) -> (i32, i32) {
    let (w, h) = (src.width() as f32, src.height() as f32);
    let mut best = (0i32, 0i32);
    let mut best_score = f32::INFINITY;
    for dist_mul in [2.4f32, 3.2, 4.4] {
        let dist = r * dist_mul;
        for k in 0..16 {
            let a = k as f32 / 16.0 * std::f32::consts::TAU;
            let (vx, vy) = (dist * a.cos(), dist * a.sin());
            let (dcx, dcy) = (cx + vx, cy + vy);
            // donor disk must stay fully in-bounds
            if dcx - r < 0.0 || dcx + r >= w || dcy - r < 0.0 || dcy + r >= h {
                continue;
            }
            // score = SSD of the border ring (spot surroundings vs the donor's,
            // shifted by v) + a smoothness penalty on the donor interior. Low =
            // donor blends well and isn't itself a busy feature.
            let mut ssd = 0.0f32;
            let mut mean = [0.0f32; 3];
            let mut m2 = [0.0f32; 3];
            let rr = r * 1.2;
            for j in 0..24 {
                let b = j as f32 / 24.0 * std::f32::consts::TAU;
                let (ox, oy) = (b.cos(), b.sin());
                let tp = px(src, (cx + ox * rr) as i32, (cy + oy * rr) as i32);
                let dp = px(src, (cx + vx + ox * rr) as i32, (cy + vy + oy * rr) as i32);
                for c in 0..3 {
                    let d = tp[c] - dp[c];
                    ssd += d * d;
                }
                let ip = px(src, (dcx + ox * r * 0.5) as i32, (dcy + oy * r * 0.5) as i32);
                for c in 0..3 {
                    mean[c] += ip[c];
                    m2[c] += ip[c] * ip[c];
                }
            }
            let n = 24.0f32;
            let mut var = 0.0;
            for c in 0..3 {
                let mu = mean[c] / n;
                var += (m2[c] / n - mu * mu).max(0.0);
            }
            let score = ssd + var * 0.5;
            if score < best_score {
                best_score = score;
                best = (vx.round() as i32, vy.round() as i32);
            }
        }
    }
    best
}

/// Copy the donor disk (spot centre + offset) over the spot, correcting its mean
/// to the spot's border (the "heal" vs "clone" part) and feathering the edge.
fn heal_one(src: &RgbImage, dst: &mut RgbImage, cx: f32, cy: f32, r: f32, feather: f32, off: (i32, i32)) {
    if off == (0, 0) {
        return; // no donor found → honest no-op rather than cloning the spot onto itself
    }
    let (w, h) = (dst.width() as i32, dst.height() as i32);
    let (ox, oy) = off;
    // Low-frequency correction: shift the donor so its border matches the spot's
    // border — this is what makes a *heal* blend where a raw *clone* would seam.
    let mut corr = [0.0f32; 3];
    for j in 0..24 {
        let a = j as f32 / 24.0 * std::f32::consts::TAU;
        let (dx, dy) = (a.cos() * r * 1.15, a.sin() * r * 1.15);
        let tp = px(src, (cx + dx) as i32, (cy + dy) as i32);
        let dp = px(src, (cx + dx) as i32 + ox, (cy + dy) as i32 + oy);
        for c in 0..3 {
            corr[c] += tp[c] - dp[c];
        }
    }
    for v in &mut corr {
        *v /= 24.0;
    }

    let r_i = r.ceil() as i32;
    let inner = r * (1.0 - feather);
    for dy in -r_i..=r_i {
        for dx in -r_i..=r_i {
            let d = ((dx * dx + dy * dy) as f32).sqrt();
            if d > r {
                continue;
            }
            // feathered weight: 1 inside `inner`, ramping to 0 at the edge.
            let alpha = if d <= inner {
                1.0
            } else if r > inner {
                (r - d) / (r - inner)
            } else {
                1.0
            };
            let (tx, ty) = (cx as i32 + dx, cy as i32 + dy);
            if tx < 0 || ty < 0 || tx >= w || ty >= h {
                continue;
            }
            let donor = px(src, tx + ox, ty + oy);
            let base = px(src, tx, ty);
            let mut out = [0u8; 3];
            for c in 0..3 {
                let healed = donor[c] + corr[c];
                let v = base[c] * (1.0 - alpha) + healed * alpha;
                out[c] = v.round().clamp(0.0, 255.0) as u8;
            }
            dst.put_pixel(tx as u32, ty as u32, image::Rgb(out));
        }
    }
}

/// Turn a painted RGBA mask (alpha < 128 = painted = heal here, matching the UI's
/// brush + the generative-mask convention) into heal spots via connected
/// components: each painted blob becomes one circular heal target. Coordinates
/// are normalised, so the mask can be at any resolution.
pub fn plan_from_mask(mask: &RgbaImage) -> Vec<HealSpot> {
    let (w, h) = mask.dimensions();
    let wu = w as usize;
    let painted: Vec<bool> = mask.pixels().map(|p| p.0[3] < 128).collect();
    let mut seen = vec![false; painted.len()];
    let short = w.min(h) as f32;
    let mut spots = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    for start in 0..painted.len() {
        if !painted[start] || seen[start] {
            continue;
        }
        stack.clear();
        stack.push(start);
        seen[start] = true;
        let (mut sx, mut sy, mut cnt) = (0f64, 0f64, 0u32);
        let mut pts: Vec<(i32, i32)> = Vec::new();
        while let Some(i) = stack.pop() {
            let (x, y) = ((i % wu) as i32, (i / wu) as i32);
            sx += x as f64;
            sy += y as f64;
            cnt += 1;
            pts.push((x, y));
            for (nx, ny) in [(x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)] {
                if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                    continue;
                }
                let j = ny as usize * wu + nx as usize;
                if painted[j] && !seen[j] {
                    seen[j] = true;
                    stack.push(j);
                }
            }
        }
        if cnt < 6 {
            continue; // ignore stray dots / brush noise
        }
        let cxp = (sx / cnt as f64) as f32;
        let cyp = (sy / cnt as f64) as f32;
        let mut rad = 0f32;
        for (x, y) in &pts {
            let dd = ((*x as f32 - cxp).powi(2) + (*y as f32 - cyp).powi(2)).sqrt();
            if dd > rad {
                rad = dd;
            }
        }
        rad = (rad * 1.1).max(2.0);
        spots.push(HealSpot {
            cx: cxp / w as f32,
            cy: cyp / h as f32,
            radius: rad / short,
            feather: 0.4,
            source: None,
            label: "painted".into(),
        });
    }
    spots
}

// --- AI auto-detection (vision) --------------------------------------------

/// Vision model auto-detects small removable defects (dust / blemishes / specks)
/// and returns them as heal spots. Constrained by the prompt + JSON schema to
/// SMALL spot-removals against fairly uniform surroundings — never large-area or
/// content-inventing edits, so the result stays "retouch, not generation".
pub fn detect_spots(cfg: &Config, jpeg: &[u8]) -> Result<RetouchPlan> {
    let key = cfg.openai_api_key.as_ref().ok_or_else(|| {
        anyhow!("OPENAI_API_KEY not set — AI spot-detection needs the image (vision) API; paint a mask instead")
    })?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(jpeg);
    let instruction = "You are a photo RETOUCHER doing blemish / dust removal. Look at the image and \
list SMALL defects that should be REMOVED by healing from surrounding pixels: sensor dust spots, \
skin blemishes / pimples, stray specks, tiny distracting objects against a fairly uniform background \
(sky, skin, wall, water). For EACH, give a circular region: cx, cy (centre, 0..1 of the frame) and \
radius (fraction of the SHORT side, keep SMALL — 0.005..0.06). DO NOT include anything that needs \
inventing new content (no large areas, no removing big / foreground subjects, no adding objects) — \
only small spot fixes a retoucher could heal from neighbours. Return up to 30 spots; fewer is fine. \
If the photo is clean, return an empty list.";

    let body = json!({
        "model": cfg.openai_model,
        "input": [{
            "role": "user",
            "content": [
                { "type": "input_text", "text": instruction },
                { "type": "input_image",
                  "image_url": format!("data:image/jpeg;base64,{b64}"),
                  "detail": "high" }
            ]
        }],
        "text": { "format": {
            "type": "json_schema",
            "name": "retouch_plan",
            "strict": true,
            "schema": plan_schema()
        }}
    });

    let url = format!("{}/responses", cfg.openai_base_url.trim_end_matches('/'));
    let resp = ureq::post(&url)
        .set("Authorization", &format!("Bearer {key}"))
        .set("Content-Type", "application/json")
        .send_json(body);

    let value: serde_json::Value = match resp {
        Ok(r) => r.into_json().context("parse vision response")?,
        Err(ureq::Error::Status(code, r)) => {
            return Err(anyhow!("vision API {code}: {}", r.into_string().unwrap_or_default()))
        }
        Err(ureq::Error::Transport(t)) => return Err(anyhow!("transport: {t}")),
    };

    let text = extract_text(&value)
        .ok_or_else(|| anyhow!("no structured output in vision response (shape mismatch)"))?;
    let mut plan: RetouchPlan =
        serde_json::from_str(strip_fence(&text)).context("parse retouch plan JSON")?;
    // Defend the engine + keep it "retouch not generation": clamp to small spots.
    plan.spots.retain(|s| s.radius > 0.0);
    for s in plan.spots.iter_mut() {
        s.cx = s.cx.clamp(0.0, 1.0);
        s.cy = s.cy.clamp(0.0, 1.0);
        s.radius = s.radius.clamp(0.003, 0.08);
        s.feather = if s.feather <= 0.0 { 0.4 } else { s.feather.clamp(0.0, 1.0) };
    }
    Ok(plan)
}

/// JSON schema (OpenAI strict mode) for the spot list the model returns. Mirrors
/// the small subset of [`HealSpot`] the AI sets; `feather`/`source` default.
fn plan_schema() -> serde_json::Value {
    let num = || json!({"type": "number"});
    json!({
        "type": "object", "additionalProperties": false,
        "required": ["spots", "rationale", "confidence"],
        "properties": {
            "spots": { "type": "array", "items": {
                "type": "object", "additionalProperties": false,
                "required": ["cx", "cy", "radius", "label"],
                "properties": { "cx": num(), "cy": num(), "radius": num(), "label": {"type": "string"} }
            }},
            "rationale": {"type": "string"},
            "confidence": num()
        }
    })
}

/// Pull the model's text out of a Responses-API reply (convenience field first,
/// then walk `output[].content[]`). Mirrors `advisor/openai.rs`.
fn extract_text(v: &serde_json::Value) -> Option<String> {
    if let Some(s) = v.get("output_text").and_then(|x| x.as_str()) {
        return Some(s.to_string());
    }
    for item in v.get("output")?.as_array()? {
        if let Some(content) = item.get("content").and_then(|c| c.as_array()) {
            for c in content {
                if c.get("type").and_then(|t| t.as_str()) == Some("output_text")
                    && let Some(s) = c.get("text").and_then(|t| t.as_str())
                {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

fn strip_fence(s: &str) -> &str {
    let t = s.trim();
    let t = t.strip_prefix("```json").or_else(|| t.strip_prefix("```")).unwrap_or(t);
    t.strip_suffix("```").unwrap_or(t).trim()
}

// --- orchestration ---------------------------------------------------------

/// Run the heal mode for one source: gather spots (AI auto-detect and/or a
/// painted mask), heal them on the developed pixels, and save a pixel master to
/// `out`. Non-XMP by nature (pixel edits don't serialise to ACR).
///
/// `full_res` heals the full-sensor develop (e.g. 61 MP) for a RAW; otherwise the
/// embedded preview (fast). Detection runs on a downscaled JPEG — coordinates are
/// normalised, so placement is resolution-independent.
pub fn heal(
    cfg: &Config,
    src_path: &Path,
    manual_mask: Option<&Path>,
    auto_detect: bool,
    full_res: bool,
    out: &Path,
) -> Result<HealReport> {
    let is_raw = decode::is_raw(src_path);
    let base = if full_res && is_raw {
        crate::render::render_to_image(src_path, &crate::recipe::EditRecipe::default(), None)?
    } else {
        decode::preview_only(src_path)?
    };
    let mut rgb = base.to_rgb8();
    let (w, h) = rgb.dimensions();

    let mut spots: Vec<HealSpot> = Vec::new();
    let mut rationale = String::new();
    if auto_detect {
        let small = image::DynamicImage::ImageRgb8(rgb.clone())
            .resize(1568, 1568, image::imageops::FilterType::Triangle);
        let mut jpeg = Vec::new();
        small
            .write_to(&mut std::io::Cursor::new(&mut jpeg), image::ImageFormat::Jpeg)
            .context("encode jpeg for detection")?;
        match detect_spots(cfg, &jpeg) {
            Ok(p) => {
                rationale = p.rationale.clone();
                spots.extend(p.spots);
            }
            Err(e) => {
                // If the user also painted, heal that and disclose the AI failure;
                // otherwise surface the error (don't silently do nothing).
                if manual_mask.is_none() {
                    return Err(e);
                }
                eprintln!("⚠ AI spot-detection failed ({e}); healing the painted mask only.");
            }
        }
    }
    if let Some(mp) = manual_mask {
        let m = image::open(mp)
            .with_context(|| format!("open mask {}", mp.display()))?
            .to_rgba8();
        spots.extend(plan_from_mask(&m));
    }

    let n = spots.len();
    heal_image(&mut rgb, &spots);
    crate::pipeline::ensure_parent(out)?;
    image::DynamicImage::ImageRgb8(rgb)
        .save(out)
        .with_context(|| format!("write {}", out.display()))?;
    Ok(HealReport { spots: n, rationale, dims: (w, h) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, Rgba};

    #[test]
    fn heal_replaces_a_spot_with_surroundings() {
        // 64×64 mid-gray with a black blotch at the centre; healing from the
        // uniform gray surroundings should pull the centre back toward gray.
        let mut img = RgbImage::from_pixel(64, 64, Rgb([128, 128, 128]));
        for y in 28..36 {
            for x in 28..36 {
                img.put_pixel(x, y, Rgb([0, 0, 0]));
            }
        }
        let spots = vec![HealSpot {
            cx: 0.5, cy: 0.5, radius: 7.0 / 64.0, feather: 0.4, source: None, label: "x".into(),
        }];
        heal_image(&mut img, &spots);
        let c = img.get_pixel(32, 32).0;
        assert!(c[0] > 100, "healed centre should approach gray, got {c:?}");
    }

    #[test]
    fn heal_with_explicit_source_removes_a_defect() {
        // White field with a small black defect; heal it using an EXPLICIT clean
        // donor offset → the defect is replaced and tone-matched to the white
        // surroundings (heal semantics: match surroundings, not transplant tone).
        let mut img = RgbImage::from_pixel(40, 20, Rgb([255, 255, 255]));
        for y in 8..12 {
            for x in 28..32 {
                img.put_pixel(x, y, Rgb([0, 0, 0])); // defect
            }
        }
        let spots = vec![HealSpot {
            cx: 30.0 / 40.0, cy: 0.5, radius: 4.0 / 20.0, feather: 0.2,
            source: Some([-0.3, 0.0]), label: "spot".into(),
        }];
        heal_image(&mut img, &spots);
        let c = img.get_pixel(30, 10).0;
        assert!(c[0] > 200, "defect should heal to the white surroundings, got {c:?}");
    }

    #[test]
    fn mask_blob_becomes_one_spot() {
        // One painted 20×20 blob (alpha=0) centred → exactly one heal spot there.
        let mut m = RgbaImage::from_pixel(100, 100, Rgba([0, 0, 0, 255])); // opaque = keep
        for y in 40..60 {
            for x in 40..60 {
                m.put_pixel(x, y, Rgba([255, 0, 0, 0])); // painted
            }
        }
        let spots = plan_from_mask(&m);
        assert_eq!(spots.len(), 1, "one blob → one spot");
        assert!((spots[0].cx - 0.495).abs() < 0.05 && (spots[0].cy - 0.495).abs() < 0.05);
    }

    #[test]
    fn empty_inputs_are_noops() {
        let mut img = RgbImage::from_pixel(8, 8, Rgb([10, 20, 30]));
        let before = img.clone();
        heal_image(&mut img, &[]);
        assert_eq!(img, before, "no spots → image unchanged");
        let clean = RgbaImage::from_pixel(16, 16, Rgba([0, 0, 0, 255])); // nothing painted
        assert!(plan_from_mask(&clean).is_empty());
    }
}
