//! Render engine v1 — apply an [`EditRecipe`] to the full-resolution RAW and
//! produce a developed image (no Lightroom needed).
//!
//! Pipeline: `rawler` demosaics + colour-calibrates the sensor data to a
//! full-res sRGB-gamma float image (`RawDevelop::develop_intermediate`), then we
//! apply the recipe. The tonal ops (exposure, contrast, whites/blacks,
//! highlights/shadows, tone curve) are all 1-D functions of a channel value, so
//! they collapse into a single per-channel lookup table; saturation/vibrance run
//! per pixel; then orientation + crop.
//!
//! HONEST SCOPE (v1): these ops are tasteful **approximations**, not bit-exact
//! Lightroom. NOT yet applied here: white-balance *temperature/tint* re-balance
//! (the develop step keeps as-shot WB), clarity, dehaze, sharpening, noise
//! reduction — those local/convolution ops are deferred; the XMP→Lightroom path
//! renders them faithfully in the meantime.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use image::{DynamicImage, RgbImage};
use rawler::decoders::RawDecodeParams;
use rawler::get_decoder;
use rawler::imgop::develop::{Intermediate, RawDevelop};
use rawler::rawsource::RawSource;
use rawler::Orientation;

use crate::recipe::EditRecipe;

const LUT_N: usize = 4096;

/// Develop `raw_path` and apply `recipe`, returning the finished image.
pub fn render_to_image(raw_path: &Path, recipe: &EditRecipe) -> Result<DynamicImage> {
    let src = RawSource::new(raw_path)
        .with_context(|| format!("open RAW {}", raw_path.display()))?;
    let decoder =
        get_decoder(&src).map_err(|e| anyhow!("no decoder for {}: {e}", raw_path.display()))?;
    let params = RawDecodeParams { image_index: 0 };

    // Full sensor data (dummy = false) → demosaic + colour pipeline → sRGB float.
    let rawimage = decoder
        .raw_image(&src, &params, false)
        .map_err(|e| anyhow!("raw_image: {e}"))?;
    let orientation = rawimage.orientation;

    let inter = RawDevelop::default()
        .develop_intermediate(&rawimage)
        .map_err(|e| anyhow!("develop: {e}"))?;
    let rgb = match inter {
        Intermediate::ThreeColor(c) => c,
        Intermediate::Monochrome(_) => bail!("monochrome RAW not supported by render v1"),
        Intermediate::FourColor(_) => bail!("4-colour develop output not supported by render v1"),
    };
    let (w, h) = (rgb.width, rgb.height);
    let mut data: Vec<[f32; 3]> = rgb.data; // sRGB-gamma, ~[0,1]; owned (no copy)

    // --- tonal ops as one per-channel LUT, then sat/vibrance per pixel --------
    let lut = build_tone_lut(recipe);
    let sat = recipe.saturation / 100.0;
    let vib = recipe.vibrance / 100.0;
    for px in data.iter_mut() {
        let r = sample_lut(&lut, px[0]);
        let g = sample_lut(&lut, px[1]);
        let b = sample_lut(&lut, px[2]);
        *px = apply_sat_vibrance(r, g, b, sat, vib);
    }

    // --- pack to 8-bit -------------------------------------------------------
    let mut buf = Vec::with_capacity(w * h * 3);
    for px in &data {
        buf.push(to_u8(px[0]));
        buf.push(to_u8(px[1]));
        buf.push(to_u8(px[2]));
    }
    let img = RgbImage::from_raw(w as u32, h as u32, buf)
        .ok_or_else(|| anyhow!("pixel buffer size mismatch"))?;
    let mut dynimg = oriented(DynamicImage::ImageRgb8(img), orientation);

    // --- crop (normalised [0,1] on the displayed frame) ----------------------
    if let Some(c) = &recipe.crop {
        let (iw, ih) = (dynimg.width() as f32, dynimg.height() as f32);
        let x = (c.left.clamp(0.0, 1.0) * iw).round() as u32;
        let y = (c.top.clamp(0.0, 1.0) * ih).round() as u32;
        let cw = (((c.right - c.left).clamp(0.0, 1.0)) * iw).round() as u32;
        let ch = (((c.bottom - c.top).clamp(0.0, 1.0)) * ih).round() as u32;
        if cw > 0 && ch > 0 {
            dynimg = dynimg.crop_imm(x, y, cw, ch);
        }
    }

    Ok(dynimg)
}

/// Render and save to `out` (format from the extension: .jpg/.png/.tif).
pub fn render_to_file(raw_path: &Path, recipe: &EditRecipe, out: &Path) -> Result<(u32, u32)> {
    let img = render_to_image(raw_path, recipe)?;
    img.save(out)
        .with_context(|| format!("save render {}", out.display()))?;
    Ok((img.width(), img.height()))
}

