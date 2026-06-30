// Release builds run WITHOUT a console window — a GUI app shouldn't flash a
// terminal on launch. Debug keeps the console so panics/logs stay visible.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Autoshop — native desktop GUI (egui/eframe).
//!
//! A real native window (no localhost server, no webview): it links the
//! `autoshop` engine library and calls `decode` / `render` / `pipeline` directly
//! in-process. Open a RAW or image, develop it with live before/after, run the
//! AI auto-develop, and export — all from one window.
//!
//! Build/run: `cargo run --release --features gui --bin autoshop-gui`

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

use eframe::egui;
use egui::load::SizedTexture;

use autoshop::recipe::{EditRecipe, Hsl, ColorGrade};
use image::GenericImageView;

const PREVIEW_EDGE: u32 = 1280; // working preview size for fast live develop
const THUMB_EDGE: u32 = 160; // decoded gallery-thumbnail long edge
const THUMB_W: f32 = 56.0; // displayed thumbnail size in the gallery
const THUMB_H: f32 = 40.0;
const GALLERY_ROW_H: f32 = 50.0; // fixed row height for ScrollArea::show_rows
const MAX_THUMB_INFLIGHT: usize = 6; // cap concurrent thumbnail decodes
const HSL_BANDS: [&str; 8] = ["Red", "Orange", "Yellow", "Green", "Aqua", "Blue", "Purple", "Magenta"];
const GRADE_REGIONS: [&str; 4] = ["shadow", "midtone", "highlight", "global"];

const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x4c, 0x8b, 0xf5);
const SEL_BG: egui::Color32 = egui::Color32::from_rgb(0x26, 0x41, 0x7a);
const PILL: egui::Color32 = egui::Color32::from_rgb(0xc9, 0xa1, 0x4a);

/// Messages from worker threads back to the UI. The large payloads are boxed so
/// the channel message stays small (clippy::large_enum_variant).
enum Msg {
    Opened(Box<anyhow::Result<image::DynamicImage>>),
    Analyzed(Box<anyhow::Result<(EditRecipe, autoshop::advisor::Verdict)>>),
    Exported(anyhow::Result<String>),
    /// A folder scan finished: (folder, sorted source paths).
    Folder(Box<anyhow::Result<(PathBuf, Vec<PathBuf>)>>),
    /// A gallery thumbnail decoded. `generation` tags the folder generation so a
    /// folder switch can't insert a stale thumbnail under a reused index.
    Thumb { generation: u64, idx: usize, img: Box<anyhow::Result<image::DynamicImage>> },
    /// A generative-fill / heal result: (preview of the saved ./out master, status).
    Retouched(Box<anyhow::Result<(image::DynamicImage, String)>>),
}

/// Editable buffers for the in-app Settings window. Key fields stay blank on
/// load and only overwrite the stored key when non-empty (the form never shows
/// an existing secret) — mirroring the web `/api/settings` contract.
#[derive(Default)]
struct SettingsForm {
    analysis_provider_api: bool, // false = OAuth (claude CLI), true = OpenAI-compatible API
    analysis_model: String,
    analysis_base_url: String,
    analysis_api_key: String,
    analysis_key_present: bool,
    image_model: String,
    image_base_url: String,
    image_gen_model: String,
    image_api_key: String,
    image_key_present: bool,
    status: String,
}

struct AutoshopApp {
    src_path: Option<PathBuf>,
    base_preview: Option<image::DynamicImage>, // decoded source preview (re-developed on edit)
    before_tex: Option<egui::TextureHandle>,
    after_tex: Option<egui::TextureHandle>,
    recipe: EditRecipe,
    dirty: bool, // recipe changed → re-develop the after preview
    status: String,
    busy: bool, // an analyze/export thread is running
    rx: Option<Receiver<Msg>>,
    tx: Sender<Msg>,
    verdict: Option<String>,
    rationale: String,
    style_strength: f32,
    hsl_band: usize,
    grade_region: usize,
    guidance: String, // free-text direction for the AI ("warmer, moodier")
    refine: bool,     // adjust the CURRENT recipe vs propose from scratch
    save_jpeg: bool,  // export/download as JPEG instead of 16-bit TIFF
    // --- undo / redo (recipe snapshots; a drag is one step, committed on release) ---
    committed: EditRecipe,        // current history head (last committed state)
    undo_stack: Vec<EditRecipe>,  // prior states, most recent last
    redo_stack: Vec<EditRecipe>,  // states undone away (cleared on a new edit)
    // --- settings / denoise ---
    save_denoise: bool,     // run SCUNet AI denoise before the full-res render
    show_settings: bool,    // the Settings window is open
    settings: SettingsForm, // editable buffers for that window
    // --- library / gallery ---
    gallery: Vec<PathBuf>,          // sources in the working folder (sorted)
    gallery_dir: Option<PathBuf>,   // the working folder
    gallery_gen: u64,               // bumped on every folder load (thumb invalidation)
    selected: Option<usize>,        // index of the open gallery photo (for highlight)
    thumbs: HashMap<usize, egui::TextureHandle>, // decoded thumbnails by index
    thumb_requested: HashSet<usize>,             // indices already queued/decoded
    thumb_inflight: usize,                       // live thumbnail-decode threads
    // --- region box-select (local-edit target on the After image) ---
    region: Option<[f32; 4]>,                      // normalized [left, top, right, bottom]
    region_drag: Option<(egui::Pos2, egui::Pos2)>, // transient drag (start, current) in screen px
    // --- retouch: mask painting + generative fill + heal ---
    paint_mode: bool,                      // brush-paint a mask (pauses box-select)
    brush: f32,                            // brush radius in After-image display px
    mask_paint: Option<image::RgbaImage>,  // painted overlay (red where painted), at preview res
    mask_tex: Option<egui::TextureHandle>, // overlay texture
    mask_dirty: bool,                      // re-upload the overlay
    paint_last: Option<(f32, f32)>,        // last brush point in mask px (line fill)
    fill_prompt: String,                   // generative-fill instruction
    fill_quality: usize,                   // 0=high 1=medium 2=low
    fill_fullres: bool,                    // composite onto the full-res develop
    heal_fullres: bool,                    // heal the full-res develop
}

impl Default for AutoshopApp {
    fn default() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            src_path: None,
            base_preview: None,
            before_tex: None,
            after_tex: None,
            recipe: EditRecipe::default(),
            dirty: false,
            status: "Open a photo, or open a folder to browse your library.".into(),
            busy: false,
            rx: Some(rx),
            tx,
            verdict: None,
            rationale: String::new(),
            style_strength: 0.30,
            hsl_band: 0,
            grade_region: 0,
            guidance: String::new(),
            refine: false,
            save_jpeg: false,
            committed: EditRecipe::default(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            save_denoise: false,
            show_settings: false,
            settings: SettingsForm::default(),
            gallery: Vec::new(),
            gallery_dir: None,
            gallery_gen: 0,
            selected: None,
            thumbs: HashMap::new(),
            thumb_requested: HashSet::new(),
            thumb_inflight: 0,
            region: None,
            region_drag: None,
            paint_mode: false,
            brush: 30.0,
            mask_paint: None,
            mask_tex: None,
            mask_dirty: false,
            paint_last: None,
            fill_prompt: String::new(),
            fill_quality: 0,
            fill_fullres: false,
            heal_fullres: false,
        }
    }
}

