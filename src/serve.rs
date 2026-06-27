//! Local web UI server. `autoshop serve <dir>` starts a tiny HTTP server; open
//! the printed URL in a browser. Photos are addressed by their index in the
//! in-memory source list (`?id=N`) so we never URL-encode Windows paths. The list
//! is mutable (behind a lock) so the UI can **import** more files/folders at
//! runtime.
//!
//! Interactive feedback (before/after, slider tweaks) runs on the *embedded
//! preview* via [`render::develop_preview`] (fast); only explicit **Export** /
//! **Download** run the full-resolution [`render::render_to_file`].

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::{anyhow, Context, Result};
use image::{DynamicImage, ImageFormat};
use serde::Deserialize;
use serde_json::json;
use tiny_http::{Header, Request, Response, Server};

use crate::config::Config;
use crate::decode;
use crate::denoise::DenoiseOpts;
use crate::pipeline;
use crate::recipe::EditRecipe;
use crate::render;

const INDEX_HTML: &str = include_str!("web/index.html");
const LIST_CAP: usize = 1000; // cap thumbnails shown

struct AppState {
    dir: PathBuf,
    /// The source list, mutable so the UI can import more at runtime.
    raws: RwLock<Vec<PathBuf>>,
    cfg: Config,
}

impl AppState {
    /// The path at index `id` (cloned, lock released immediately).
    fn at(&self, id: usize) -> Option<PathBuf> {
        self.raws.read().ok()?.get(id).cloned()
    }
    fn count(&self) -> usize {
        self.raws.read().map(|r| r.len()).unwrap_or(0)
    }
}

pub fn serve(dir: &Path, port: u16) -> Result<()> {
    // Sources = RAWs + already-baked PNG/TIFF/JPEG (the PNG-source edit mode).
    let raws = pipeline::find_sources(dir)?;
    let n = raws.len();
    let state = Arc::new(AppState {
        dir: dir.to_path_buf(),
        raws: RwLock::new(raws),
        cfg: Config::load(),
    });
    let addr = format!("127.0.0.1:{port}");
    let server = Server::http(&addr).map_err(|e| anyhow!("start server on {addr}: {e}"))?;
    println!("Autoshop UI: {n} source(s) under {}", dir.display());
    println!("  open  →  http://{addr}");
    if state.cfg.openai_api_key.is_none() {
        println!("  note: OPENAI_API_KEY not set — Analyze will use the heuristic baseline.");
    }

    for request in server.incoming_requests() {
        let state = Arc::clone(&state);
        std::thread::spawn(move || {
            if let Err(e) = handle(request, &state) {
                eprintln!("request error: {e}");
            }
        });
    }
    Ok(())
}

fn handle(request: Request, state: &AppState) -> Result<()> {
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or("/");
    let is_post = request.method() == &tiny_http::Method::Post;

    match (is_post, path) {
        (false, "/") => respond_html(request, INDEX_HTML),
        (false, "/api/list") => api_list(request, state),
        (false, "/api/thumb") => api_image(request, state, 256),
        (false, "/api/preview") => api_image(request, state, 1200),
        (false, "/api/recipe") => api_recipe(request, state),
        (false, "/api/style-info") => api_style_info(request, state),
        (true, "/api/import") => api_import(request, state),
        (true, "/api/upload") => api_upload(request, state),
        (true, "/api/analyze") => api_analyze(request, state),
        (true, "/api/develop") => api_develop(request, state),
        (true, "/api/export") => api_export(request, state),
        (true, "/api/download") => api_download(request, state),
        (true, "/api/xmp") => api_xmp(request, state),
        _ => respond_status(request, 404, "not found"),
    }
}

// --- handlers --------------------------------------------------------------

fn api_list(request: Request, state: &AppState) -> Result<()> {
    let raws = state.raws.read().map_err(|_| anyhow!("lock poisoned"))?;
    let items: Vec<_> = raws
        .iter()
        .take(LIST_CAP)
        .enumerate()
        .map(|(id, raw)| {
            let analyzed = pipeline::default_out(raw, "recipe", "json").exists()
                || pipeline::xmp_target(raw).exists();
            json!({
                "id": id,
                "stem": pipeline::stem(raw),
                "baked": !decode::is_raw(raw),
                "analyzed": analyzed,
            })
        })
        .collect();
    let body = json!({
        "dir": state.dir.display().to_string(),
        "total": raws.len(),
        "shown": items.len(),
        "items": items,
    });
    respond_json(request, &body)
}

