//! Generative image editing (V2_PLAN §5) — a SEPARATE, EXPERIMENTAL concern from
//! the parametric develop pipeline. Calls OpenAI's Images `edits` endpoint
//! (gpt-image-*), which RE-GENERATES pixels.
//!
//! Phase 4 raises the pixel quality of this path ("give GPT higher-level pixels"):
//!   * **Aspect-correct sizing** — pick 1536×1024 / 1024×1536 / 1024×1024 by
//!     orientation instead of squashing every photo into a 1:1 square.
//!   * **Configurable quality tier** (`low|medium|high|auto`, default `high`).
//!   * **`retouch` composites back onto the source's native preview** — only the
//!     masked (inpainted) region carries generative pixels; the rest keeps the
//!     original pixels, with a feathered seam. "Native" here is whatever
//!     [`decode::preview_only`] yields: the camera's EMBEDDED preview for a RAW
//!     (e.g. ~1616×1080 on a Sony A7RIV — NOT the full sensor), or the actual
//!     full image for an already-baked PNG/TIFF.
//!
//! `reimagine` = full-frame restyle (no mask) → still a generative re-render at
//! the chosen size, so it stays a low-res experiment / preview, NOT a master.
//! `retouch` = object removal / generative fill (RGBA mask; transparent pixels =
//! the region to regenerate) → preview-resolution composite where only the
//! masked region is generative; the rest is the untouched source preview.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView, Rgba, RgbaImage};

use crate::config::Config;
use crate::{decode, pipeline};

const BOUNDARY: &str = "----autoshopBoundaryX7MA4YWxkTrZu0gW";

/// Full-frame generative restyle (the user's experiment). `fidelity` = "high"
/// keeps it recognizably the same photo; "low" gives the model free rein.
/// `quality` is the output tier (low|medium|high|auto).
pub fn reimagine(
    cfg: &Config,
    raw_path: &Path,
    prompt: &str,
    fidelity: &str,
    quality: &str,
    out: &Path,
) -> Result<()> {
    let src = decode::preview_only(raw_path)?;
    let (w, h) = src.dimensions();
    let size = pick_size(w, h);
    let (sw, sh) = parse_size(size);
    let small = src.resize_exact(sw, sh, FilterType::Triangle);
    let png = encode_png(&small)?;
    println!(
        "⚠ EXPERIMENTAL generative re-render via {} ({size}, quality={quality} — low-res, lossy, not a master)",
        cfg.openai_image_model
    );
    let result = call_images_edit(cfg, &png, None, prompt, fidelity, size, quality)?;
    pipeline::ensure_parent(out)?;
    std::fs::write(out, result).with_context(|| format!("write {}", out.display()))?;
    println!("generative -> {} ({size}, generative re-render)", out.display());
    Ok(())
}

