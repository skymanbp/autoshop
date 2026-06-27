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
use image::{DynamicImage, ImageBuffer, ImageEncoder, Rgb, RgbImage};
use rawler::decoders::RawDecodeParams;
use rawler::get_decoder;
use rawler::imgop::develop::{Intermediate, RawDevelop};
use rawler::rawsource::RawSource;
use rawler::Orientation;

use crate::recipe::{EditRecipe, MaskGeometry};

const LUT_N: usize = 4096;

/// Develop `raw_path` and apply `recipe`, returning the finished image. When
/// `denoise` is set, the demosaiced buffer is AI-denoised (via the Python
/// sidecar) before any tonal/colour work — i.e. denoise-before-sharpen.
pub fn render_to_image(
    raw_path: &Path,
    recipe: &EditRecipe,
    denoise: Option<&crate::denoise::DenoiseOpts>,
) -> Result<DynamicImage> {
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

    // --- AI denoise (opt-in) on the clean demosaiced pixels, before tone/sharpen
    if let Some(opts) = denoise {
        println!("AI denoise ({}) on {}x{} ...", opts.model, w, h);
        crate::denoise::denoise_buffer(opts, &mut data, w, h).context("AI denoise")?;
    }

    // --- white balance (target Kelvin/tint) in linear light -------------------
    // The buffer is already at as-shot WB. We anchor as-shot at 5500 K (daylight)
    // and shift toward the target — a direction-correct approximation. A precise
    // as-shot-K estimate needs the camera colour matrix (raw→XYZ), not a naive
    // blackbody match; that's the future upgrade. develop_preview skips WB.
    if let Some(target_k) = recipe.temperature_k {
        apply_wb(&mut data, 5500.0, target_k, recipe.tint);
    }

    // --- tone + clarity + sat/vibrance + NR + sharpen (shared pipeline) -------
    apply_develop(&mut data, w, h, recipe);

    // --- pack to 16-bit (highest precision; JPEG downconverts at encode) ------
    let mut buf: Vec<u16> = Vec::with_capacity(w * h * 3);
    for px in &data {
        buf.push(to_u16(px[0]));
        buf.push(to_u16(px[1]));
        buf.push(to_u16(px[2]));
    }
    let img: ImageBuffer<Rgb<u16>, _> = ImageBuffer::from_raw(w as u32, h as u32, buf)
        .ok_or_else(|| anyhow!("pixel buffer size mismatch"))?;
    let mut dynimg = oriented(DynamicImage::ImageRgb16(img), orientation);

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

/// Develop an already-baked image (the "PNG source" mode: edit an LR/PS-denoised
/// export). Runs the SAME tonal/colour pipeline as the RAW engine on the loaded
/// pixels — but no demosaic and no Kelvin white balance, since a baked sRGB image
/// carries no raw WB coefficients (temperature_k is a no-op here; relative tweaks
/// still apply). Optional AI denoise runs first; output is 16-bit.
pub fn render_baked_to_image(
    img: &DynamicImage,
    recipe: &EditRecipe,
    denoise: Option<&crate::denoise::DenoiseOpts>,
) -> Result<DynamicImage> {
    let rgb = img.to_rgb16();
    let (w, h) = (rgb.width() as usize, rgb.height() as usize);
    let mut data: Vec<[f32; 3]> = rgb
        .pixels()
        .map(|p| [p[0] as f32 / 65535.0, p[1] as f32 / 65535.0, p[2] as f32 / 65535.0])
        .collect();

    if let Some(opts) = denoise {
        println!("AI denoise ({}) on {}x{} ...", opts.model, w, h);
        crate::denoise::denoise_buffer(opts, &mut data, w, h).context("AI denoise")?;
    }

    apply_develop(&mut data, w, h, recipe);

    let mut buf: Vec<u16> = Vec::with_capacity(w * h * 3);
    for px in &data {
        buf.push(to_u16(px[0]));
        buf.push(to_u16(px[1]));
        buf.push(to_u16(px[2]));
    }
    let out: ImageBuffer<Rgb<u16>, _> = ImageBuffer::from_raw(w as u32, h as u32, buf)
        .ok_or_else(|| anyhow!("baked pixel buffer size mismatch"))?;
    let mut dynimg = DynamicImage::ImageRgb16(out);

    // Crop (normalised [0,1]) — orientation is already baked into the source.
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

/// Render and save to `out` at the highest fidelity the format allows:
/// `.tif`/`.png` keep the full **16-bit** depth; `.jpg` downconverts to 8-bit at
/// quality 95. Extension picks the format. Dispatches RAW (demosaic engine) vs
/// baked image (the PNG-source engine) automatically.
pub fn render_to_file(
    src_path: &Path,
    recipe: &EditRecipe,
    out: &Path,
    denoise: Option<&crate::denoise::DenoiseOpts>,
) -> Result<(u32, u32)> {
    let img = if crate::decode::is_raw(src_path) {
        render_to_image(src_path, recipe, denoise)?
    } else {
        let src = crate::decode::load_image(src_path)?;
        render_baked_to_image(&src, recipe, denoise)?
    };
    let (w, h) = (img.width(), img.height());
    let ext = out
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => {
            // JPEG is 8-bit only — downconvert from 16-bit and encode at q95.
            let rgb8 = img.to_rgb8();
            let file = std::fs::File::create(out)
                .with_context(|| format!("create {}", out.display()))?;
            let mut w = std::io::BufWriter::new(file);
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut w, 95)
                .write_image(rgb8.as_raw(), rgb8.width(), rgb8.height(), image::ExtendedColorType::Rgb8)
                .with_context(|| format!("encode jpeg {}", out.display()))?;
        }
        // .tif / .png (and anything else) keep the full 16-bit data.
        _ => img
            .save(out)
            .with_context(|| format!("save render {}", out.display()))?,
    }
    Ok((w, h))
}