#[derive(Deserialize)]
struct ImportReq {
    /// A file or folder path on disk (this server runs locally).
    path: String,
}

/// Add a file or (recursively) a folder of sources to the gallery at runtime.
fn api_import(mut request: Request, state: &AppState) -> Result<()> {
    let req: ImportReq = read_json(&mut request)?;
    // Tolerate Windows "Copy as path" (wraps the path in quotes) + stray spaces.
    let cleaned = req.path.trim().trim_matches('"').trim();
    let p = PathBuf::from(cleaned);
    let found: Vec<PathBuf> = if p.is_dir() {
        pipeline::find_sources(&p)?
    } else if p.is_file() && (decode::is_raw(&p) || is_baked_ext(&p)) {
        vec![p.clone()]
    } else {
        return respond_status(request, 400, &format!("not a file/folder I can read: {cleaned}"));
    };

    let mut added = 0usize;
    {
        let mut raws = state.raws.write().map_err(|_| anyhow!("lock poisoned"))?;
        for np in found {
            if !raws.contains(&np) {
                raws.push(np);
                added += 1;
            }
        }
    }
    respond_json(request, &json!({ "added": added, "total": state.count() }))
}

/// Accept dropped/picked file BYTES, save under ./out/imported, and add it to the
/// gallery. Browsers can't hand a local server the original disk path, so
/// drag-drop uploads the bytes (path-based Import stays for your on-disk library).
/// Filename comes from the `X-Filename` header.
fn api_upload(mut request: Request, state: &AppState) -> Result<()> {
    let name = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("X-Filename"))
        .map(|h| h.value.as_str().to_string())
        .unwrap_or_default();
    // basename only — never let an upload name escape ./out/imported.
    let safe = Path::new(&name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    let as_path = PathBuf::from(&safe);
    if safe.is_empty() || !(decode::is_raw(&as_path) || is_baked_ext(&as_path)) {
        return respond_status(request, 400, "unsupported or unnamed file");
    }

    let mut bytes = Vec::new();
    request.as_reader().read_to_end(&mut bytes).context("read upload body")?;

    let dir = PathBuf::from("out").join("imported");
    std::fs::create_dir_all(&dir).context("create out/imported")?;
    let dest = dir.join(&safe);
    std::fs::write(&dest, &bytes).with_context(|| format!("write {}", dest.display()))?;

    let id = {
        let mut raws = state.raws.write().map_err(|_| anyhow!("lock poisoned"))?;
        match raws.iter().position(|p| p == &dest) {
            Some(i) => i,
            None => {
                raws.push(dest.clone());
                raws.len() - 1
            }
        }
    };
    respond_json(
        request,
        &json!({ "id": id, "total": state.count(), "stem": pipeline::stem(&dest) }),
    )
}

fn api_image(request: Request, state: &AppState, max_edge: u32) -> Result<()> {
    let raw = raw_for(&request, state)?;
    let preview = decode::preview_only(&raw)?;
    let resized = preview.resize(max_edge, max_edge, image::imageops::FilterType::Triangle);
    respond_jpeg(request, &resized)
}

fn api_recipe(request: Request, state: &AppState) -> Result<()> {
    let raw = raw_for(&request, state)?;
    let path = pipeline::default_out(&raw, "recipe", "json");
    if !path.exists() {
        return respond_status(request, 404, "no recipe yet");
    }
    let text = std::fs::read_to_string(&path)?;
    let header = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
    request
        .respond(Response::from_string(text).with_header(header))
        .map_err(Into::into)
}

/// Style-library info for the UI's info box: is an index built, how many of the
/// user's edits it holds, and the scene "tags" it covers. Instant (just reads the
/// JSON; no per-photo decode).
fn api_style_info(request: Request, _state: &AppState) -> Result<()> {
    match crate::style::StyleIndex::load(Path::new("out/style-index.json")) {
        Ok(ix) => {
            let mut tags: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
            for e in &ix.exemplars {
                *tags.entry(e.tag.clone()).or_default() += 1;
            }
            let mut top: Vec<_> = tags.into_iter().collect();
            top.sort_by(|a, b| b.1.cmp(&a.1));
            top.truncate(6);
            let scenes: Vec<_> = top.into_iter().map(|(t, n)| json!({ "tag": t, "n": n })).collect();
            respond_json(
                request,
                &json!({ "built": true, "total": ix.exemplars.len(), "scenes": scenes }),
            )
        }
        Err(_) => respond_json(request, &json!({ "built": false })),
    }
}

