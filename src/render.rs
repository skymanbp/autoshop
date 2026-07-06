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
    // 1) tonal ops via the LUT (exposure/contrast/whites/blacks/highlights/
    //    shadows/tone-curve). Tone the pixel's LUMINANCE and scale RGB by the
    //    ratio (scale_chroma) so hue + saturation are preserved — NOT per-channel.
    //    Running each channel through the curve independently lets opposing pushes
    //    (e.g. strong −highlights + +shadows) converge the channels, desaturating
    //    saturated colour to grey. The LUT itself is monotone with a pinned white
    //    point (see build_tone_lut), so no per-channel greying and no flat/inverted
    //    midtones — the tone model is correct by construction, not patched.
    let lut = build_tone_lut(r);
    for px in data.iter_mut() {
        let l_old = luma601(px);
        let l_new = sample_lut(&lut, l_old);
        scale_chroma(px, l_old, l_new);
    }
    // 1b) per-channel RGB curves (red/green/blue), right after the master curve.
    apply_rgb_curves(data, r);
    // 2) per-colour HSL (the 8 ACR bands): rotate/scale each colour family,
    //    after global tone and before clarity/saturation (ACR ordering).
    apply_hsl(data, &r.hsl);
    // 2b) colour grading wheels (shadow/midtone/highlight/global toning + lum).
    apply_color_grade(data, &r.color_grade);
    // 3) clarity — large-radius, midtone-masked local contrast.
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
                // Luminance-preserving local tone (same anti-greying reason as the
                // global pass), then local saturation.
                let mut t = p;
                let l_old = luma601(&p);
                let l_new = sample_lut(&lut, l_old);
                scale_chroma(&mut t, l_old, l_new);
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

/// Tone-model knot inputs. 0.66 is an explicit vertex so mid-bright water (≈0.66)
/// stays separated from the midtone (0.50) under a strong −Highlights; 0.82 shapes
/// the highlight shoulder; 0.92 is where whites concentrate. Shared with the
/// reverse-fit (fit.rs), which solves slider values against this same model.
pub(crate) const TONE_KNOTS_X: [f32; 8] = [0.0, 0.10, 0.25, 0.50, 0.66, 0.82, 0.92, 1.0];

/// Per-slider knot-output basis at input `x`: how far a fully-pushed +100 slider
/// moves the knot, in order `[contrast, highlights, shadows, whites, blacks]`.
/// The knot output is `tone_exposure_curve(x, ev) + basis · sliders/100`; keeping
/// this the ONLY definition means render and reverse-fit cannot drift apart.
pub(crate) fn tone_slider_basis(x: f32) -> [f32; 5] {
    // Authority: how far a fully-pushed ±100 slider moves its knot(s).
    const A_SHADOW: f32 = 0.33;
    const A_HIGHLIGHT: f32 = 0.34;
    const A_CONTRAST: f32 = 0.20;
    const A_WB: f32 = 0.32; // whites & blacks share it

    // Region basis functions over knot input x (each ∈ [0,1]).
    let w_shadow = smoothstep(0.0, 0.25, x) * (1.0 - smoothstep(0.25, 0.50, x));
    // highlights: peak 0.82, ZERO at 0.50 and PINNED to 0 at 1.0 — so highlights can
    // never move the white point; specular foam near white is never dragged down.
    let w_high = smoothstep(0.60, 0.82, x) * (1.0 - smoothstep(0.82, 1.0, x));
    // contrast: shoulder lobe minus toe lobe → antisymmetric, 0 at the ends and 0.50.
    let w_contrast = smoothstep(0.50, 0.75, x) * (1.0 - smoothstep(0.75, 1.0, x)) - w_shadow;
    // whites/blacks own the literal end knots (+ a touch of the adjacent knot).
    let w_white = if x >= 0.999 {
        1.0
    } else if (x - 0.92).abs() < 1e-3 {
        0.45
    } else {
        0.0
    };
    let w_black = if x <= 0.001 {
        1.0
    } else if (x - 0.10).abs() < 1e-3 {
        0.45
    } else {
        0.0
    };
    [
        A_CONTRAST * w_contrast,
        A_HIGHLIGHT * w_high,
        A_SHADOW * w_shadow,
        A_WB * w_white,
        A_WB * w_black,
    ]
}

/// The exposure component of a knot output: a linear-light gain of `ev` stops
/// applied under the sRGB transfer curve (the identity curve when ev = 0).
pub(crate) fn tone_exposure_curve(x: f32, ev: f32) -> f32 {
    linear_to_srgb((srgb_to_linear(x) * 2.0_f32.powf(ev)).clamp(0.0, 1.0))
}

