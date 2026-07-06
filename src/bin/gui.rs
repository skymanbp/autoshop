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
use std::time::{Duration, Instant};

use eframe::egui;
use egui::load::SizedTexture;

use autoshop::recipe::{ColorGrade, CurvePoint, EditRecipe, Hsl, MaskGeometry};
use image::GenericImageView;

/// How the preview area is laid out. `AfterOnly` gives the edit the whole
/// canvas (hold **B** to flash the source in place — the Lightroom gesture);
/// `SideBySide` keeps the permanent comparison.
#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
enum ViewMode {
    SideBySide,
    AfterOnly,
}

/// A transient corner notification. Errors linger twice as long as successes —
/// a one-line status bar alone is too easy to miss for a failed export.
struct Toast {
    text: String,
    kind: ToastKind,
    born: Instant,
}

#[derive(Clone, Copy, PartialEq)]
enum ToastKind {
    Success,
    Error,
}

impl Toast {
    fn ttl(&self) -> Duration {
        match self.kind {
            ToastKind::Success => Duration::from_secs(4),
            ToastKind::Error => Duration::from_secs(8),
        }
    }
}

/// The prefs worth remembering across launches (stored via eframe persistence
/// next to the window geometry). Everything here must stay cheap to re-apply.
#[derive(serde::Serialize, serde::Deserialize)]
struct Prefs {
    gallery_dir: Option<PathBuf>,
    style_strength: f32,
    save_jpeg: bool,
    save_denoise: bool,
    view_mode: ViewMode,
}

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
    /// A generative-fill / heal / reimagine result: (preview of the saved ./out
    /// master, status, the saved path when it is a REIMAGINE output — i.e. a
    /// whole-frame rendition that can be reverse-fitted into a recipe).
    Retouched(Box<anyhow::Result<(image::DynamicImage, String, Option<PathBuf>)>>),
    /// Reverse-fit finished: the fitted recipe + a status note (fit.rs).
    Fitted(Box<anyhow::Result<(EditRecipe, String)>>),
    /// Style-prompt extraction finished: the reusable prompt text.
    Styled(Box<anyhow::Result<String>>),
    /// A `GET /models` fetch finished: the account's model ids (Settings pick-list).
    Models(anyhow::Result<Vec<String>>),
    /// A batch recipe paste finished: the human summary (counts; Err on any failure).
    Pasted(anyhow::Result<String>),
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
    // --- live model pick-lists (populated by the "Fetch models" button) ---
    chat_choices: Vec<String>,      // text/vision chat ids (proposer + api verifier)
    image_gen_choices: Vec<String>, // gpt-image-* ids (generative edits)
    fetching_models: bool,          // a GET /models worker is in flight
}

/// Build the option list for a model ComboBox: the live-fetched ids if we have
/// them, else a grounded fallback; the current value is always included so a
/// custom/manual id is never dropped from the menu.
fn model_opts(fetched: &[String], fallback: &[&str], current: &str) -> Vec<String> {
    let mut v: Vec<String> = if fetched.is_empty() {
        fallback.iter().map(|s| s.to_string()).collect()
    } else {
        fetched.to_vec()
    };
    if !current.trim().is_empty() && !v.iter().any(|x| x == current) {
        v.insert(0, current.to_string());
    }
    v
}

/// Every source type the app opens — ONE list shared by the file dialog, drag &
/// drop, and any future association, so they can't drift apart.
const PHOTO_EXTS: [&str; 11] =
    ["arw", "dng", "raf", "nef", "cr2", "cr3", "png", "tif", "tiff", "jpg", "jpeg"];

fn is_photo_path(p: &std::path::Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| PHOTO_EXTS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

fn photo_file_dialog() -> Option<PathBuf> {
    rfd::FileDialog::new().add_filter("Photos", &PHOTO_EXTS).pick_file()
}

/// Visualise a mask geometry on the image: linear = the zero→full vector with
/// end bars (solid = full-effect side); radial = the ellipse outline. Clipped
/// to the image rect by the painter.
fn draw_mask_overlay(ui: &egui::Ui, xf: ViewXform, geom: &MaskGeometry) {
    let p = ui.painter_at(xf.rect);
    let stroke = egui::Stroke::new(2.0, ACCENT);
    match geom {
        MaskGeometry::Linear { zero_x, zero_y, full_x, full_y } => {
            let a = xf.to_screen(*zero_x, *zero_y);
            let b = xf.to_screen(*full_x, *full_y);
            p.line_segment([a, b], stroke);
            let v = b - a;
            let len = v.length().max(1.0);
            let n = egui::vec2(-v.y / len, v.x / len) * 28.0;
            p.line_segment([a - n, a + n], egui::Stroke::new(1.0, ACCENT));
            p.line_segment([b - n, b + n], stroke);
            p.circle_filled(b, 4.0, ACCENT); // full-effect end
            p.circle_stroke(a, 4.0, stroke); // untouched end
        }
        MaskGeometry::Radial { top, left, bottom, right, .. } => {
            let c = xf.to_screen((left + right) / 2.0, (top + bottom) / 2.0);
            let rx = (xf.to_screen(*right, 0.0).x - xf.to_screen(*left, 0.0).x).abs() / 2.0;
            let ry = (xf.to_screen(0.0, *bottom).y - xf.to_screen(0.0, *top).y).abs() / 2.0;
            p.add(egui::Shape::ellipse_stroke(c, egui::vec2(rx, ry), stroke));
            p.circle_filled(c, 3.0, ACCENT);
        }
    }
}

/// Scale `tex_size` to fit a `max_w` × `avail_y` box (both dimensions — width
/// alone lets a portrait overflow the panel), never upscaling past 4×.
fn fit_in(tex_size: egui::Vec2, max_w: f32, avail_y: f32) -> egui::Vec2 {
    let s = (max_w / tex_size.x.max(1.0))
        .min(avail_y.max(1.0) / tex_size.y.max(1.0))
        .clamp(0.01, 4.0);
    tex_size * s
}

/// Section header text with an activity dot when the section holds non-neutral
/// values — a collapsed active adjustment must never be invisible.
fn section_title(base: &str, active: bool) -> String {
    if active {
        format!("{base}  ●")
    } else {
        base.to_string()
    }
}

/// Curve-editor channels: label + draw colour, indexed by `curve_channel`
/// (0 = master, then R/G/B — the recipe's tone/red/green/blue_curve fields).
const CURVE_CHANNELS: [(&str, egui::Color32); 4] = [
    ("主", egui::Color32::from_gray(225)),
    ("红", egui::Color32::from_rgb(235, 90, 90)),
    ("绿", egui::Color32::from_rgb(90, 205, 90)),
    ("蓝", egui::Color32::from_rgb(90, 130, 240)),
];

/// The recipe field behind a curve-editor channel index.
fn curve_points(recipe: &EditRecipe, ch: usize) -> &Vec<CurvePoint> {
    match ch {
        0 => &recipe.tone_curve,
        1 => &recipe.red_curve,
        2 => &recipe.green_curve,
        _ => &recipe.blue_curve,
    }
}

fn curve_points_mut(recipe: &mut EditRecipe, ch: usize) -> &mut Vec<CurvePoint> {
    match ch {
        0 => &mut recipe.tone_curve,
        1 => &mut recipe.red_curve,
        2 => &mut recipe.green_curve,
        _ => &mut recipe.blue_curve,
    }
}

/// Insert a curve control point keeping inputs sorted and UNIQUE — a second
/// point at the same input overwrites instead of duplicating (the engine's
/// piecewise-linear interp needs distinct inputs). Returns the point's index.
fn insert_curve_point(pts: &mut Vec<CurvePoint>, input: u8, output: u8) -> usize {
    match pts.binary_search_by_key(&input, |p| p.input) {
        Ok(i) => {
            pts[i].output = output;
            i
        }
        Err(i) => {
            pts.insert(i, CurvePoint { input, output });
            i
        }
    }
}

/// Move point `i` to (input, output), clamping input STRICTLY between its
/// neighbours so the control points always stay sorted with unique inputs.
fn drag_curve_point(pts: &mut [CurvePoint], i: usize, input: u8, output: u8) {
    let lo = if i > 0 { pts[i - 1].input.saturating_add(1) } else { 0 };
    let hi = if i + 1 < pts.len() { pts[i + 1].input.saturating_sub(1) } else { 255 };
    // lo > hi would need two neighbours ≤ 2 apart around an existing point —
    // unreachable via insert/drag above; keep the current input if it happens.
    if lo <= hi {
        pts[i].input = input.clamp(lo, hi);
    }
    pts[i].output = output;
}

/// Two API base URLs are the "same endpoint" ignoring whitespace / a trailing slash.
/// Used so model ids fetched with the image key are only offered to the analysis
/// picker when both roles point at the same server.
fn same_base(a: &str, b: &str) -> bool {
    a.trim().trim_end_matches('/') == b.trim().trim_end_matches('/')
}

/// A model picker: a dropdown of `options` that writes the chosen id into `value`,
/// next to a text field so any custom id can still be typed. Both edit `value`.
fn model_picker(ui: &mut egui::Ui, salt: &str, value: &mut String, options: &[String]) {
    let sel = if value.trim().is_empty() { "选择… / pick".to_owned() } else { value.clone() };
    egui::ComboBox::from_id_salt(salt)
        .selected_text(sel)
        .width(200.0)
        .show_ui(ui, |ui| {
            for opt in options {
                ui.selectable_value(value, opt.clone(), opt.as_str());
            }
        });
    ui.add(
        egui::TextEdit::singleline(value)
            .desired_width(170.0)
            .hint_text("or type a custom id"),
    );
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
    // --- reverse-fit ("match") ---
    last_generated: Option<PathBuf>,       // this photo's last reimagine output (fit target)
    // --- production niceties ---
    view_mode: ViewMode,                   // side-by-side vs after-only (hold B = compare)
    toasts: Vec<Toast>,                    // transient corner notifications
    histogram: Option<Vec<[f32; 4]>>,      // live RGB+luma histogram of the After preview
    last_title: String,                    // window title cache (send only on change)
    // --- zoom / pan (per-photo, reset on open) ---
    zoom: f32,                             // 1.0 = fit; up to 12×
    pan: egui::Vec2,                       // visible-window centre in crop-window coords
    // --- crop tool ---
    crop_mode: bool,                       // the crop overlay is active on the After image
    crop_aspect: usize,                    // index into CROP_ASPECTS
    crop_drag: Option<(u8, egui::Pos2, [f32; 4])>, // (handle, drag start, crop at start)
    // --- manual local adjustments (masks) ---
    sel_mask: Option<usize>,               // selected recipe.masks index (overlay + sliders)
    placing_mask: Option<(MaskKind, Option<usize>)>, // next image drag defines a mask (replace idx)
    place_start: Option<(f32, f32)>,       // placement drag origin, full-frame normalized
    // --- tone-curve editor ---
    curve_channel: usize,                  // CURVE_CHANNELS index: 0=master 1=R 2=G 3=B
    curve_drag: Option<usize>,             // control point being dragged (active channel)
    // --- batch recipe copy / paste ---
    multi_sel: HashSet<usize>,             // Ctrl+click gallery multi-selection
    copied: Option<EditRecipe>,            // the recipe "clipboard" (in-app only)
    paste_geometry: bool,                  // keep crop/straighten when pasting
    // --- WB eyedropper ---
    wb_picking: bool,                      // next image click samples a neutral point
}

/// The two mask geometries a user can place by dragging.
#[derive(Clone, Copy, PartialEq)]
enum MaskKind {
    Linear,
    Radial,
}

/// Crop aspect presets. `None` = free; `Some(r)` = width/height in PIXELS
/// (0.0 is the "original" sentinel, resolved against the photo at drag time).
const CROP_ASPECTS: [(&str, Option<f32>); 7] = [
    ("自由", None),
    ("原始", Some(0.0)),
    ("1:1", Some(1.0)),
    ("3:2", Some(1.5)),
    ("2:3", Some(2.0 / 3.0)),
    ("16:9", Some(16.0 / 9.0)),
    ("9:16", Some(9.0 / 16.0)),
];

/// Screen ⇄ full-frame-normalized mapping through the visible uv window
/// (committed crop × zoom/pan). ALL image-space state — regions, mask
/// geometry, crop, the paint canvas — lives in full-frame normalized
/// coordinates, so every interaction handler maps through this one struct and
/// zoom/crop can never silently break any of them.
#[derive(Clone, Copy)]
struct ViewXform {
    rect: egui::Rect, // where the (visible part of the) image is drawn
    uv: egui::Rect,   // which full-frame region is visible (texture uv)
}

impl ViewXform {
    fn to_norm(self, p: egui::Pos2) -> (f32, f32) {
        let fx = ((p.x - self.rect.min.x) / self.rect.width().max(1.0)).clamp(0.0, 1.0);
        let fy = ((p.y - self.rect.min.y) / self.rect.height().max(1.0)).clamp(0.0, 1.0);
        (self.uv.min.x + fx * self.uv.width(), self.uv.min.y + fy * self.uv.height())
    }

    fn to_screen(self, nx: f32, ny: f32) -> egui::Pos2 {
        egui::pos2(
            self.rect.min.x
                + (nx - self.uv.min.x) / self.uv.width().max(1e-6) * self.rect.width(),
            self.rect.min.y
                + (ny - self.uv.min.y) / self.uv.height().max(1e-6) * self.rect.height(),
        )
    }
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
            last_generated: None,
            view_mode: ViewMode::SideBySide,
            toasts: Vec::new(),
            histogram: None,
            last_title: String::new(),
            zoom: 1.0,
            pan: egui::vec2(0.5, 0.5),
            crop_mode: false,
            crop_aspect: 0,
            crop_drag: None,
            sel_mask: None,
            placing_mask: None,
            place_start: None,
            curve_channel: 0,
            curve_drag: None,
            multi_sel: HashSet::new(),
            copied: None,
            paste_geometry: false,
            wb_picking: false,
        }
    }
}