#[derive(Deserialize)]
struct AnalyzeReq {
    id: usize,
    /// Optional user direction woven into the AI prompt.
    #[serde(default)]
    guidance: Option<String>,
    /// Refine mode: the user's CURRENT edit to adjust instead of starting fresh.
    /// `None` (the default) = propose from the original.
    #[serde(default)]
    base: Option<EditRecipe>,
    /// 0..1 — how strongly to follow the user's historical style (the Style
    /// slider). `None` falls back to the configured default.
    #[serde(default)]
    style_strength: Option<f32>,
    /// A box the user dragged on the image (normalized 0..1) to target a local
    /// edit; the direction is then applied to a mask over that region.
    #[serde(default)]
    region: Option<Region>,
}

#[derive(Deserialize)]
struct Region {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}
#[derive(Deserialize)]
struct DevelopReq {
    id: usize,
    recipe: EditRecipe,
    /// Export/download only: run AI denoise first (ignored by live preview).
    #[serde(default)]
    denoise: bool,
    #[serde(default)]
    denoise_strength: Option<f32>,
    /// Export/download only: "tif" (16-bit master, default) or "jpg".
    #[serde(default)]
    format: Option<String>,
}
#[derive(Deserialize)]
struct XmpReq {
    id: usize,
    recipe: EditRecipe,
}

fn api_analyze(mut request: Request, state: &AppState) -> Result<()> {
    let req: AnalyzeReq = read_json(&mut request)?;
    let raw = state.at(req.id).ok_or_else(|| anyhow!("bad id"))?;
    // A dragged region anchors the edit: fold its coords into the direction so the
    // AI places a mask over exactly that box (reuses the Phase-2 area→mask prompt).
    let region_guidance = req.region.as_ref().map(|g| {
        format!(
            "The user SELECTED a target region (normalized 0..1 frame coords): left={:.3} top={:.3} \
             right={:.3} bottom={:.3}. Apply the direction ONLY to that region — emit a mask covering \
             it (a radial mask with those exact left/top/right/bottom bounds and feather ~0.4 is \
             ideal, or a linear gradient for a thin edge band). Direction: {}",
            g.left,
            g.top,
            g.right,
            g.bottom,
            req.guidance.as_deref().unwrap_or("make a tasteful local improvement"),
        )
    });
    let guidance = region_guidance.as_deref().or(req.guidance.as_deref());
    // base = Some → refine the current edit; None → fresh proposal from original.
    let style = req.style_strength.unwrap_or(state.cfg.style_strength);
    let (recipe, verdict) =
        pipeline::produce_recipe(&raw, &state.cfg, false, guidance, req.base.as_ref(), style)?;
    pipeline::write_recipe(&raw, &recipe, None)?;
    if decode::is_raw(&raw) {
        pipeline::write_xmp(&raw, &recipe)?;
    }
    respond_json(request, &json!({ "recipe": recipe, "verdict": verdict }))
}

fn api_develop(mut request: Request, state: &AppState) -> Result<()> {
    let req: DevelopReq = read_json(&mut request)?;
    let raw = state.at(req.id).ok_or_else(|| anyhow!("bad id"))?;
    let preview =
        decode::preview_only(&raw)?.resize(1200, 1200, image::imageops::FilterType::Triangle);
    let after = render::develop_preview(&preview, &req.recipe);
    respond_jpeg(request, &after)
}

/// Resolve the output extension from the request ("jpg" → jpg, else 16-bit tif).
fn fmt_ext(req: &DevelopReq) -> &'static str {
    match req.format.as_deref() {
        Some("jpg") | Some("jpeg") => "jpg",
        _ => "tif",
    }
}

fn denoise_opts(req: &DevelopReq, cfg: &Config) -> Option<DenoiseOpts> {
    req.denoise
        .then(|| DenoiseOpts::from_config(cfg, None, req.denoise_strength.unwrap_or(1.0)))
}