/// Object removal / generative fill. `mask_path` is an RGBA PNG; transparent
/// (alpha=0) pixels mark the region to regenerate. The generative result is
/// composited back over the base so only the masked region is re-rendered.
///
/// The base is [`decode::preview_only`] by default — for a RAW that is the
/// camera's embedded preview (e.g. ~1616×1080 on Sony), not the full sensor.
/// Set `full_res` to instead composite onto the full-sensor develop (e.g. 61 MP)
/// so the untouched area keeps native resolution; this is slow and the
/// regenerated patch is upscaled. For a baked PNG/TIFF the base is already the
/// full image, so `full_res` changes nothing.
pub fn retouch(
    cfg: &Config,
    raw_path: &Path,
    mask_path: &Path,
    prompt: &str,
    quality: &str,
    full_res: bool,
    out: &Path,
) -> Result<()> {
    // Base to composite onto. Default = the source preview (fast; for a RAW that
    // is the camera's embedded preview, ~1.6 MP). `full_res` renders the full
    // sensor through the develop engine (e.g. 61 MP) so the untouched area keeps
    // native resolution — slow, and only the small masked patch is generative
    // (upscaled). Baked PNG/TIFF are already full-res, so the flag is a no-op there.
    let raw = decode::is_raw(raw_path);
    let base = if full_res && raw {
        crate::render::render_to_image(raw_path, &crate::recipe::EditRecipe::default(), None)?
    } else {
        decode::preview_only(raw_path)?
    };
    let (bw, bh) = base.dimensions();
    let size = pick_size(bw, bh);
    let (sw, sh) = parse_size(size);

    // API input must be 8-bit (the full-res base is 16-bit). Derive the small
    // image from THIS base so the generated pixels match its look (no seam shift).
    let small = DynamicImage::ImageRgb8(base.resize_exact(sw, sh, FilterType::Triangle).to_rgb8());
    let png = encode_png(&small)?;
    let mask_img = image::open(mask_path)
        .with_context(|| format!("open mask {}", mask_path.display()))?;
    let mask_png = encode_png(&mask_img.resize_exact(sw, sh, FilterType::Nearest))?;

    println!(
        "⚠ EXPERIMENTAL generative fill via {} ({size}, quality={quality}, base={bw}x{bh} {}; composite)",
        cfg.openai_image_model,
        if full_res && raw { "full-res" } else { "preview" }
    );
    let result = call_images_edit(cfg, &png, Some(&mask_png), prompt, "high", size, quality)?;

    // Composite the regenerated region back onto the base. Upscale the generative
    // tile to base dimensions; the user's mask (alpha=0 = regenerate) becomes the
    // blend weight, feathered for a soft seam.
    let gen_img = image::load_from_memory(&result)
        .context("decode generative result")?
        .resize_exact(bw, bh, FilterType::Lanczos3)
        .to_rgba8();
    let mask_full = mask_img.resize_exact(bw, bh, FilterType::Nearest).to_rgba8();
    let base_rgba = base.to_rgba8();
    let feather = ((bw.min(bh) as usize) / 100).clamp(2, 64); // ~1% of short side, capped
    let composite = composite_region(&base_rgba, &gen_img, &mask_full, feather);

    pipeline::ensure_parent(out)?;
    composite
        .save(out)
        .with_context(|| format!("write {}", out.display()))?;
    println!("generative fill -> {} ({bw}x{bh}, composite)", out.display());
    Ok(())
}

/// Pick the supported gpt-image output size whose aspect best matches the source,
/// so we stop squashing every photo into a 1:1 square. gpt-image supports exactly
/// 1024×1024, 1536×1024 (landscape 3:2) and 1024×1536 (portrait 2:3).
fn pick_size(w: u32, h: u32) -> &'static str {
    if h == 0 {
        return "1024x1024";
    }
    let r = w as f32 / h as f32;
    if r >= 1.2 {
        "1536x1024"
    } else if r <= 0.833 {
        "1024x1536"
    } else {
        "1024x1024"
    }
}

fn parse_size(s: &str) -> (u32, u32) {
    s.split_once('x')
        .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
        .unwrap_or((1024, 1024))
}

/// Blend `gen_img` into `base` ONLY where `mask` is transparent (alpha→0 =
/// regenerate), feathering the boundary so the seam is soft. All three share
/// dimensions. Untouched areas keep the original `base` pixels; only the
/// inpainted region carries generative pixels.
fn composite_region(
    base: &RgbaImage,
    gen_img: &RgbaImage,
    mask: &RgbaImage,
    feather: usize,
) -> RgbaImage {
    let (w, h) = base.dimensions();
    let (wu, hu) = (w as usize, h as usize);
    // weight = 1 where mask is transparent (regenerate), 0 where opaque (keep base).
    let mut weight: Vec<f32> = mask.pixels().map(|p| 1.0 - p[3] as f32 / 255.0).collect();
    if feather > 0 {
        weight = box_blur(&weight, wu, hu, feather);
    }
    let mut out = base.clone();
    for y in 0..h {
        for x in 0..w {
            let a = weight[(y as usize) * wu + x as usize].clamp(0.0, 1.0);
            if a <= 0.0001 {
                continue; // outside the (feathered) mask → keep the full-res original
            }
            let b = base.get_pixel(x, y);
            let g = gen_img.get_pixel(x, y);
            let mix =
                |bc: u8, gc: u8| (bc as f32 * (1.0 - a) + gc as f32 * a).round().clamp(0.0, 255.0) as u8;
            out.put_pixel(x, y, Rgba([mix(b[0], g[0]), mix(b[1], g[1]), mix(b[2], g[2]), 255]));
        }
    }
    out
}