/// `image::DynamicImage` → egui texture-ready colour image.
fn to_color_image(img: &image::DynamicImage) -> egui::ColorImage {
    let rgba = img.to_rgba8();
    let (w, h) = img.dimensions();
    egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw())
}

/// Stamp a filled brush dot into the paint mask (painted = translucent red).
fn stamp_dot(m: &mut image::RgbaImage, c: (f32, f32), r: f32) {
    let (w, h) = (m.width() as i32, m.height() as i32);
    let (cx, cy) = c;
    let r2 = r * r;
    let x0 = (cx - r).floor().max(0.0) as i32;
    let x1 = ((cx + r).ceil() as i32).min(w - 1);
    let y0 = (cy - r).floor().max(0.0) as i32;
    let y1 = ((cy + r).ceil() as i32).min(h - 1);
    for y in y0..=y1 {
        for x in x0..=x1 {
            let (dx, dy) = (x as f32 - cx, y as f32 - cy);
            if dx * dx + dy * dy <= r2 {
                m.put_pixel(x as u32, y as u32, image::Rgba([255, 64, 64, 160]));
            }
        }
    }
}

/// Stamp a brush stroke between two points (interpolated dots — no gaps).
fn stamp_line(m: &mut image::RgbaImage, a: (f32, f32), b: (f32, f32), r: f32) {
    let dist = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
    let steps = (dist / (r * 0.5).max(1.0)).ceil().max(1.0) as i32;
    for i in 0..=steps {
        let t = i as f32 / steps as f32;
        stamp_dot(m, (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t), r);
    }
}