/// Export to ./out (the library stays read-only). Returns the written path.
fn api_export(mut request: Request, state: &AppState) -> Result<()> {
    let req: DevelopReq = read_json(&mut request)?;
    let raw = state.at(req.id).ok_or_else(|| anyhow!("bad id"))?;
    let out = pipeline::default_out(&raw, "developed", fmt_ext(&req));
    pipeline::ensure_parent(&out)?;
    render::render_to_file(&raw, &req.recipe, &out, denoise_opts(&req, &state.cfg).as_ref())?;
    respond_text(request, &out.display().to_string())
}

/// Render and stream the image back as a download (browser "Save As"), without
/// leaving a copy in ./out. Renders to a temp file, then streams + deletes it.
fn api_download(mut request: Request, state: &AppState) -> Result<()> {
    let req: DevelopReq = read_json(&mut request)?;
    let raw = state.at(req.id).ok_or_else(|| anyhow!("bad id"))?;
    let ext = fmt_ext(&req);
    let tmp = std::env::temp_dir().join(format!(
        "autoshop_dl_{}_{}.{ext}",
        std::process::id(),
        DL_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    render::render_to_file(&raw, &req.recipe, &tmp, denoise_opts(&req, &state.cfg).as_ref())?;
    let bytes = std::fs::read(&tmp).with_context(|| format!("read {}", tmp.display()))?;
    let _ = std::fs::remove_file(&tmp);
    let ctype = if ext == "jpg" { "image/jpeg" } else { "image/tiff" };
    let filename = format!("{}.developed.{ext}", pipeline::stem(&raw));
    let ct = Header::from_bytes(&b"Content-Type"[..], ctype.as_bytes()).unwrap();
    let cd = Header::from_bytes(
        &b"Content-Disposition"[..],
        format!("attachment; filename=\"{filename}\"").as_bytes(),
    )
    .unwrap();
    request
        .respond(Response::from_data(bytes).with_header(ct).with_header(cd))
        .map_err(Into::into)
}

static DL_SEQ: AtomicU64 = AtomicU64::new(0);

fn api_xmp(mut request: Request, state: &AppState) -> Result<()> {
    let req: XmpReq = read_json(&mut request)?;
    let raw = state.at(req.id).ok_or_else(|| anyhow!("bad id"))?;
    let path = pipeline::write_xmp(&raw, &req.recipe)?;
    respond_text(request, &path.display().to_string())
}

// --- helpers ---------------------------------------------------------------

fn is_baked_ext(p: &Path) -> bool {
    p.extension().and_then(|x| x.to_str()).is_some_and(|x| {
        matches!(x.to_ascii_lowercase().as_str(), "png" | "tif" | "tiff" | "jpg" | "jpeg")
    })
}

fn raw_for(request: &Request, state: &AppState) -> Result<PathBuf> {
    let id = query_param(request.url(), "id")
        .and_then(|v| v.parse::<usize>().ok())
        .ok_or_else(|| anyhow!("missing/invalid id"))?;
    state.at(id).ok_or_else(|| anyhow!("bad id"))
}

fn query_param(url: &str, key: &str) -> Option<String> {
    let q = url.split_once('?')?.1;
    q.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (k == key).then(|| v.to_string())
    })
}

fn read_json<T: serde::de::DeserializeOwned>(request: &mut Request) -> Result<T> {
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body).context("read body")?;
    serde_json::from_str(&body).context("parse request JSON")
}

fn respond_json(request: Request, v: &serde_json::Value) -> Result<()> {
    let header = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
    request
        .respond(Response::from_string(v.to_string()).with_header(header))
        .map_err(Into::into)
}

fn respond_html(request: Request, html: &str) -> Result<()> {
    let header = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap();
    request
        .respond(Response::from_string(html).with_header(header))
        .map_err(Into::into)
}

fn respond_text(request: Request, text: &str) -> Result<()> {
    request.respond(Response::from_string(text)).map_err(Into::into)
}

fn respond_jpeg(request: Request, img: &DynamicImage) -> Result<()> {
    let mut buf = Vec::new();
    img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Jpeg)
        .context("encode jpeg")?;
    let header = Header::from_bytes(&b"Content-Type"[..], &b"image/jpeg"[..]).unwrap();
    request
        .respond(Response::from_data(buf).with_header(header))
        .map_err(Into::into)
}

fn respond_status(request: Request, code: u16, msg: &str) -> Result<()> {
    request
        .respond(Response::from_string(msg).with_status_code(code))
        .map_err(Into::into)
}