// ---------------------------------------------------------------------------

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}
fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}
fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Build a 1-D tone curve combining exposure (in linear light), contrast,
/// whites/blacks/highlights/shadows (region-weighted), and the recipe's tone
/// curve, over input gamma values [0,1].
fn build_tone_lut(r: &EditRecipe) -> Vec<f32> {
    let gain = 2.0_f32.powf(r.exposure_ev);
    let contrast = r.contrast / 100.0;
    let (whites, blacks) = (r.whites / 100.0, r.blacks / 100.0);
    let (highlights, shadows) = (r.highlights / 100.0, r.shadows / 100.0);
    // Pre-build the user tone curve as a 256-entry LUT (identity if empty).
    let curve = tone_curve_lut(r);

    (0..LUT_N)
        .map(|i| {
            let x = i as f32 / (LUT_N - 1) as f32;

            // 1) exposure in linear light
            let mut v = linear_to_srgb((srgb_to_linear(x) * gain).clamp(0.0, 1.0));

            // 2) contrast (linear slope around mid-grey)
            v = (0.5 + (v - 0.5) * (1.0 + contrast)).clamp(0.0, 1.0);

            // 3) region tones (gentle ±0.15 weighting toward each end)
            let hi = smoothstep(0.5, 1.0, v);
            let lo = 1.0 - smoothstep(0.0, 0.5, v);
            v = (v + 0.15 * (whites * hi + highlights * hi + blacks * lo + shadows * lo))
                .clamp(0.0, 1.0);

            // 4) user tone curve
            sample_lut(&curve, v)
        })
        .collect()
}

/// The recipe's tone curve as a 256-entry [0,1]→[0,1] LUT; identity if empty.
fn tone_curve_lut(r: &EditRecipe) -> Vec<f32> {
    if r.tone_curve.is_empty() {
        return (0..256).map(|i| i as f32 / 255.0).collect();
    }
    let mut pts: Vec<(f32, f32)> = r
        .tone_curve
        .iter()
        .map(|p| (p.input as f32 / 255.0, p.output as f32 / 255.0))
        .collect();
    pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    (0..256)
        .map(|i| {
            let x = i as f32 / 255.0;
            interp(&pts, x)
        })
        .collect()
}

/// Piecewise-linear interpolation over sorted (x,y) control points, clamped at
/// the ends.
fn interp(pts: &[(f32, f32)], x: f32) -> f32 {
    if pts.is_empty() {
        return x;
    }
    if x <= pts[0].0 {
        return pts[0].1;
    }
    if x >= pts[pts.len() - 1].0 {
        return pts[pts.len() - 1].1;
    }
    for w in pts.windows(2) {
        let (x0, y0) = w[0];
        let (x1, y1) = w[1];
        if x >= x0 && x <= x1 {
            let t = if (x1 - x0).abs() < 1e-6 { 0.0 } else { (x - x0) / (x1 - x0) };
            return y0 + (y1 - y0) * t;
        }
    }
    x
}

/// Sample a LUT (any length) at a normalised [0,1] position with linear interp.
fn sample_lut(lut: &[f32], x: f32) -> f32 {
    let n = lut.len();
    if n == 0 {
        return x;
    }
    let pos = x.clamp(0.0, 1.0) * (n - 1) as f32;
    let i = pos.floor() as usize;
    if i >= n - 1 {
        return lut[n - 1];
    }
    let t = pos - i as f32;
    lut[i] * (1.0 - t) + lut[i + 1] * t
}

/// Saturation + vibrance around the pixel's luma. Vibrance boosts low-saturation
/// pixels more (so already-vivid colours don't blow out).
fn apply_sat_vibrance(r: f32, g: f32, b: f32, sat: f32, vib: f32) -> [f32; 3] {
    let l = 0.299 * r + 0.587 * g + 0.114 * b;
    let mx = r.max(g).max(b);
    let mn = r.min(g).min(b);
    let pixel_sat = if mx > 1e-4 { (mx - mn) / mx } else { 0.0 };
    let factor = (1.0 + sat + vib * (1.0 - pixel_sat)).max(0.0);
    [
        (l + (r - l) * factor).clamp(0.0, 1.0),
        (l + (g - l) * factor).clamp(0.0, 1.0),
        (l + (b - l) * factor).clamp(0.0, 1.0),
    ]
}

fn to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Apply the RAW's stored orientation so portraits/flips display correctly.
fn oriented(img: DynamicImage, o: Orientation) -> DynamicImage {
    match o {
        Orientation::Normal | Orientation::Unknown => img,
        Orientation::HorizontalFlip => img.fliph(),
        Orientation::Rotate180 => img.rotate180(),
        Orientation::VerticalFlip => img.flipv(),
        Orientation::Rotate90 => img.rotate90(),
        Orientation::Rotate270 => img.rotate270(),
        Orientation::Transpose => img.rotate90().fliph(),
        Orientation::Transverse => img.rotate270().fliph(),
    }
}