/// Fast "after" render for the UI: apply the recipe's tonal + colour ops to an
/// already-demosaiced preview image (no full-res develop, no demosaic). Crop is
/// intentionally NOT applied here so sliders give immediate full-frame feedback;
/// the full-res `render_to_image` path applies crop on export.
pub fn develop_preview(preview: &DynamicImage, recipe: &EditRecipe) -> DynamicImage {
    let rgb = preview.to_rgb8();
    let (w, h) = rgb.dimensions();
    let mut data: Vec<[f32; 3]> = rgb
        .pixels()
        .map(|p| [p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0])
        .collect();
    apply_develop(&mut data, w as usize, h as usize, recipe);
    let mut buf = Vec::with_capacity((w * h * 3) as usize);
    for px in &data {
        buf.push(to_u8(px[0]));
        buf.push(to_u8(px[1]));
        buf.push(to_u8(px[2]));
    }
    DynamicImage::ImageRgb8(RgbImage::from_raw(w, h, buf).expect("preview buffer size matches"))
}

/// The full per-pixel + spatial develop pipeline (everything except WB, crop,
/// orientation), shared by full-res render and the UI preview. Order follows
/// ACR: tone → clarity → saturation/vibrance → noise reduction → sharpening.
/// Operates in place on sRGB-gamma RGB in [0,1].
fn apply_develop(data: &mut [[f32; 3]], w: usize, h: usize, r: &EditRecipe) {
    // 1) tonal ops via the per-channel LUT (exposure/contrast/whites/blacks/
    //    highlights/shadows/tone-curve).
    let lut = build_tone_lut(r);
    for px in data.iter_mut() {
        px[0] = sample_lut(&lut, px[0]);
        px[1] = sample_lut(&lut, px[1]);
        px[2] = sample_lut(&lut, px[2]);
    }
    // 2) clarity — large-radius, midtone-masked local contrast.
    if r.clarity != 0.0 {
        let radius = ((0.02 * w.min(h) as f32).round() as usize).max(8);
        unsharp_luma(data, w, h, radius, r.clarity / 100.0, true);
    }
    // 3) saturation / vibrance.
    let (sat, vib) = (r.saturation / 100.0, r.vibrance / 100.0);
    if sat != 0.0 || vib != 0.0 {
        for px in data.iter_mut() {
            *px = apply_sat_vibrance(px[0], px[1], px[2], sat, vib);
        }
    }
    // 4) noise reduction — BEFORE sharpening (the order that matters most).
    if r.noise_reduction > 0.0 {
        noise_reduce_luma(data, w, h, r.noise_reduction / 100.0);
    }
    // 5) sharpening — small-radius unsharp mask.
    if r.sharpening > 0.0 {
        unsharp_luma(data, w, h, 1, r.sharpening / 100.0, false);
    }
    // 6) local masked adjustments (linear/radial gradients).
    if !r.masks.is_empty() {
        apply_masks(data, w, h, r);
    }
}