/// Separable box blur with prefix sums — cost is O(w·h), independent of `radius`,
/// so a wide feather on a full-res frame stays cheap.
fn box_blur(src: &[f32], w: usize, h: usize, radius: usize) -> Vec<f32> {
    if radius == 0 || w == 0 || h == 0 {
        return src.to_vec();
    }
    let mut tmp = vec![0.0f32; src.len()];
    let mut prefix = vec![0.0f32; w + 1];
    for y in 0..h {
        let row = y * w;
        for x in 0..w {
            prefix[x + 1] = prefix[x] + src[row + x];
        }
        for x in 0..w {
            let lo = x.saturating_sub(radius);
            let hi = (x + radius + 1).min(w);
            tmp[row + x] = (prefix[hi] - prefix[lo]) / (hi - lo) as f32;
        }
    }
    let mut out = vec![0.0f32; src.len()];
    let mut col = vec![0.0f32; h + 1];
    for x in 0..w {
        for y in 0..h {
            col[y + 1] = col[y] + tmp[y * w + x];
        }
        for y in 0..h {
            let lo = y.saturating_sub(radius);
            let hi = (y + radius + 1).min(h);
            out[y * w + x] = (col[hi] - col[lo]) / (hi - lo) as f32;
        }
    }
    out
}

fn encode_png(img: &DynamicImage) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .context("encode png")?;
    Ok(buf)
}

fn part_text(buf: &mut Vec<u8>, name: &str, value: &str) {
    buf.extend_from_slice(
        format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
            .as_bytes(),
    );
}

fn part_file(buf: &mut Vec<u8>, name: &str, filename: &str, bytes: &[u8]) {
    buf.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: image/png\r\n\r\n"
        )
        .as_bytes(),
    );
    buf.extend_from_slice(bytes);
    buf.extend_from_slice(b"\r\n");
}

