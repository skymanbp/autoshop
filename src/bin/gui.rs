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
    // --- library / gallery ---
    gallery: Vec<PathBuf>,          // sources in the working folder (sorted)
    gallery_dir: Option<PathBuf>,   // the working folder
    gallery_gen: u64,               // bumped on every folder load (thumb invalidation)
    selected: Option<usize>,        // index of the open gallery photo (for highlight)
    thumbs: HashMap<usize, egui::TextureHandle>, // decoded thumbnails by index
    thumb_requested: HashSet<usize>,             // indices already queued/decoded
    thumb_inflight: usize,                       // live thumbnail-decode threads
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
            gallery: Vec::new(),
            gallery_dir: None,
            gallery_gen: 0,
            selected: None,
            thumbs: HashMap::new(),
            thumb_requested: HashSet::new(),
            thumb_inflight: 0,
        }
    }
}

/// `image::DynamicImage` → egui texture-ready colour image.
fn to_color_image(img: &image::DynamicImage) -> egui::ColorImage {
    use image::GenericImageView;
    let rgba = img.to_rgba8();
    let (w, h) = img.dimensions();
    egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw())
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
        // from scratch.
        let guidance = {
            let g = self.guidance.trim();
            (!g.is_empty()).then(|| g.to_string())
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
        self.status = format!("rendering full-resolution → {} …", out.display());
        let tx = self.tx.clone();
        let recipe = self.recipe.clone();
        std::thread::spawn(move || {
            let res = (|| {
                if let Some(p) = out.parent() {
                    std::fs::create_dir_all(p)?;
                }
                autoshop::render::render_to_file(&path, &recipe, &out, None)?;
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
                        self.base_preview = Some(base);
                        self.recipe = EditRecipe::default();
                        self.reset_history(); // a new photo starts a fresh undo history
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
                    ui.label(egui::RichText::new("After (edit)").weak());
                    if let Some(t) = &self.after_tex {
                        ui.add(egui::Image::new(SizedTexture::new(t.id(), t.size_vec2())).max_width(half));
                    }
                });
            });
        });

        // Land a finished edit gesture (slider release, AI Analyze, Reset) into
        // the undo history — once per gesture, after all controls are read.
        self.commit_if_settled(ctx);
    }
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
    eframe::run_native("Autoshop", opts, Box::new(|_cc| Ok(Box::new(AutoshopApp::default()))))
}