impl AutoshopApp {
    fn open_path(&mut self, path: PathBuf) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.src_path = Some(path.clone());
        self.status = format!("decoding {} …", path.display());
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            // Build a CLEAN preview base by developing the RAW sensor data
            // (downscaled), NOT the camera's already-baked 8-bit JPEG preview:
            // re-developing that double-processes it and amplifies its grain when
            // you push tone/clarity. Baked images (PNG/TIFF/JPEG) are their own
            // source. Demosaic is slow, so this runs off the UI thread.
            let res = (|| -> anyhow::Result<image::DynamicImage> {
                let full = if autoshop::decode::is_raw(&path) {
                    autoshop::render::render_to_image(&path, &EditRecipe::default(), None)?
                } else {
                    autoshop::decode::load_image(&path)?
                };
                Ok(full.thumbnail(PREVIEW_EDGE, PREVIEW_EDGE))
            })();
            let _ = tx.send(Msg::Opened(Box::new(res)));
        });
    }

    /// Open one of the gallery photos by index (keeps the thumbnail highlighted).
    fn open_gallery_index(&mut self, idx: usize) {
        if self.busy {
            return;
        }
        let Some(path) = self.gallery.get(idx).cloned() else { return };
        self.selected = Some(idx);
        self.open_path(path);
    }

    /// Scan `dir` (recursively) for sources off the UI thread and replace the
    /// gallery — folders can hold thousands of RAWs, so this never blocks paint.
    fn open_folder(&mut self, dir: PathBuf) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.status = format!("scanning {} …", dir.display());
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let res = autoshop::pipeline::find_sources(&dir).map(|list| (dir, list));
            let _ = tx.send(Msg::Folder(Box::new(res)));
        });
    }

    /// Reset undo history — call when a brand-new photo opens (you can't undo
    /// across photos). `committed` becomes the current head.
    fn reset_history(&mut self) {
        self.committed = self.recipe.clone();
        self.undo_stack.clear();
        self.redo_stack.clear();
    }

    /// Commit the current recipe as ONE undo step once the edit gesture settles
    /// (pointer released) — dragging a slider is one step, not one per frame.
    /// Programmatic edits (Analyze, Reset) also land here on the next frame.
    fn commit_if_settled(&mut self, ctx: &egui::Context) {
        if self.recipe != self.committed && !ctx.input(|i| i.pointer.any_down()) {
            self.undo_stack.push(self.committed.clone());
            if self.undo_stack.len() > 100 {
                self.undo_stack.remove(0); // cap history memory
            }
            self.committed = self.recipe.clone();
            self.redo_stack.clear();
        }
    }

    fn undo(&mut self) {
        if let Some(prev) = self.undo_stack.pop() {
            self.redo_stack.push(self.committed.clone());
            self.committed = prev.clone();
            self.recipe = prev;
            self.dirty = true;
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.redo_stack.pop() {
            self.undo_stack.push(self.committed.clone());
            self.committed = next.clone();
            self.recipe = next;
            self.dirty = true;
        }
    }

    /// Populate the Settings form from the resolved config (keys are shown only as
    /// "present", never revealed). Called when the window opens.
    fn load_settings_form(&mut self) {
        let cfg = autoshop::config::Config::load();
        self.settings = SettingsForm {
            analysis_provider_api: cfg.analysis_is_api(),
            analysis_model: cfg.analysis_model.clone(),
            analysis_base_url: cfg.analysis_base_url.clone(),
            analysis_api_key: String::new(),
            analysis_key_present: cfg.analysis_api_key.is_some(),
            image_model: cfg.openai_model.clone(),
            image_base_url: cfg.openai_base_url.clone(),
            image_gen_model: cfg.openai_image_model.clone(),
            image_api_key: String::new(),
            image_key_present: cfg.openai_api_key.is_some(),
            status: String::new(),
        };
    }

    /// Persist the Settings form to autoshop.local.json (gitignored). A blank key
    /// keeps the stored one. The next Analyze/Export reloads Config, so it applies.
    fn save_settings_form(&mut self) {
        let mut cur = autoshop::config::load_local_settings();
        cur.analysis_provider =
            Some(if self.settings.analysis_provider_api { "api" } else { "oauth" }.to_string());
        cur.analysis_model = Some(self.settings.analysis_model.trim().to_string());
        cur.analysis_base_url = Some(self.settings.analysis_base_url.trim().to_string());
        cur.image_model = Some(self.settings.image_model.trim().to_string());
        cur.image_base_url = Some(self.settings.image_base_url.trim().to_string());
        cur.image_gen_model = Some(self.settings.image_gen_model.trim().to_string());
        // Secrets: only overwrite when a non-empty value was actually typed.
        let ak = self.settings.analysis_api_key.trim().to_string();
        let ik = self.settings.image_api_key.trim().to_string();
        if !ak.is_empty() {
            cur.analysis_api_key = Some(ak);
        }
        if !ik.is_empty() {
            cur.image_api_key = Some(ik);
        }
        match autoshop::config::save_local_settings(&cur) {
            Ok(p) => {
                self.settings.analysis_api_key.clear();
                self.settings.image_api_key.clear();
                self.settings.analysis_key_present = cur.analysis_api_key.is_some();
                self.settings.image_key_present = cur.image_api_key.is_some();
                self.settings.status = format!("saved → {}", p.display());
                self.status = "settings saved — applies to the next Analyze".into();
            }
            Err(e) => self.settings.status = format!("save failed: {e}"),
        }
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        let mut do_save = false;
        ui.label(
            egui::RichText::new(
                "Saved to autoshop.local.json (gitignored, stays on this machine). Applies to the next Analyze.",
            )
            .weak()
            .small(),
        );
        {
            let f = &mut self.settings;
            ui.separator();
            ui.heading("Analysis — the verifier");
            ui.horizontal(|ui| {
                ui.label("Provider");
                ui.radio_value(&mut f.analysis_provider_api, false, "OAuth (Claude CLI)");
                ui.radio_value(&mut f.analysis_provider_api, true, "API (OpenAI-compatible)");
            });
            ui.horizontal(|ui| {
                ui.label("Model");
                ui.text_edit_singleline(&mut f.analysis_model);
            });
            if f.analysis_provider_api {
                ui.horizontal(|ui| {
                    ui.label("Base URL");
                    ui.text_edit_singleline(&mut f.analysis_base_url);
                });
                ui.horizontal(|ui| {
                    ui.label("API Key");
                    let hint = if f.analysis_key_present { "key set — blank keeps it" } else { "no key set" };
                    ui.add(egui::TextEdit::singleline(&mut f.analysis_api_key).password(true).hint_text(hint));
                });
            }
            ui.separator();
            ui.heading("Image — the vision proposer (API only)");
            ui.horizontal(|ui| {
                ui.label("Vision model");
                ui.text_edit_singleline(&mut f.image_model);
            });
            ui.horizontal(|ui| {
                ui.label("Base URL");
                ui.text_edit_singleline(&mut f.image_base_url);
            });
            ui.horizontal(|ui| {
                ui.label("Image-gen model");
                ui.text_edit_singleline(&mut f.image_gen_model);
            });
            ui.horizontal(|ui| {
                ui.label("API Key");
                let hint = if f.image_key_present { "key set — blank keeps it" } else { "no key set" };
                ui.add(egui::TextEdit::singleline(&mut f.image_api_key).password(true).hint_text(hint));
            });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Save settings").clicked() {
                    do_save = true;
                }
                if !f.status.is_empty() {
                    ui.label(egui::RichText::new(&f.status).weak().small());
                }
            });
        }
        if do_save {
            self.save_settings_form();
        }
    }

    /// Queue a thumbnail decode for `idx` if it isn't cached/queued and we're
    /// under the concurrency cap. Uses the camera's embedded preview (fast) — the
    /// double-processing concern only applies to the develop base, not a 56px chip.
    fn request_thumb(&mut self, idx: usize) {
        if self.thumbs.contains_key(&idx) || self.thumb_requested.contains(&idx) {
            return;
        }
        if self.thumb_inflight >= MAX_THUMB_INFLIGHT {
            return;
        }
        let Some(path) = self.gallery.get(idx).cloned() else { return };
        self.thumb_requested.insert(idx);
        self.thumb_inflight += 1;
        let tx = self.tx.clone();
        let generation = self.gallery_gen;
        std::thread::spawn(move || {
            let res = (|| -> anyhow::Result<image::DynamicImage> {
                Ok(autoshop::decode::preview_only(&path)?.thumbnail(THUMB_EDGE, THUMB_EDGE))
            })();
            let _ = tx.send(Msg::Thumb { generation, idx, img: Box::new(res) });
        });
    }

    /// Re-develop the working preview through the current recipe.
    fn redevelop(&mut self, ctx: &egui::Context) {
        if let Some(base) = &self.base_preview {
            let after = autoshop::render::develop_preview(base, &self.recipe);
            self.after_tex = Some(ctx.load_texture("after", to_color_image(&after), egui::TextureOptions::LINEAR));
        }
        self.dirty = false;
    }

    fn start_analyze(&mut self) {
        let Some(path) = self.src_path.clone() else { return };
        if self.busy {
            return;
        }
        self.busy = true;
        self.status = if self.refine {
            "refining your current edit with AI…".into()
        } else {
            "analyzing with AI (GPT + Claude)…".into()
        };
        let tx = self.tx.clone();
        let style = self.style_strength;
        // Free-text direction ("warmer, moodier") steers the proposal; when
        // `refine` is on, the AI ADJUSTS the current recipe instead of starting
        // from scratch. A box-selected region (if any) folds into the direction so
        // the AI masks exactly there — same prompt the web UI sends.
        let guidance = {
            let g = self.guidance.trim();
            match self.region {
                Some([l, t, r, b]) => Some(format!(
                    "The user SELECTED a target region (normalized 0..1 frame coords): \
                     left={l:.3} top={t:.3} right={r:.3} bottom={b:.3}. Apply the direction ONLY to \
                     that region — emit a mask covering it (a radial mask with those exact \
                     left/top/right/bottom bounds and feather ~0.4 is ideal, or a linear gradient \
                     for a thin edge band). Direction: {}",
                    if g.is_empty() { "make a tasteful local improvement" } else { g }
                )),
                None => (!g.is_empty()).then(|| g.to_string()),
            }
        };
        let base = self.refine.then(|| self.recipe.clone());
        std::thread::spawn(move || {
            // Config is reloaded in-thread (cheap) so we don't need it to be Clone.
            let cfg = autoshop::config::Config::load();
            let res = autoshop::pipeline::produce_recipe(
                &path,
                &cfg,
                false,
                guidance.as_deref(),
                base.as_ref(),
                style,
            );
            let _ = tx.send(Msg::Analyzed(Box::new(res)));
        });
    }

    /// `./out/<stem>.developed.{tif|jpg}` — the default export target.
    fn default_out(&self) -> PathBuf {
        let stem = self
            .src_path
            .as_deref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("out")
            .to_string();
        let ext = if self.save_jpeg { "jpg" } else { "tif" };
        PathBuf::from("out").join(format!("{stem}.developed.{ext}"))
    }

    /// Render the full-resolution develop to `out` on a worker thread (16-bit
    /// TIFF, or 8-bit JPEG when the path ends in .jpg).
    fn start_render_to(&mut self, out: PathBuf) {
        let Some(path) = self.src_path.clone() else { return };
        if self.busy {
            return;
        }
        self.busy = true;
        self.status = if self.save_denoise {
            format!("rendering + AI denoise → {} … (GPU sidecar, can take minutes)", out.display())
        } else {
            format!("rendering full-resolution → {} …", out.display())
        };
        let tx = self.tx.clone();
        let recipe = self.recipe.clone();
        let denoise = self.save_denoise;
        std::thread::spawn(move || {
            let res = (|| {
                if let Some(p) = out.parent() {
                    std::fs::create_dir_all(p)?;
                }
                // SCUNet AI denoise (python sidecar) runs before the develop when on.
                let opts = denoise.then(|| {
                    autoshop::denoise::DenoiseOpts::from_config(&autoshop::config::Config::load(), None, 1.0)
                });
                autoshop::render::render_to_file(&path, &recipe, &out, opts.as_ref())?;
                Ok::<String, anyhow::Error>(out.display().to_string())
            })();
            let _ = tx.send(Msg::Exported(res));
        });
    }

    fn start_export(&mut self) {
        let out = self.default_out();
        self.start_render_to(out);
    }

    /// Write the Lightroom / Camera-Raw XMP sidecar to ./out (RAW sources only).
    fn save_xmp(&mut self) {
        let Some(path) = self.src_path.clone() else { return };
        if !autoshop::decode::is_raw(&path) {
            self.status = "XMP applies to RAW files only".into();
            return;
        }
        match autoshop::pipeline::write_xmp(&path, &self.recipe) {
            Ok(p) => self.status = format!("XMP saved → {}", p.display()),
            Err(e) => self.status = format!("XMP save failed: {e}"),
        }
    }

    fn poll_workers(&mut self, ctx: &egui::Context) {
        // Drain a bounded batch each frame so a burst of thumbnails doesn't take
        // one-per-frame to land (try_recv borrow is released before we mutate).
        for _ in 0..64 {
            let Some(msg) = self.rx.as_ref().and_then(|rx| rx.try_recv().ok()) else { break };
            match msg {
                Msg::Opened(boxed) => match *boxed {
                    Ok(base) => {
                        self.before_tex = Some(ctx.load_texture(
                            "before",
                            to_color_image(&base),
                            egui::TextureOptions::LINEAR,
                        ));
                        let (mw, mh) = base.dimensions();
                        self.base_preview = Some(base);
                        // A fresh, fully-transparent paint mask sized to the preview.
                        self.mask_paint = Some(image::RgbaImage::new(mw, mh));
                        self.mask_tex = None;
                        self.mask_dirty = false;
                        self.paint_last = None;
                        self.paint_mode = false;
                        self.recipe = EditRecipe::default();
                        self.reset_history(); // a new photo starts a fresh undo history
                        self.region = None; // and a fresh local-edit selection
                        self.region_drag = None;
                        self.verdict = None;
                        self.rationale.clear();
                        self.dirty = true; // render the (neutral) after
                        self.busy = false;
                        self.status = "ready — adjust sliders or run AI Analyze".into();
                    }
                    Err(e) => {
                        self.busy = false;
                        self.status = format!("could not open: {e}");
                    }
                },
                Msg::Analyzed(boxed) => match *boxed {
                    Ok((recipe, verdict)) => {
                        self.recipe = recipe;
                        self.verdict = Some(format!("{:?} — {}", verdict.decision, verdict.reasons.join("; ")));
                        self.rationale = self.recipe.rationale.clone();
                        self.dirty = true;
                        self.busy = false;
                        self.status = "AI develop applied".into();
                    }
                    Err(e) => {
                        self.busy = false;
                        self.status = format!("analyze failed: {e}");
                    }
                },
                Msg::Exported(Ok(p)) => {
                    self.busy = false;
                    self.status = format!("exported → {p}");
                }
                Msg::Exported(Err(e)) => {
                    self.busy = false;
                    self.status = format!("export failed: {e}");
                }
                Msg::Folder(boxed) => match *boxed {
                    Ok((dir, list)) => {
                        let n = list.len();
                        self.gallery = list;
                        self.gallery_dir = Some(dir);
                        self.gallery_gen += 1; // invalidate any in-flight old thumbs
                        self.thumbs.clear();
                        self.thumb_requested.clear();
                        self.thumb_inflight = 0;
                        self.selected = None;
                        self.busy = false;
                        self.status = format!("{n} photo{} — click a thumbnail to open", if n == 1 { "" } else { "s" });
                    }
                    Err(e) => {
                        self.busy = false;
                        self.status = format!("scan failed: {e}");
                    }
                },
                Msg::Thumb { generation, idx, img } => {
                    // Ignore thumbnails from a previous folder generation (their
                    // inflight count was already discarded when the folder changed).
                    if generation == self.gallery_gen {
                        self.thumb_inflight = self.thumb_inflight.saturating_sub(1);
                        if let Ok(im) = *img {
                            let tex = ctx.load_texture(
                                format!("thumb{idx}"),
                                to_color_image(&im),
                                egui::TextureOptions::LINEAR,
                            );
                            self.thumbs.insert(idx, tex);
                        }
                    }
                }
                Msg::Retouched(boxed) => match *boxed {
                    Ok((img, msg)) => {
                        // Show the saved master in the After pane (it's a separate
                        // ./out artifact, not the develop recipe — a slider edit
                        // re-develops over it, exactly like the web UI).
                        self.after_tex = Some(ctx.load_texture(
                            "after",
                            to_color_image(&img),
                            egui::TextureOptions::LINEAR,
                        ));
                        self.clear_mask();
                        self.busy = false;
                        self.status = msg;
                    }
                    Err(e) => {
                        self.busy = false;
                        self.status = format!("retouch failed: {e}");
                    }
                },
            }
        }
        // Keep the frame loop alive while any worker (analyze/export/thumbs) runs.
        if self.busy || self.thumb_inflight > 0 {
            ctx.request_repaint();
        }
    }

    /// One labelled slider; returns true if the value changed this frame.
    fn slider(ui: &mut egui::Ui, label: &str, value: &mut f32, min: f32, max: f32) -> bool {
        ui.add(egui::Slider::new(value, min..=max).text(label)).changed()
    }

    /// Left-most panel: the working-folder thumbnail gallery. Only visible rows
    /// are laid out (show_rows) and only their thumbnails are queued to decode.
    fn gallery_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Library");
            if ui.button("Open folder…").clicked()
                && let Some(dir) = rfd::FileDialog::new().pick_folder()
            {
                self.open_folder(dir);
            }
        });
        if let Some(d) = &self.gallery_dir {
            ui.label(
                egui::RichText::new(format!("{} · {} photos", d.display(), self.gallery.len()))
                    .weak()
                    .small(),
            );
        }
        ui.separator();
        if self.gallery.is_empty() {
            ui.label(egui::RichText::new("Open a folder to browse your photos here.").weak());
            return;
        }

        let count = self.gallery.len();
        // Borrow only the fields the row closure reads; collect actions to apply
        // after (request_thumb / open_gallery_index both need &mut self).
        let thumbs = &self.thumbs;
        let gallery = &self.gallery;
        let selected = self.selected;
        let mut to_open: Option<usize> = None;
        let mut to_request: Vec<usize> = Vec::new();

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, GALLERY_ROW_H, count, |ui, range| {
                for i in range {
                    let path = &gallery[i];
                    let is_sel = selected == Some(i);
                    let fill = if is_sel { SEL_BG } else { egui::Color32::TRANSPARENT };
                    let resp = egui::Frame::none()
                        .fill(fill)
                        .inner_margin(egui::Margin::same(3.0))
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.horizontal(|ui| {
                                if let Some(t) = thumbs.get(&i) {
                                    ui.add(
                                        egui::Image::new(SizedTexture::new(
                                            t.id(),
                                            egui::vec2(THUMB_W, THUMB_H),
                                        ))
                                        .rounding(3.0),
                                    );
                                } else {
                                    let (rect, _) = ui.allocate_exact_size(
                                        egui::vec2(THUMB_W, THUMB_H),
                                        egui::Sense::hover(),
                                    );
                                    ui.painter().rect_filled(rect, 3.0, egui::Color32::from_gray(24));
                                    to_request.push(i);
                                }
                                ui.vertical(|ui| {
                                    let mut name = egui::RichText::new(autoshop::pipeline::stem(path)).small();
                                    if is_sel {
                                        name = name.strong().color(ACCENT);
                                    }
                                    ui.label(name);
                                    let edited = autoshop::pipeline::default_out(path, "recipe", "json").exists()
                                        || autoshop::pipeline::xmp_target(path).exists();
                                    let baked = !autoshop::decode::is_raw(path);
                                    ui.horizontal(|ui| {
                                        if baked {
                                            ui.label(egui::RichText::new("PNG/TIFF").color(PILL).small());
                                        }
                                        if edited {
                                            ui.label(egui::RichText::new("● edited").color(ACCENT).small());
                                        }
                                    });
                                });
                            });
                        })
                        .response
                        .interact(egui::Sense::click());
                    if resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if resp.clicked() {
                        to_open = Some(i);
                    }
                }
            });

        for i in to_request {
            self.request_thumb(i);
        }
        if let Some(i) = to_open {
            self.open_gallery_index(i);
        }
    }

    fn develop_panel(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        ui.heading("Develop");

        // White balance (temperature is nullable = as-shot).
        let mut custom_wb = self.recipe.temperature_k.is_some();
        if ui.checkbox(&mut custom_wb, "Custom white balance (off = as-shot)").changed() {
            self.recipe.temperature_k = if custom_wb { Some(5500.0) } else { None };
            changed = true;
        }
        if let Some(mut k) = self.recipe.temperature_k
            && Self::slider(ui, "Temp (K)", &mut k, 2000.0, 40000.0)
        {
            self.recipe.temperature_k = Some(k);
            changed = true;
        }

        let r = &mut self.recipe;
        changed |= Self::slider(ui, "Exposure", &mut r.exposure_ev, -5.0, 5.0);
        changed |= Self::slider(ui, "Contrast", &mut r.contrast, -100.0, 100.0);
        changed |= Self::slider(ui, "Highlights", &mut r.highlights, -100.0, 100.0);
        changed |= Self::slider(ui, "Shadows", &mut r.shadows, -100.0, 100.0);
        changed |= Self::slider(ui, "Whites", &mut r.whites, -100.0, 100.0);
        changed |= Self::slider(ui, "Blacks", &mut r.blacks, -100.0, 100.0);
        changed |= Self::slider(ui, "Clarity", &mut r.clarity, -100.0, 100.0);
        changed |= Self::slider(ui, "Dehaze", &mut r.dehaze, -100.0, 100.0);
        changed |= Self::slider(ui, "Vibrance", &mut r.vibrance, -100.0, 100.0);
        changed |= Self::slider(ui, "Saturation", &mut r.saturation, -100.0, 100.0);
        changed |= Self::slider(ui, "Tint", &mut r.tint, -100.0, 100.0);
        changed |= Self::slider(ui, "Sharpening", &mut r.sharpening, 0.0, 150.0);
        changed |= Self::slider(ui, "Noise Reduction", &mut r.noise_reduction, 0.0, 100.0);

        ui.separator();
        ui.horizontal(|ui| {
            ui.heading("Color Mixer");
            ui.label(egui::RichText::new("· HSL").weak());
            if ui.small_button("reset").clicked() {
                self.recipe.hsl = Hsl::default();
                changed = true;
            }
        });
        egui::ComboBox::from_id_salt("hsl_band")
            .selected_text(HSL_BANDS[self.hsl_band])
            .show_ui(ui, |ui| {
                for (i, name) in HSL_BANDS.iter().enumerate() {
                    ui.selectable_value(&mut self.hsl_band, i, *name);
                }
            });
        let b = self.hsl_band;
        changed |= Self::slider(ui, "Hue", &mut self.recipe.hsl.hue[b], -100.0, 100.0);
        changed |= Self::slider(ui, "Saturation", &mut self.recipe.hsl.saturation[b], -100.0, 100.0);
        changed |= Self::slider(ui, "Luminance", &mut self.recipe.hsl.luminance[b], -100.0, 100.0);

        ui.separator();
        ui.horizontal(|ui| {
            ui.heading("Color Grading");
            if ui.small_button("reset").clicked() {
                self.recipe.color_grade = ColorGrade::default();
                changed = true;
            }
        });
        egui::ComboBox::from_id_salt("grade_region")
            .selected_text(GRADE_REGIONS[self.grade_region])
            .show_ui(ui, |ui| {
                for (i, name) in GRADE_REGIONS.iter().enumerate() {
                    ui.selectable_value(&mut self.grade_region, i, *name);
                }
            });
        let cg = &mut self.recipe.color_grade;
        let (mut hue, mut sat, mut lum) = match self.grade_region {
            0 => (cg.shadow_hue, cg.shadow_sat, cg.shadow_lum),
            1 => (cg.midtone_hue, cg.midtone_sat, cg.midtone_lum),
            2 => (cg.highlight_hue, cg.highlight_sat, cg.highlight_lum),
            _ => (cg.global_hue, cg.global_sat, cg.global_lum),
        };
        let mut wheel_changed = false;
        wheel_changed |= Self::slider(ui, "Hue", &mut hue, 0.0, 360.0);
        wheel_changed |= Self::slider(ui, "Saturation", &mut sat, 0.0, 100.0);
        wheel_changed |= Self::slider(ui, "Luminance", &mut lum, -100.0, 100.0);
        if wheel_changed {
            match self.grade_region {
                0 => { cg.shadow_hue = hue; cg.shadow_sat = sat; cg.shadow_lum = lum; }
                1 => { cg.midtone_hue = hue; cg.midtone_sat = sat; cg.midtone_lum = lum; }
                2 => { cg.highlight_hue = hue; cg.highlight_sat = sat; cg.highlight_lum = lum; }
                _ => { cg.global_hue = hue; cg.global_sat = sat; cg.global_lum = lum; }
            }
            changed = true;
        }
        changed |= Self::slider(ui, "Blending", &mut cg.blending, 0.0, 100.0);
        changed |= Self::slider(ui, "Balance", &mut cg.balance, -100.0, 100.0);

        if changed {
            self.recipe.clamp();
            self.dirty = true;
        }
    }

    /// Box-select on the After image: drag a rectangle to target a local edit;
    /// the normalized box is folded into the AI direction so it masks exactly
    /// there (mirrors the web region→mask prompt). A plain click — or a tiny
    /// drag — clears the selection.
    fn handle_region_select(&mut self, ui: &egui::Ui, resp: &egui::Response, rect: egui::Rect) {
        if resp.drag_started() {
            if let Some(p) = resp.interact_pointer_pos() {
                self.region_drag = Some((p, p));
            }
        } else if resp.dragged() {
            if let (Some(p), Some((s, _))) = (resp.interact_pointer_pos(), self.region_drag) {
                self.region_drag = Some((s, p));
            }
        } else if resp.drag_stopped() {
            if let Some((s, e)) = self.region_drag.take() {
                let nx = |x: f32| ((x - rect.min.x) / rect.width().max(1.0)).clamp(0.0, 1.0);
                let ny = |y: f32| ((y - rect.min.y) / rect.height().max(1.0)).clamp(0.0, 1.0);
                let (l, r) = (nx(s.x).min(nx(e.x)), nx(s.x).max(nx(e.x)));
                let (t, b) = (ny(s.y).min(ny(e.y)), ny(s.y).max(ny(e.y)));
                if r - l > 0.02 && b - t > 0.02 {
                    self.region = Some([l, t, r, b]);
                    self.status = format!(
                        "region {}×{}% — type a direction, then AI Analyze (click to clear)",
                        ((r - l) * 100.0).round() as i32,
                        ((b - t) * 100.0).round() as i32
                    );
                } else {
                    self.region = None; // a tiny drag clears the selection
                }
            }
        } else if resp.clicked() {
            self.region = None; // a plain click clears the region
        }

        // Draw the live drag box, else the committed region outline.
        let stroke = egui::Stroke::new(2.0, ACCENT);
        let fill = egui::Color32::from_rgba_unmultiplied(0x4c, 0x8b, 0xf5, 40);
        let draw = |r: egui::Rect| {
            ui.painter().rect_filled(r, 0.0, fill);
            ui.painter().rect_stroke(r, 0.0, stroke);
        };
        if let Some((s, e)) = self.region_drag {
            draw(egui::Rect::from_two_pos(s, e).intersect(rect));
        } else if let Some([l, t, rr, bb]) = self.region {
            draw(egui::Rect::from_min_max(
                egui::pos2(rect.min.x + l * rect.width(), rect.min.y + t * rect.height()),
                egui::pos2(rect.min.x + rr * rect.width(), rect.min.y + bb * rect.height()),
            ));
        }
    }

    /// PNG bytes of the EXPORT mask: painted → transparent (regenerate / heal
    /// here), unpainted → opaque. None if nothing is painted — mirrors the web.
    fn export_mask_png(&self) -> Option<Vec<u8>> {
        let m = self.mask_paint.as_ref()?;
        let (w, h) = (m.width(), m.height());
        let mut out = image::RgbaImage::new(w, h);
        let mut any = false;
        for (x, y, p) in m.enumerate_pixels() {
            let painted = p.0[3] > 10;
            any |= painted;
            out.put_pixel(x, y, image::Rgba([0, 0, 0, if painted { 0 } else { 255 }]));
        }
        if !any {
            return None;
        }
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgba8(out)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .ok()?;
        Some(buf)
    }

    fn clear_mask(&mut self) {
        if let Some(m) = &mut self.mask_paint {
            for p in m.pixels_mut() {
                *p = image::Rgba([0, 0, 0, 0]);
            }
            self.mask_dirty = true;
        }
        self.paint_last = None;
    }

    fn ensure_mask_tex(&mut self, ctx: &egui::Context) {
        if self.mask_dirty {
            if let Some(m) = &self.mask_paint {
                let ci = egui::ColorImage::from_rgba_unmultiplied(
                    [m.width() as usize, m.height() as usize],
                    m.as_raw(),
                );
                self.mask_tex = Some(ctx.load_texture("paintmask", ci, egui::TextureOptions::LINEAR));
            }
            self.mask_dirty = false;
        }
    }

    /// Brush-paint into the mask while dragging on the After image.
    fn handle_paint(&mut self, resp: &egui::Response, rect: egui::Rect) {
        let brush = self.brush;
        let Some(m) = self.mask_paint.as_mut() else { return };
        let (mw, mh) = (m.width() as f32, m.height() as f32);
        let to_mask = |p: egui::Pos2| {
            (
                (p.x - rect.min.x) / rect.width().max(1.0) * mw,
                (p.y - rect.min.y) / rect.height().max(1.0) * mh,
            )
        };
        let brush_mask = (brush * mw / rect.width().max(1.0)).max(1.0);
        if resp.dragged() || resp.drag_started() {
            if let Some(p) = resp.interact_pointer_pos() {
                let cur = to_mask(p);
                match self.paint_last {
                    Some(prev) => stamp_line(m, prev, cur, brush_mask),
                    None => stamp_dot(m, cur, brush_mask),
                }
                self.paint_last = Some(cur);
                self.mask_dirty = true;
            }
        } else if resp.drag_stopped() {
            self.paint_last = None;
        }
    }

    /// Generative fill: regenerate the painted area (gpt-image), composite onto
    /// the source, save to ./out. Runs on a worker thread.
    fn start_fill(&mut self) {
        let Some(path) = self.src_path.clone() else { return };
        if self.busy {
            return;
        }
        let prompt = self.fill_prompt.trim().to_string();
        if prompt.is_empty() {
            self.status = "write what should fill the painted area".into();
            return;
        }
        let Some(mask_png) = self.export_mask_png() else {
            self.status = "paint the area to remove/fill first (tick Paint mask)".into();
            return;
        };
        self.busy = true;
        self.status = if self.fill_fullres {
            "generative fill (full-res render)… (slow, minutes)".into()
        } else {
            "generative fill via gpt-image… (~15-40s)".into()
        };
        let tx = self.tx.clone();
        let quality = ["high", "medium", "low"][self.fill_quality.min(2)].to_string();
        let full_res = self.fill_fullres;
        std::thread::spawn(move || {
            let res = (|| -> anyhow::Result<(image::DynamicImage, String)> {
                let cfg = autoshop::config::Config::load();
                let out = autoshop::pipeline::default_out(&path, "retouch", "png");
                let mask_tmp =
                    std::env::temp_dir().join(format!("autoshop_gui_fill_{}.png", std::process::id()));
                std::fs::write(&mask_tmp, &mask_png)?;
                let r = autoshop::generative::retouch(&cfg, &path, &mask_tmp, &prompt, &quality, full_res, &out);
                let _ = std::fs::remove_file(&mask_tmp);
                r?;
                let img = autoshop::decode::load_image(&out)?.thumbnail(PREVIEW_EDGE, PREVIEW_EDGE);
                Ok((img, format!("filled → {} (saved to ./out)", out.display())))
            })();
            let _ = tx.send(Msg::Retouched(Box::new(res)));
        });
    }

    /// Heal: AI auto-detect (use_mask=false) or the painted mask (use_mask=true).
    /// Pixel retouch from surrounding real pixels; saves to ./out.
    fn start_heal(&mut self, use_mask: bool) {
        let Some(path) = self.src_path.clone() else { return };
        if self.busy {
            return;
        }
        let mask_png = if use_mask {
            match self.export_mask_png() {
                Some(b) => Some(b),
                None => {
                    self.status = "tick Paint mask and paint the spots, then Heal painted area".into();
                    return;
                }
            }
        } else {
            None
        };
        self.busy = true;
        self.status = if use_mask {
            "healing painted area…".into()
        } else {
            "AI 去瑕疵中… (~10-30s)".into()
        };
        let tx = self.tx.clone();
        let full_res = self.heal_fullres;
        std::thread::spawn(move || {
            let res = (|| -> anyhow::Result<(image::DynamicImage, String)> {
                let cfg = autoshop::config::Config::load();
                let out = autoshop::pipeline::default_out(&path, "heal", "png");
                let mask_tmp = match mask_png {
                    Some(bytes) => {
                        let t = std::env::temp_dir()
                            .join(format!("autoshop_gui_heal_{}.png", std::process::id()));
                        std::fs::write(&t, &bytes)?;
                        Some(t)
                    }
                    None => None,
                };
                let rep = autoshop::retouch::heal(&cfg, &path, mask_tmp.as_deref(), !use_mask, full_res, &out);
                if let Some(t) = &mask_tmp {
                    let _ = std::fs::remove_file(t);
                }
                let rep = rep?;
                let img = autoshop::decode::load_image(&out)?.thumbnail(PREVIEW_EDGE, PREVIEW_EDGE);
                Ok((img, format!("healed {} spot(s) → {}", rep.spots, out.display())))
            })();
            let _ = tx.send(Msg::Retouched(Box::new(res)));
        });
    }

    /// Full-frame generative re-render via gpt-image — the OPTIONAL "let GPT
    /// directly make the picture" path. Uses the Direction text as the look
    /// prompt. Unlike Analyze (a faithful parametric recipe), this REGENERATES
    /// pixels: non-faithful, ~1.5K px, no XMP — a creative restyle, not a master.
    /// Saved to ./out, shown in the After pane. Hits the gpt-image (image) endpoint.
    fn start_reimagine(&mut self) {
        let Some(path) = self.src_path.clone() else { return };
        if self.busy {
            return;
        }
        let prompt = {
            let g = self.guidance.trim();
            if g.is_empty() {
                "Develop this photo into a finished, natural-looking edit: balanced exposure and \
                 contrast, pleasing realistic colour; keep the scene true to the original."
                    .to_string()
            } else {
                g.to_string()
            }
        };
        self.busy = true;
        self.status = "AI 生成出片中… (gpt-image, ~15–40s)".into();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let res = (|| -> anyhow::Result<(image::DynamicImage, String)> {
                let cfg = autoshop::config::Config::load();
                let out = autoshop::pipeline::default_out(&path, "reimagine", "png");
                // fidelity "high" keeps it recognisably the same photo.
                autoshop::generative::reimagine(&cfg, &path, &prompt, "high", &cfg.openai_image_quality, &out)?;
                let img = autoshop::decode::load_image(&out)?.thumbnail(PREVIEW_EDGE, PREVIEW_EDGE);
                Ok((img, format!("generated → {} (gpt-image; saved to ./out)", out.display())))
            })();
            let _ = tx.send(Msg::Retouched(Box::new(res)));
        });
    }

    fn retouch_panel(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.heading("Retouch");

        // Whole-image generative re-render: let gpt-image DIRECTLY produce the
        // picture (the optional "GPT makes the image" path). Distinct from
        // AI Analyze, which emits a faithful parametric recipe.
        ui.label(egui::RichText::new("整图 AI 生成 · Reimagine (gpt-image 直接出图)").strong());
        ui.add_enabled_ui(!self.busy, |ui| {
            if ui
                .button("✨ AI 生成出片")
                .on_hover_text(
                    "用 gpt-image 直接重绘整张图（拿上方 Direction 文本当风格描述）。实验：重绘像素=非保真、约1.5K、无 XMP——创意改图，非精修。需 OPENAI_API_KEY",
                )
                .clicked()
            {
                self.start_reimagine();
            }
        });
        ui.label(
            egui::RichText::new("拿上方 Direction 当风格描述；重绘像素=非保真、低分辨率、无 XMP。需 OPENAI_API_KEY (gpt-image)。")
                .weak()
                .small(),
        );
        ui.separator();

        ui.horizontal(|ui| {
            ui.checkbox(&mut self.paint_mode, "Paint mask")
                .on_hover_text("Brush over the area; box-select is paused while on");
            if ui.button("Clear").clicked() {
                self.clear_mask();
            }
        });
        ui.add(egui::Slider::new(&mut self.brush, 4.0..=80.0).text("brush"));

        ui.label(egui::RichText::new("Generative Fill · 实验").strong());
        ui.add(
            egui::TextEdit::singleline(&mut self.fill_prompt)
                .desired_width(f32::INFINITY)
                .hint_text("what belongs there, e.g. remove the trash can, extend the sky"),
        );
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("fill_quality")
                .selected_text(["high", "medium", "low"][self.fill_quality.min(2)])
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.fill_quality, 0, "high");
                    ui.selectable_value(&mut self.fill_quality, 1, "medium");
                    ui.selectable_value(&mut self.fill_quality, 2, "low");
                });
            ui.checkbox(&mut self.fill_fullres, "Full-res")
                .on_hover_text("Composite onto the full-sensor develop (slow, RAW only)");
        });
        ui.add_enabled_ui(!self.busy, |ui| {
            if ui.button("Remove / Fill").clicked() {
                self.start_fill();
            }
        });
        ui.label(
            egui::RichText::new("Paint the area, write what belongs there, then Remove/Fill. Needs OPENAI_API_KEY.")
                .weak()
                .small(),
        );

        ui.label(egui::RichText::new("修图 · 去瑕疵 Heal（像素）").strong());
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!self.busy, |ui| {
                if ui.button("✦ AI 去瑕疵 (auto)").clicked() {
                    self.start_heal(false);
                }
                if ui.button("Heal painted area").clicked() {
                    self.start_heal(true);
                }
            });
            ui.checkbox(&mut self.heal_fullres, "Full-res");
        });
        ui.label(
            egui::RichText::new(
                "AI auto-detects dust / blemishes, or paint a mask and Heal it. Pixel retouch from surrounding pixels; saved to ./out.",
            )
            .weak()
            .small(),
        );
    }
}