impl AutoshopApp {
    /// Restore persisted prefs (last folder, view mode, export options) and
    /// re-open the library the user was browsing. Window geometry itself is
    /// restored by eframe's own persistence layer.
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut app = Self::default();
        if let Some(prefs) =
            cc.storage.and_then(|s| eframe::get_value::<Prefs>(s, eframe::APP_KEY))
        {
            app.style_strength = prefs.style_strength.clamp(0.0, 1.0);
            app.save_jpeg = prefs.save_jpeg;
            app.save_denoise = prefs.save_denoise;
            app.view_mode = prefs.view_mode;
            if let Some(dir) = prefs.gallery_dir.filter(|d| d.is_dir()) {
                app.open_folder(dir);
            }
        }
        app
    }

    fn toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        self.toasts.push(Toast { text: text.into(), kind, born: Instant::now() });
        if self.toasts.len() > 5 {
            self.toasts.remove(0); // keep the stack readable
        }
    }

    /// A worker finished successfully: status line + a success toast, unbusy.
    fn done(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.status = text.clone();
        self.toast(ToastKind::Success, text);
        self.busy = false;
    }

    /// A worker failed: status line + a lingering error toast, unbusy. A single
    /// status line is too easy to miss for a failed export or API call.
    fn fail(&mut self, what: &str, e: impl std::fmt::Display) {
        let text = format!("{what}: {e}");
        self.status = text.clone();
        self.toast(ToastKind::Error, text);
        self.busy = false;
    }
}