#[allow(clippy::too_many_arguments)]
fn call_images_edit(
    cfg: &Config,
    image_png: &[u8],
    mask_png: Option<&[u8]>,
    prompt: &str,
    fidelity: &str,
    size: &str,
    quality: &str,
) -> Result<Vec<u8>> {
    let key = cfg
        .openai_api_key
        .as_ref()
        .ok_or_else(|| anyhow!("OPENAI_API_KEY not set — generative editing needs the OpenAI API"))?;

    // `input_fidelity` is a gpt-image-1.x parameter (keeps the edit recognizably the
    // same photo). Newer models reject it — e.g. gpt-image-2 returns 400
    // `invalid_input_fidelity_model`. We can't hard-code which models accept it
    // (the list drifts), so send it and, if the model rejects *that specific
    // parameter*, retry once without it instead of failing the whole edit.
    let build_body = |include_fidelity: bool| -> Vec<u8> {
        let mut body = Vec::new();
        part_text(&mut body, "model", &cfg.openai_image_model);
        part_text(&mut body, "prompt", prompt);
        if include_fidelity {
            part_text(&mut body, "input_fidelity", fidelity);
        }
        part_text(&mut body, "size", size);
        part_text(&mut body, "quality", quality);
        part_file(&mut body, "image", "image.png", image_png);
        if let Some(m) = mask_png {
            part_file(&mut body, "mask", "mask.png", m);
        }
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        body
    };

    let url = format!("{}/images/edits", cfg.openai_base_url.trim_end_matches('/'));
    let mut include_fidelity = true;
    let value: serde_json::Value = loop {
        let body = build_body(include_fidelity);
        let resp = ureq::post(&url)
            .set("Authorization", &format!("Bearer {key}"))
            .set("Content-Type", &format!("multipart/form-data; boundary={BOUNDARY}"))
            .send_bytes(&body);
        match resp {
            Ok(r) => break r.into_json().context("parse image API response")?,
            Err(ureq::Error::Status(code, r)) => {
                let b = r.into_string().unwrap_or_default();
                // Retry once dropping the unsupported param; the guard flips
                // `include_fidelity` to false so this can fire at most once.
                if include_fidelity && b.contains("input_fidelity") {
                    eprintln!(
                        "  note: {} rejected input_fidelity — retrying without it",
                        cfg.openai_image_model
                    );
                    include_fidelity = false;
                    continue;
                }
                return Err(anyhow!("image API {code}: {b}"));
            }
            Err(ureq::Error::Transport(t)) => return Err(anyhow!("transport: {t}")),
        }
    };

    if let Some(u) = value.get("usage") {
        eprintln!("  usage: {u}");
    }
    let b64 = value
        .get("data")
        .and_then(|d| d.get(0))
        .and_then(|x| x.get("b64_json"))
        .and_then(|s| s.as_str())
        .ok_or_else(|| anyhow!("no data[0].b64_json in response: {value}"))?;
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("decode b64_json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    #[test]
    fn pick_size_matches_orientation() {
        assert_eq!(pick_size(6000, 4000), "1536x1024"); // 3:2 landscape
        assert_eq!(pick_size(4000, 6000), "1024x1536"); // 2:3 portrait
        assert_eq!(pick_size(4000, 4000), "1024x1024"); // square
        assert_eq!(pick_size(4000, 0), "1024x1024"); // divide-by-zero guard
    }

    #[test]
    fn parse_size_roundtrips_and_falls_back() {
        assert_eq!(parse_size("1536x1024"), (1536, 1024));
        assert_eq!(parse_size("1024x1536"), (1024, 1536));
        assert_eq!(parse_size("garbage"), (1024, 1024));
    }

    #[test]
    fn composite_keeps_base_outside_mask_and_gen_inside() {
        let (w, h) = (8u32, 4u32);
        let base = RgbaImage::from_pixel(w, h, Rgba([0, 0, 0, 255])); // black original
        let gen_img = RgbaImage::from_pixel(w, h, Rgba([255, 255, 255, 255])); // white generative
        // Left half transparent (regenerate), right half opaque (keep original).
        let mut mask = RgbaImage::from_pixel(w, h, Rgba([0, 0, 0, 255]));
        for y in 0..h {
            for x in 0..w / 2 {
                mask.put_pixel(x, y, Rgba([0, 0, 0, 0]));
            }
        }
        let out = composite_region(&base, &gen_img, &mask, 0); // no feather → crisp boundary
        assert_eq!(out.get_pixel(0, 0)[0], 255, "inside mask should be generative");
        assert_eq!(out.get_pixel(w - 1, 0)[0], 0, "outside mask should stay original");
    }

    #[test]
    fn feather_softens_the_seam() {
        let (w, h) = (16u32, 1u32);
        let base = RgbaImage::from_pixel(w, h, Rgba([0, 0, 0, 255]));
        let gen_img = RgbaImage::from_pixel(w, h, Rgba([255, 255, 255, 255]));
        let mut mask = RgbaImage::from_pixel(w, h, Rgba([0, 0, 0, 255]));
        for x in 0..w / 2 {
            mask.put_pixel(x, 0, Rgba([0, 0, 0, 0]));
        }
        let out = composite_region(&base, &gen_img, &mask, 3);
        let mid = out.get_pixel(w / 2, 0)[0];
        assert!(mid > 0 && mid < 255, "seam pixel should feather to gray, got {mid}");
    }
}