fn luma601(p: &[f32; 3]) -> f32 {
    0.299 * p[0] + 0.587 * p[1] + 0.114 * p[2]
}

/// Apply each local masked adjustment: blend the masked region toward a locally
/// re-toned version, weighted by the gradient mask × amount. Applies local tone
/// (exposure/contrast/highlights/shadows/whites/blacks) + saturation, then local
/// **noise reduction** (smooth luma toward its neighbourhood, inside the mask —
/// for "this region is noisy" requests). Local clarity/dehaze/texture/temp/tint
/// are deferred (the XMP→Lightroom path renders those). Mask coords are
/// normalised so this works at any resolution.
fn apply_masks(data: &mut [[f32; 3]], w: usize, h: usize, r: &EditRecipe) {
    for m in &r.masks {
        let local = EditRecipe {
            exposure_ev: m.exposure_ev,
            contrast: m.contrast,
            highlights: m.highlights,
            shadows: m.shadows,
            whites: m.whites,
            blacks: m.blacks,
            ..EditRecipe::default()
        };
        let lut = build_tone_lut(&local);
        let sat = m.saturation / 100.0;
        let amount = m.amount.clamp(0.0, 1.0);
        // mask coverage × master amount at a pixel (with optional inversion).
        let weight_at = |x: usize, y: usize| -> f32 {
            let mut wgt = mask_weight(&m.mask, x as f32 / w as f32, y as f32 / h as f32);
            if m.inverted {
                wgt = 1.0 - wgt;
            }
            wgt * amount
        };

        // --- tone + saturation pass ---
        for y in 0..h {
            for x in 0..w {
                let wgt = weight_at(x, y);
                if wgt <= 0.001 {
                    continue;
                }
                let i = y * w + x;
                let p = data[i];
                let t = [sample_lut(&lut, p[0]), sample_lut(&lut, p[1]), sample_lut(&lut, p[2])];
                let t = apply_sat_vibrance(t[0], t[1], t[2], sat, 0.0);
                for c in 0..3 {
                    data[i][c] = p[c] * (1.0 - wgt) + t[c] * wgt;
                }
            }
        }

        // --- local noise reduction pass (only where the mask covers) ---
        let nr = (m.noise_reduction / 100.0).clamp(0.0, 1.0);
        if nr > 0.0 {
            let luma: Vec<f32> = data.iter().map(luma601).collect();
            let blur = blur_plane(&luma, w, h, 2);
            for y in 0..h {
                for x in 0..w {
                    let nw = weight_at(x, y) * nr;
                    if nw <= 0.001 {
                        continue;
                    }
                    let i = y * w + x;
                    let l = luma[i];
                    let new_l = l + (blur[i] - l) * nw;
                    scale_chroma(&mut data[i], l, new_l);
                }
            }
        }
    }
}

/// Mask coverage [0,1] at normalised frame coordinate (nx, ny).
fn mask_weight(g: &MaskGeometry, nx: f32, ny: f32) -> f32 {
    match g {
        MaskGeometry::Linear { zero_x, zero_y, full_x, full_y } => {
            let (vx, vy) = (full_x - zero_x, full_y - zero_y);
            let len2 = vx * vx + vy * vy;
            if len2 < 1e-9 {
                return 1.0;
            }
            (((nx - zero_x) * vx + (ny - zero_y) * vy) / len2).clamp(0.0, 1.0)
        }
        // Roundness is ignored in v1 (pure ellipse).
        MaskGeometry::Radial { top, left, bottom, right, feather, roundness: _, flipped } => {
            let cx = (left + right) / 2.0;
            let cy = (top + bottom) / 2.0;
            let rx = ((right - left) / 2.0).abs().max(1e-4);
            let ry = ((bottom - top) / 2.0).abs().max(1e-4);
            let d = (((nx - cx) / rx).powi(2) + ((ny - cy) / ry).powi(2)).sqrt();
            let f = feather.clamp(0.0, 1.0);
            let wgt = 1.0 - smoothstep(1.0 - f, 1.0, d);
            if *flipped {
                1.0 - wgt
            } else {
                wgt
            }
        }
    }
}