/// Build the develop tone curve as a [`LUT_N`]-entry LUT over input gamma [0,1].
///
/// It is an 8-knot control-point curve fit by a MONOTONE cubic Hermite spline
/// (Fritsch–Carlson), so it is monotone *by construction* (no post-hoc clamp) and
/// the endpoints are pinned. Exposure is a linear-light gain applied before the
/// curve; contrast is an antisymmetric S; shadows/highlights shape the toe/shoulder
/// WITHOUT reaching the midtones or the white point (so a strong −Highlights can't
/// drag specular foam to grey — that is the white point's job, owned by whites);
/// whites/blacks move the end knots. The recipe's own `tone_curve` is composed on
/// top. This replaces a summed-region-hump model that could go non-monotonic and
/// crush mid-bright water / near-white foam (which had needed ad-hoc patches).
pub(crate) fn build_tone_lut(r: &EditRecipe) -> Vec<f32> {
    // Knot OUTPUTS: exposure-mapped identity, then the slider offsets — all from
    // the shared basis below so the reverse-fit (fit.rs) solves against the SAME
    // model the engine renders.
    let contrast = (r.contrast / 100.0).clamp(-1.0, 1.0);
    let highlights = (r.highlights / 100.0).clamp(-1.0, 1.0);
    let shadows = (r.shadows / 100.0).clamp(-1.0, 1.0);
    let whites = (r.whites / 100.0).clamp(-1.0, 1.0);
    let blacks = (r.blacks / 100.0).clamp(-1.0, 1.0);

    let mut ys = [0.0f32; 8];
    for (idx, &x) in TONE_KNOTS_X.iter().enumerate() {
        let b = tone_slider_basis(x);
        ys[idx] = tone_exposure_curve(x, r.exposure_ev)
            + b[0] * contrast
            + b[1] * highlights
            + b[2] * shadows
            + b[3] * whites
            + b[4] * blacks;
    }
    // Force the knot outputs non-decreasing (a tone curve cannot invert) then clamp.
    // Fritsch–Carlson on monotone data ⇒ the whole spline is monotone, so there is
    // NO running-max pass over the sampled LUT — monotonicity is structural.
    const EPS: f32 = 1e-4;
    for i in 1..ys.len() {
        if ys[i] < ys[i - 1] + EPS {
            ys[i] = ys[i - 1] + EPS;
        }
    }
    for v in &mut ys {
        *v = v.clamp(0.0, 1.0);
    }

    let m = fc_tangents(&TONE_KNOTS_X, &ys);
    let curve = curve_lut(&r.tone_curve); // the recipe's own tone_curve, composed on top
    (0..LUT_N)
        .map(|i| {
            let x = i as f32 / (LUT_N - 1) as f32;
            sample_lut(&curve, hermite_eval(&TONE_KNOTS_X, &ys, &m, x))
        })
        .collect()
}

/// Monotone cubic Hermite tangents (Fritsch–Carlson). With `xs` strictly increasing
/// and `ys` non-decreasing, the resulting Hermite spline is monotone everywhere.
fn fc_tangents(xs: &[f32], ys: &[f32]) -> Vec<f32> {
    let n = xs.len();
    let d: Vec<f32> = (0..n - 1).map(|i| (ys[i + 1] - ys[i]) / (xs[i + 1] - xs[i])).collect();
    let mut m = vec![0.0f32; n];
    m[0] = d[0];
    m[n - 1] = d[n - 2];
    for i in 1..n - 1 {
        if d[i - 1] * d[i] <= 0.0 {
            m[i] = 0.0; // local extremum → flat tangent (keeps monotonicity)
        } else {
            let w1 = 2.0 * (xs[i + 1] - xs[i]) + (xs[i] - xs[i - 1]);
            let w2 = (xs[i + 1] - xs[i]) + 2.0 * (xs[i] - xs[i - 1]);
            m[i] = (w1 + w2) / (w1 / d[i - 1] + w2 / d[i]); // weighted harmonic mean
        }
    }
    // Monotonicity limiter: keep each (α,β) inside the circle α²+β² ≤ 9.
    for i in 0..n - 1 {
        if d[i] == 0.0 {
            m[i] = 0.0;
            m[i + 1] = 0.0;
        } else {
            let a = m[i] / d[i];
            let b = m[i + 1] / d[i];
            let s = a * a + b * b;
            if s > 9.0 {
                let t = 3.0 / s.sqrt();
                m[i] = t * a * d[i];
                m[i + 1] = t * b * d[i];
            }
        }
    }
    m
}

