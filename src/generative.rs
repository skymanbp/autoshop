//! Generative image editing (V2_PLAN §5) — a SEPARATE, EXPERIMENTAL concern from
//! the parametric develop pipeline. Calls OpenAI's Images `edits` endpoint
//! (gpt-image-*), which RE-GENERATES pixels, so output is:
//!   * low resolution (≈1 MP here vs the 61 MP RAW),
//!   * a lossy generative re-render (not pixel-faithful),
//!   * non-deterministic.
//!
//! Strictly a low-res experiment / preview — NOT a deliverable master. The
//! XMP-first parametric path remains the real workflow.
//!
//! `reimagine` = full-frame restyle (no mask). `retouch` = object removal /
//! inpainting (RGBA mask; transparent pixels = the region to regenerate).

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use image::DynamicImage;

use crate::config::Config;
use crate::{decode, pipeline};

const BOUNDARY: &str = "----autoshopBoundaryX7MA4YWxkTrZu0gW";
const SIZE: &str = "1024x1024";
/// Cost guard: "low" keeps the experiment cheap (~$0.02). Raise for quality.
const QUALITY: &str = "low";

/// Full-frame generative restyle (the user's experiment). `fidelity` = "high"
/// keeps it recognizably the same photo; "low" gives the model free rein.
pub fn reimagine(cfg: &Config, raw_path: &Path, prompt: &str, fidelity: &str, out: &Path) -> Result<()> {
    let src = decode::preview_only(raw_path)?
        .resize_exact(1024, 1024, image::imageops::FilterType::Triangle);
    let png = encode_png(&src)?;
    println!(
        "⚠ EXPERIMENTAL generative re-render via {} (low-res, lossy — not a master)",
        cfg.openai_image_model
    );
    let result = call_images_edit(cfg, &png, None, prompt, fidelity)?;
    pipeline::ensure_parent(out)?;
    std::fs::write(out, result).with_context(|| format!("write {}", out.display()))?;
    println!("generative -> {}", out.display());
    Ok(())
}

/// Object removal / inpainting. `mask_path` is an RGBA PNG; transparent (alpha=0)
/// pixels mark the region to regenerate.
pub fn retouch(cfg: &Config, raw_path: &Path, mask_path: &Path, prompt: &str, out: &Path) -> Result<()> {
    let src = decode::preview_only(raw_path)?
        .resize_exact(1024, 1024, image::imageops::FilterType::Triangle);
    let png = encode_png(&src)?;
    let mask = image::open(mask_path)
        .with_context(|| format!("open mask {}", mask_path.display()))?
        .resize_exact(1024, 1024, image::imageops::FilterType::Nearest);
    let mask_png = encode_png(&mask)?;
    println!("⚠ EXPERIMENTAL generative retouch via {} (low-res)", cfg.openai_image_model);
    let result = call_images_edit(cfg, &png, Some(&mask_png), prompt, "high")?;
    pipeline::ensure_parent(out)?;
    std::fs::write(out, result).with_context(|| format!("write {}", out.display()))?;
    println!("generative -> {}", out.display());
    Ok(())
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

fn call_images_edit(
    cfg: &Config,
    image_png: &[u8],
    mask_png: Option<&[u8]>,
    prompt: &str,
    fidelity: &str,
) -> Result<Vec<u8>> {
    let key = cfg
        .openai_api_key
        .as_ref()
        .ok_or_else(|| anyhow!("OPENAI_API_KEY not set — generative editing needs the OpenAI API"))?;

    let mut body = Vec::new();
    part_text(&mut body, "model", &cfg.openai_image_model);
    part_text(&mut body, "prompt", prompt);
    part_text(&mut body, "input_fidelity", fidelity);
    part_text(&mut body, "size", SIZE);
    part_text(&mut body, "quality", QUALITY);
    part_file(&mut body, "image", "image.png", image_png);
    if let Some(m) = mask_png {
        part_file(&mut body, "mask", "mask.png", m);
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());

    let url = format!("{}/images/edits", cfg.openai_base_url.trim_end_matches('/'));
    let resp = ureq::post(&url)
        .set("Authorization", &format!("Bearer {key}"))
        .set("Content-Type", &format!("multipart/form-data; boundary={BOUNDARY}"))
        .send_bytes(&body);

    let value: serde_json::Value = match resp {
        Ok(r) => r.into_json().context("parse image API response")?,
        Err(ureq::Error::Status(code, r)) => {
            let b = r.into_string().unwrap_or_default();
            return Err(anyhow!("image API {code}: {b}"));
        }
        Err(ureq::Error::Transport(t)) => return Err(anyhow!("transport: {t}")),
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