/// Scale a pixel's chroma so its luma moves `l_old`→`l_new` while preserving hue.
fn scale_chroma(px: &mut [f32; 3], l_old: f32, l_new: f32) {
    if l_old > 1e-4 {
        let k = l_new / l_old;
        px[0] = (px[0] * k).clamp(0.0, 1.0);
        px[1] = (px[1] * k).clamp(0.0, 1.0);
        px[2] = (px[2] * k).clamp(0.0, 1.0);
    } else {
        *px = [l_new, l_new, l_new];
    }
}

/// Unsharp mask on luminance (chroma-preserving). `amount` scales the detail;
/// `midtone` weights the effect toward midtones (for clarity).
fn unsharp_luma(data: &mut [[f32; 3]], w: usize, h: usize, radius: usize, amount: f32, midtone: bool) {
    let luma: Vec<f32> = data.iter().map(luma601).collect();
    let blurred = blur_plane(&luma, w, h, radius);
    for (i, px) in data.iter_mut().enumerate() {
        let l = luma[i];
        let detail = l - blurred[i];
        let m = if midtone { 1.0 - (2.0 * l - 1.0).powi(2) } else { 1.0 };
        let new_l = (l + amount * detail * m).clamp(0.0, 1.0);
        scale_chroma(px, l, new_l);
    }
}

/// Bilateral-lite luminance denoise: smooth flat areas, keep edges. `t` in 0..1.
/// `denoised = l − t·w_edge·detail`, w_edge≈1 in flat regions, ≈0 at edges.
fn noise_reduce_luma(data: &mut [[f32; 3]], w: usize, h: usize, t: f32) {
    let luma: Vec<f32> = data.iter().map(luma601).collect();
    let radius = (1.0 + 2.0 * t).round().max(1.0) as usize;
    let blurred = blur_plane(&luma, w, h, radius);
    let range = 0.05_f32;
    for (i, px) in data.iter_mut().enumerate() {
        let l = luma[i];
        let detail = l - blurred[i];
        let w_edge = (-(detail / range) * (detail / range)).exp();
        let new_l = (l - t * w_edge * detail).clamp(0.0, 1.0);
        scale_chroma(px, l, new_l);
    }
}

/// Approximate a Gaussian blur with 3 separable box-blur passes. Box blur uses a
/// running sum, so cost is O(N) regardless of `radius` — essential for clarity's
/// large radius on a 60 MP frame.
fn blur_plane(src: &[f32], w: usize, h: usize, radius: usize) -> Vec<f32> {
    if radius == 0 {
        return src.to_vec();
    }
    let mut buf = src.to_vec();
    for _ in 0..3 {
        buf = box_blur_h(&buf, w, h, radius);
        buf = box_blur_v(&buf, w, h, radius);
    }
    buf
}

fn box_blur_h(src: &[f32], w: usize, h: usize, radius: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; src.len()];
    let r = radius as isize;
    let win = (2 * radius + 1) as f32;
    for y in 0..h {
        let base = y * w;
        let mut sum = 0.0f32;
        for k in -r..=r {
            sum += src[base + k.clamp(0, w as isize - 1) as usize];
        }
        out[base] = sum / win;
        for x in 1..w {
            let add = (x as isize + r).min(w as isize - 1) as usize;
            let sub = (x as isize - 1 - r).max(0) as usize;
            sum += src[base + add] - src[base + sub];
            out[base + x] = sum / win;
        }
    }
    out
}

