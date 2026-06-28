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
use base64::Engine as _;
use image::{DynamicImage, ImageFormat};
use serde::Deserialize;
use serde_json::json;
use tiny_http::{Header, Request, Response, Server};

use crate::config::{Config, LocalSettings};
use crate::decode;
use crate::denoise::DenoiseOpts;
use crate::pipeline;
use crate::recipe::EditRecipe;
use crate::render;

const INDEX_HTML: &str = include_str!("web/index.html");
const LIST_CAP: usize = 1000; // cap thumbnails shown

struct AppState {
    /// The working directory the gallery lists from. Behind a lock so the UI can
    /// switch folders at runtime (POST /api/setdir re-scans and swaps it + `raws`).
    dir: RwLock<PathBuf>,
    /// The source list, mutable so the UI can import more / switch folders at runtime.
    raws: RwLock<Vec<PathBuf>>,
    /// Config behind a lock so the Settings panel can hot-reload it (POST
    /// /api/settings rewrites the local file, then swaps in a fresh `Config`).
    cfg: RwLock<Config>,
}

impl AppState {
    /// The path at index `id` (cloned, lock released immediately).
    fn at(&self, id: usize) -> Option<PathBuf> {
        self.raws.read().ok()?.get(id).cloned()
    }
    fn count(&self) -> usize {
        self.raws.read().map(|r| r.len()).unwrap_or(0)
    }
    /// Current working-directory path as a display string (recovers from poison).
    fn dir_display(&self) -> String {
        self.dir.read().unwrap_or_else(|e| e.into_inner()).display().to_string()
    }
    /// Current config snapshot (read guard; recovers from a poisoned lock).
    fn config(&self) -> std::sync::RwLockReadGuard<'_, Config> {
        self.cfg.read().unwrap_or_else(|e| e.into_inner())
    }
}

pub fn serve(dir: &Path, port: u16) -> Result<()> {
    // Sources = RAWs + already-baked PNG/TIFF/JPEG (the PNG-source edit mode).
    let raws = pipeline::find_sources(dir)?;
    let n = raws.len();
    let state = Arc::new(AppState {
        dir: RwLock::new(dir.to_path_buf()),
        raws: RwLock::new(raws),
        cfg: RwLock::new(Config::load()),
    });
    let addr = format!("127.0.0.1:{port}");
    let server = Server::http(&addr).map_err(|e| anyhow!("start server on {addr}: {e}"))?;
    println!("Autoshop UI: {n} source(s) under {}", dir.display());
    println!("  open  →  http://{addr}");
    if state.config().openai_api_key.is_none() {
        println!("  note: no image API key set — Analyze will use the heuristic baseline.");
        println!("        configure providers + keys in the in-app Settings (⚙) panel.");
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
        (true, "/api/style-build") => api_style_build(request, state),
        (false, "/api/settings") => api_settings_get(request, state),
        (true, "/api/settings") => api_settings_post(request, state),
        (true, "/api/setdir") => api_setdir(request, state),
        (true, "/api/import") => api_import(request, state),
        (true, "/api/upload") => api_upload(request, state),
        (true, "/api/analyze") => api_analyze(request, state),
        (true, "/api/develop") => api_develop(request, state),
        (true, "/api/retouch") => api_retouch(request, state),
        (true, "/api/heal") => api_heal(request, state),
        (true, "/api/export") => api_export(request, state),
        (true, "/api/download") => api_download(request, state),
        (true, "/api/xmp") => api_xmp(request, state),
        _ => respond_status(request, 404, "not found"),
    }
}

// --- handlers --------------------------------------------------------------