/// Evaluate the monotone cubic Hermite spline at `x` (clamped to the knot range).
fn hermite_eval(xs: &[f32], ys: &[f32], m: &[f32], x: f32) -> f32 {
    let n = xs.len();
    if x <= xs[0] {
        return ys[0];
    }
    if x >= xs[n - 1] {
        return ys[n - 1];
    }
    let mut i = 0;
    while i + 1 < n && x > xs[i + 1] {
        i += 1;
    }
    let h = xs[i + 1] - xs[i];
    let t = (x - xs[i]) / h;
    let (t2, t3) = (t * t, t * t * t);
    let h00 = 2.0 * t3 - 3.0 * t2 + 1.0;
    let h10 = t3 - 2.0 * t2 + t;
    let h01 = -2.0 * t3 + 3.0 * t2;
    let h11 = t3 - t2;
    h00 * ys[i] + h10 * h * m[i] + h01 * ys[i + 1] + h11 * h * m[i + 1]
}

/// Curve control points → a 256-entry [0,1]→[0,1] LUT; identity when empty.
/// The ONE curve sampler shared by the master tone curve, the per-channel RGB
/// curves, and the GUI curve editor's on-screen preview — public so what the
/// editor draws is exactly what the engine applies (same sort + linear interp).
pub fn curve_lut(points: &[crate::recipe::CurvePoint]) -> Vec<f32> {
    if points.is_empty() {
        return (0..256).map(|i| i as f32 / 255.0).collect();
    }
    let mut pts: Vec<(f32, f32)> = points
        .iter()
        .map(|p| (p.input as f32 / 255.0, p.output as f32 / 255.0))
        .collect();
    pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    (0..256).map(|i| interp(&pts, i as f32 / 255.0)).collect()
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
pub(crate) fn sample_lut(lut: &[f32], x: f32) -> f32 {
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

/// Apply the per-channel RGB curves (red/green/blue) in place — the colour
/// companion to the master tone curve. No-op when all three are empty.
fn apply_rgb_curves(data: &mut [[f32; 3]], r: &EditRecipe) {
    let curves = [&r.red_curve, &r.green_curve, &r.blue_curve];
    if curves.iter().all(|c| c.is_empty()) {
        return;
    }
    let luts: [Vec<f32>; 3] =
        [curve_lut(curves[0]), curve_lut(curves[1]), curve_lut(curves[2])];
    for px in data.iter_mut() {
        for ch in 0..3 {
            if !curves[ch].is_empty() {
                px[ch] = sample_lut(&luts[ch], px[ch]);
            }
        }
    }
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

/// Per-colour HSL (the 8 ACR bands). For each pixel: find which colour band(s)
/// its hue falls in (triangular partition of unity over the band centres), then
/// rotate hue / scale saturation / scale luminance by the band-weighted amounts.
/// Achromatic pixels (no hue) are untouched. Runs in sRGB-gamma space — a
/// tasteful approximation; the XMP→Lightroom path renders the exact ACR model.
fn apply_hsl(data: &mut [[f32; 3]], hsl: &crate::recipe::Hsl) {
    if hsl.is_neutral() {
        return;
    }
    for px in data.iter_mut() {
        let (h, s, l) = rgb_to_hsl(px[0], px[1], px[2]);
        // Fade the WHOLE HSL effect out on low-CHROMA pixels. Gate on chroma
        // (max−min), NOT HSL saturation: HSL `s` is ill-conditioned near white and
        // black — a bright, faintly-blue sea-foam pixel has chroma ≈ 0.12 yet HSL
        // s ≈ 1.0, so an HSL-`s` gate hits specular highlights at FULL strength and
        // a Blue-band luminance push crushes white foam to grey. Chroma is a true
        // colourfulness measure: ≈0 for near-grey (the overcast-sky blotch case)
        // AND for near-white foam, ramping to full only on genuinely saturated
        // colour, so both are protected while real colours are still adjusted.
        let chroma = px[0].max(px[1]).max(px[2]) - px[0].min(px[1]).min(px[2]);
        let satw = smoothstep(0.05, 0.22, chroma);
        if satw <= 0.0 {
            continue;
        }
        let (b0, b1, w1) = bracket_bands(h * 360.0, &HSL_CENTERS);
        let w0 = 1.0 - w1;
        let hue_adj = (w0 * hsl.hue[b0] + w1 * hsl.hue[b1]) * satw;
        let sat_adj = (w0 * hsl.saturation[b0] + w1 * hsl.saturation[b1]) * satw;
        let lum_adj = (w0 * hsl.luminance[b0] + w1 * hsl.luminance[b1]) * satw;
        // hue: ±100 → ±30° rotation; sat: ±100 → ±100%; lum gentler (×0.5).
        let new_h = (h + (hue_adj / 100.0) * (30.0 / 360.0)).rem_euclid(1.0);
        let new_s = (s * (1.0 + sat_adj / 100.0)).clamp(0.0, 1.0);
        let new_l = (l * (1.0 + 0.5 * lum_adj / 100.0)).clamp(0.0, 1.0);
        let (r, g, b) = hsl_to_rgb(new_h, new_s, new_l);
        *px = [r, g, b];
    }
}

/// ACR band centres in degrees (red..magenta), matching recipe::HSL_BANDS.
/// Shared with the reverse-fit so its per-band statistics use the SAME partition.
pub(crate) const HSL_CENTERS: [f32; 8] = [0.0, 30.0, 60.0, 120.0, 180.0, 240.0, 270.0, 300.0];

/// The two band indices bracketing hue `deg` and the blend weight toward the
/// second (partition of unity). Centres are non-uniform and wrap (magenta 300°
/// → red 360°/0°), so the last segment spans 300..360 back to red.
pub(crate) fn bracket_bands(deg: f32, centers: &[f32; 8]) -> (usize, usize, f32) {
    let d = deg.rem_euclid(360.0);
    for i in 0..8 {
        let lo = centers[i];
        let hi = if i + 1 < 8 { centers[i + 1] } else { 360.0 };
        if d >= lo && d < hi {
            let upper = if i + 1 < 8 { i + 1 } else { 0 };
            return (i, upper, (d - lo) / (hi - lo));
        }
    }
    (0, 1, 0.0) // unreachable: the segments tile [0,360)
}

/// sRGB-gamma RGB → HSL, all in [0,1] (hue normalised to turns).
pub(crate) fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let d = max - min;
    if d < 1e-6 {
        return (0.0, 0.0, l); // achromatic
    }
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } / 6.0;
    (h.rem_euclid(1.0), s, l)
}

/// HSL → sRGB-gamma RGB (inverse of [`rgb_to_hsl`]).
fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    if s < 1e-6 {
        return (l, l, l);
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let hue2rgb = |mut t: f32| -> f32 {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 1.0 / 2.0 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    (hue2rgb(h + 1.0 / 3.0), hue2rgb(h), hue2rgb(h - 1.0 / 3.0))
}

/// Lightroom-style colour grading: tint + lift the shadow / midtone / highlight
/// tonal regions (and a global wheel) by their hue/sat/lum. Region membership is a
/// smoothstep split on luma; `blending` scales the regional effect, `balance`
/// shifts the shadow/highlight split. Approximation; XMP→Lightroom is exact.
fn apply_color_grade(data: &mut [[f32; 3]], cg: &crate::recipe::ColorGrade) {
    if cg.is_neutral() {
        return;
    }
    let blend = (cg.blending / 100.0).clamp(0.0, 1.0);
    // balance shifts the shadow/highlight midpoint: positive leans toward highlights.
    let mid = (0.5 - 0.25 * (cg.balance / 100.0)).clamp(0.05, 0.95);
    for px in data.iter_mut() {
        let l = luma601(px);
        let w_hi = smoothstep(mid, 1.0, l);
        let w_sh = 1.0 - smoothstep(0.0, mid, l);
        let w_mid = (1.0 - w_hi - w_sh).clamp(0.0, 1.0);
        apply_wheel(px, cg.shadow_hue, cg.shadow_sat, cg.shadow_lum, w_sh * blend);
        apply_wheel(px, cg.midtone_hue, cg.midtone_sat, cg.midtone_lum, w_mid * blend);
        apply_wheel(px, cg.highlight_hue, cg.highlight_sat, cg.highlight_lum, w_hi * blend);
        apply_wheel(px, cg.global_hue, cg.global_sat, cg.global_lum, 1.0); // global: all tones
    }
}

/// Apply one colour-grade wheel to a pixel: shift chroma toward the wheel's hue
/// (scaled by sat × weight) and scale brightness by its luminance — both gentle.
fn apply_wheel(px: &mut [f32; 3], hue_deg: f32, sat: f32, lum: f32, weight: f32) {
    if weight <= 1e-4 {
        return;
    }
    if sat.abs() > 1e-4 {
        // Tint toward the pure hue AT THIS PIXEL'S OWN LUMINANCE (not a fixed
        // 0.5-grey anchor) and blend — this keeps luma roughly constant, so deep
        // shadows / bright highlights aren't crushed past [0,1] the way a fixed
        // additive push does. Closer to ACR's luma-aware toning.
        let l = luma601(px);
        let tint = hsl_to_rgb((hue_deg / 360.0).rem_euclid(1.0), 1.0, l);
        let amt = (sat / 100.0) * weight * 0.4;
        px[0] = (px[0] + (tint.0 - px[0]) * amt).clamp(0.0, 1.0);
        px[1] = (px[1] + (tint.1 - px[1]) * amt).clamp(0.0, 1.0);
        px[2] = (px[2] + (tint.2 - px[2]) * amt).clamp(0.0, 1.0);
    }
    if lum.abs() > 1e-4 {
        let k = (1.0 + (lum / 100.0) * weight * 0.5).max(0.0);
        for c in px.iter_mut() {
            *c = (*c * k).clamp(0.0, 1.0);
        }
    }
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
    fn white_point_is_invariant_to_highlights_and_bright_stays_bright() {
        // The engine renders faithfully: Highlights shapes the shoulder but must NOT
        // move the white point, so the brightest tone stays pinned at white. (Keeping
        // bright FOAM bright under an over-cooked recipe is the recipe layer's job —
        // EditRecipe::temper — not an engine override.)
        for h in [-100.0, -78.81, -30.0, 30.0, 100.0] {
            let lut = build_tone_lut(&EditRecipe { highlights: h, ..Default::default() });
            assert!(
                (sample_lut(&lut, 1.0) - 1.0).abs() < 1e-3,
                "highlights {h} moved the white point: {}",
                sample_lut(&lut, 1.0)
            );
        }
        // A neutral recipe must leave bright near-white foam bright.
        let mut foam = vec![[0.90_f32, 0.93, 0.96]];
        apply_develop(&mut foam, 1, 1, &EditRecipe::default());
        let lum = 0.299 * foam[0][0] + 0.587 * foam[0][1] + 0.114 * foam[0][2];
        assert!(lum > 0.90, "neutral recipe dimmed bright foam: {lum}");
    }

    #[test]
    fn tempered_recipe_renders_foam_light_and_water_saturated() {
        // End-to-end: the over-cooked AI recipe (the one that greyed the foam),
        // after clamp + temper, rendered through the monotone curve. Foam must be
        // LIGHT (not crushed to the muddy ~0.6 grey it was) and water must stay
        // turquoise — the engine + recipe layers compose, no engine override.
        let mut r = EditRecipe {
            highlights: -78.81,
            shadows: 36.56,
            whites: 10.27,
            blacks: -14.59,
            contrast: 4.68,
            exposure_ev: -0.177,
            vibrance: 11.19,
            saturation: 2.9,
            ..Default::default()
        };
        r.clamp();
        r.temper();
        let lum = |p: [f32; 3]| 0.299 * p[0] + 0.587 * p[1] + 0.114 * p[2];
        let mut foam = vec![[0.90_f32, 0.93, 0.96]];
        apply_develop(&mut foam, 1, 1, &r);
        assert!(lum(foam[0]) > 0.80, "foam crushed (should stay light): luma {}", lum(foam[0]));
        let mut water = vec![[0.35_f32, 0.62, 0.66]];
        apply_develop(&mut water, 1, 1, &r);
        let [rr, gg, bb] = water[0];
        assert!(gg > rr + 0.10 && bb > rr + 0.10, "water lost its turquoise: [{rr}, {gg}, {bb}]");
    }

    #[test]
    fn region_tones_pin_endpoints_and_stay_monotonic() {
        // Highlights/shadows/contrast must never move the endpoints (only whites/
        // blacks may), and the curve must stay monotone under any extreme combo.
        let recipes = [
            EditRecipe::default(),
            EditRecipe { highlights: -100.0, shadows: 100.0, contrast: 100.0, ..Default::default() },
            EditRecipe { highlights: 100.0, shadows: -100.0, contrast: -100.0, ..Default::default() },
        ];
        for r in recipes {
            let lut = build_tone_lut(&r);
            for i in 1..lut.len() {
                assert!(lut[i] >= lut[i - 1] - 1e-6, "non-monotonic at {i}");
            }
            assert!(sample_lut(&lut, 0.0) < 1e-3, "black point moved by hi/sh/contrast");
            assert!((sample_lut(&lut, 1.0) - 1.0).abs() < 1e-3, "white point moved by hi/sh/contrast");
        }
    }

    #[test]
    fn tone_lut_is_monotonic_and_keeps_midtone_separation() {
        // The reported "flat muddy water": strong opposing highlights/shadows made
        // the per-region tone curve non-monotonic and collapsed mid-bright tones
        // into one dark band. The curve must stay monotonic and keep midtones apart.
        let r = EditRecipe {
            highlights: -73.89,
            shadows: 33.28,
            whites: 6.99,
            blacks: -12.94,
            contrast: 4.68,
            ..Default::default()
        };
        let lut = build_tone_lut(&r);
        for i in 1..lut.len() {
            assert!(lut[i] >= lut[i - 1] - 1e-6, "tone LUT inverts at {i}: {} < {}", lut[i], lut[i - 1]);
        }
        // mid-bright water tones (0.50 vs 0.66) must NOT collapse to one value.
        let (a, b) = (sample_lut(&lut, 0.50), sample_lut(&lut, 0.66));
        assert!(b - a > 0.05, "midtone separation crushed flat: {a}..{b}");
        // and a true midtone (0.5) is no longer crushed deep into shadow.
        assert!(a > 0.45, "midtone water still crushed dark: {a}");
    }

    #[test]
    fn aggressive_highlights_keep_saturated_water_from_greying() {
        // Reported bug: strong −highlights + +shadows turned bright turquoise water
        // flat grey, because the tone LUT ran per-channel and the channels converged.
        // Luminance-preserving tone must keep the cyan recognizably cyan (just darker).
        let r = EditRecipe {
            highlights: -73.89,
            shadows: 33.28,
            whites: 6.99,
            blacks: -12.94,
            contrast: 4.68,
            ..Default::default()
        };
        let cyan = [0.35_f32, 0.62, 0.66]; // mid-bright sunlit turquoise
        let mut data = vec![cyan];
        apply_develop(&mut data, 1, 1, &r);
        let [rr, gg, bb] = data[0];
        // green & blue stay clearly above red → still cyan, not neutral grey.
        assert!(gg > rr + 0.08 && bb > rr + 0.08, "water greyed out: [{rr}, {gg}, {bb}]");
        // channel spread preserved (not converged toward equal = grey).
        let spread = rr.max(gg).max(bb) - rr.min(gg).min(bb);
        assert!(spread > 0.12, "channels converged toward grey: spread {spread}");
    }

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
    fn specular_white_handling_diagnosis() {
        // Push one pixel through the full per-pixel develop (1x1 → spatial ops are
        // no-ops) to learn: is bright near-white "foam" greyed by a render BUG, or
        // only by aggressive recipe values? Run with `--nocapture` to read numbers.
        fn run(px: [f32; 3], r: &EditRecipe) -> [f32; 3] {
            let mut d = vec![px];
            apply_develop(&mut d, 1, 1, r);
            d[0]
        }
        let white = [1.0_f32, 1.0, 1.0];
        let foam = [0.88_f32, 0.93, 1.00]; // sky-lit foam: bright, slightly blue
        let lum = |p: [f32; 3]| 0.299 * p[0] + 0.587 * p[1] + 0.114 * p[2];
        let hsv_sat = |p: [f32; 3]| {
            let mx = p[0].max(p[1]).max(p[2]);
            let mn = p[0].min(p[1]).min(p[2]);
            if mx > 1e-4 { (mx - mn) / mx } else { 0.0 }
        };

        // (1) NEUTRAL must preserve white — guards against a standalone render bug.
        let wn = run(white, &EditRecipe::default());
        eprintln!("neutral white -> {wn:?}");
        assert!(wn[0] > 0.99 && wn[1] > 0.99 && wn[2] > 0.99, "neutral greyed white: {wn:?}");

        let (_h, hsl_s, _l) = rgb_to_hsl(foam[0], foam[1], foam[2]);
        eprintln!("foam HSL-sat={hsl_s:.3}  HSV-sat={:.3}", hsv_sat(foam));

        let mut hsl_lum = crate::recipe::Hsl::default();
        hsl_lum.luminance[5] = -60.0; // Blue band
        let blue_lum = EditRecipe { hsl: hsl_lum, ..Default::default() };

        // THE FIX: a Blue-band luminance push must NOT crush near-white foam to
        // grey (chroma ≈ 0.12 → gate ≈ 0.37), yet MUST still darken a genuinely
        // vivid blue (chroma ≈ 0.65 → gate ≈ 1.0). Pre-fix the HSL-`s` gate (s≈1.0)
        // hit foam at full strength and it landed at luma 0.71 (a blue-grey).
        let foam_out = run(foam, &blue_lum);
        let vivid = [0.20_f32, 0.45, 0.85];
        let vivid_out = run(vivid, &blue_lum);
        eprintln!("foam  + blue lum-60 -> {foam_out:?} luma {:.2}", lum(foam_out));
        eprintln!("vivid + blue lum-60 -> {vivid_out:?} luma {:.2}", lum(vivid_out));
        assert!(lum(foam_out) > 0.80, "near-white foam must stay bright, got luma {:.2}", lum(foam_out));
        assert!(lum(vivid_out) < 0.90 * lum(vivid), "vivid blue must still darken (HSL still works)");
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
    fn scurve_contrast_pins_ends_and_steepens_midtones() {
        // Positive contrast must keep 0→0 and 1→1 (pinned endpoints), darken a
        // shadow value, brighten a highlight value (the S shape), and stay
        // monotonic — the old linear stretch clipped instead of pinning.
        let lut = build_tone_lut(&EditRecipe { contrast: 80.0, ..Default::default() });
        assert!(sample_lut(&lut, 0.0) < 0.01, "black pinned: {}", sample_lut(&lut, 0.0));
        assert!(sample_lut(&lut, 1.0) > 0.99, "white pinned: {}", sample_lut(&lut, 1.0));
        assert!(sample_lut(&lut, 0.25) < 0.25, "shadow darkened: {}", sample_lut(&lut, 0.25));
        assert!(sample_lut(&lut, 0.75) > 0.75, "highlight brightened: {}", sample_lut(&lut, 0.75));
        let mut prev = -1.0;
        for &y in &lut {
            assert!(y >= prev - 1e-4, "non-monotonic: {y} after {prev}");
            prev = y;
        }
    }

    #[test]
    fn region_tones_target_four_different_zones() {
        // Each region owns a DISTINCT tonal zone, and — the muddy-water fix —
        // highlights/shadows act on the UPPER/LOWER tones and leave the MIDTONES
        // alone (the old wide bands gave highlights 0.6–1.0 authority at v≈0.5–0.65,
        // crushing mid-bright water). Gentle ±30 pushes keep the curve unclamped
        // except very near white.
        let base = build_tone_lut(&EditRecipe::default());
        let d = |r: &EditRecipe, x: f32| sample_lut(&build_tone_lut(r), x) - sample_lut(&base, x);
        let whites = EditRecipe { whites: 30.0, ..Default::default() };
        let highs = EditRecipe { highlights: 30.0, ..Default::default() };
        let shadows = EditRecipe { shadows: 30.0, ..Default::default() };
        let blacks = EditRecipe { blacks: 30.0, ..Default::default() };
        // The fix: neither highlights nor shadows may touch the midtone (0.5).
        assert!(d(&highs, 0.5).abs() < 0.01, "highlights must NOT touch the midtone: {}", d(&highs, 0.5));
        assert!(d(&shadows, 0.5).abs() < 0.01, "shadows must NOT touch the midtone: {}", d(&shadows, 0.5));
        // Each region still owns its zone (upper / white-point / lower / black-point).
        assert!(d(&highs, 0.75) > 0.03, "highlights lift the upper tones: {}", d(&highs, 0.75));
        assert!(d(&whites, 0.92) > 0.03, "whites lift the white point: {}", d(&whites, 0.92));
        assert!(d(&shadows, 0.25) > 0.03, "shadows lift the lower tones: {}", d(&shadows, 0.25));
        assert!(d(&blacks, 0.08) > 0.03, "blacks lift the black point: {}", d(&blacks, 0.08));
        // Differentiation: highlights concentrate BELOW white; whites concentrate AT white.
        assert!(d(&highs, 0.75) > d(&highs, 0.97), "highlights concentrate below the white point");
        assert!(d(&whites, 0.95) > d(&whites, 0.70), "whites concentrate at the white point");
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

    #[test]
    fn hsl_adjusts_only_the_targeted_colour_band() {
        use crate::recipe::Hsl;
        // Red-band saturation -100 desaturates a red pixel toward grey but leaves
        // a blue pixel (a different band) untouched.
        let mut hsl = Hsl::default();
        hsl.saturation[0] = -100.0; // red band
        let mut data = vec![[0.8_f32, 0.1, 0.1], [0.1, 0.1, 0.8]];
        apply_hsl(&mut data, &hsl);
        let red = data[0];
        assert!(
            (red[0] - red[1]).abs() < 0.05 && (red[1] - red[2]).abs() < 0.05,
            "red pixel desaturated toward grey: {red:?}"
        );
        let blue = data[1];
        assert!(
            (blue[0] - 0.1).abs() < 0.02 && (blue[2] - 0.8).abs() < 0.02,
            "blue pixel untouched: {blue:?}"
        );
    }

    #[test]
    fn hsl_neutral_is_identity_and_grey_is_untouched() {
        use crate::recipe::Hsl;
        // A neutral HSL is an exact no-op.
        let mut data = vec![[0.6_f32, 0.2, 0.2], [0.5, 0.5, 0.5]];
        let orig = data.clone();
        apply_hsl(&mut data, &Hsl::default());
        assert_eq!(data, orig);
        // A grey pixel has no hue, so even a strong all-band push leaves it alone.
        let hsl = Hsl { saturation: [100.0; 8], ..Hsl::default() };
        let mut grey = vec![[0.5_f32, 0.5, 0.5]];
        apply_hsl(&mut grey, &hsl);
        assert!(
            (grey[0][0] - 0.5).abs() < 1e-4 && (grey[0][2] - 0.5).abs() < 1e-4,
            "grey untouched: {:?}",
            grey[0]
        );
    }

    #[test]
    fn hsl_does_not_blotch_a_near_grey_sky() {
        use crate::recipe::Hsl;
        // A near-grey overcast "sky": alternating pixels lean faintly blue vs
        // faintly aqua (s ≈ 3%), the way real demosaiced sky noise does. With
        // OPPOSITE luminance on the blue and aqua bands, the un-weighted code
        // would slam adjacent pixels to wildly different luma (a checkerboard
        // blotch). The saturation fade must keep the patch smooth.
        let mut data: Vec<[f32; 3]> = (0..64)
            .map(|i| if i % 2 == 0 { [0.71, 0.715, 0.726] } else { [0.71, 0.726, 0.722] })
            .collect();
        let hsl = Hsl { luminance: [0.0, 0.0, 0.0, 0.0, 60.0, -80.0, 0.0, 0.0], ..Hsl::default() };
        apply_hsl(&mut data, &hsl);
        let lumas: Vec<f32> = data.iter().map(luma601).collect();
        let spread = lumas.iter().cloned().fold(f32::MIN, f32::max)
            - lumas.iter().cloned().fold(f32::MAX, f32::min);
        assert!(spread < 0.04, "near-grey sky must not blotch — luma spread {spread}");
    }

    #[test]
    fn color_grade_tints_the_targeted_tonal_region() {
        use crate::recipe::ColorGrade;
        // A blue shadow wheel pushes a DARK pixel toward blue; neutral is a no-op.
        let cg = ColorGrade { shadow_hue: 240.0, shadow_sat: 100.0, blending: 100.0, ..Default::default() };
        let mut data = vec![[0.15_f32, 0.15, 0.15]]; // dark grey
        apply_color_grade(&mut data, &cg);
        let p = data[0];
        assert!(p[2] > p[0] && p[2] > p[1], "shadow tinted blue: {p:?}");

        let mut d2 = vec![[0.4_f32, 0.3, 0.2]];
        let orig = d2.clone();
        apply_color_grade(&mut d2, &ColorGrade::default()); // neutral
        assert_eq!(d2, orig);
    }

    #[test]
    fn rgb_curves_shape_each_channel_independently() {
        use crate::recipe::CurvePoint;
        // A red curve lifting the black point brightens RED only, via the full pipeline.
        let r = EditRecipe {
            red_curve: vec![CurvePoint { input: 0, output: 60 }, CurvePoint { input: 255, output: 255 }],
            ..Default::default()
        };
        let mut data = vec![[0.0_f32, 0.0, 0.0]];
        apply_develop(&mut data, 1, 1, &r);
        let p = data[0];
        assert!(p[0] > 0.15, "red channel lifted: {p:?}");
        assert!(p[1] < 0.02 && p[2] < 0.02, "green/blue untouched: {p:?}");
    }
}
