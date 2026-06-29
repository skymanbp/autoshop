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

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

use eframe::egui;
use egui::load::SizedTexture;

use autoshop::recipe::{EditRecipe, Hsl, ColorGrade};

const PREVIEW_EDGE: u32 = 1280; // working preview size for fast live develop
const HSL_BANDS: [&str; 8] = ["Red", "Orange", "Yellow", "Green", "Aqua", "Blue", "Purple", "Magenta"];
const GRADE_REGIONS: [&str; 4] = ["shadow", "midtone", "highlight", "global"];

/// Messages from worker threads back to the UI. The `Analyzed` payload is boxed
/// because `(EditRecipe, Verdict)` is large; boxing keeps the channel message
/// small (clippy::large_enum_variant).
enum Msg {
    Opened(Box<anyhow::Result<image::DynamicImage>>),
    Analyzed(Box<anyhow::Result<(EditRecipe, autoshop::advisor::Verdict)>>),
    Exported(anyhow::Result<String>),
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
            status: "Open a RAW or image to begin.".into(),
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
        let mut got = None;
        if let Some(rx) = &self.rx {
            if let Ok(msg) = rx.try_recv() {
                got = Some(msg);
            } else if self.busy {
                ctx.request_repaint(); // keep polling while a thread runs
            }
        }
        match got {
            Some(Msg::Opened(boxed)) => match *boxed {
                Ok(base) => {
                    self.before_tex = Some(ctx.load_texture(
                        "before",
                        to_color_image(&base),
                        egui::TextureOptions::LINEAR,
                    ));
                    self.base_preview = Some(base);
                    self.recipe = EditRecipe::default();
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
            Some(Msg::Analyzed(boxed)) => match *boxed {
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
            Some(Msg::Exported(Ok(p))) => {
                self.busy = false;
                self.status = format!("exported → {p}");
            }
            Some(Msg::Exported(Err(e))) => {
                self.busy = false;
                self.status = format!("export failed: {e}");
            }
            None => {}
        }
    }

    /// One labelled slider; returns true if the value changed this frame.
    fn slider(ui: &mut egui::Ui, label: &str, value: &mut f32, min: f32, max: f32) -> bool {
        ui.add(egui::Slider::new(value, min..=max).text(label)).changed()
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

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Autoshop");
                ui.separator();
                if ui.button("Open photo…").clicked()
                    && let Some(path) = rfd::FileDialog::new()
                        .add_filter("Photos", &["arw", "dng", "raf", "nef", "cr2", "cr3", "png", "tif", "tiff", "jpg", "jpeg"])
                        .pick_file()
                {
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
            .with_inner_size([1280.0, 860.0])
            .with_title("Autoshop")
            .with_icon(std::sync::Arc::new(app_icon())),
        ..Default::default()
    };
    eframe::run_native("Autoshop", opts, Box::new(|_cc| Ok(Box::new(AutoshopApp::default()))))
}