fn api_list(request: Request, state: &AppState) -> Result<()> {
    // Pagination: `?offset=&limit=` page through the full list (a folder can hold
    // thousands). `id` stays the GLOBAL index (enumerate BEFORE skip), so
    // selecting / previewing by id works across pages. `limit` is capped at
    // LIST_CAP to bound the per-request decode/JSON work.
    let offset = query_param(request.url(), "offset")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let limit = query_param(request.url(), "limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(LIST_CAP)
        .clamp(1, LIST_CAP);
    let raws = state.raws.read().map_err(|_| anyhow!("lock poisoned"))?;
    let total = raws.len();
    let items: Vec<_> = raws
        .iter()
        .enumerate()
        .skip(offset)
        .take(limit)
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
        "dir": state.dir_display(),
        "total": total,
        "offset": offset,
        "limit": limit,
        "shown": items.len(),
        "items": items,
    });
    respond_json(request, &body)
}

#[derive(Deserialize)]
struct SetDirReq {
    /// A folder path on disk to make the new working directory.
    path: String,
}

/// Switch the working directory at runtime: re-scan `path` for sources and
/// replace the gallery. Path-based (a browser can't hand a local server a picked
/// folder's real disk path), mirroring the Import field. Any files uploaded into
/// ./out/imported drop out of the view but stay on disk.
fn api_setdir(mut request: Request, state: &AppState) -> Result<()> {
    let req: SetDirReq = read_json(&mut request)?;
    // Tolerate Windows "Copy as path" (quotes) + stray whitespace, like Import.
    let cleaned = req.path.trim().trim_matches('"').trim();
    let p = PathBuf::from(cleaned);
    if !p.is_dir() {
        return respond_status(request, 400, &format!("not a folder: {cleaned}"));
    }
    let found = pipeline::find_sources(&p)?;
    let total = found.len();
    {
        let mut raws = state.raws.write().map_err(|_| anyhow!("lock poisoned"))?;
        *raws = found;
    }
    {
        let mut dir = state.dir.write().map_err(|_| anyhow!("lock poisoned"))?;
        *dir = p.clone();
    }
    respond_json(request, &json!({ "dir": p.display().to_string(), "total": total }))
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
        .map(|h| percent_decode(h.value.as_str()))
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
fn api_style_info(request: Request, state: &AppState) -> Result<()> {
    let abs = |p: &str| {
        std::path::absolute(p).map(|x| x.display().to_string()).unwrap_or_else(|_| p.to_string())
    };
    // Style reference library status (built? how many edits? scene tags?). The
    // index doesn't record the folder it was built from, so we don't claim one.
    let style = match crate::style::StyleIndex::load(Path::new("out/style-index.json")) {
        Ok(ix) => {
            let mut tags: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
            for e in &ix.exemplars {
                *tags.entry(e.tag.clone()).or_default() += 1;
            }
            let mut top: Vec<_> = tags.into_iter().collect();
            top.sort_by(|a, b| b.1.cmp(&a.1));
            top.truncate(6);
            let scenes: Vec<_> = top.into_iter().map(|(t, n)| json!({ "tag": t, "n": n })).collect();
            json!({ "built": true, "total": ix.exemplars.len(), "scenes": scenes,
                    "index_file": abs("out/style-index.json"), "source_dir": ix.source_dir })
        }
        Err(_) => json!({ "built": false }),
    };
    respond_json(
        request,
        &json!({
            // Where the photos being browsed live (the "原图库"), where outputs
            // land (the "成片库" = ./out), and the style-library status.
            "working_dir": state.dir_display(),
            "working_count": state.count(),
            "out_dir": abs("out"),
            "style": style,
        }),
    )
}

#[derive(Deserialize)]
struct StyleBuildReq {
    /// Folder of the user's edited RAWs (each RAW with its Lightroom .xmp beside it).
    dir: String,
}

/// Build the style reference index from a folder of the user's RAW+.xmp pairs, so
/// non-CLI users can point the app at THEIR OWN library from the info panel. Writes
/// out/style-index.json (same as `autoshop style-index <dir>`). Decodes every RAW,
/// so it can take minutes on a large library.
fn api_style_build(mut request: Request, _state: &AppState) -> Result<()> {
    let req: StyleBuildReq = read_json(&mut request)?;
    let cleaned = req.dir.trim().trim_matches('"').trim();
    let p = PathBuf::from(cleaned);
    if !p.is_dir() {
        return respond_status(request, 400, &format!("not a folder: {cleaned}"));
    }
    let index = match crate::style::StyleIndex::build(&p) {
        Ok(ix) => ix,
        Err(e) => return respond_status(request, 500, &format!("build failed: {e}")),
    };
    let total = index.exemplars.len();
    if let Err(e) = index.save(Path::new("out/style-index.json")) {
        return respond_status(request, 500, &format!("save index: {e}"));
    }
    respond_json(
        request,
        &json!({ "ok": true, "total": total, "source_dir": p.display().to_string() }),
    )
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
#[derive(Deserialize)]
struct RetouchReq {
    id: usize,
    /// What should fill the painted region (e.g. "remove the trash can").
    prompt: String,
    /// RGBA PNG mask as a data URL or bare base64 — transparent pixels = the
    /// region to regenerate (the brush-painted area in the UI).
    mask: String,
    /// Output quality tier (low|medium|high|auto). Falls back to the config default.
    #[serde(default)]
    quality: Option<String>,
    /// Composite onto the full-sensor develop (61 MP) instead of the embedded
    /// preview. Slow; the regenerated patch is upscaled. RAW only.
    #[serde(default)]
    full_res: bool,
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
    let cfg = state.config();
    let style = req.style_strength.unwrap_or(cfg.style_strength);
    let (recipe, verdict) =
        pipeline::produce_recipe(&raw, &cfg, false, guidance, req.base.as_ref(), style)?;
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
    render::render_to_file(&raw, &req.recipe, &out, denoise_opts(&req, &state.config()).as_ref())?;
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
    render::render_to_file(&raw, &req.recipe, &tmp, denoise_opts(&req, &state.config()).as_ref())?;
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

/// Generative fill (Phase 4 in the UI): the browser posts a painted RGBA mask
/// (transparent = regenerate) + a prompt; we run [`generative::retouch`], which
/// composites the regenerated region back onto the FULL-resolution source and
/// writes the master to ./out. We return a resized JPEG of the result for inline
/// display, with the saved master path in `X-Output-Path`. Needs OPENAI_API_KEY.
fn api_retouch(mut request: Request, state: &AppState) -> Result<()> {
    let req: RetouchReq = read_json(&mut request)?;
    let raw = match state.at(req.id) {
        Some(r) => r,
        None => return respond_status(request, 400, "bad id"),
    };
    // Accept either a "data:image/png;base64,XXXX" URL or bare base64.
    let b64 = req.mask.rsplit(',').next().unwrap_or(&req.mask).trim();
    let mask_bytes = match base64::engine::general_purpose::STANDARD.decode(b64) {
        Ok(b) => b,
        Err(e) => return respond_status(request, 400, &format!("bad mask base64: {e}")),
    };
    // generative::retouch takes a mask FILE path, so stage the PNG in a temp file.
    let mask_tmp = std::env::temp_dir().join(format!(
        "autoshop_mask_{}_{}.png",
        std::process::id(),
        DL_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    if let Err(e) = std::fs::write(&mask_tmp, &mask_bytes) {
        return respond_status(request, 500, &format!("stage mask: {e}"));
    }
    let out = pipeline::default_out(&raw, "retouch", "png");
    let cfg = state.config();
    let quality = req.quality.unwrap_or_else(|| cfg.openai_image_quality.clone());
    let result =
        crate::generative::retouch(&cfg, &raw, &mask_tmp, &req.prompt, &quality, req.full_res, &out);
    drop(cfg);
    let _ = std::fs::remove_file(&mask_tmp);
    match result {
        Ok(()) => {
            let img = decode::load_image(&out)?
                .resize(1400, 1400, image::imageops::FilterType::Triangle);
            let mut buf = Vec::new();
            img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Jpeg)
                .context("encode jpeg")?;
            let ct = Header::from_bytes(&b"Content-Type"[..], &b"image/jpeg"[..]).unwrap();
            let xp = Header::from_bytes(&b"X-Output-Path"[..], out.display().to_string().as_bytes())
                .unwrap();
            request
                .respond(Response::from_data(buf).with_header(ct).with_header(xp))
                .map_err(Into::into)
        }
        Err(e) => respond_status(request, 500, &format!("retouch failed: {e}")),
    }
}

#[derive(Deserialize)]
struct HealReq {
    id: usize,
    /// Optional painted RGBA PNG mask (data URL or bare base64); transparent = heal here.
    #[serde(default)]
    mask: Option<String>,
    /// Auto-detect spots with the vision model (default true).
    #[serde(default = "default_true")]
    auto: bool,
    #[serde(default)]
    full_res: bool,
}
fn default_true() -> bool {
    true
}

/// Pixel-retouch (heal) mode: the vision model auto-detects small defects and/or
/// the browser posts a painted mask; the deterministic engine heals each from
/// SURROUNDING REAL pixels (no generation). Saves a pixel master to ./out and
/// returns a JPEG of the result for inline display, path in `X-Output-Path`.
fn api_heal(mut request: Request, state: &AppState) -> Result<()> {
    let req: HealReq = read_json(&mut request)?;
    let raw = match state.at(req.id) {
        Some(r) => r,
        None => return respond_status(request, 400, "bad id"),
    };
    // Stage the optional painted mask (data URL or bare base64) to a temp PNG.
    let mask_tmp = match &req.mask {
        Some(m) if !m.trim().is_empty() => {
            let b64 = m.rsplit(',').next().unwrap_or(m).trim();
            match base64::engine::general_purpose::STANDARD.decode(b64) {
                Ok(bytes) => {
                    let t = std::env::temp_dir().join(format!(
                        "autoshop_heal_{}_{}.png",
                        std::process::id(),
                        DL_SEQ.fetch_add(1, Ordering::Relaxed)
                    ));
                    if let Err(e) = std::fs::write(&t, &bytes) {
                        return respond_status(request, 500, &format!("stage mask: {e}"));
                    }
                    Some(t)
                }
                Err(e) => return respond_status(request, 400, &format!("bad mask base64: {e}")),
            }
        }
        _ => None,
    };
    let out = pipeline::default_out(&raw, "heal", "png");
    let cfg = state.config();
    let result = crate::retouch::heal(&cfg, &raw, mask_tmp.as_deref(), req.auto, req.full_res, &out);
    drop(cfg);
    if let Some(t) = &mask_tmp {
        let _ = std::fs::remove_file(t);
    }
    match result {
        Ok(rep) => {
            let img =
                decode::load_image(&out)?.resize(1400, 1400, image::imageops::FilterType::Triangle);
            let mut buf = Vec::new();
            img.write_to(&mut Cursor::new(&mut buf), ImageFormat::Jpeg)
                .context("encode jpeg")?;
            let ct = Header::from_bytes(&b"Content-Type"[..], &b"image/jpeg"[..]).unwrap();
            let xp = Header::from_bytes(&b"X-Output-Path"[..], out.display().to_string().as_bytes())
                .unwrap();
            let xs =
                Header::from_bytes(&b"X-Heal-Spots"[..], rep.spots.to_string().as_bytes()).unwrap();
            request
                .respond(Response::from_data(buf).with_header(ct).with_header(xp).with_header(xs))
                .map_err(Into::into)
        }
        Err(e) => respond_status(request, 500, &format!("heal failed: {e}")),
    }
}

/// Current provider/model settings for the Settings panel. Never returns the raw
/// API keys — only whether each is present.
fn api_settings_get(request: Request, state: &AppState) -> Result<()> {
    let cfg = state.config();
    let body = json!({
        "analysis": {
            "provider": cfg.analysis_provider,
            "model": cfg.analysis_model,
            "base_url": cfg.analysis_base_url,
            "key_present": cfg.analysis_api_key.is_some(),
        },
        "image": {
            "model": cfg.openai_model,
            "base_url": cfg.openai_base_url,
            "gen_model": cfg.openai_image_model,
            "key_present": cfg.openai_api_key.is_some(),
        },
        // The `claude` CLI has no image input in print mode → image-via-OAuth is
        // not available; the image role always uses an OpenAI-compatible API.
        "image_oauth_supported": false,
        "settings_file": crate::config::local_settings_path().display().to_string(),
    });
    respond_json(request, &body)
}

/// Persist provider/model/key changes to the gitignored local file, then
/// hot-reload the running config. Blank key fields are left unchanged (the GET
/// side never reveals existing keys, so the UI sends a key only when it changes).
fn api_settings_post(mut request: Request, state: &AppState) -> Result<()> {
    let inc: LocalSettings = read_json(&mut request)?;
    let mut cur = crate::config::load_local_settings();
    // Non-secret fields: take whatever the UI sent (empty ⇒ falls back to default).
    if inc.analysis_provider.is_some() {
        cur.analysis_provider = inc.analysis_provider;
    }
    if inc.analysis_model.is_some() {
        cur.analysis_model = inc.analysis_model;
    }
    if inc.analysis_base_url.is_some() {
        cur.analysis_base_url = inc.analysis_base_url;
    }
    if inc.image_model.is_some() {
        cur.image_model = inc.image_model;
    }
    if inc.image_base_url.is_some() {
        cur.image_base_url = inc.image_base_url;
    }
    if inc.image_gen_model.is_some() {
        cur.image_gen_model = inc.image_gen_model;
    }
    // Secrets: only overwrite when a non-empty value was actually provided.
    if let Some(k) = inc.analysis_api_key.filter(|s| !s.trim().is_empty()) {
        cur.analysis_api_key = Some(k);
    }
    if let Some(k) = inc.image_api_key.filter(|s| !s.trim().is_empty()) {
        cur.image_api_key = Some(k);
    }

    let path = crate::config::save_local_settings(&cur).map_err(|e| anyhow!("write settings: {e}"))?;
    *state.cfg.write().unwrap_or_else(|e| e.into_inner()) = Config::load();
    respond_json(request, &json!({ "ok": true, "saved": path.display().to_string() }))
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

/// Percent-decode a value (e.g. an `encodeURIComponent`-encoded filename) back to
/// its UTF-8 string. HTTP header values are ISO-8859-1 only, so the browser must
/// percent-encode non-ASCII filenames (Chinese, emoji, …) — we decode them here.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) =
                ((b[i + 1] as char).to_digit(16), (b[i + 2] as char).to_digit(16))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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
    let ct = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap();
    // No-cache: the UI HTML is embedded in the binary and changes on every rebuild,
    // so the browser MUST re-fetch it after a restart — otherwise a stale cached
    // page hides fixes/features until a manual Ctrl+F5.
    let cc = Header::from_bytes(
        &b"Cache-Control"[..],
        &b"no-cache, no-store, must-revalidate"[..],
    )
    .unwrap();
    request
        .respond(Response::from_string(html).with_header(ct).with_header(cc))
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

#[cfg(test)]
mod tests {
    use super::percent_decode;

    #[test]
    fn percent_decode_unicode_and_literals() {
        assert_eq!(percent_decode("DSC09528.ARW"), "DSC09528.ARW"); // ASCII untouched
        // encodeURIComponent("测试照片.png")
        assert_eq!(percent_decode("%E6%B5%8B%E8%AF%95%E7%85%A7%E7%89%87.png"), "测试照片.png");
        assert_eq!(percent_decode("a%20b.jpg"), "a b.jpg"); // %20 = space
        assert_eq!(percent_decode("100%25.png"), "100%.png"); // literal percent round-trips
        assert_eq!(percent_decode("bad%ZZ"), "bad%ZZ"); // invalid escape passes through
        assert_eq!(percent_decode("tail%E6"), "tail\u{fffd}"); // truncated UTF-8 → replacement
    }
}