impl eframe::App for AutoshopApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_workers(ctx);

        // Global undo/redo keys. Skip while a widget is focused so the Direction
        // text field keeps its own text undo. Ctrl+Z = undo; Ctrl+Y / Ctrl+Shift+Z = redo.
        if ctx.memory(|m| m.focused()).is_none() {
            let (mut do_undo, mut do_redo) = (false, false);
            ctx.input_mut(|i| {
                if i.consume_key(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::Z) { do_redo = true; }
                if i.consume_key(egui::Modifiers::COMMAND, egui::Key::Y) { do_redo = true; }
                if i.consume_key(egui::Modifiers::COMMAND, egui::Key::Z) { do_undo = true; }
            });
            if do_undo { self.undo(); }
            if do_redo { self.redo(); }
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Autoshop");
                ui.separator();
                if ui.button("Open photo…").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("Photos", &["arw", "dng", "raf", "nef", "cr2", "cr3", "png", "tif", "tiff", "jpg", "jpeg"])
                        .pick_file()
                {
                    self.selected = None; // a one-off file isn't a gallery selection
                    self.open_path(path);
                }
                let ready = self.src_path.is_some() && !self.busy;
                ui.add_enabled_ui(ready, |ui| {
                    if ui.button("✨ AI Analyze").clicked() {
                        self.start_analyze();
                    }
                    ui.checkbox(&mut self.refine, "Refine")
                        .on_hover_text("Adjust the CURRENT edit instead of proposing from scratch");
                    if ui.button("Reset").clicked() {
                        self.recipe = EditRecipe::default();
                        self.region = None;
                        self.dirty = true;
                    }
                    ui.separator();
                    ui.add_enabled_ui(!self.undo_stack.is_empty(), |ui| {
                        if ui.button("↶ Undo").on_hover_text("Ctrl+Z").clicked() {
                            self.undo();
                        }
                    });
                    ui.add_enabled_ui(!self.redo_stack.is_empty(), |ui| {
                        if ui.button("↷ Redo").on_hover_text("Ctrl+Y").clicked() {
                            self.redo();
                        }
                    });
                });
                ui.separator();
                ui.label("Style");
                ui.add(egui::Slider::new(&mut self.style_strength, 0.0..=1.0).show_value(false));
                ui.label(format!("{:.0}%", self.style_strength * 100.0));
                ui.separator();
                if ui.button("⚙ Settings").on_hover_text("AI provider / model / API key").clicked() {
                    self.show_settings = true;
                    self.load_settings_form();
                }
            });
            // AI direction (free text) + save options.
            ui.horizontal(|ui| {
                ui.label("Direction:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.guidance)
                        .desired_width(340.0)
                        .hint_text("e.g. warmer and moodier, lift the shadows"),
                );
                let ready = self.src_path.is_some() && !self.busy;
                ui.add_enabled_ui(ready, |ui| {
                    ui.separator();
                    egui::ComboBox::from_id_salt("save_fmt")
                        .selected_text(if self.save_jpeg { "JPEG" } else { "16-bit TIFF" })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.save_jpeg, false, "16-bit TIFF");
                            ui.selectable_value(&mut self.save_jpeg, true, "JPEG");
                        });
                    ui.checkbox(&mut self.save_denoise, "AI Denoise").on_hover_text(
                        "SCUNet AI denoise before developing — high-ISO / astro (slow, GPU; needs the python sidecar)",
                    );
                    if ui.button("Export → ./out").clicked() {
                        self.start_export();
                    }
                    if ui.button("Download…").clicked() {
                        let ext = if self.save_jpeg { "jpg" } else { "tif" };
                        let stem = self
                            .src_path
                            .as_deref()
                            .and_then(|p| p.file_stem())
                            .and_then(|s| s.to_str())
                            .unwrap_or("photo")
                            .to_string();
                        if let Some(p) = rfd::FileDialog::new()
                            .add_filter(ext, &[ext])
                            .set_file_name(format!("{stem}.developed.{ext}"))
                            .save_file()
                        {
                            self.start_render_to(p);
                        }
                    }
                    if ui.button("Save XMP").clicked() {
                        self.save_xmp();
                    }
                });
            });
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if self.busy {
                    ui.spinner();
                }
                ui.label(&self.status);
            });
        });

        // Left-most: the library gallery (folder browse + thumbnails).
        egui::SidePanel::left("gallery").default_width(240.0).show(ctx, |ui| {
            self.gallery_panel(ui);
        });

        egui::SidePanel::left("controls").default_width(320.0).show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                if self.src_path.is_some() {
                    self.develop_panel(ui);
                    self.retouch_panel(ui);
                    if let Some(v) = &self.verdict {
                        ui.separator();
                        ui.label(egui::RichText::new("Verdict").strong());
                        ui.label(v);
                    }
                    if !self.rationale.is_empty() {
                        ui.label(egui::RichText::new(format!("“{}”", self.rationale)).italics().weak());
                    }
                } else {
                    ui.label("No photo open.");
                }
            });
        });

        // Re-develop AFTER the controls are read (so this frame reflects edits).
        if self.dirty {
            self.redevelop(ctx);
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            let avail = ui.available_size();
            let half = (avail.x - 16.0) * 0.5;
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(egui::RichText::new("Before (source)").weak());
                    if let Some(t) = &self.before_tex {
                        ui.add(egui::Image::new(SizedTexture::new(t.id(), t.size_vec2())).max_width(half));
                    }
                });
                ui.separator();
                ui.vertical(|ui| {
                    let hint = if self.paint_mode {
                        "After (edit) — paint over the area to fill / heal"
                    } else {
                        "After (edit) — drag a box to target a local edit"
                    };
                    ui.label(egui::RichText::new(hint).weak());
                    // Copy id/size out so we don't hold a borrow of self.after_tex
                    // while the handlers mutate self.region / self.mask_paint.
                    if let Some((id, size)) = self.after_tex.as_ref().map(|t| (t.id(), t.size_vec2())) {
                        let resp = ui.add(
                            egui::Image::new(SizedTexture::new(id, size))
                                .max_width(half)
                                .sense(egui::Sense::click_and_drag()),
                        );
                        let rect = resp.rect;
                        if self.paint_mode {
                            self.handle_paint(&resp, rect);
                            self.ensure_mask_tex(ui.ctx());
                            if let Some(t) = &self.mask_tex {
                                let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                                ui.painter().image(t.id(), rect, uv, egui::Color32::WHITE);
                            }
                        } else {
                            self.handle_region_select(ui, &resp, rect);
                        }
                    }
                });
            });
        });

        // Settings window (provider / model / API keys). A local `open` avoids a
        // double &mut self borrow (Window::open vs the closure that reads self).
        if self.show_settings {
            let mut open = true;
            egui::Window::new("⚙ Settings")
                .collapsible(false)
                .resizable(false)
                .default_width(480.0)
                .open(&mut open)
                .show(ctx, |ui| self.settings_ui(ui));
            if !open {
                self.show_settings = false;
            }
        }

        // Land a finished edit gesture (slider release, AI Analyze, Reset) into
        // the undo history — once per gesture, after all controls are read.
        self.commit_if_settled(ctx);
    }
}

