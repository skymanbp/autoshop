//! Local web UI server. `autoshop serve --dir <library>` starts a tiny HTTP
//! server; open the printed URL in a browser. Photos are addressed by their
//! index in the scanned list (`?id=N`) so we never URL-encode Windows paths.
//!
//! Interactive feedback (before/after, slider tweaks) runs on the *embedded
//! preview* via [`render::develop_preview`] (fast); only explicit "Export" runs
//! the full-resolution [`render::render_to_image`].

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use image::{DynamicImage, ImageFormat};
use serde::Deserialize;
use serde_json::json;
use tiny_http::{Header, Request, Response, Server};

use crate::config::Config;
use crate::decode;
use crate::pipeline;
use crate::recipe::EditRecipe;
use crate::render;

const INDEX_HTML: &str = include_str!("web/index.html");
const LIST_CAP: usize = 300; // cap thumbnails shown in v1

struct AppState {
    dir: PathBuf,
    raws: Vec<PathBuf>,
    cfg: Config,
}

pub fn serve(dir: &Path, port: u16) -> Result<()> {
    let raws = pipeline::find_raws(dir)?;
    let state = Arc::new(AppState {
        dir: dir.to_path_buf(),
        raws,
        cfg: Config::load(),
    });
    let addr = format!("127.0.0.1:{port}");
    let server = Server::http(&addr).map_err(|e| anyhow!("start server on {addr}: {e}"))?;
    println!("Autoshop UI: {} RAW(s) under {}", state.raws.len(), dir.display());
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
        (true, "/api/analyze") => api_analyze(request, state),
        (true, "/api/develop") => api_develop(request, state),
        (true, "/api/export") => api_export(request, state),
        (true, "/api/xmp") => api_xmp(request, state),
        _ => respond_status(request, 404, "not found"),
    }
}

// --- handlers --------------------------------------------------------------

fn api_list(request: Request, state: &AppState) -> Result<()> {
    let items: Vec<_> = state
        .raws
        .iter()
        .take(LIST_CAP)
        .enumerate()
        .map(|(id, raw)| {
            let analyzed = pipeline::default_out(raw, "recipe", "json").exists()
                || pipeline::xmp_target(raw, false).exists()
                || pipeline::xmp_target(raw, true).exists();
            json!({ "id": id, "stem": pipeline::stem(raw), "analyzed": analyzed })
        })
        .collect();
    let body = json!({
        "dir": state.dir.display().to_string(),
        "total": state.raws.len(),
        "shown": items.len(),
        "items": items,
    });
    respond_json(request, &body)
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

#[derive(Deserialize)]
struct IdReq {
    id: usize,
}
#[derive(Deserialize)]
struct DevelopReq {
    id: usize,
    recipe: EditRecipe,
}
#[derive(Deserialize)]
struct XmpReq {
    id: usize,
    recipe: EditRecipe,
    beside: bool,
}

fn api_analyze(mut request: Request, state: &AppState) -> Result<()> {
    let req: IdReq = read_json(&mut request)?;
    let raw = state.raws.get(req.id).ok_or_else(|| anyhow!("bad id"))?.clone();
    let (recipe, verdict) = pipeline::produce_recipe(&raw, &state.cfg, false)?;
    pipeline::write_recipe(&raw, &recipe, None)?;
    pipeline::write_xmp(&raw, &recipe, false)?;
    respond_json(request, &json!({ "recipe": recipe, "verdict": verdict }))
}

fn api_develop(mut request: Request, state: &AppState) -> Result<()> {
    let req: DevelopReq = read_json(&mut request)?;
    let raw = state.raws.get(req.id).ok_or_else(|| anyhow!("bad id"))?.clone();
    let preview = decode::preview_only(&raw)?
        .resize(1200, 1200, image::imageops::FilterType::Triangle);
    let after = render::develop_preview(&preview, &req.recipe);
    respond_jpeg(request, &after)
}

fn api_export(mut request: Request, state: &AppState) -> Result<()> {
    let req: DevelopReq = read_json(&mut request)?;
    let raw = state.raws.get(req.id).ok_or_else(|| anyhow!("bad id"))?.clone();
    let out = pipeline::default_out(&raw, "developed", "jpg");
    pipeline::ensure_parent(&out)?;
    render::render_to_file(&raw, &req.recipe, &out)?;
    respond_text(request, &out.display().to_string())
}

fn api_xmp(mut request: Request, state: &AppState) -> Result<()> {
    let req: XmpReq = read_json(&mut request)?;
    let raw = state.raws.get(req.id).ok_or_else(|| anyhow!("bad id"))?.clone();
    let path = pipeline::write_xmp(&raw, &req.recipe, req.beside)?;
    respond_text(request, &path.display().to_string())
}

// --- helpers ---------------------------------------------------------------

fn raw_for(request: &Request, state: &AppState) -> Result<PathBuf> {
    let id = query_param(request.url(), "id")
        .and_then(|v| v.parse::<usize>().ok())
        .ok_or_else(|| anyhow!("missing/invalid id"))?;
    state.raws.get(id).cloned().ok_or_else(|| anyhow!("bad id"))
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