/// 64-bin RGB+luma histogram of a preview, each channel normalised to its own
/// peak bin (shape is what matters; absolute counts depend on preview size).
fn compute_histogram(img: &image::DynamicImage) -> Vec<[f32; 4]> {
    const BINS: usize = 64;
    let rgb = img.to_rgb8();
    let mut counts = vec![[0u32; 4]; BINS];
    for px in rgb.pixels() {
        let (r, g, b) = (px[0] as usize, px[1] as usize, px[2] as usize);
        let luma = (0.299 * px[0] as f32 + 0.587 * px[1] as f32 + 0.114 * px[2] as f32) as usize;
        counts[r * BINS / 256][0] += 1;
        counts[g * BINS / 256][1] += 1;
        counts[b * BINS / 256][2] += 1;
        counts[(luma * BINS / 256).min(BINS - 1)][3] += 1;
    }
    let mut max = [1u32; 4];
    for bins in &counts {
        for ch in 0..4 {
            max[ch] = max[ch].max(bins[ch]);
        }
    }
    counts
        .iter()
        .map(|bins| std::array::from_fn(|ch| bins[ch] as f32 / max[ch] as f32))
        .collect()
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
        // Keep any model lists already fetched this session so reopening Settings
        // doesn't force a re-fetch.
        let chat_choices = std::mem::take(&mut self.settings.chat_choices);
        let image_gen_choices = std::mem::take(&mut self.settings.image_gen_choices);
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
            chat_choices,
            image_gen_choices,
            fetching_models: false,
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

    /// Fetch the account's model ids (`GET /models`) on a worker thread and fill the
    /// Settings pick-lists. Uses the key/base typed in the form if present, else the
    /// saved config — so it works whether or not the user has saved a key yet.
    fn fetch_models(&mut self) {
        if self.settings.fetching_models {
            return;
        }
        self.settings.fetching_models = true;
        self.settings.status = "fetching models…".into();
        let form_key = self.settings.image_api_key.trim().to_string();
        let form_base = self.settings.image_base_url.trim().to_string();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            // RAII: guarantee the UI's `fetching_models` flag is always cleared —
            // if this thread panics before sending, the guard sends an Err on unwind
            // so the button never stays stuck disabled.
            struct Guard {
                tx: Sender<Msg>,
                armed: bool,
            }
            impl Drop for Guard {
                fn drop(&mut self) {
                    if self.armed {
                        let _ = self
                            .tx
                            .send(Msg::Models(Err(anyhow::anyhow!("fetch worker ended unexpectedly"))));
                    }
                }
            }
            let mut guard = Guard { tx: tx.clone(), armed: true };

            let cfg = autoshop::config::Config::load();
            let base = if form_base.is_empty() { cfg.openai_base_url.clone() } else { form_base };
            let key = if form_key.is_empty() {
                cfg.openai_api_key.clone().unwrap_or_default()
            } else {
                form_key
            };
            let res = autoshop::openai_models::list_models(&base, &key);
            guard.armed = false; // normal completion — don't double-send from Drop
            let _ = tx.send(Msg::Models(res));
        });
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        let mut do_save = false;
        let mut do_fetch = false;
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
                // OAuth uses Claude CLI aliases; API uses the fetched OpenAI chat ids,
                // but only when the analysis endpoint matches the one we fetched from
                // (the image key/base) — otherwise those ids may not exist there.
                let opts = if f.analysis_provider_api {
                    let fetched = if same_base(&f.analysis_base_url, &f.image_base_url) {
                        f.chat_choices.as_slice()
                    } else {
                        &[]
                    };
                    model_opts(fetched, &["gpt-5.5", "gpt-4o"], &f.analysis_model)
                } else {
                    model_opts(&[], &["opus", "sonnet", "haiku"], &f.analysis_model)
                };
                model_picker(ui, "set_analysis_model", &mut f.analysis_model, &opts);
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
            ui.heading("Image — the vision proposer + generative edits (API only)");
            ui.horizontal(|ui| {
                let label = if f.fetching_models { "拉取中… / fetching…" } else { "🔄 拉取可用模型 / Fetch models" };
                let clicked = ui
                    .add_enabled(!f.fetching_models, egui::Button::new(label))
                    .on_hover_text(
                        "List the models this API key can use (GET /models) so you can pick instead of guess. \
                         Uses the key typed above, or the saved one if blank.",
                    )
                    .clicked();
                if clicked {
                    do_fetch = true;
                }
                if !f.chat_choices.is_empty() || !f.image_gen_choices.is_empty() {
                    ui.label(
                        egui::RichText::new(format!(
                            "{} chat · {} image",
                            f.chat_choices.len(),
                            f.image_gen_choices.len()
                        ))
                        .weak()
                        .small(),
                    );
                }
            });
            ui.horizontal(|ui| {
                ui.label("Vision model");
                let opts = model_opts(&f.chat_choices, &["gpt-5.5", "gpt-4o"], &f.image_model);
                model_picker(ui, "set_vision_model", &mut f.image_model, &opts);
            });
            ui.horizontal(|ui| {
                ui.label("Base URL");
                ui.text_edit_singleline(&mut f.image_base_url);
            });
            ui.horizontal(|ui| {
                ui.label("Image-gen model");
                let opts = model_opts(
                    &f.image_gen_choices,
                    &["gpt-image-1.5", "gpt-image-2", "gpt-image-1", "gpt-image-1-mini", "chatgpt-image-latest"],
                    &f.image_gen_model,
                );
                model_picker(ui, "set_imagegen_model", &mut f.image_gen_model, &opts);
            });
            ui.label(
                egui::RichText::new(
                    "Tip: gpt-image-1.5 keeps the photo most faithful (input_fidelity); newer models \
                     like gpt-image-2 ignore that lock and edit more freely.",
                )
                .weak()
                .small(),
            );
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
        if do_fetch {
            self.fetch_models();
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

    /// Re-develop the working preview through the current recipe, and refresh
    /// the live histogram from the developed pixels (same buffer, one pass).
    fn redevelop(&mut self, ctx: &egui::Context) {
        if let Some(base) = &self.base_preview {
            let after = autoshop::render::develop_preview(base, &self.recipe);
            self.histogram = Some(compute_histogram(&after));
            self.after_tex = Some(ctx.load_texture("after", to_color_image(&after), egui::TextureOptions::LINEAR));
        }
        self.dirty = false;
    }

    /// Draw the live histogram (R/G/B filled, luma outline) — the tone readout a
    /// photo editor is expected to have. Sqrt-scaled so shadow detail reads.
    fn histogram_ui(&self, ui: &mut egui::Ui) {
        let Some(hist) = &self.histogram else { return };
        let h = 72.0;
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), h),
            egui::Sense::hover(),
        );
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 3.0, egui::Color32::from_gray(16));
        let n = hist.len().max(1);
        let bar_w = rect.width() / n as f32;
        // Additive-ish RGB: draw each channel as translucent filled bars.
        let colors = [
            egui::Color32::from_rgba_unmultiplied(220, 70, 70, 110),
            egui::Color32::from_rgba_unmultiplied(90, 200, 90, 110),
            egui::Color32::from_rgba_unmultiplied(90, 130, 240, 110),
        ];
        for (ch, color) in colors.iter().enumerate() {
            for (i, bins) in hist.iter().enumerate() {
                let v = bins[ch].sqrt(); // sqrt: make shadow counts visible
                if v <= 0.0 {
                    continue;
                }
                let x0 = rect.min.x + i as f32 * bar_w;
                let y0 = rect.max.y - v * (h - 2.0);
                p.rect_filled(
                    egui::Rect::from_min_max(egui::pos2(x0, y0), egui::pos2(x0 + bar_w, rect.max.y)),
                    0.0,
                    *color,
                );
            }
        }
        // Luma as a thin outline on top for the overall tone shape.
        let pts: Vec<egui::Pos2> = hist
            .iter()
            .enumerate()
            .map(|(i, bins)| {
                egui::pos2(
                    rect.min.x + (i as f32 + 0.5) * bar_w,
                    rect.max.y - bins[3].sqrt() * (h - 2.0),
                )
            })
            .collect();
        p.add(egui::Shape::line(pts, egui::Stroke::new(1.0, egui::Color32::from_gray(210))));
    }

    /// The interactive tone-curve editor: a channel picker (master / R / G / B)
    /// over a painted square — histogram backdrop, quarter grid, and the curve
    /// drawn straight from `render::curve_lut`, the SAME sampler the engine
    /// applies (so the preview line can never drift from the render). Click
    /// adds a point ON the curve, dragging moves it (inputs stay strictly
    /// increasing), dragging well outside the box deletes it — the Lightroom
    /// gestures. Returns true when the recipe changed this frame.
    fn curve_editor(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            for (i, (name, color)) in CURVE_CHANNELS.iter().enumerate() {
                if ui
                    .selectable_label(
                        self.curve_channel == i,
                        egui::RichText::new(*name).color(*color).small(),
                    )
                    .clicked()
                {
                    self.curve_channel = i;
                    self.curve_drag = None;
                }
            }
            if ui.small_button("↺").on_hover_text("清空当前通道曲线").clicked() {
                let pts = curve_points_mut(&mut self.recipe, self.curve_channel);
                if !pts.is_empty() {
                    pts.clear();
                    changed = true;
                }
                self.curve_drag = None;
            }
        });

        let side = ui.available_width().clamp(160.0, 240.0);
        let (rect, resp) =
            ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::click_and_drag());
        let p = ui.painter_at(rect);
        let accent = CURVE_CHANNELS[self.curve_channel].1;
        p.rect_filled(rect, 3.0, egui::Color32::from_gray(16));

        // Value space: x = input 0..1 (left→right), y = output 0..1 (bottom→top).
        let to_screen = |x: f32, y: f32| {
            egui::pos2(rect.min.x + x * rect.width(), rect.max.y - y * rect.height())
        };
        let to_val = |q: egui::Pos2| {
            (
                ((q.x - rect.min.x) / rect.width().max(1.0)).clamp(0.0, 1.0),
                ((rect.max.y - q.y) / rect.height().max(1.0)).clamp(0.0, 1.0),
            )
        };

        // Histogram backdrop for the active channel (luma behind the master curve)
        // — same data as the panel histogram, sqrt-scaled the same way.
        if let Some(hist) = &self.histogram {
            let ch = [3usize, 0, 1, 2][self.curve_channel];
            let bar_w = rect.width() / hist.len().max(1) as f32;
            let fill =
                egui::Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 34);
            for (i, bins) in hist.iter().enumerate() {
                let v = bins[ch].sqrt();
                if v <= 0.0 {
                    continue;
                }
                let x0 = rect.min.x + i as f32 * bar_w;
                p.rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(x0, rect.max.y - v * (rect.height() - 2.0)),
                        egui::pos2(x0 + bar_w, rect.max.y),
                    ),
                    0.0,
                    fill,
                );
            }
        }

        // Quarter grid + the identity diagonal for reference.
        let grid = egui::Stroke::new(1.0, egui::Color32::from_gray(38));
        for i in 1..4 {
            let t = i as f32 / 4.0;
            p.line_segment([to_screen(t, 0.0), to_screen(t, 1.0)], grid);
            p.line_segment([to_screen(0.0, t), to_screen(1.0, t)], grid);
        }
        p.line_segment(
            [to_screen(0.0, 0.0), to_screen(1.0, 1.0)],
            egui::Stroke::new(1.0, egui::Color32::from_gray(56)),
        );

        // --- interaction (mutates the active channel's control points) --------
        const HIT: f32 = 10.0; // grab radius around a point, screen px
        let lut_before =
            autoshop::render::curve_lut(curve_points(&self.recipe, self.curve_channel));
        let pts = curve_points_mut(&mut self.recipe, self.curve_channel);
        if (resp.drag_started() || resp.clicked())
            && let Some(q) = resp.interact_pointer_pos()
        {
            let near = pts.iter().position(|c| {
                to_screen(c.input as f32 / 255.0, c.output as f32 / 255.0).distance(q) <= HIT
            });
            let idx = match near {
                Some(i) => i,
                None => {
                    // Add ON the current curve at the clicked input — the shape
                    // doesn't jump; the user then drags the new point away.
                    let (vx, _) = to_val(q);
                    let input = (vx * 255.0).round() as u8;
                    let output = (lut_before[input as usize] * 255.0).round() as u8;
                    changed = true;
                    insert_curve_point(pts, input, output)
                }
            };
            if resp.drag_started() {
                self.curve_drag = Some(idx);
            }
        }
        if let Some(i) = self.curve_drag.filter(|&i| i < pts.len()) {
            if resp.dragged()
                && let Some(q) = resp.interact_pointer_pos()
            {
                if rect.expand(28.0).contains(q) {
                    let (vx, vy) = to_val(q);
                    drag_curve_point(
                        pts,
                        i,
                        (vx * 255.0).round() as u8,
                        (vy * 255.0).round() as u8,
                    );
                } else {
                    pts.remove(i); // dragged well outside → delete (LR gesture)
                    self.curve_drag = None;
                }
                changed = true;
            }
            if resp.drag_stopped() {
                self.curve_drag = None;
            }
        }

        // Engine-faithful curve line: all 256 samples straight from the shared LUT.
        let lut = autoshop::render::curve_lut(pts);
        let line: Vec<egui::Pos2> = lut
            .iter()
            .enumerate()
            .map(|(i, &y)| to_screen(i as f32 / 255.0, y))
            .collect();
        p.add(egui::Shape::line(line, egui::Stroke::new(1.6, accent)));

        // Control-point handles (the dragged one filled with the channel colour).
        for (i, c) in pts.iter().enumerate() {
            let q = to_screen(c.input as f32 / 255.0, c.output as f32 / 255.0);
            if self.curve_drag == Some(i) {
                p.circle_filled(q, 5.0, accent);
            } else {
                p.circle_filled(q, 3.5, egui::Color32::from_gray(230));
                p.circle_stroke(q, 3.5, egui::Stroke::new(1.0, egui::Color32::from_gray(60)));
            }
        }

        ui.label(
            egui::RichText::new("点击加点 · 拖动移点 · 拖出框外删点 — 预览/导出/XMP 同源生效")
                .weak()
                .small(),
        );
        changed
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

    /// Paste the copied recipe onto every Ctrl+click-selected photo on a worker
    /// thread — Lightroom's "sync settings", without rendering anything: a
    /// ./out recipe JSON per photo plus an XMP sidecar for RAWs. Geometry
    /// (crop/straighten) is stripped unless `paste_geometry` is on, because
    /// composition rarely transfers between frames. Library files are never
    /// touched (write_recipe / write_xmp only ever land in ./out).
    fn start_paste(&mut self) {
        let Some(src) = self.copied.clone() else { return };
        if self.busy {
            return;
        }
        let targets: Vec<PathBuf> = {
            let mut idx: Vec<usize> = self.multi_sel.iter().copied().collect();
            idx.sort_unstable(); // report in gallery order, not hash order
            idx.into_iter().filter_map(|i| self.gallery.get(i).cloned()).collect()
        };
        if targets.is_empty() {
            return;
        }
        let mut recipe = src;
        if !self.paste_geometry {
            recipe.crop = None;
            recipe.straighten_deg = 0.0;
        }
        // If the open photo is one of the targets, take the paste live in the
        // editor too (undo-able through the usual committed-snapshot step).
        if let Some(open) = &self.src_path
            && targets.iter().any(|t| t == open)
        {
            self.recipe = recipe.clone();
            self.dirty = true;
        }
        self.busy = true;
        self.status = format!("粘贴配方到 {} 张…", targets.len());
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let res = (|| -> anyhow::Result<String> {
                let (mut okn, mut xmpn) = (0usize, 0usize);
                let mut errs: Vec<String> = Vec::new();
                for path in &targets {
                    let step = || -> anyhow::Result<bool> {
                        autoshop::pipeline::write_recipe(path, &recipe, None)?;
                        if autoshop::decode::is_raw(path) {
                            autoshop::pipeline::write_xmp(path, &recipe)?;
                            return Ok(true);
                        }
                        Ok(false)
                    };
                    match step() {
                        Ok(wrote_xmp) => {
                            okn += 1;
                            xmpn += usize::from(wrote_xmp);
                        }
                        Err(e) => errs.push(format!("{}: {e}", autoshop::pipeline::stem(path))),
                    }
                }
                // Any failure surfaces as an error toast WITH the success count —
                // a partial failure must never read as a clean success.
                if errs.is_empty() {
                    Ok(format!("配方已粘贴到 {okn} 张（{xmpn} 个 XMP）→ ./out"))
                } else {
                    anyhow::bail!("{okn} 成功、{} 失败：{}", errs.len(), errs.join(" · "))
                }
            })();
            let _ = tx.send(Msg::Pasted(res));
        });
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
                        self.last_generated = None; // a fit target belongs to ONE photo
                        // View + tool state is per-photo.
                        self.zoom = 1.0;
                        self.pan = egui::vec2(0.5, 0.5);
                        self.crop_mode = false;
                        self.crop_drag = None;
                        self.sel_mask = None;
                        self.placing_mask = None;
                        self.place_start = None;
                        self.curve_drag = None; // curve_channel is a UI pref, keep it
                        self.wb_picking = false;
                        self.verdict = None;
                        self.rationale.clear();
                        self.dirty = true; // render the (neutral) after
                        self.busy = false;
                        self.status = "ready — adjust sliders or run AI Analyze".into();
                    }
                    Err(e) => {
                        self.fail("could not open", e);
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
                        self.fail("analyze failed", e);
                    }
                },
                Msg::Exported(Ok(p)) => {
                    self.done(format!("exported → {p}"));
                }
                Msg::Exported(Err(e)) => {
                    self.fail("export failed", e);
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
                        self.multi_sel.clear(); // indices belong to the old folder
                        self.busy = false;
                        self.status = format!("{n} photo{} — click a thumbnail to open", if n == 1 { "" } else { "s" });
                    }
                    Err(e) => {
                        self.fail("scan failed", e);
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
                    Ok((img, msg, generated)) => {
                        // Show the saved master in the After pane (it's a separate
                        // ./out artifact, not the develop recipe — a slider edit
                        // re-develops over it, exactly like the web UI).
                        self.after_tex = Some(ctx.load_texture(
                            "after",
                            to_color_image(&img),
                            egui::TextureOptions::LINEAR,
                        ));
                        // A whole-frame reimagine output becomes the reverse-fit
                        // target; fill/heal artifacts don't (they're mostly the
                        // original pixels, so a fit would just be neutral).
                        if let Some(p) = generated {
                            self.last_generated = Some(p);
                        }
                        self.clear_mask();
                        self.done(msg);
                    }
                    Err(e) => {
                        self.fail("retouch failed", e);
                    }
                },
                Msg::Fitted(boxed) => match *boxed {
                    Ok((recipe, note)) => {
                        // The fitted recipe replaces the working recipe like an AI
                        // Analyze result — the undo history picks it up as one step
                        // via the committed-snapshot diff, and the preview
                        // re-develops through the normal dirty path.
                        self.recipe = recipe;
                        self.rationale = self.recipe.rationale.clone();
                        self.verdict = None;
                        self.dirty = true;
                        self.done(note);
                    }
                    Err(e) => {
                        self.fail("反推失败", e);
                    }
                },
                Msg::Styled(boxed) => match *boxed {
                    Ok(prompt) => {
                        // Into the Direction box: ready to restyle OTHER photos.
                        self.guidance = prompt;
                        self.done("风格提示词已提取 → 已填入 Direction（同时存 ./out/<stem>.style.txt）");
                    }
                    Err(e) => {
                        self.fail("风格提取失败", e);
                    }
                },
                Msg::Pasted(res) => match res {
                    Ok(s) => self.done(s),
                    Err(e) => self.fail("批量粘贴", e),
                },
                Msg::Models(res) => match res {
                    Ok(ids) => {
                        let chat: Vec<String> = ids
                            .iter()
                            .filter(|s| autoshop::openai_models::is_chat_model(s))
                            .cloned()
                            .collect();
                        let imgs: Vec<String> = ids
                            .iter()
                            .filter(|s| autoshop::openai_models::is_image_model(s))
                            .cloned()
                            .collect();
                        self.settings.status =
                            format!("fetched {} models ({} chat · {} image)", ids.len(), chat.len(), imgs.len());
                        self.settings.chat_choices = chat;
                        self.settings.image_gen_choices = imgs;
                        self.settings.fetching_models = false;
                    }
                    Err(e) => {
                        self.settings.fetching_models = false;
                        self.settings.status = format!("fetch failed: {e}");
                    }
                },
            }
        }
        // Keep the frame loop alive while any worker (analyze/export/thumbs/models) runs.
        if self.busy || self.thumb_inflight > 0 || self.settings.fetching_models {
            ctx.request_repaint();
        }
    }

    /// One labelled slider; double-click resets to `default` (the Lightroom
    /// gesture). Returns true if the value changed this frame.
    fn slider(
        ui: &mut egui::Ui,
        label: &str,
        value: &mut f32,
        min: f32,
        max: f32,
        default: f32,
    ) -> bool {
        let resp = ui
            .add(egui::Slider::new(value, min..=max).text(label))
            .on_hover_text("双击归零 / double-click resets");
        if resp.double_clicked() && *value != default {
            *value = default;
            return true;
        }
        resp.changed()
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
        // Batch: copy the open photo's recipe → Ctrl+click a selection → paste.
        // Lightroom's "sync settings" for the whole working folder.
        ui.horizontal(|ui| {
            ui.add_enabled_ui(self.src_path.is_some(), |ui| {
                if ui
                    .small_button("⎘ 复制配方")
                    .on_hover_text("复制当前照片的全部 develop 参数")
                    .clicked()
                {
                    self.copied = Some(self.recipe.clone());
                    self.status = "配方已复制 — Ctrl+点击选多张，再「粘贴到选中」".into();
                }
            });
            let n = self.multi_sel.len();
            ui.add_enabled_ui(self.copied.is_some() && n > 0 && !self.busy, |ui| {
                if ui
                    .small_button(format!("⇩ 粘贴到选中({n})"))
                    .on_hover_text("对每张写 ./out 配方 JSON；RAW 同时写 XMP 边车。不动库文件、不渲染成品")
                    .clicked()
                {
                    self.start_paste();
                }
            });
            if n > 0 && ui.small_button("✕").on_hover_text("清除多选").clicked() {
                self.multi_sel.clear();
            }
        });
        if self.copied.is_some() {
            ui.checkbox(&mut self.paste_geometry, "粘贴时含裁剪/拉直")
                .on_hover_text("默认不带几何 — 构图在照片间通常不可复用");
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
        let multi_sel = &self.multi_sel;
        let mut to_open: Option<usize> = None;
        let mut to_toggle: Option<usize> = None;
        let mut to_request: Vec<usize> = Vec::new();

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, GALLERY_ROW_H, count, |ui, range| {
                for i in range {
                    let path = &gallery[i];
                    let is_sel = selected == Some(i);
                    let is_multi = multi_sel.contains(&i);
                    let fill = if is_sel {
                        SEL_BG
                    } else if is_multi {
                        egui::Color32::from_rgb(0x1a, 0x2a, 0x4e) // dimmer than SEL_BG
                    } else {
                        egui::Color32::TRANSPARENT
                    };
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
                                        if is_multi {
                                            ui.label(egui::RichText::new("✓ 选中").color(ACCENT).small());
                                        }
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
                        // Ctrl+click toggles the batch selection; plain click opens.
                        if ui.input(|inp| inp.modifiers.command) {
                            to_toggle = Some(i);
                        } else {
                            to_open = Some(i);
                        }
                    }
                }
            });

        for i in to_request {
            self.request_thumb(i);
        }
        if let Some(i) = to_toggle
            && !self.multi_sel.remove(&i)
        {
            self.multi_sel.insert(i);
        }
        if let Some(i) = to_open {
            self.open_gallery_index(i);
        }
    }

    fn develop_panel(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        ui.heading("Develop");
        self.histogram_ui(ui);
        ui.add_space(4.0);

        // Lightroom-style grouping: a wall of 16 sliders scans terribly; four
        // titled sections (tone open, the rest by activity) scan at a glance.
        // A section whose values are non-neutral shows a ● so a collapsed
        // active adjustment is never invisible. Flags are snapshot up front —
        // Copy bools, so no borrow spans the section closures (E0500).
        let (presence_active, detail_active, hsl_active, grade_active, curves_active) = {
            let r = &self.recipe;
            (
                r.clarity != 0.0 || r.dehaze != 0.0 || r.vibrance != 0.0 || r.saturation != 0.0,
                r.sharpening != 0.0 || r.noise_reduction != 0.0,
                !r.hsl.is_neutral(),
                !r.color_grade.is_neutral(),
                !r.tone_curve.is_empty()
                    || !r.red_curve.is_empty()
                    || !r.green_curve.is_empty()
                    || !r.blue_curve.is_empty(),
            )
        };

        egui::CollapsingHeader::new("色调 · Tone & WB")
            .id_salt("sec_tone")
            .default_open(true)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let mut custom_wb = self.recipe.temperature_k.is_some();
                    if ui.checkbox(&mut custom_wb, "Custom white balance (off = as-shot)").changed() {
                        self.recipe.temperature_k = if custom_wb { Some(5500.0) } else { None };
                        changed = true;
                    }
                    let label = if self.wb_picking { "💧 点击图中…" } else { "💧 吸管" };
                    if ui
                        .small_button(label)
                        .on_hover_text(
                            "点击图上应为中性灰/白的位置，自动反解 Temp/Tint（与引擎同一正向模型）。再点一次取消",
                        )
                        .clicked()
                    {
                        self.wb_picking = !self.wb_picking;
                        if self.wb_picking {
                            // One canvas tool at a time.
                            self.crop_mode = false;
                            self.paint_mode = false;
                            self.placing_mask = None;
                            self.status = "WB 吸管：点击图中应为中性灰/白的位置".into();
                        }
                    }
                });
                if let Some(mut k) = self.recipe.temperature_k
                    && Self::slider(ui, "Temp (K)", &mut k, 2000.0, 40000.0, 5500.0)
                {
                    self.recipe.temperature_k = Some(k);
                    changed = true;
                }
                let r = &mut self.recipe;
                changed |= Self::slider(ui, "Tint", &mut r.tint, -100.0, 100.0, 0.0);
                changed |= Self::slider(ui, "Exposure", &mut r.exposure_ev, -5.0, 5.0, 0.0);
                changed |= Self::slider(ui, "Contrast", &mut r.contrast, -100.0, 100.0, 0.0);
                changed |= Self::slider(ui, "Highlights", &mut r.highlights, -100.0, 100.0, 0.0);
                changed |= Self::slider(ui, "Shadows", &mut r.shadows, -100.0, 100.0, 0.0);
                changed |= Self::slider(ui, "Whites", &mut r.whites, -100.0, 100.0, 0.0);
                changed |= Self::slider(ui, "Blacks", &mut r.blacks, -100.0, 100.0, 0.0);
            });

        // --- 曲线: master + RGB tone curves (engine + XMP already apply them,
        // this is purely the editing surface — Lightroom's panel order) --------
        egui::CollapsingHeader::new(section_title("曲线 · Curves", curves_active))
            .id_salt("sec_curves")
            .default_open(false)
            .show(ui, |ui| {
                changed |= self.curve_editor(ui);
            });

        egui::CollapsingHeader::new(section_title("质感 · Presence", presence_active))
            .id_salt("sec_presence")
            .default_open(true)
            .show(ui, |ui| {
                let r = &mut self.recipe;
                changed |= Self::slider(ui, "Clarity", &mut r.clarity, -100.0, 100.0, 0.0);
                changed |= Self::slider(ui, "Dehaze", &mut r.dehaze, -100.0, 100.0, 0.0);
                changed |= Self::slider(ui, "Vibrance", &mut r.vibrance, -100.0, 100.0, 0.0);
                changed |= Self::slider(ui, "Saturation", &mut r.saturation, -100.0, 100.0, 0.0);
            });

        egui::CollapsingHeader::new(section_title("细节 · Detail", detail_active))
            .id_salt("sec_detail")
            .default_open(false)
            .show(ui, |ui| {
                let r = &mut self.recipe;
                changed |= Self::slider(ui, "Sharpening", &mut r.sharpening, 0.0, 150.0, 0.0);
                changed |=
                    Self::slider(ui, "Noise Reduction", &mut r.noise_reduction, 0.0, 100.0, 0.0);
            });

        egui::CollapsingHeader::new(section_title("混色器 · Color Mixer (HSL)", hsl_active))
            .id_salt("sec_hsl")
            .default_open(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("hsl_band")
                        .selected_text(HSL_BANDS[self.hsl_band])
                        .show_ui(ui, |ui| {
                            for (i, name) in HSL_BANDS.iter().enumerate() {
                                ui.selectable_value(&mut self.hsl_band, i, *name);
                            }
                        });
                    if ui.small_button("↺ reset all").clicked() {
                        self.recipe.hsl = Hsl::default();
                        changed = true;
                    }
                });
                let b = self.hsl_band;
                changed |= Self::slider(ui, "Hue", &mut self.recipe.hsl.hue[b], -100.0, 100.0, 0.0);
                changed |=
                    Self::slider(ui, "Saturation", &mut self.recipe.hsl.saturation[b], -100.0, 100.0, 0.0);
                changed |=
                    Self::slider(ui, "Luminance", &mut self.recipe.hsl.luminance[b], -100.0, 100.0, 0.0);
            });

        egui::CollapsingHeader::new(section_title("调色 · Color Grading", grade_active))
            .id_salt("sec_grade")
            .default_open(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("grade_region")
                        .selected_text(GRADE_REGIONS[self.grade_region])
                        .show_ui(ui, |ui| {
                            for (i, name) in GRADE_REGIONS.iter().enumerate() {
                                ui.selectable_value(&mut self.grade_region, i, *name);
                            }
                        });
                    if ui.small_button("↺ reset all").clicked() {
                        self.recipe.color_grade = ColorGrade::default();
                        changed = true;
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
                wheel_changed |= Self::slider(ui, "Hue", &mut hue, 0.0, 360.0, 0.0);
                wheel_changed |= Self::slider(ui, "Saturation", &mut sat, 0.0, 100.0, 0.0);
                wheel_changed |= Self::slider(ui, "Luminance", &mut lum, -100.0, 100.0, 0.0);
                if wheel_changed {
                    match self.grade_region {
                        0 => { cg.shadow_hue = hue; cg.shadow_sat = sat; cg.shadow_lum = lum; }
                        1 => { cg.midtone_hue = hue; cg.midtone_sat = sat; cg.midtone_lum = lum; }
                        2 => { cg.highlight_hue = hue; cg.highlight_sat = sat; cg.highlight_lum = lum; }
                        _ => { cg.global_hue = hue; cg.global_sat = sat; cg.global_lum = lum; }
                    }
                    changed = true;
                }
                changed |= Self::slider(ui, "Blending", &mut cg.blending, 0.0, 100.0, 50.0);
                changed |= Self::slider(ui, "Balance", &mut cg.balance, -100.0, 100.0, 0.0);
            });

        // --- 裁剪: recipe.crop is what export + XMP already apply ------------
        egui::CollapsingHeader::new(section_title("裁剪 · Crop", self.recipe.crop.is_some()))
            .id_salt("sec_crop")
            .default_open(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let label = if self.crop_mode { "✅ 完成" } else { "⛶ 进入裁剪" };
                    if ui.button(label).clicked() {
                        self.crop_mode = !self.crop_mode;
                        if self.crop_mode {
                            // One tool at a time on the canvas.
                            self.paint_mode = false;
                            self.placing_mask = None;
                            self.wb_picking = false;
                        }
                    }
                    egui::ComboBox::from_id_salt("crop_aspect")
                        .selected_text(CROP_ASPECTS[self.crop_aspect].0)
                        .width(70.0)
                        .show_ui(ui, |ui| {
                            for (i, (name, _)) in CROP_ASPECTS.iter().enumerate() {
                                ui.selectable_value(&mut self.crop_aspect, i, *name);
                            }
                        });
                    if ui.button("清除").clicked() {
                        self.recipe.crop = None;
                    }
                });
                ui.label(
                    egui::RichText::new(
                        "进入后在图上拖角柄/移动裁剪框；预览、导出与 XMP 一致生效。",
                    )
                    .weak()
                    .small(),
                );
            });

        // --- 局部调整: manual masks — the SAME recipe.masks the AI writes -----
        let n_masks = self.recipe.masks.len();
        egui::CollapsingHeader::new(section_title(
            &format!("局部调整 · Local Masks ({n_masks})"),
            n_masks > 0,
        ))
        .id_salt("sec_local")
        .default_open(false)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button("＋ 线性渐变").on_hover_text("在图上拖拽：起点=不受影响侧，终点=完全生效侧").clicked() {
                    self.placing_mask = Some((MaskKind::Linear, None));
                    self.paint_mode = false;
                    self.crop_mode = false;
                    self.wb_picking = false;
                    self.status = "在图上拖拽画出线性渐变（起点不受影响 → 终点完全生效）".into();
                }
                if ui.button("＋ 径向").on_hover_text("在图上拖拽画出椭圆范围").clicked() {
                    self.placing_mask = Some((MaskKind::Radial, None));
                    self.paint_mode = false;
                    self.crop_mode = false;
                    self.wb_picking = false;
                    self.status = "在图上拖拽画出径向（椭圆）范围".into();
                }
            });
            // Mask list: click to select (shows overlay + sliders), 🗑 deletes.
            let mut delete: Option<usize> = None;
            for i in 0..n_masks {
                ui.horizontal(|ui| {
                    let m = &self.recipe.masks[i];
                    let kind = match m.mask {
                        MaskGeometry::Linear { .. } => "线性",
                        MaskGeometry::Radial { .. } => "径向",
                    };
                    let label = format!("{} · {}", if m.name.is_empty() { "mask" } else { &m.name }, kind);
                    if ui.selectable_label(self.sel_mask == Some(i), label).clicked() {
                        self.sel_mask = if self.sel_mask == Some(i) { None } else { Some(i) };
                    }
                    if ui.small_button("🗑").clicked() {
                        delete = Some(i);
                    }
                });
            }
            if let Some(i) = delete {
                self.recipe.masks.remove(i);
                self.sel_mask = match self.sel_mask {
                    Some(s) if s == i => None,
                    Some(s) if s > i => Some(s - 1),
                    other => other,
                };
                changed = true;
            }
            // Selected mask: its full slider set.
            if let Some(i) = self.sel_mask.filter(|&i| i < self.recipe.masks.len()) {
                ui.separator();
                ui.horizontal(|ui| {
                    let m = &mut self.recipe.masks[i];
                    ui.add(egui::TextEdit::singleline(&mut m.name).desired_width(110.0).hint_text("名称"));
                    let kind = match m.mask {
                        MaskGeometry::Linear { .. } => MaskKind::Linear,
                        MaskGeometry::Radial { .. } => MaskKind::Radial,
                    };
                    if ui.small_button("↻ 重画").on_hover_text("在图上重新拖拽这个 mask 的范围").clicked() {
                        self.placing_mask = Some((kind, Some(i)));
                        self.paint_mode = false;
                        self.crop_mode = false;
                        self.wb_picking = false;
                    }
                    let m = &mut self.recipe.masks[i];
                    ui.checkbox(&mut m.inverted, "反转");
                });
                let m = &mut self.recipe.masks[i];
                changed |= Self::slider(ui, "Amount", &mut m.amount, 0.0, 1.0, 1.0);
                changed |= Self::slider(ui, "Exposure", &mut m.exposure_ev, -5.0, 5.0, 0.0);
                changed |= Self::slider(ui, "Contrast", &mut m.contrast, -100.0, 100.0, 0.0);
                changed |= Self::slider(ui, "Highlights", &mut m.highlights, -100.0, 100.0, 0.0);
                changed |= Self::slider(ui, "Shadows", &mut m.shadows, -100.0, 100.0, 0.0);
                changed |= Self::slider(ui, "Whites", &mut m.whites, -100.0, 100.0, 0.0);
                changed |= Self::slider(ui, "Blacks", &mut m.blacks, -100.0, 100.0, 0.0);
                changed |= Self::slider(ui, "Saturation", &mut m.saturation, -100.0, 100.0, 0.0);
                changed |= Self::slider(ui, "Noise Red.", &mut m.noise_reduction, 0.0, 100.0, 0.0);
                // These serialise to the XMP but the in-app preview doesn't
                // render them yet (documented engine scope) — honest label.
                egui::CollapsingHeader::new("更多（仅 XMP/Lightroom 生效）")
                    .id_salt("sec_local_xmp")
                    .default_open(false)
                    .show(ui, |ui| {
                        let m = &mut self.recipe.masks[i];
                        changed |= Self::slider(ui, "Clarity", &mut m.clarity, -100.0, 100.0, 0.0);
                        changed |= Self::slider(ui, "Dehaze", &mut m.dehaze, -100.0, 100.0, 0.0);
                        changed |= Self::slider(ui, "Texture", &mut m.texture, -100.0, 100.0, 0.0);
                        changed |= Self::slider(ui, "Temp", &mut m.temperature, -100.0, 100.0, 0.0);
                        changed |= Self::slider(ui, "Tint", &mut m.tint, -100.0, 100.0, 0.0);
                    });
            } else if n_masks == 0 {
                ui.label(
                    egui::RichText::new("像 Lightroom 的局部调整：加一个渐变压暗天空、径向提亮主体。AI Analyze 也会写到同一列表。")
                        .weak()
                        .small(),
                );
            }
        });

        if changed {
            self.recipe.clamp();
            self.dirty = true;
        }
    }

    /// The visible full-frame uv window: committed crop (shown cropped, like
    /// Lightroom — except while the crop tool is open, which needs the full
    /// frame) narrowed by zoom/pan. `pan` is stored in crop-window coords and
    /// re-clamped here so edge panning never accumulates out of range.
    fn view_uv(&mut self) -> egui::Rect {
        let win = match (&self.recipe.crop, self.crop_mode) {
            (Some(c), false) => egui::Rect::from_min_max(
                egui::pos2(c.left.min(c.right), c.top.min(c.bottom)),
                egui::pos2(c.right.max(c.left), c.bottom.max(c.top)),
            ),
            _ => egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        };
        let half = 0.5 / self.zoom.clamp(1.0, 12.0);
        self.pan = egui::vec2(
            self.pan.x.clamp(half, 1.0 - half),
            self.pan.y.clamp(half, 1.0 - half),
        );
        egui::Rect::from_min_max(
            egui::pos2(
                win.min.x + (self.pan.x - half) * win.width(),
                win.min.y + (self.pan.y - half) * win.height(),
            ),
            egui::pos2(
                win.min.x + (self.pan.x + half) * win.width(),
                win.min.y + (self.pan.y + half) * win.height(),
            ),
        )
    }

    /// The After image with its interaction layers — crop tool, mask placement,
    /// paint canvas, box-select — or the SOURCE flashed in the same rect while
    /// `comparing` (B held). Scroll zooms to the cursor; middle-drag or
    /// Space+drag pans; all image-space handlers map through [`ViewXform`].
    fn after_view(&mut self, ui: &mut egui::Ui, max_w: f32, avail_y: f32, comparing: bool) {
        let tex = if comparing { self.before_tex.as_ref() } else { self.after_tex.as_ref() };
        let Some((id, tex_size)) = tex.map(|t| (t.id(), t.size_vec2())) else {
            ui.label(egui::RichText::new("…").weak());
            return;
        };

        let uv = self.view_uv();
        // Display size fits the VISIBLE window's aspect (in image pixels).
        let vis_px = egui::vec2(uv.width() * tex_size.x, uv.height() * tex_size.y);
        let disp = fit_in(vis_px, max_w, avail_y);
        let scale = disp.x / vis_px.x.max(1.0); // display px per image px

        // Caption row: mode hint left, zoom readout + Fit / 1:1 right.
        let hint = if comparing {
            "Before (source) — 松开 B 回到编辑"
        } else if self.crop_mode {
            "裁剪 — 拖角柄调整，框内拖动移动"
        } else if self.placing_mask.is_some() {
            "局部调整 — 在图上拖拽画出渐变范围"
        } else if self.wb_picking {
            "WB 吸管 — 点击应为中性灰/白的位置"
        } else if self.paint_mode {
            "After — paint over the area to fill / heal"
        } else {
            "After — 拖框=局部AI · 滚轮缩放 · 空格/中键平移 · 按住B对比"
        };
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(hint).weak().small());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("1:1").on_hover_text("预览像素 1:1（双击图片可切换）").clicked() {
                    self.zoom = (vis_px.x * self.zoom / disp.x).max(1.0);
                }
                if ui.small_button("Fit").clicked() {
                    self.zoom = 1.0;
                    self.pan = egui::vec2(0.5, 0.5);
                }
                ui.label(egui::RichText::new(format!("{:.0}%", scale * 100.0)).weak().small());
            });
        });

        let (rect, resp) = ui.allocate_exact_size(disp, egui::Sense::click_and_drag());
        ui.painter_at(rect).image(id, rect, uv, egui::Color32::WHITE);
        let xf = ViewXform { rect, uv };

        // --- zoom to cursor (scroll) -----------------------------------------
        if resp.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll.abs() > 0.1
                && let Some(p) = resp.hover_pos()
            {
                let half = 0.5 / self.zoom;
                let (fx, fy) = (
                    ((p.x - rect.min.x) / rect.width().max(1.0)).clamp(0.0, 1.0),
                    ((p.y - rect.min.y) / rect.height().max(1.0)).clamp(0.0, 1.0),
                );
                // Cursor's point in crop-window coords, kept stationary across the zoom.
                let q = egui::vec2(
                    self.pan.x - half + fx * 2.0 * half,
                    self.pan.y - half + fy * 2.0 * half,
                );
                self.zoom = (self.zoom * (scroll * 0.003).exp()).clamp(1.0, 12.0);
                let nh = 0.5 / self.zoom;
                self.pan = q - egui::vec2((fx - 0.5) * 2.0 * nh, (fy - 0.5) * 2.0 * nh);
            }
        }
        // Double-click toggles fit ↔ 1:1 (preview pixels).
        if resp.double_clicked() {
            if self.zoom > 1.01 {
                self.zoom = 1.0;
                self.pan = egui::vec2(0.5, 0.5);
            } else {
                self.zoom = (vis_px.x / disp.x).max(1.0);
            }
        }

        // --- pan: middle-drag, or Space + left-drag (the Photoshop gesture) ---
        let space = ui.input(|i| i.key_down(egui::Key::Space));
        let panning = resp.dragged_by(egui::PointerButton::Middle)
            || (space && resp.dragged_by(egui::PointerButton::Primary));
        if panning {
            let d = resp.drag_delta();
            let ext = 1.0 / self.zoom; // visible extent in crop-window coords
            self.pan -= egui::vec2(
                d.x / rect.width().max(1.0) * ext,
                d.y / rect.height().max(1.0) * ext,
            );
        }

        if comparing || panning {
            return; // tools pause while comparing / panning
        }

        // --- tool dispatch (one active interaction at a time) -----------------
        if self.crop_mode {
            self.handle_crop(ui, &resp, xf, tex_size);
        } else if self.placing_mask.is_some() {
            self.handle_place_mask(ui, &resp, xf);
        } else if self.wb_picking {
            self.handle_wb_pick(ui, &resp, xf);
        } else if self.paint_mode {
            self.handle_paint(&resp, xf);
            self.ensure_mask_tex(ui.ctx());
            if let Some(t) = &self.mask_tex {
                ui.painter_at(rect).image(t.id(), rect, uv, egui::Color32::WHITE);
            }
        } else {
            self.handle_region_select(ui, &resp, xf);
        }

        // Selected mask stays visualised so its sliders have visual feedback.
        if !self.crop_mode
            && self.placing_mask.is_none()
            && let Some(m) = self.sel_mask.and_then(|i| self.recipe.masks.get(i))
        {
            draw_mask_overlay(ui, xf, &m.mask);
        }
    }

    /// WB eyedropper: click a pixel that SHOULD be neutral and the engine's
    /// inverse solver (`render::solve_wb_from_neutral` — the same 5500 K
    /// anchored forward model the render applies) turns it into Temp + Tint.
    /// Samples a 5×5 mean of the SOURCE preview: WB runs before develop, so
    /// the solve must see pre-develop pixels, not the current edit.
    fn handle_wb_pick(&mut self, ui: &egui::Ui, resp: &egui::Response, xf: ViewXform) {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        if !resp.clicked() {
            return;
        }
        let Some(q) = resp.interact_pointer_pos() else { return };
        let (nx, ny) = xf.to_norm(q);
        let px = {
            let Some(base) = &self.base_preview else { return };
            let rgb = base.to_rgb8();
            let (w, h) = rgb.dimensions();
            let (cx, cy) = (
                (nx * (w.saturating_sub(1)) as f32).round() as i64,
                (ny * (h.saturating_sub(1)) as f32).round() as i64,
            );
            let (mut acc, mut n) = ([0.0f32; 3], 0.0f32);
            for dy in -2..=2i64 {
                for dx in -2..=2i64 {
                    let (x, y) = (cx + dx, cy + dy);
                    if x >= 0 && y >= 0 && (x as u32) < w && (y as u32) < h {
                        let p = rgb.get_pixel(x as u32, y as u32);
                        for c in 0..3 {
                            acc[c] += p[c] as f32 / 255.0;
                        }
                        n += 1.0;
                    }
                }
            }
            if n == 0.0 {
                return;
            }
            [acc[0] / n, acc[1] / n, acc[2] / n]
        };
        let (k, tint) = autoshop::render::solve_wb_from_neutral(px);
        self.recipe.temperature_k = Some(k);
        self.recipe.tint = tint;
        self.wb_picking = false;
        self.dirty = true;
        self.status = format!("WB 吸管：{k:.0} K · tint {tint:+.0} — 可在色调区微调");
    }

    /// Box-select on the After image: drag a rectangle to target a local edit;
    /// the normalized box is folded into the AI direction so it masks exactly
    /// there (mirrors the web region→mask prompt). A plain click — or a tiny
    /// drag — clears the selection. Coordinates are full-frame normalized (the
    /// AI mask space), mapped through the view transform.
    fn handle_region_select(&mut self, ui: &egui::Ui, resp: &egui::Response, xf: ViewXform) {
        let rect = xf.rect;
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
                let (sn, en) = (xf.to_norm(s), xf.to_norm(e));
                let (l, r) = (sn.0.min(en.0), sn.0.max(en.0));
                let (t, b) = (sn.1.min(en.1), sn.1.max(en.1));
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
            draw(
                egui::Rect::from_min_max(xf.to_screen(l, t), xf.to_screen(rr, bb))
                    .intersect(rect),
            );
        }
    }

    /// The interactive crop overlay: darkened surround, thirds grid, four
    /// corner handles (aspect-constrained) and move-inside. The crop lives in
    /// `recipe.crop` (full-frame normalized) — exactly what the export render
    /// and the XMP already apply, so the tool adds no new data path.
    fn handle_crop(
        &mut self,
        ui: &egui::Ui,
        resp: &egui::Response,
        xf: ViewXform,
        tex_size: egui::Vec2,
    ) {
        use autoshop::recipe::Crop;
        let cur = self
            .recipe
            .crop
            .map(|c| [c.left, c.top, c.right, c.bottom])
            .unwrap_or([0.0, 0.0, 1.0, 1.0]);

        // Pixel aspect ratio (w/h) requested by the preset; "原始" resolves here.
        let aspect = CROP_ASPECTS[self.crop_aspect.min(CROP_ASPECTS.len() - 1)]
            .1
            .map(|r| if r == 0.0 { tex_size.x / tex_size.y.max(1.0) } else { r });

        // Handle order: 0=TL 1=TR 2=BL 3=BR, 4=move (inside).
        let corner_pos = |c: &[f32; 4], k: u8| match k {
            0 => xf.to_screen(c[0], c[1]),
            1 => xf.to_screen(c[2], c[1]),
            2 => xf.to_screen(c[0], c[3]),
            _ => xf.to_screen(c[2], c[3]),
        };
        if resp.drag_started()
            && let Some(p) = resp.interact_pointer_pos()
        {
            const HIT: f32 = 12.0;
            let handle = (0..4)
                .find(|&k| corner_pos(&cur, k).distance(p) <= HIT)
                .or_else(|| {
                    let r = egui::Rect::from_min_max(
                        xf.to_screen(cur[0], cur[1]),
                        xf.to_screen(cur[2], cur[3]),
                    );
                    r.contains(p).then_some(4)
                });
            if let Some(h) = handle {
                self.crop_drag = Some((h, p, cur));
            }
        }
        if resp.dragged()
            && let (Some((h, start, orig)), Some(p)) = (self.crop_drag, resp.interact_pointer_pos())
        {
            let (sn, pn) = (xf.to_norm(start), xf.to_norm(p));
            let (dx, dy) = (pn.0 - sn.0, pn.1 - sn.1);
            let c: [f32; 4];
            if h == 4 {
                // Move: shift, clamped so the rect stays inside the frame.
                let (w, hg) = (orig[2] - orig[0], orig[3] - orig[1]);
                let nl = (orig[0] + dx).clamp(0.0, 1.0 - w);
                let nt = (orig[1] + dy).clamp(0.0, 1.0 - hg);
                c = [nl, nt, nl + w, nt + hg];
            } else {
                // Corner: drag it, anchored at the opposite corner.
                let (ax, ay) = match h {
                    0 => (orig[2], orig[3]),
                    1 => (orig[0], orig[3]),
                    2 => (orig[2], orig[1]),
                    _ => (orig[0], orig[1]),
                };
                let mut x = (match h {
                    0 | 2 => orig[0] + dx,
                    _ => orig[2] + dx,
                })
                .clamp(0.0, 1.0);
                let mut y = (match h {
                    0 | 1 => orig[1] + dy,
                    _ => orig[3] + dy,
                })
                .clamp(0.0, 1.0);
                if let Some(r_px) = aspect {
                    // Width drives; height follows the pixel ratio; if the
                    // derived height leaves the frame, shrink both to fit.
                    let top_corner = h == 0 || h == 1;
                    let mut w_n = (x - ax).abs();
                    let mut h_n = w_n * tex_size.x / (r_px * tex_size.y.max(1.0));
                    let room = if top_corner { ay } else { 1.0 - ay };
                    if h_n > room {
                        h_n = room;
                        w_n = h_n * r_px * tex_size.y / tex_size.x.max(1.0);
                    }
                    x = if x >= ax { ax + w_n } else { ax - w_n };
                    y = if top_corner { ay - h_n } else { ay + h_n };
                }
                c = [x.min(ax), y.min(ay), x.max(ax), y.max(ay)];
            }
            if c[2] - c[0] >= 0.05 && c[3] - c[1] >= 0.05 {
                self.recipe.crop =
                    Some(Crop { left: c[0], top: c[1], right: c[2], bottom: c[3] });
            }
        }
        if resp.drag_stopped() {
            self.crop_drag = None;
        }

        // --- overlay: darkened surround + thirds + handles --------------------
        let c = self
            .recipe
            .crop
            .map(|c| [c.left, c.top, c.right, c.bottom])
            .unwrap_or([0.0, 0.0, 1.0, 1.0]);
        let p = ui.painter_at(xf.rect);
        let r = egui::Rect::from_min_max(xf.to_screen(c[0], c[1]), xf.to_screen(c[2], c[3]))
            .intersect(xf.rect);
        let dark = egui::Color32::from_black_alpha(140);
        let full = xf.rect;
        for shade in [
            egui::Rect::from_min_max(full.min, egui::pos2(full.max.x, r.min.y)), // top
            egui::Rect::from_min_max(egui::pos2(full.min.x, r.max.y), full.max), // bottom
            egui::Rect::from_min_max(egui::pos2(full.min.x, r.min.y), egui::pos2(r.min.x, r.max.y)),
            egui::Rect::from_min_max(egui::pos2(r.max.x, r.min.y), egui::pos2(full.max.x, r.max.y)),
        ] {
            if shade.width() > 0.0 && shade.height() > 0.0 {
                p.rect_filled(shade, 0.0, dark);
            }
        }
        let grid = egui::Stroke::new(1.0, egui::Color32::from_white_alpha(70));
        for i in 1..3 {
            let t = i as f32 / 3.0;
            p.line_segment(
                [egui::pos2(r.min.x + t * r.width(), r.min.y), egui::pos2(r.min.x + t * r.width(), r.max.y)],
                grid,
            );
            p.line_segment(
                [egui::pos2(r.min.x, r.min.y + t * r.height()), egui::pos2(r.max.x, r.min.y + t * r.height())],
                grid,
            );
        }
        p.rect_stroke(r, 0.0, egui::Stroke::new(1.5, egui::Color32::WHITE));
        for k in 0..4u8 {
            p.rect_filled(
                egui::Rect::from_center_size(corner_pos(&c, k), egui::vec2(9.0, 9.0)),
                1.0,
                egui::Color32::WHITE,
            );
        }
    }

    /// Place (or re-draw) a manual local-adjustment mask by dragging: the drag
    /// vector defines a linear gradient (start = untouched side) or the
    /// bounding box of a radial. Commits into `recipe.masks` — the SAME field
    /// the AI writes, so render + XMP need nothing new.
    fn handle_place_mask(&mut self, ui: &egui::Ui, resp: &egui::Response, xf: ViewXform) {
        let Some((kind, replace)) = self.placing_mask else { return };
        if resp.drag_started()
            && let Some(p) = resp.interact_pointer_pos()
        {
            self.place_start = Some(xf.to_norm(p));
        }
        let Some(s) = self.place_start else { return };
        let Some(p) = resp.interact_pointer_pos() else { return };
        let e = xf.to_norm(p);
        let geom = match kind {
            MaskKind::Linear => autoshop::recipe::MaskGeometry::Linear {
                zero_x: s.0,
                zero_y: s.1,
                full_x: e.0,
                full_y: e.1,
            },
            MaskKind::Radial => autoshop::recipe::MaskGeometry::Radial {
                top: s.1.min(e.1),
                left: s.0.min(e.0),
                bottom: s.1.max(e.1),
                right: s.0.max(e.0),
                feather: 0.5,
                roundness: 0.0,
                flipped: false,
            },
        };
        draw_mask_overlay(ui, xf, &geom); // live preview while dragging
        if resp.drag_stopped() {
            match replace {
                Some(i) if i < self.recipe.masks.len() => self.recipe.masks[i].mask = geom,
                _ => {
                    let n = self.recipe.masks.len();
                    self.recipe.masks.push(autoshop::recipe::LocalAdjustment {
                        mask: geom,
                        name: format!("手动 {}", n + 1),
                        ..Default::default()
                    });
                    self.sel_mask = Some(n);
                }
            }
            self.placing_mask = None;
            self.place_start = None;
            self.dirty = true;
            self.status = "mask 已放置 — 在左侧「局部调整」里拉滑杆（当前全为 0，无可见效果）".into();
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

    /// Brush-paint into the mask while dragging on the After image. The canvas
    /// is full-frame at preview resolution; pointer→canvas goes through the
    /// view transform so painting stays accurate at any zoom, and the brush
    /// radius converts display px → canvas px by the current pixel scale.
    fn handle_paint(&mut self, resp: &egui::Response, xf: ViewXform) {
        let brush = self.brush;
        let Some(m) = self.mask_paint.as_mut() else { return };
        let (mw, mh) = (m.width() as f32, m.height() as f32);
        let to_mask = |p: egui::Pos2| {
            let (nx, ny) = xf.to_norm(p);
            (nx * mw, ny * mh)
        };
        let brush_mask = (brush * (xf.uv.width() * mw) / xf.rect.width().max(1.0)).max(1.0);
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
            let res = (|| -> anyhow::Result<(image::DynamicImage, String, Option<PathBuf>)> {
                let cfg = autoshop::config::Config::load();
                let out = autoshop::pipeline::default_out(&path, "retouch", "png");
                let mask_tmp =
                    std::env::temp_dir().join(format!("autoshop_gui_fill_{}.png", std::process::id()));
                std::fs::write(&mask_tmp, &mask_png)?;
                let r = autoshop::generative::retouch(&cfg, &path, &mask_tmp, &prompt, &quality, full_res, &out);
                let _ = std::fs::remove_file(&mask_tmp);
                r?;
                let img = autoshop::decode::load_image(&out)?.thumbnail(PREVIEW_EDGE, PREVIEW_EDGE);
                // Not a fit target: a fill keeps most source pixels, so a reverse
                // fit against it would just recover a neutral recipe.
                Ok((img, format!("filled → {} (saved to ./out)", out.display()), None))
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
            let res = (|| -> anyhow::Result<(image::DynamicImage, String, Option<PathBuf>)> {
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
                // Not a fit target (pixel heal, not a whole-frame rendition).
                Ok((img, format!("healed {} spot(s) → {}", rep.spots, out.display()), None))
            })();
            let _ = tx.send(Msg::Retouched(Box::new(res)));
        });
    }

    /// Full-frame generative re-render via gpt-image — the OPTIONAL "let GPT
    /// directly make the picture" path. Uses the Direction text as the look
    /// prompt. Unlike Analyze (a faithful parametric recipe), this REGENERATES
    /// pixels — a creative restyle, not a master (up to ~8 MP on flexible-size
    /// models, ~1.5K on older ones). The saved path is remembered as the
    /// reverse-fit ("反推配方") target, which turns the look back into sliders +
    /// XMP at full resolution. Hits the gpt-image (image) endpoint.
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
        self.status = "AI 生成出片中… (gpt-image, ~15–60s; 高分辨率输入需先全幅显影)".into();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let res = (|| -> anyhow::Result<(image::DynamicImage, String, Option<PathBuf>)> {
                let cfg = autoshop::config::Config::load();
                let out = autoshop::pipeline::default_out(&path, "reimagine", "png");
                // fidelity "high" keeps it recognisably the same photo.
                autoshop::generative::reimagine(&cfg, &path, &prompt, "high", &cfg.openai_image_quality, &out)?;
                let img = autoshop::decode::load_image(&out)?.thumbnail(PREVIEW_EDGE, PREVIEW_EDGE);
                let msg = format!("generated → {} (可再点「反推配方」得滑杆/XMP)", out.display());
                Ok((img, msg, Some(out)))
            })();
            let _ = tx.send(Msg::Retouched(Box::new(res)));
        });
    }

    /// Reverse-fit ("match"): statistically solve the develop parameters that map
    /// the source preview onto the last reimagine output — the sliders update in
    /// place (undo-able), and for a RAW the XMP sidecar is written immediately.
    /// Deterministic, in-process, no API call (fit.rs).
    fn start_fit(&mut self) {
        let (Some(base), Some(tgt)) = (self.base_preview.clone(), self.last_generated.clone())
        else {
            return;
        };
        if self.busy {
            return;
        }
        let src_path = self.src_path.clone();
        self.busy = true;
        self.status = "反推配方中…（统计拟合，本地运算）".into();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let res = (|| -> anyhow::Result<(EditRecipe, String)> {
                let target = autoshop::decode::load_image(&tgt)?;
                let rep = autoshop::fit::fit_recipe(&base, &target);
                let mut note = format!(
                    "反推完成：look 残差 {:.3}→{:.3}（滑杆已更新，可 undo）",
                    rep.err_before, rep.err_after
                );
                if let Some(p) = src_path.filter(|p| autoshop::decode::is_raw(p)) {
                    let x = autoshop::pipeline::write_xmp(&p, &rep.recipe)?;
                    note.push_str(&format!(" · XMP → {}", x.display()));
                }
                Ok((rep.recipe, note))
            })();
            let _ = tx.send(Msg::Fitted(Box::new(res)));
        });
    }

    /// Extract a reusable STYLE PROMPT from the before/after pair via the vision
    /// model. The result lands in the Direction box (ready to reimagine OTHER
    /// photos with the same look) and is saved to ./out/<stem>.style.txt.
    fn start_style_prompt(&mut self) {
        let (Some(base), Some(tgt)) = (self.base_preview.clone(), self.last_generated.clone())
        else {
            return;
        };
        if self.busy {
            return;
        }
        let src_path = self.src_path.clone();
        self.busy = true;
        self.status = "提取风格提示词中… (vision, ~5-20s)".into();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let res = (|| -> anyhow::Result<String> {
                let cfg = autoshop::config::Config::load();
                let jpg = |img: &image::DynamicImage| -> anyhow::Result<Vec<u8>> {
                    let mut buf = Vec::new();
                    image::DynamicImage::ImageRgb8(img.thumbnail(768, 768).to_rgb8())
                        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Jpeg)?;
                    Ok(buf)
                };
                let target = autoshop::decode::load_image(&tgt)?;
                let prompt = autoshop::advisor::describe_style(&cfg, &jpg(&base)?, &jpg(&target)?)?;
                if let Some(p) = &src_path {
                    let out = autoshop::pipeline::default_out(p, "style", "txt");
                    autoshop::pipeline::ensure_parent(&out)?;
                    std::fs::write(&out, &prompt)?;
                }
                Ok(prompt)
            })();
            let _ = tx.send(Msg::Styled(Box::new(res)));
        });
    }

    fn retouch_panel(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.heading("Retouch");

        // Whole-image generative re-render: let gpt-image DIRECTLY produce the
        // picture (the optional "GPT makes the image" path). Distinct from
        // AI Analyze, which emits a faithful parametric recipe — but the
        // reverse-fit button closes the loop: generated look → recipe.
        egui::CollapsingHeader::new("整图 AI 生成 · Reimagine")
            .id_salt("sec_reimagine")
            .default_open(true)
            .show(ui, |ui| {
                ui.add_enabled_ui(!self.busy, |ui| {
                    if ui
                        .button("✨ AI 生成出片")
                        .on_hover_text(
                            "用 gpt-image 直接重绘整张图（拿上方 Direction 文本当风格描述）。重绘像素=非保真；\
                             支持任意尺寸的模型（gpt-image-2）可达 ~8MP，其余 ~1.5K。需 OPENAI_API_KEY",
                        )
                        .clicked()
                    {
                        self.start_reimagine();
                    }
                });
                // Reverse-fit the generated look back into an editable recipe —
                // how the low-res experiment becomes a full-res, XMP-able edit.
                let can_fit = self.last_generated.is_some() && self.base_preview.is_some();
                ui.add_enabled_ui(!self.busy && can_fit, |ui| {
                    ui.horizontal(|ui| {
                        if ui
                            .button("🎛 反推配方 → 滑杆/XMP")
                            .on_hover_text(
                                "统计拟合：把刚生成的观感反解成可编辑的 develop 参数（本地运算，无 API 费）。\
                                 滑杆会更新（可 undo），RAW 同时写 ./out XMP；再点 Save 可出全分辨率成品",
                            )
                            .clicked()
                        {
                            self.start_fit();
                        }
                        if ui
                            .button("📝 提取风格提示词")
                            .on_hover_text(
                                "对比 原图/生成图，让 vision 模型写一段可复用的风格 prompt：\
                                 自动填入 Direction（可直接给别的照片 Reimagine 用）并存 ./out/<stem>.style.txt",
                            )
                            .clicked()
                        {
                            self.start_style_prompt();
                        }
                    });
                });
                ui.label(
                    egui::RichText::new(
                        "拿上方 Direction 当风格描述。生成后可「反推配方」把观感变成滑杆+XMP（全分辨率的正道）。",
                    )
                    .weak()
                    .small(),
                );
            });

        // Mask tools shared by Fill AND Heal — one brush, two consumers.
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.paint_mode, "Paint mask")
                .on_hover_text("Brush over the area; box-select is paused while on. Fill 与 Heal 共用");
            if ui.button("Clear").clicked() {
                self.clear_mask();
            }
        });
        ui.add(egui::Slider::new(&mut self.brush, 4.0..=80.0).text("brush"));

        egui::CollapsingHeader::new("生成填充 · Generative Fill")
            .id_salt("sec_fill")
            .default_open(false)
            .show(ui, |ui| {
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
                    ui.add_enabled_ui(!self.busy, |ui| {
                        if ui.button("Remove / Fill").clicked() {
                            self.start_fill();
                        }
                    });
                });
                ui.label(
                    egui::RichText::new(
                        "Paint the area, write what belongs there, then Remove/Fill. Needs OPENAI_API_KEY.",
                    )
                    .weak()
                    .small(),
                );
            });

        egui::CollapsingHeader::new("去瑕疵 · Heal（像素）")
            .id_salt("sec_heal")
            .default_open(false)
            .show(ui, |ui| {
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
            });
    }
}