/// Register a system CJK font so Chinese / Japanese UI text renders — egui ships
/// no CJK glyphs, so without this every non-Latin character is a tofu box (□).
///
/// Reads the user's installed font at runtime (no ~16 MB binary bloat) and
/// pre-validates it with `ab_glyph` (egui's own backend) before handing it over —
/// egui PANICS on a font it can't parse, so a missing/odd font must be skipped,
/// not registered. Appended as a FALLBACK so Latin keeps egui's default look.
fn install_cjk_font(ctx: &egui::Context) {
    // Single-face TTFs first (always parse); TTC collections (face 0) last.
    const CANDIDATES: &[&str] = &[
        r"C:\Windows\Fonts\Deng.ttf",   // DengXian — clean modern UI face
        r"C:\Windows\Fonts\simhei.ttf", // SimHei
        r"C:\Windows\Fonts\msyh.ttc",   // Microsoft YaHei (collection, face 0)
        r"C:\Windows\Fonts\simsun.ttc", // SimSun (collection, face 0)
    ];
    let Some(bytes) = CANDIDATES.iter().find_map(|p| {
        let b = std::fs::read(p).ok()?;
        // Only accept it if egui's backend can actually parse face 0 (no panic).
        ab_glyph::FontVec::try_from_vec_and_index(b.clone(), 0).ok()?;
        Some(b)
    }) else {
        return; // no usable CJK font found — Latin text still renders fine
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("cjk".to_owned(), egui::FontData::from_owned(bytes));
    for fam in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(fam).or_default().push("cjk".to_owned());
    }
    ctx.set_fonts(fonts);
}

/// Decode the embedded Autoshop icon for the window title bar / taskbar.
fn app_icon() -> egui::IconData {
    let img = image::load_from_memory(include_bytes!("../../assets/icon_256.png"))
        .expect("embedded icon decodes")
        .to_rgba8();
    let (width, height) = img.dimensions();
    egui::IconData { rgba: img.into_raw(), width, height }
}

fn main() -> eframe::Result<()> {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 880.0])
            .with_title("Autoshop")
            .with_icon(std::sync::Arc::new(app_icon())),
        ..Default::default()
    };
    eframe::run_native(
        "Autoshop",
        opts,
        Box::new(|cc| {
            install_cjk_font(&cc.egui_ctx); // CJK glyphs so Chinese labels aren't tofu
            Ok(Box::new(AutoshopApp::default()))
        }),
    )
}