fn box_blur_v(src: &[f32], w: usize, h: usize, radius: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; src.len()];
    let r = radius as isize;
    let win = (2 * radius + 1) as f32;
    for x in 0..w {
        let mut sum = 0.0f32;
        for k in -r..=r {
            sum += src[k.clamp(0, h as isize - 1) as usize * w + x];
        }
        out[x] = sum / win;
        for y in 1..h {
            let add = (y as isize + r).min(h as isize - 1) as usize;
            let sub = (y as isize - 1 - r).max(0) as usize;
            sum += src[add * w + x] - src[sub * w + x];
            out[y * w + x] = sum / win;
        }
    }
    out
}

// ---------------------------------------------------------------------------

/// Blackbody colour at temperature `k` Kelvin as RGB in [0,1].
/// Tanner-Helland piecewise fit [verified: tannerhelland.com/2012/09/18,
/// R²>0.987]. Valid 1000–40000 K.
fn kelvin_to_rgb(k: f32) -> [f32; 3] {
    let t = k.clamp(1000.0, 40000.0) / 100.0;
    let red = if t <= 66.0 {
        255.0
    } else {
        (329.698_73 * (t - 60.0).powf(-0.133_204_76)).clamp(0.0, 255.0)
    };
    let green = if t <= 66.0 {
        (99.470_8 * t.ln() - 161.119_57).clamp(0.0, 255.0)
    } else {
        (288.122_16 * (t - 60.0).powf(-0.075_514_846)).clamp(0.0, 255.0)
    };
    let blue = if t >= 66.0 {
        255.0
    } else if t <= 19.0 {
        0.0
    } else {
        (138.517_73 * (t - 10.0).ln() - 305.044_8).clamp(0.0, 255.0)
    };
    [red / 255.0, green / 255.0, blue / 255.0]
}

/// Per-channel gains to move WB from `as_shot_k` to `target_k` (+ tint), green
/// normalised to 1.0 (WB changes colour, not brightness). Lightroom convention:
/// higher target K = warmer result (boosts red, cuts blue).
fn wb_gains(as_shot_k: f32, target_k: f32, tint: f32) -> [f32; 3] {
    let a = kelvin_to_rgb(as_shot_k);
    let t = kelvin_to_rgb(target_k);
    let g1 = a[1] / t[1].max(1e-4);
    let gr = (a[0] / t[0].max(1e-4)) / g1;
    let gb = (a[2] / t[2].max(1e-4)) / g1;
    // Tint: positive = magenta (less green), negative = green.
    let gg = 1.0 - 0.20 * (tint / 100.0);
    [gr, gg, gb]
}

/// Apply white-balance gains in linear light. No-op when gains are ~neutral.
fn apply_wb(data: &mut [[f32; 3]], as_shot_k: f32, target_k: f32, tint: f32) {
    let g = wb_gains(as_shot_k, target_k, tint);
    if (g[0] - 1.0).abs() < 1e-3 && (g[1] - 1.0).abs() < 1e-3 && (g[2] - 1.0).abs() < 1e-3 {
        return;
    }
    for px in data.iter_mut() {
        for c in 0..3 {
            let lin = srgb_to_linear(px[c]) * g[c];
            px[c] = linear_to_srgb(lin.clamp(0.0, 1.0));
        }
    }
}

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