impl eframe::App for AutoshopApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_workers(ctx);

        // Global shortcuts. Skip while a widget is focused so the Direction text
        // field keeps its own text editing / undo. Ctrl+Z/Y = undo/redo,
        // Ctrl+O = open, Ctrl+E = export, Ctrl+S = save XMP, ←/→ = walk the
        // gallery — the keyboard grammar of every desktop photo editor.
        if ctx.memory(|m| m.focused()).is_none() {
            let (mut do_undo, mut do_redo, mut do_open, mut do_export, mut do_xmp) =
                (false, false, false, false, false);
            let mut nav: i32 = 0;
            ctx.input_mut(|i| {
                if i.consume_key(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, egui::Key::Z) { do_redo = true; }
                if i.consume_key(egui::Modifiers::COMMAND, egui::Key::Y) { do_redo = true; }
                if i.consume_key(egui::Modifiers::COMMAND, egui::Key::Z) { do_undo = true; }
                if i.consume_key(egui::Modifiers::COMMAND, egui::Key::O) { do_open = true; }
                if i.consume_key(egui::Modifiers::COMMAND, egui::Key::E) { do_export = true; }
                if i.consume_key(egui::Modifiers::COMMAND, egui::Key::S) { do_xmp = true; }
                if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight) { nav = 1; }
                if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft) { nav = -1; }
            });
            if do_undo { self.undo(); }
            if do_redo { self.redo(); }
            if do_open && !self.busy
                && let Some(path) = photo_file_dialog()
            {
                self.selected = None;
                self.open_path(path);
            }
            if do_export && self.src_path.is_some() && !self.busy {
                self.start_export();
            }
            if do_xmp && self.src_path.is_some() && !self.busy {
                self.save_xmp();
            }
            if nav != 0 && !self.busy && !self.gallery.is_empty() {
                let cur = self.selected.map(|i| i as i32).unwrap_or(-nav.min(0));
                let next = (cur + nav).clamp(0, self.gallery.len() as i32 - 1);
                if Some(next as usize) != self.selected {
                    self.open_gallery_index(next as usize);
                }
            }
        }

        // Drag & drop: dropping a photo opens it, a folder opens the library.
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect()
        });
        if let Some(p) = dropped.into_iter().next() {
            if self.busy {
                self.toast(ToastKind::Error, "busy — 等当前任务完成再打开");
            } else if p.is_dir() {
                self.open_folder(p);
            } else if is_photo_path(&p) {
                self.selected = None;
                self.open_path(p);
            } else {
                self.toast(ToastKind::Error, format!("不支持的文件类型: {}", p.display()));
            }
        }

        // Window title mirrors the open photo (send only on change).
        let title = match &self.src_path {
            Some(p) => format!(
                "{} — Autoshop",
                p.file_name().and_then(|s| s.to_str()).unwrap_or("photo")
            ),
            None => "Autoshop".to_string(),
        };
        if title != self.last_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Autoshop");
                ui.separator();
                if ui.button("Open photo…").on_hover_text("Ctrl+O · 或直接拖拽进窗口").clicked()
                    && let Some(path) = photo_file_dialog()
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
                // View mode: side-by-side vs a full-width edit (hold B = compare).
                ui.selectable_value(&mut self.view_mode, ViewMode::SideBySide, "⿲ 对比")
                    .on_hover_text("Before/After side by side");
                ui.selectable_value(&mut self.view_mode, ViewMode::AfterOnly, "⬛ 单图")
                    .on_hover_text("编辑图占满画布；按住 B 快速对比原图");
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
            // Empty state: a real landing surface instead of a blank canvas.
            if self.src_path.is_none() {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.32);
                    ui.heading("Autoshop");
                    ui.label(egui::RichText::new("AI 自动出片 · RAW develop").weak());
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        // Center the button pair by padding half the leftover width.
                        let w = 300.0;
                        ui.add_space((ui.available_width() - w).max(0.0) * 0.5);
                        if ui.button("📷 打开照片…  (Ctrl+O)").clicked()
                            && let Some(p) = photo_file_dialog()
                        {
                            self.open_path(p);
                        }
                        if ui.button("🗂 打开文件夹…").clicked()
                            && let Some(d) = rfd::FileDialog::new().pick_folder()
                        {
                            self.open_folder(d);
                        }
                    });
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new("或把 RAW / 图片直接拖进窗口 · drag & drop anywhere")
                            .weak()
                            .small(),
                    );
                });
                return;
            }

            // Fit BOTH dimensions (max_width alone lets a portrait overflow the
            // panel). The displayed rect is what paint/box-select map against,
            // so sizing here never changes their coordinate math.
            let avail = ui.available_size() - egui::vec2(0.0, 22.0); // room for the caption row
            // Hold B to flash the source in place — the Lightroom compare gesture.
            let comparing = ctx.input(|i| i.key_down(egui::Key::B));

            match self.view_mode {
                ViewMode::SideBySide => {
                    let half = (avail.x - 16.0) * 0.5;
                    let uv = self.view_uv(); // same window for both panes (synced zoom)
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(egui::RichText::new("Before (source)").weak().small());
                            if let Some(t) = &self.before_tex {
                                let size = t.size_vec2();
                                let vis = egui::vec2(uv.width() * size.x, uv.height() * size.y);
                                let disp = fit_in(vis, half, avail.y);
                                let (rect, _) =
                                    ui.allocate_exact_size(disp, egui::Sense::hover());
                                ui.painter_at(rect).image(t.id(), rect, uv, egui::Color32::WHITE);
                            }
                        });
                        ui.separator();
                        ui.vertical(|ui| self.after_view(ui, half, avail.y, comparing));
                    });
                }
                ViewMode::AfterOnly => {
                    ui.vertical(|ui| self.after_view(ui, avail.x, avail.y, comparing));
                }
            }
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

        // Drag & drop affordance: show a full-window overlay while files hover.
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("drop_overlay"),
            ));
            let rect = ctx.screen_rect();
            painter.rect_filled(rect, 0.0, egui::Color32::from_rgba_unmultiplied(16, 36, 72, 150));
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "松开打开 · Drop to open",
                egui::FontId::proportional(28.0),
                egui::Color32::WHITE,
            );
        }

        // Transient toasts (bottom-right). Errors linger longer than successes.
        self.toasts.retain(|t| t.born.elapsed() < t.ttl());
        if !self.toasts.is_empty() {
            egui::Area::new(egui::Id::new("toasts"))
                .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-12.0, -40.0))
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    for t in &self.toasts {
                        let (bg, fg) = match t.kind {
                            ToastKind::Success => (
                                egui::Color32::from_rgb(22, 58, 34),
                                egui::Color32::from_rgb(150, 230, 170),
                            ),
                            ToastKind::Error => (
                                egui::Color32::from_rgb(70, 26, 26),
                                egui::Color32::from_rgb(255, 165, 165),
                            ),
                        };
                        egui::Frame::none()
                            .fill(bg)
                            .rounding(6.0)
                            .inner_margin(egui::Margin::symmetric(10.0, 8.0))
                            .show(ui, |ui| {
                                ui.set_max_width(420.0);
                                ui.label(egui::RichText::new(&t.text).color(fg));
                            });
                        ui.add_space(6.0);
                    }
                });
            // Keep repainting so expiry doesn't wait for the next input event.
            ctx.request_repaint_after(Duration::from_millis(200));
        }

        // Land a finished edit gesture (slider release, AI Analyze, Reset) into
        // the undo history — once per gesture, after all controls are read.
        self.commit_if_settled(ctx);
    }

    /// Persist the prefs (last folder, view mode, export options) — restored by
    /// [`AutoshopApp::new`]. Window geometry is saved by eframe itself.
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(
            storage,
            eframe::APP_KEY,
            &Prefs {
                gallery_dir: self.gallery_dir.clone(),
                style_strength: self.style_strength,
                save_jpeg: self.save_jpeg,
                save_denoise: self.save_denoise,
                view_mode: self.view_mode,
            },
        );
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
            Ok(Box::new(AutoshopApp::new(cc))) // restores prefs + last library
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_curve_point_keeps_inputs_sorted_and_unique() {
        let mut pts = Vec::new();
        insert_curve_point(&mut pts, 128, 140);
        insert_curve_point(&mut pts, 32, 20);
        insert_curve_point(&mut pts, 200, 210);
        assert_eq!(pts.iter().map(|p| p.input).collect::<Vec<_>>(), vec![32, 128, 200]);
        // Same input again → overwrite in place, never a duplicate input.
        let i = insert_curve_point(&mut pts, 128, 100);
        assert_eq!(i, 1);
        assert_eq!(pts.len(), 3);
        assert_eq!(pts[1].output, 100);
    }

    #[test]
    fn drag_curve_point_clamps_strictly_between_neighbours() {
        let mut pts = vec![
            CurvePoint { input: 30, output: 30 },
            CurvePoint { input: 128, output: 128 },
            CurvePoint { input: 200, output: 200 },
        ];
        // Dragging the middle point past its right neighbour stops 1 short.
        drag_curve_point(&mut pts, 1, 240, 250);
        assert_eq!(pts[1].input, 199);
        assert_eq!(pts[1].output, 250);
        // …and past its left neighbour stops 1 above it.
        drag_curve_point(&mut pts, 1, 0, 10);
        assert_eq!(pts[1].input, 31);
        // Endpoints reach the full 0 / 255 range.
        drag_curve_point(&mut pts, 0, 0, 0);
        drag_curve_point(&mut pts, 2, 255, 255);
        assert_eq!((pts[0].input, pts[2].input), (0, 255));
        // Invariant after any sequence: inputs strictly increasing.
        assert!(pts.windows(2).all(|w| w[0].input < w[1].input));
    }

    #[test]
    fn curve_editor_edits_render_identically_to_the_engine() {
        // The editor mutates recipe.*_curve and previews via render::curve_lut —
        // the exact LUT the engine applies. Empty = identity; an anchored lift
        // keeps the ends and raises the anchored midpoint.
        let id = autoshop::render::curve_lut(&[]);
        assert!(id[0].abs() < 1e-6 && (id[255] - 1.0).abs() < 1e-6);
        assert!((id[128] - 128.0 / 255.0).abs() < 1e-3);

        let mut r = EditRecipe::default();
        let pts = curve_points_mut(&mut r, 0);
        insert_curve_point(pts, 0, 0);
        insert_curve_point(pts, 255, 255);
        insert_curve_point(pts, 64, 96); // classic shadow lift between pinned ends
        let lut = autoshop::render::curve_lut(pts);
        assert!(lut[0].abs() < 1e-6 && (lut[255] - 1.0).abs() < 1e-6);
        assert!((lut[64] - 96.0 / 255.0).abs() < 1e-3, "anchored point maps exactly");
        // The channel selector reaches the right recipe field (master only here).
        for ch in 0..4 {
            assert_eq!(curve_points(&r, ch).len(), if ch == 0 { 3 } else { 0 });
        }
    }
}