fn to_u16(v: f32) -> u16 {
    (v.clamp(0.0, 1.0) * 65535.0).round() as u16
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe::{EditRecipe, LocalAdjustment};

    #[test]
    fn linear_mask_affects_only_the_full_end() {
        // Linear mask: zero at top (ny=0), full at bottom (ny=1) + strong darken.
        let r = EditRecipe {
            masks: vec![LocalAdjustment {
                mask: MaskGeometry::Linear { zero_x: 0.5, zero_y: 0.0, full_x: 0.5, full_y: 1.0 },
                amount: 1.0,
                exposure_ev: -4.0,
                ..Default::default()
            }],
            ..Default::default()
        };
        let (w, h) = (1usize, 4usize);
        let mut data = vec![[0.6_f32, 0.6, 0.6]; w * h];
        apply_develop(&mut data, w, h, &r);
        assert!((data[0][0] - 0.6).abs() < 0.03, "top should be ~unchanged: {}", data[0][0]);
        assert!(data[3][0] < 0.5, "bottom should darken: {}", data[3][0]);
    }

    #[test]
    fn local_noise_reduction_smooths_only_inside_the_mask() {
        // 8x1 strip of alternating luma (= noise). A linear mask covering the
        // RIGHT half with full local NR should flatten the right; left untouched.
        let (w, h) = (8usize, 1usize);
        let mut data: Vec<[f32; 3]> =
            (0..w).map(|x| { let v = if x % 2 == 0 { 0.3 } else { 0.7 }; [v, v, v] }).collect();
        let r = EditRecipe {
            masks: vec![LocalAdjustment {
                mask: MaskGeometry::Linear { zero_x: 0.5, zero_y: 0.5, full_x: 1.0, full_y: 0.5 },
                amount: 1.0,
                noise_reduction: 100.0,
                ..Default::default()
            }],
            ..Default::default()
        };
        let var = |d: &[[f32; 3]], rng: std::ops::Range<usize>| {
            let v: Vec<f32> = rng.map(|i| d[i][0]).collect();
            let m = v.iter().sum::<f32>() / v.len() as f32;
            v.iter().map(|x| (x - m).powi(2)).sum::<f32>() / v.len() as f32
        };
        let (left0, right0) = (var(&data, 0..4), var(&data, 4..8));
        apply_develop(&mut data, w, h, &r);
        assert!(var(&data, 4..8) < right0 * 0.8, "right half should smooth");
        assert!((var(&data, 0..4) - left0).abs() < 1e-4, "left half untouched");
    }

    #[test]
    fn kelvin_to_rgb_warm_is_redder_than_cool() {
        let warm = kelvin_to_rgb(3000.0);
        let cool = kelvin_to_rgb(9000.0);
        assert!(warm[0] >= cool[0], "warm red {} >= cool red {}", warm[0], cool[0]);
        assert!(warm[2] <= cool[2], "warm blue {} <= cool blue {}", warm[2], cool[2]);
    }

    #[test]
    fn wb_warmer_target_boosts_red_cuts_blue() {
        // Target warmer (higher K) than as-shot ⇒ Lightroom warms: red gain > 1, blue < 1.
        let g = wb_gains(5000.0, 7000.0, 0.0);
        assert!(g[0] > 1.0, "red gain {}", g[0]);
        assert!(g[2] < 1.0, "blue gain {}", g[2]);
        // Neutral (same K, no tint) ⇒ all gains ~1.
        let n = wb_gains(5500.0, 5500.0, 0.0);
        assert!((n[0] - 1.0).abs() < 1e-3 && (n[2] - 1.0).abs() < 1e-3);
    }

    #[test]
    fn box_blur_preserves_uniform_plane() {
        // A flat plane must stay flat (DC preserved) after blurring.
        let (w, h) = (40usize, 30usize);
        let plane = vec![0.4_f32; w * h];
        let blurred = blur_plane(&plane, w, h, 5);
        assert!(blurred.iter().all(|&v| (v - 0.4).abs() < 1e-4));
    }

    #[test]
    fn neutral_recipe_is_near_identity() {
        // All-zero recipe ⇒ no clarity/sat/NR/sharpen, near-identity tone LUT.
        let mut data = vec![[0.2_f32, 0.5, 0.8], [0.9, 0.1, 0.4]];
        let orig = data.clone();
        apply_develop(&mut data, 2, 1, &EditRecipe::default());
        for (a, b) in data.iter().zip(orig.iter()) {
            for c in 0..3 {
                assert!((a[c] - b[c]).abs() < 0.02, "channel drift {} vs {}", a[c], b[c]);
            }
        }
    }

    #[test]
    fn sharpening_raises_local_contrast_at_an_edge() {
        // A vertical edge: sharpening should push the dark side darker / bright
        // side brighter (overshoot), increasing the edge step.
        let (w, h) = (8usize, 1usize);
        let mut data: Vec<[f32; 3]> = (0..w)
            .map(|x| { let v = if x < 4 { 0.3 } else { 0.7 }; [v, v, v] })
            .collect();
        let before = data[4][0] - data[3][0];
        let r = EditRecipe { sharpening: 120.0, ..Default::default() };
        apply_develop(&mut data, w, h, &r);
        let after = data[4][0] - data[3][0];
        assert!(after > before, "edge step {after} should exceed {before}");
    }
}
