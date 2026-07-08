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

use autoshop::recipe::{ColorGrade, CurvePoint, EditRecipe, Hsl, MaskGeometry, RangeMask};
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
/// `serde(default)` so prefs saved by an older build (missing newer keys) still
/// load instead of silently resetting everything.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Prefs {
    gallery_dir: Option<PathBuf>,
    style_strength: f32,
    save_jpeg: bool,
    save_denoise: bool,
    view_mode: ViewMode,
    exp_long_edge: u32,
    exp_sharpen: f32,
    exp_quality: f32,
    exp_space: u8,
    preview_edge: u32,
    show_clipping: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        // Mirror AutoshopApp's own defaults (see its Default impl) so a pref
        // key missing from an older save degrades to exactly the app default.
        Self {
            gallery_dir: None,
            style_strength: 0.30,
            save_jpeg: false,
            save_denoise: false,
            view_mode: ViewMode::SideBySide,
            exp_long_edge: 0,
            exp_sharpen: 0.0,
            exp_quality: 95.0,
            exp_space: 0,
            preview_edge: PREVIEW_EDGE,
            show_clipping: false,
        }
    }
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

/// How a finished retouch enters the variant strip.
#[derive(Clone, Copy, PartialEq)]
enum RetouchKind {
    /// A whole-frame REIMAGINE rendition → a NEW「AI 生成」variant (its look
    /// lives in the pixels).
    NewGenerated,
    /// A fill/heal/clone touch-up of the CURRENT rendition → bake into the
    /// active variant's base AND repoint its `origin` at the saved artifact, so
    /// export / reverse-fit / a further retouch all follow the retouched pixels
    /// (WYSIWYG) instead of the pre-retouch source.
    InPlace,
}

/// A finished retouch from any of the four pixel paths (fill/heal/clone/
/// reimagine): `(preview of the ./out result, status message, the saved
/// full-resolution ./out artifact, kind)`. The saved path becomes the affected
/// variant's `origin` — its export / reverse-fit / next-retouch source.
type RetouchDone = anyhow::Result<(image::DynamicImage, String, PathBuf, RetouchKind)>;

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
    /// A generative-fill / heal / clone / reimagine result — see [`RetouchDone`].
    Retouched(Box<RetouchDone>),
    /// AI segmentation finished: (mask display name, grayscale raster path)
    /// — attached to the recipe as a `MaskGeometry::Bitmap` local mask.
    Segmented(anyhow::Result<(String, PathBuf)>),
    /// Batch render advanced: `done` of `total` photos finished (ok or err).
    BatchProgress { done: usize, total: usize },
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
        // Raster masks have no parametric outline to draw — mark the selection
        // with a badge instead of pretending a shape (rendering the raster as a
        // live translucent overlay is the A② follow-up).
        MaskGeometry::Bitmap { .. } => {
            p.text(
                xf.rect.left_top() + egui::vec2(10.0, 10.0),
                egui::Align2::LEFT_TOP,
                "▨ 位图蒙版",
                egui::FontId::proportional(14.0),
                ACCENT,
            );
        }
    }
}

/// Pick radius (px) for on-image mask knobs — matches the crop handles' feel.
const HANDLE_HIT: f32 = 12.0;

/// Screen-space knob positions for on-image mask editing (geometry given in
/// VIEW space, i.e. already through geom_to_view). Handle ids: 0 = move
/// (linear midpoint / radial centre); linear 1 = zero end, 2 = full end;
/// radial 1..4 = left/top/right/bottom edge midpoints. Bitmap masks carry no
/// parametric knobs (empty).
fn mask_handle_points(geom: &MaskGeometry, xf: ViewXform) -> Vec<(u8, egui::Pos2)> {
    match *geom {
        MaskGeometry::Linear { zero_x, zero_y, full_x, full_y } => {
            let a = xf.to_screen(zero_x, zero_y);
            let b = xf.to_screen(full_x, full_y);
            vec![(0, a + (b - a) * 0.5), (1, a), (2, b)]
        }
        MaskGeometry::Radial { top, left, bottom, right, .. } => {
            let (cx, cy) = ((left + right) / 2.0, (top + bottom) / 2.0);
            vec![
                (0, xf.to_screen(cx, cy)),
                (1, xf.to_screen(left, cy)),
                (2, xf.to_screen(cx, top)),
                (3, xf.to_screen(right, cy)),
                (4, xf.to_screen(cx, bottom)),
            ]
        }
        MaskGeometry::Bitmap { .. } => Vec::new(),
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

/// Drag-reorder bookkeeping: element `from` moves to sit before `insert`
/// (both indices in the PRE-move order; `insert == len` appends). Returns the
/// element's final index plus the remap for every OTHER stored index (e.g.
/// the selection), composed as remove-at-`from` then insert-at-`to`.
/// `insert == from` and `insert == from + 1` are the two no-op drop slots —
/// callers skip the move entirely for those.
fn reorder_move(from: usize, insert: usize) -> (usize, impl Fn(usize) -> usize) {
    let to = if insert > from { insert - 1 } else { insert };
    (to, move |s: usize| {
        if s == from {
            to
        } else {
            let after_rm = if s > from { s - 1 } else { s };
            if after_rm >= to { after_rm + 1 } else { after_rm }
        }
    })
}

// --- geometric coordinate mapping (straighten + distortion) ------------------
// When straighten_deg ≠ 0 or lens_distortion ≠ 0 the After view shows the
// geometrically transformed frame (original → distortion-corrected →
// rotated + auto-cropped, see render.rs's C2 contract), but recipe masks, the
// paint canvas, fill/heal masks and base_preview pixels all live in the
// ORIGINAL frame (the engine applies masks before it remaps; fill/heal edit
// source pixels). These two maps convert between the spaces at the data
// boundaries, sharing the engine's own inscribed_dims / distort_norm formulas
// and rotation convention (clockwise-positive, y-down) so GUI and render can
// never disagree. recipe.crop stays in the view space — the export applies
// the user crop AFTER the geometric chain, so the crop tool needs no mapping.
// All maps are the identity when both controls are zero.

/// View normalized point → original-frame normalized point:
/// un-rotate (view → corrected), then the engine's forward sampling map
/// (corrected → original). Clamped once, at the end.
fn view_norm_to_orig(nx: f32, ny: f32, dims: (f32, f32), deg: f32, dist: f32) -> (f32, f32) {
    let (w, h) = dims;
    let (cx, cy) = if deg == 0.0 {
        (nx, ny)
    } else {
        let (cw, ch) = autoshop::render::inscribed_dims(w, h, deg);
        let rad = deg.to_radians();
        let (s, c) = (rad.sin(), rad.cos());
        let (dx, dy) = ((nx - 0.5) * cw, (ny - 0.5) * ch);
        // Content was rotated clockwise; undo with the counter-clockwise matrix.
        (((c * dx + s * dy) / w) + 0.5, ((-s * dx + c * dy) / h) + 0.5)
    };
    let (ox, oy) = autoshop::render::distort_norm(cx, cy, dims, dist);
    (ox.clamp(0.0, 1.0), oy.clamp(0.0, 1.0))
}

/// Original-frame normalized point → view normalized point: the engine's
/// inverse distortion map (original → corrected), then the forward rotation.
/// NOT clamped: an original point can legitimately fall outside the view
/// window (the painter clips overlays to the image rect anyway; original
/// content a barrel fix crops away comes back just outside the unit square).
fn orig_norm_to_view(nx: f32, ny: f32, dims: (f32, f32), deg: f32, dist: f32) -> (f32, f32) {
    let (nx, ny) = autoshop::render::undistort_norm(nx, ny, dims, dist);
    if deg == 0.0 {
        return (nx, ny);
    }
    let (w, h) = dims;
    let (cw, ch) = autoshop::render::inscribed_dims(w, h, deg);
    let rad = deg.to_radians();
    let (s, c) = (rad.sin(), rad.cos());
    let (dx, dy) = ((nx - 0.5) * w, (ny - 0.5) * h);
    let rx = c * dx - s * dy; // clockwise forward
    let ry = s * dx + c * dy;
    (rx / cw.max(1e-3) + 0.5, ry / ch.max(1e-3) + 0.5)
}

/// A mask geometry mapped from the ORIGINAL frame into the view for on-screen
/// display (identity when straighten and distortion are both zero). Linear /
/// Radial anchor points map exactly; the radial ellipse is shown as the
/// bounding box of its transformed corners — display-only, and tight at the
/// small tilt angles and gentle distortions the sliders allow.
fn geom_to_view(geom: &MaskGeometry, dims: (f32, f32), deg: f32, dist: f32) -> MaskGeometry {
    if deg == 0.0 && dist == 0.0 {
        return geom.clone();
    }
    match *geom {
        // Raster masks carry no parametric anchor points to remap; their
        // overlay is a screen-anchored badge (see draw_mask_overlay).
        MaskGeometry::Bitmap { .. } => geom.clone(),
        MaskGeometry::Linear { zero_x, zero_y, full_x, full_y } => {
            let a = orig_norm_to_view(zero_x, zero_y, dims, deg, dist);
            let b = orig_norm_to_view(full_x, full_y, dims, deg, dist);
            MaskGeometry::Linear { zero_x: a.0, zero_y: a.1, full_x: b.0, full_y: b.1 }
        }
        MaskGeometry::Radial { top, left, bottom, right, feather, roundness, flipped } => {
            let pts = [
                orig_norm_to_view(left, top, dims, deg, dist),
                orig_norm_to_view(right, top, dims, deg, dist),
                orig_norm_to_view(left, bottom, dims, deg, dist),
                orig_norm_to_view(right, bottom, dims, deg, dist),
            ];
            let (mut l, mut t, mut r, mut b) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
            for (x, y) in pts {
                l = l.min(x);
                t = t.min(y);
                r = r.max(x);
                b = b.max(y);
            }
            MaskGeometry::Radial { top: t, left: l, bottom: b, right: r, feather, roundness, flipped }
        }
    }
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
    // The ACTIVE variant's base pixels — what the sliders develop over. Equals
    // `source_preview` for source-based variants (Original / Fitted), or a
    // baked raster for a Generated / retouched variant. Every existing reader
    // ("develop from here", "fit from here", mask sizing) uses this unchanged.
    base_preview: Option<image::DynamicImage>,
    // The pristine source neutral (RAW develop / loaded image), decoded once
    // per open. It is the `None`-base for Original and Fitted variants and the
    // base a reverse-fit maps FROM — kept separate so switching a source-based
    // variant back never re-decodes and a reimagine can't overwrite it.
    source_preview: Option<image::DynamicImage>,
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
    // --- production niceties ---
    view_mode: ViewMode,                   // side-by-side vs after-only (hold B = compare)
    toasts: Vec<Toast>,                    // transient corner notifications
    histogram: Option<Vec<[f32; 4]>>,      // live RGB+luma histogram of the After preview
    last_title: String,                    // window title cache (send only on change)
    // --- diagnostic view layers (UX batch) ---
    show_mask_overlay: bool,               // translucent red coverage of the selected mask (O)
    mask_overlay_tex: Option<egui::TextureHandle>,
    overlay_stale: bool,                   // rebuild the coverage texture next frame
    hover_mask: Option<usize>,             // mask row under the cursor — previews its coverage
    batch_progress: Option<(usize, usize)>, // (done, total) while a batch render runs
    // Cached masks-cleared develop the coverage's range weights are judged
    // on — reused while the global (non-mask) recipe is unchanged, so a
    // mask-slider drag rebuilds only the coverage map, not a second develop.
    overlay_ref: Option<(EditRecipe, image::DynamicImage)>,
    show_clipping: bool,                   // clipping warnings: red blown / blue crushed (J)
    clip_tex: Option<egui::TextureHandle>,
    // --- zoom / pan (per-photo, reset on open) ---
    zoom: f32,                             // 1.0 = fit; up to 12×
    pan: egui::Vec2,                       // visible-window centre in crop-window coords
    // --- crop tool ---
    crop_mode: bool,                       // the crop overlay is active on the After image
    crop_aspect: usize,                    // index into CROP_ASPECTS
    crop_drag: Option<(u8, egui::Pos2, [f32; 4])>, // (handle, drag start, crop at start)
    mask_drag: Option<(u8, (f32, f32))>, // on-image mask knob drag: (handle, last pos in ORIG norm)
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
    // --- colour-range sample (Range Mask) ---
    range_picking: Option<usize>,          // next image click keys masks[i]'s Color range
    // --- clone stamp ---
    clone_mode: bool,                      // brush paints the clone target; Alt+click = source
    clone_src: Option<(f32, f32)>,         // picked source point, original-frame normalized
    clone_fullres: bool,                   // clone on the full-res develop (RAW only)
    // --- variants (版本/变体): parallel renditions of the open photo ---
    // Original + any AI-generated / reverse-fitted versions. The active one
    // drives the sliders, histogram and canvas; switching is lossless (each
    // remembers its own base + recipe), so an AI develop no longer "reverts"
    // when you touch a slider — you're editing that variant's own base.
    variants: Vec<Variant>,
    active: usize,                         // index into `variants` (always valid once a photo is open)
    keep_recipe: bool,                     // one-shot: next Opened keeps recipe/variants (preview-res re-decode)
    // --- export pipeline (gap batch F + D2) ---
    exp_long_edge: u32,                    // resize long edge in px; 0 = full resolution
    exp_sharpen: f32,                      // output sharpening 0..100, post-resize
    exp_quality: f32,                      // JPEG quality 1..100 (f32 for the shared slider)
    exp_space: u8,                         // delivery color space: 0 sRGB / 1 Display P3 / 2 Adobe RGB
    // --- preview resolution (gap batch E) ---
    preview_edge: u32,                     // working-preview long edge: 1280 fluid / 2560 / 4096 detail
    // --- recipe versions (gap batch G): ./out/<stem>.v<N>.recipe.json ---
    versions: Vec<u32>,                    // snapshot numbers found for the open photo (sorted)
}

/// One rendition in the variant strip — a Lightroom-style virtual copy /
/// Capture One variant, NOT a compositing layer (variants never blend; you
/// switch between them losslessly). A variant is fully defined by its base
/// pixels + its develop recipe.
struct Variant {
    kind: VariantKind,
    /// This variant's develop. The ACTIVE variant's recipe is mirrored in
    /// `AutoshopApp::recipe` (the live working copy the sliders edit); it is
    /// saved back here when you switch away.
    recipe: EditRecipe,
    /// Base pixels this variant develops from. `None` ⇒ the shared source
    /// neutral (`AutoshopApp::source_preview`): used by Original and by a
    /// reverse-fit (Fitted re-develops the SAME negative — only the recipe
    /// carries the look). `Some` ⇒ a raster the look is baked into (a
    /// reimagine result, or a fill/heal/clone touch-up), developed on top.
    base: Option<image::DynamicImage>,
    /// The ./out artifact behind a raster variant (the reimagine PNG) — the
    /// reverse-fit target and the full-res export source. `None` for
    /// source-based variants.
    origin: Option<PathBuf>,
    /// Small developed thumbnail for the strip (rebuilt for the active variant
    /// on every develop; built once for the others when created / left).
    thumb: Option<egui::TextureHandle>,
}

#[derive(Clone, Copy, PartialEq)]
enum VariantKind {
    Original,  // 原片 — the loaded RAW / image, your develop
    Generated, // AI 生成 — a whole-frame gpt-image restyle (look in the pixels)
    Fitted,    // 反推 — the generated look solved back into an editable recipe
}

impl VariantKind {
    /// Strip label (icon + name).
    fn label(self) -> &'static str {
        match self {
            VariantKind::Original => "▣ 原片",
            VariantKind::Generated => "✨ AI 生成",
            VariantKind::Fitted => "◭ 反推",
        }
    }
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
            source_preview: None,
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
            view_mode: ViewMode::SideBySide,
            toasts: Vec::new(),
            histogram: None,
            last_title: String::new(),
            zoom: 1.0,
            pan: egui::vec2(0.5, 0.5),
            crop_mode: false,
            crop_aspect: 0,
            crop_drag: None,
            mask_drag: None,
            sel_mask: None,
            placing_mask: None,
            place_start: None,
            curve_channel: 0,
            curve_drag: None,
            multi_sel: HashSet::new(),
            copied: None,
            paste_geometry: false,
            wb_picking: false,
            range_picking: None,
            clone_mode: false,
            clone_src: None,
            clone_fullres: false,
            variants: Vec::new(),
            active: 0,
            keep_recipe: false,
            exp_long_edge: 0,
            exp_sharpen: 0.0,
            exp_quality: 95.0,
            exp_space: 0,
            preview_edge: PREVIEW_EDGE,
            versions: Vec::new(),
            show_mask_overlay: true,
            mask_overlay_tex: None,
            overlay_stale: false,
            overlay_ref: None,
            hover_mask: None,
            batch_progress: None,
            show_clipping: false,
            clip_tex: None,
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
            app.exp_long_edge = prefs.exp_long_edge;
            app.exp_sharpen = prefs.exp_sharpen.clamp(0.0, 100.0);
            app.exp_quality = prefs.exp_quality.clamp(1.0, 100.0);
            app.show_clipping = prefs.show_clipping;
            // Only known color spaces — an out-of-range pref falls back to sRGB.
            if prefs.exp_space <= 2 {
                app.exp_space = prefs.exp_space;
            }
            // Only the known steps — a corrupt pref must not produce a 1-px
            // or 100-MP working preview.
            if [1280, 2560, 4096].contains(&prefs.preview_edge) {
                app.preview_edge = prefs.preview_edge;
            }
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

/// Clipping-warning layer over the DEVELOPED preview (what the export would
/// clip): red where any channel blows out (≥254), blue where all channels
/// crush to black (≤1), transparent elsewhere. Lightroom's J toggle.
fn clipping_overlay(img: &image::DynamicImage) -> egui::ColorImage {
    let rgb = img.to_rgb8();
    let (w, h) = (rgb.width() as usize, rgb.height() as usize);
    let mut rgba = vec![0u8; w * h * 4];
    for (i, p) in rgb.pixels().enumerate() {
        let px = &mut rgba[i * 4..i * 4 + 4];
        if p[0] >= 254 || p[1] >= 254 || p[2] >= 254 {
            px.copy_from_slice(&[255, 40, 40, 255]);
        } else if p[0] <= 1 && p[1] <= 1 && p[2] <= 1 {
            px.copy_from_slice(&[70, 110, 255, 255]);
        }
    }
    egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba)
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
        // Working-preview size is a user choice now (gap batch E): 1280 keeps
        // sliders fluid; 2560/4096 trade tick latency for real 1:1 detail when
        // checking focus / noise.
        let edge = self.preview_edge.clamp(640, 8192);
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
                Ok(full.thumbnail(edge, edge))
            })();
            let _ = tx.send(Msg::Opened(Box::new(res)));
        });
    }

    /// The active variant, if a photo is open.
    fn active_variant(&self) -> Option<&Variant> {
        self.variants.get(self.active)
    }

    /// The reverse-fit / style-prompt target: the ./out PNG behind the active
    /// variant when it is an AI-generated rendition (nothing to fit otherwise).
    /// Reverse-fit maps the SOURCE neutral onto this rendition, so it only
    /// makes sense when the look lives in a generated raster.
    fn fit_target(&self) -> Option<PathBuf> {
        let v = self.active_variant()?;
        (v.kind == VariantKind::Generated).then(|| v.origin.clone()).flatten()
    }

    /// The on-disk PIXEL SOURCE the active variant renders / retouches /
    /// exports FROM. Any variant whose pixels are baked into a ./out raster — a
    /// reimagine (Generated), OR an in-place fill/heal/clone on ANY variant —
    /// carries that full-resolution artifact in `origin` and renders from it;
    /// a pristine source-based variant (原片 / 反推 with no pixel retouch) has
    /// `origin = None` and renders from `src_path` (the RAW / loaded image)
    /// developed by the recipe. Retouch and export key off THIS — not raw
    /// `src_path` — so what exports matches what's on screen (WYSIWYG), never
    /// the untouched negative underneath a generated / retouched rendition.
    fn active_source_path(&self) -> Option<PathBuf> {
        match self.active_variant() {
            Some(v) => v.origin.clone().or_else(|| self.src_path.clone()),
            None => self.src_path.clone(),
        }
    }

    /// Is the active variant an AI-generated raster (look baked into pixels,
    /// not the recipe)? Such a variant has no parametric XMP representation —
    /// exporting a sidecar for it would be a lie; steer the user to 反推 first.
    fn active_is_generated(&self) -> bool {
        self.active_variant().is_some_and(|v| v.kind == VariantKind::Generated)
    }

    /// Make `self.active`'s recipe + base pixels the live working state and
    /// rebuild the before texture. Per-variant transient state (undo history,
    /// local selection, view) restarts — like a soft re-open; what persists is
    /// each variant's recipe + pixels. Shared by switch / push / delete.
    fn load_active(&mut self, ctx: &egui::Context) {
        let Some(v) = self.variants.get(self.active) else { return };
        self.recipe = v.recipe.clone();
        self.rationale = self.recipe.rationale.clone();
        // Base pixels: the variant's own baked raster, else the shared source
        // neutral (Original / Fitted re-develop the same negative).
        let base = v.base.clone().or_else(|| self.source_preview.clone());
        if let Some(base) = base {
            let (mw, mh) = base.dimensions();
            self.before_tex = Some(ctx.load_texture(
                "before",
                to_color_image(&base),
                egui::TextureOptions::LINEAR,
            ));
            self.base_preview = Some(base);
            // A fresh transparent paint mask sized to THIS base (a generated
            // raster and the source neutral can differ in dimensions).
            self.mask_paint = Some(image::RgbaImage::new(mw, mh));
            self.mask_tex = None;
            self.mask_dirty = false;
            self.paint_last = None;
        }
        self.reset_history(); // you can't undo across variants
        self.region = None;
        self.region_drag = None;
        self.sel_mask = None;
        self.overlay_ref = None;
        self.overlay_stale = true;
        self.placing_mask = None;
        self.place_start = None;
        self.paint_mode = false;
        self.clone_mode = false;
        self.clone_src = None;
        self.wb_picking = false;
        self.range_picking = None;
        self.crop_mode = false;
        self.crop_drag = None;
        self.zoom = 1.0;
        self.pan = egui::vec2(0.5, 0.5);
        self.verdict = None;
        self.dirty = true; // re-develop the newly active variant
    }

    /// Switch the active variant losslessly (strip click): in-flight slider
    /// edits are saved back into the variant you leave, then the target's
    /// recipe + pixels become current.
    fn switch_variant(&mut self, idx: usize, ctx: &egui::Context) {
        if idx == self.active || idx >= self.variants.len() || self.busy {
            return;
        }
        if let Some(cur) = self.variants.get_mut(self.active) {
            cur.recipe = self.recipe.clone(); // don't lose the edits in progress
        }
        self.active = idx;
        self.load_active(ctx);
        self.status = format!(
            "已切到「{}」变体 — 各版本独立，切换无损",
            self.variants[self.active].kind.label()
        );
    }

    /// Append a variant and switch to it (its recipe/pixels become live).
    /// Saves the outgoing variant's edits first so nothing is lost.
    fn push_variant(&mut self, v: Variant, ctx: &egui::Context) {
        if let Some(cur) = self.variants.get_mut(self.active) {
            cur.recipe = self.recipe.clone();
        }
        self.variants.push(v);
        self.active = self.variants.len() - 1;
        self.load_active(ctx);
    }

    /// Remove variant `idx` (never the last one — the strip stays non-empty).
    /// Only reloads the working state when the ACTIVE variant's identity moves,
    /// so deleting a background variant can't clobber live edits. Refuses while
    /// busy: an in-flight retouch worker resolves its target by `self.active` at
    /// COMPLETION time, so re-anchoring `active` mid-flight would bake its result
    /// onto the wrong variant.
    fn delete_variant(&mut self, idx: usize, ctx: &egui::Context) {
        if self.busy || self.variants.len() <= 1 || idx >= self.variants.len() {
            return;
        }
        let active_removed = idx == self.active;
        self.variants.remove(idx);
        if self.active > idx {
            self.active -= 1; // same variant, shifted left
        }
        if self.active >= self.variants.len() {
            self.active = self.variants.len() - 1; // removed the tail
        }
        if active_removed {
            self.load_active(ctx); // the active variant changed identity
        }
    }

    /// Recipe snapshot path for version `n` — `./out/<stem>.v<n>.recipe.json`
    /// (gap batch G, ≈ Lightroom virtual copies: cheap parametric versions,
    /// never touching the library or the working `<stem>.recipe.json`).
    fn version_path(src: &std::path::Path, n: u32) -> PathBuf {
        PathBuf::from("out").join(format!("{}.v{n}.recipe.json", autoshop::pipeline::stem(src)))
    }

    /// Rescan ./out for this photo's version snapshots (cached in
    /// `self.versions`; called on photo open and after saving a version — NOT
    /// every frame, ./out can hold thousands of artifacts).
    fn refresh_versions(&mut self) {
        self.versions.clear();
        let Some(src) = self.src_path.as_deref() else { return };
        let prefix = format!("{}.v", autoshop::pipeline::stem(src));
        if let Ok(dir) = std::fs::read_dir("out") {
            for entry in dir.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                if let Some(rest) = name.strip_prefix(&prefix)
                    && let Some(nums) = rest.strip_suffix(".recipe.json")
                    && let Ok(n) = nums.parse::<u32>()
                {
                    self.versions.push(n);
                }
            }
        }
        self.versions.sort_unstable();
    }

    /// Save the CURRENT develop as the next numbered version snapshot.
    fn save_version(&mut self) {
        let Some(src) = self.src_path.clone() else { return };
        let n = self.versions.last().map_or(1, |m| m + 1);
        match autoshop::pipeline::write_recipe(&src, &self.recipe, Some(Self::version_path(&src, n))) {
            Ok(p) => {
                self.refresh_versions();
                self.status = format!("版本 v{n} 已存 → {}", p.display());
            }
            Err(e) => self.status = format!("存版本失败: {e}"),
        }
    }

    /// Load version `n` as the working recipe (one undo step, like AI Analyze).
    fn load_version(&mut self, n: u32) {
        let Some(src) = self.src_path.clone() else { return };
        let p = Self::version_path(&src, n);
        match std::fs::read_to_string(&p)
            .map_err(anyhow::Error::from)
            .and_then(|s| Ok(serde_json::from_str::<EditRecipe>(&s)?))
        {
            Ok(mut r) => {
                r.clamp();
                self.recipe = r;
                self.rationale = self.recipe.rationale.clone();
                self.dirty = true;
                self.status = format!("已载入版本 v{n} — Ctrl+Z 可回到载入前");
            }
            Err(e) => self.status = format!("载入 v{n} 失败: {e}"),
        }
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
    /// The geometric chain (distortion, then straighten) runs through the
    /// engine's own apply_lens_distortion / rotate_straighten so the preview
    /// shows exactly the geometry the export will produce.
    fn redevelop(&mut self, ctx: &egui::Context) {
        if let Some(base) = &self.base_preview {
            let mut after = autoshop::render::develop_preview(base, &self.recipe);
            if self.recipe.lens_distortion != 0.0 {
                after = autoshop::render::apply_lens_distortion(&after, self.recipe.lens_distortion);
            }
            if self.recipe.straighten_deg != 0.0 {
                after = autoshop::render::rotate_straighten(&after, self.recipe.straighten_deg);
            }
            self.histogram = Some(compute_histogram(&after));
            // Clipping warnings read the same developed pixels the export
            // will encode — rebuilt with every develop while enabled.
            self.clip_tex = self.show_clipping.then(|| {
                ctx.load_texture("clip", clipping_overlay(&after), egui::TextureOptions::NEAREST)
            });
            self.after_tex = Some(ctx.load_texture("after", to_color_image(&after), egui::TextureOptions::LINEAR));
            // Keep the active variant's strip thumbnail in step with its
            // develop (the other variants' thumbs were built when last active).
            let thumb = ctx.load_texture(
                "vthumb",
                to_color_image(&after.thumbnail(96, 96)),
                egui::TextureOptions::LINEAR,
            );
            if let Some(v) = self.variants.get_mut(self.active) {
                v.thumb = Some(thumb);
            }
        }
        // Any recipe change can move the selected mask's coverage (geometry,
        // range, amount, straighten, distortion) — refresh the overlay too.
        self.overlay_stale = true;
        self.dirty = false;
    }

    /// (Re)build the translucent red coverage layer for the selected mask —
    /// Lightroom's mask overlay. The map is the ENGINE's own weight math
    /// ([`autoshop::render::mask_coverage`]) evaluated on a masks-cleared
    /// develop (the same reference the range sampler uses), then run through
    /// the same geometric chain as the image so it lands on the same content
    /// in the view. Cleared when the toggle is off or nothing is selected.
    fn refresh_mask_overlay(&mut self, ctx: &egui::Context) {
        self.mask_overlay_tex = None;
        if !self.show_mask_overlay || self.base_preview.is_none() {
            return;
        }
        // A hovered mask-list row previews ITS coverage; otherwise the selection.
        let target = self
            .hover_mask
            .filter(|&i| i < self.recipe.masks.len())
            .or_else(|| self.sel_mask.filter(|&i| i < self.recipe.masks.len()));
        let Some(i) = target else { return };
        let mut pre = self.recipe.clone();
        pre.masks.clear();
        // develop_preview never reads the geometry fields (straighten /
        // distortion / crop are applied by its CALLERS) — neutralise them in
        // the cache key so dragging those sliders doesn't rebuild the
        // reference for nothing. lens_vignette stays: it IS a develop stage.
        pre.straighten_deg = 0.0;
        pre.lens_distortion = 0.0;
        pre.crop = None;
        // Reuse the cached masks-cleared develop while the global recipe is
        // unchanged — a mask-slider drag then rebuilds only the coverage map.
        if !matches!(&self.overlay_ref, Some((r, _)) if *r == pre) {
            let img = {
                let Some(base) = &self.base_preview else { return };
                autoshop::render::develop_preview(base, &pre)
            };
            self.overlay_ref = Some((pre, img));
        }
        let reference = &self.overlay_ref.as_ref().expect("cache filled above").1;
        let mut cov = image::DynamicImage::ImageLuma8(autoshop::render::mask_coverage(
            &self.recipe.masks[i],
            reference,
        ));
        if self.recipe.lens_distortion != 0.0 {
            cov = autoshop::render::apply_lens_distortion(&cov, self.recipe.lens_distortion);
        }
        if self.recipe.straighten_deg != 0.0 {
            cov = autoshop::render::rotate_straighten(&cov, self.recipe.straighten_deg);
        }
        let g = cov.to_luma8();
        let (w, h) = (g.width() as usize, g.height() as usize);
        let mut rgba = vec![0u8; w * h * 4];
        for (i, p) in g.pixels().enumerate() {
            // LR-style red wash, alpha ∝ engine weight (max ≈ 55% opacity).
            rgba[i * 4..i * 4 + 4]
                .copy_from_slice(&[255, 40, 40, (p[0] as u16 * 140 / 255) as u8]);
        }
        self.mask_overlay_tex = Some(ctx.load_texture(
            "mask_overlay",
            egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba),
            egui::TextureOptions::LINEAR,
        ));
    }

    /// The geometric mapping context every interaction boundary needs:
    /// original preview pixel dims + the current straighten angle + the
    /// current manual distortion amount.
    fn geom_ctx(&self) -> ((f32, f32), f32, f32) {
        let dims = self
            .base_preview
            .as_ref()
            .map(|b| {
                let (w, h) = b.dimensions();
                (w as f32, h as f32)
            })
            .unwrap_or((1.0, 1.0));
        (dims, self.recipe.straighten_deg, self.recipe.lens_distortion)
    }

    /// Draw the live histogram (R/G/B filled, luma outline) — the tone readout a
    /// photo editor is expected to have. Sqrt-scaled so shadow detail reads.
    /// The corner triangles are LR's clipping indicators: lit when the extreme
    /// bin holds pixels; clicking either toggles the on-image J overlay.
    fn histogram_ui(&mut self, ui: &mut egui::Ui) {
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

        // Clipping triangles, per-channel (the LR convention): the colour
        // names WHICH channels sit in the extreme bin — one channel reads as
        // that primary, two mix to yellow/magenta/cyan, all three to white
        // (a neutral crush/blow-out vs. a colour cast at a glance). Shadows
        // top-left, highlights top-right; grey when clean; click = the same
        // toggle as ▲ / J.
        let tri_color = |bins: &[f32; 4]| -> Option<egui::Color32> {
            let (r, g, b) = (bins[0] > 0.0, bins[1] > 0.0, bins[2] > 0.0);
            (r || g || b).then(|| {
                let c = |on: bool| if on { 255u8 } else { 45 };
                egui::Color32::from_rgb(c(r), c(g), c(b))
            })
        };
        let chan_names = |bins: &[f32; 4]| -> String {
            ["R", "G", "B"]
                .iter()
                .zip(bins)
                .filter(|(_, v)| **v > 0.0)
                .map(|(n, _)| *n)
                .collect::<Vec<_>>()
                .join("+")
        };
        let mut toggle = false;
        for (right, bins, what) in [
            (false, &hist[0], "阴影死黑"),
            (true, &hist[hist.len() - 1], "高光溢出"),
        ] {
            let s = 10.0;
            let x0 = if right { rect.max.x - s - 4.0 } else { rect.min.x + 4.0 };
            let tri = egui::Rect::from_min_size(egui::pos2(x0, rect.min.y + 4.0), egui::vec2(s, s));
            let lit = tri_color(bins);
            let tip = match lit {
                Some(_) => format!("{}：{} 通道 — 点击切换削波警告 (J)", what, chan_names(bins)),
                None => format!("{}指示（干净）— 点击切换削波警告 (J)", what),
            };
            let resp = ui
                .interact(tri, ui.id().with(("clip_tri", right)), egui::Sense::click())
                .on_hover_text(tip);
            let color = lit.unwrap_or(if self.show_clipping {
                egui::Color32::from_gray(130)
            } else {
                egui::Color32::from_gray(60)
            });
            p.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(tri.center().x, tri.min.y),
                    egui::pos2(tri.max.x, tri.max.y),
                    egui::pos2(tri.min.x, tri.max.y),
                ],
                color,
                egui::Stroke::NONE,
            ));
            if resp.clicked() {
                toggle = true;
            }
        }
        if toggle {
            self.show_clipping = !self.show_clipping;
            self.dirty = true; // the on-image layer is rebuilt inside redevelop
        }
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

    /// `./out/<stem>.developed.{tif|jpg}` — the default export target. The stem
    /// follows the ACTIVE variant's pixel source, so a Generated variant exports
    /// under its reimagine stem (and its AI pixels), not the original's.
    fn default_out(&self) -> PathBuf {
        let src = self.active_source_path();
        let stem = src
            .as_deref()
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("out")
            .to_string();
        let ext = if self.save_jpeg { "jpg" } else { "tif" };
        PathBuf::from("out").join(format!("{stem}.developed.{ext}"))
    }

    /// Render the full-resolution develop to `out` on a worker thread (16-bit
    /// TIFF, or 8-bit JPEG when the path ends in .jpg). Renders the ACTIVE
    /// variant's pixel source (a Generated variant → its full-res reimagine PNG,
    /// developed by the recipe), so what exports matches what's on screen.
    fn start_render_to(&mut self, out: PathBuf) {
        let Some(path) = self.active_source_path() else { return };
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
        let export = self.export_opts();
        std::thread::spawn(move || {
            let res = (|| {
                if let Some(p) = out.parent() {
                    std::fs::create_dir_all(p)?;
                }
                // SCUNet AI denoise (python sidecar) runs before the develop when on.
                let opts = denoise.then(|| {
                    autoshop::denoise::DenoiseOpts::from_config(&autoshop::config::Config::load(), None, 1.0)
                });
                autoshop::render::render_to_file(&path, &recipe, &out, opts.as_ref(), Some(&export))?;
                Ok::<String, anyhow::Error>(out.display().to_string())
            })();
            let _ = tx.send(Msg::Exported(res));
        });
    }

    /// Run the AI segmentation sidecar on the ORIGINAL-frame preview and attach
    /// the resulting raster as a Bitmap local mask (gap batch A②). The AI only
    /// picks WHERE — every actual edit stays a deterministic recipe slider.
    fn start_segment(&mut self, target: &'static str, label: &'static str) {
        if self.busy {
            return;
        }
        let Some(base) = self.base_preview.clone() else { return };
        let Some(src) = self.src_path.clone() else { return };
        self.busy = true;
        self.status = format!("AI {label}分割中…（首次运行会自动下载模型，看控制台日志）");
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let res = (|| -> anyhow::Result<(String, PathBuf)> {
                let cfg = autoshop::config::Config::load();
                let opts = autoshop::segment::SegmentOpts::from_config(&cfg, target);
                // The sidecar sees the ORIGINAL-frame preview — the space recipe
                // masks live in. Preview resolution is enough: the engine samples
                // the raster bilinearly in normalised coords at any render size.
                let mut tmp = std::env::temp_dir();
                tmp.push(format!("autoshop_seg_{}_{target}.png", std::process::id()));
                base.to_rgb8()
                    .save(&tmp)
                    .map_err(|e| anyhow::anyhow!("write segmentation input {}: {e}", tmp.display()))?;
                // One raster per (photo, target): re-running a segmentation
                // refreshes the same file, and the existing mask follows it.
                let mask = autoshop::pipeline::default_out(&src, &format!("mask-{target}"), "png");
                let run = autoshop::segment::segment_file(&opts, &tmp, &mask);
                let _ = std::fs::remove_file(&tmp);
                run?;
                Ok((label.to_string(), mask))
            })();
            let _ = tx.send(Msg::Segmented(res));
        });
    }

    /// The delivery options the export UI currently dials in (gap batch F) —
    /// shared by single export, Download… and batch render.
    fn export_opts(&self) -> autoshop::render::ExportOpts {
        autoshop::render::ExportOpts {
            long_edge: (self.exp_long_edge > 0).then_some(self.exp_long_edge),
            sharpen: self.exp_sharpen.clamp(0.0, 100.0),
            jpeg_quality: self.exp_quality.round().clamp(1.0, 100.0) as u8,
            color_space: match self.exp_space {
                1 => autoshop::render::ExportColorSpace::DisplayP3,
                2 => autoshop::render::ExportColorSpace::AdobeRgb,
                _ => autoshop::render::ExportColorSpace::Srgb,
            },
        }
    }

    /// Batch-render every Ctrl+click-selected photo through its own saved
    /// ./out recipe JSON (falling back to a neutral develop when none exists)
    /// with the current export options — Lightroom's "export selected".
    /// Sequential on one worker: each full-res develop is already multi-second
    /// and memory-heavy (61 MP frames), so parallelism would thrash, not speed
    /// up. AI denoise is deliberately excluded (minutes per photo via the GPU
    /// sidecar — run it per-photo from the export panel instead).
    fn start_batch_render(&mut self) {
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
        let ext = if self.save_jpeg { "jpg" } else { "tif" };
        let export = self.export_opts();
        self.busy = true;
        self.status = format!("批量渲染 {} 张 → ./out …", targets.len());
        self.batch_progress = Some((0, targets.len())); // the top-bar progress bar
        let tx = self.tx.clone();
        let ext = ext.to_string();
        std::thread::spawn(move || {
            let res = (|| {
                let total = targets.len();
                let (mut okn, mut errs) = (0usize, Vec::<String>::new());
                for p in &targets {
                    let one = (|| -> anyhow::Result<()> {
                        let rj = autoshop::pipeline::default_out(p, "recipe", "json");
                        let recipe = if rj.exists() {
                            serde_json::from_str::<EditRecipe>(&std::fs::read_to_string(&rj)?)?
                        } else {
                            EditRecipe::default()
                        };
                        let out = autoshop::pipeline::default_out(p, "developed", &ext);
                        autoshop::pipeline::ensure_parent(&out)?;
                        autoshop::render::render_to_file(p, &recipe, &out, None, Some(&export))?;
                        Ok(())
                    })();
                    match one {
                        Ok(()) => okn += 1,
                        Err(e) => errs.push(format!("{}: {e}", autoshop::pipeline::stem(p))),
                    }
                    let _ = tx.send(Msg::BatchProgress { done: okn + errs.len(), total });
                }
                if errs.is_empty() {
                    Ok(format!("./out — 批量 {okn} 张完成"))
                } else {
                    anyhow::bail!("批量：{okn} 成功、{} 失败：{}", errs.len(), errs.join("; "))
                }
            })();
            let _ = tx.send(Msg::Exported(res));
        });
    }

    fn start_export(&mut self) {
        let out = self.default_out();
        self.start_render_to(out);
    }

    /// Write the Lightroom / Camera-Raw XMP sidecar to ./out (RAW sources only).
    /// An XMP reproduces a look via develop PARAMETERS; a Generated variant's
    /// look lives in its pixels, not the recipe, so there's nothing faithful to
    /// write — steer the user to 反推 (which produces a Fitted variant whose XMP
    /// IS the look). Always keyed to the original RAW `src_path`.
    fn save_xmp(&mut self) {
        if self.active_is_generated() {
            self.status = "生成变体的观感在像素里，没有参数配方可导；先「反推配方」得到可导出的 XMP".into();
            return;
        }
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
                Msg::Opened(boxed) => {
                    // `keep` distinguishes a fresh open from a preview-resolution
                    // re-decode (the px combo): consumed whether the open
                    // succeeds or fails so a failure can't leak it into a later
                    // open.
                    let keep = std::mem::take(&mut self.keep_recipe);
                    match *boxed {
                    Ok(base) => {
                        self.busy = false;
                        if keep {
                            // Preview-resolution re-decode: the SOURCE pixels just
                            // changed resolution — keep the whole variant set,
                            // recipe, undo history and view (zoom included: you
                            // switched to 4096px to inspect 1:1, losing the zoom
                            // would defeat the point). Refresh the base a
                            // source-based active variant develops from; a
                            // baked-raster variant keeps its own pixels.
                            let (mw, mh) = base.dimensions();
                            self.source_preview = Some(base.clone());
                            let active_source =
                                self.active_variant().is_none_or(|v| v.base.is_none());
                            if active_source {
                                self.before_tex = Some(ctx.load_texture(
                                    "before",
                                    to_color_image(&base),
                                    egui::TextureOptions::LINEAR,
                                ));
                                self.mask_paint = Some(image::RgbaImage::new(mw, mh));
                                self.mask_tex = None;
                                self.mask_dirty = false;
                                self.paint_last = None;
                                self.base_preview = Some(base);
                            }
                            self.dirty = true;
                            self.status = format!("预览分辨率 {}px — 已重解码", self.preview_edge);
                        } else {
                            // Fresh open: a single Original variant, neutral
                            // recipe, all per-photo state reset.
                            let (mw, mh) = base.dimensions();
                            self.before_tex = Some(ctx.load_texture(
                                "before",
                                to_color_image(&base),
                                egui::TextureOptions::LINEAR,
                            ));
                            self.source_preview = Some(base.clone());
                            self.base_preview = Some(base);
                            self.recipe = EditRecipe::default();
                            self.variants = vec![Variant {
                                kind: VariantKind::Original,
                                recipe: EditRecipe::default(),
                                base: None,
                                origin: None,
                                thumb: None,
                            }];
                            self.active = 0;
                            // A fresh, fully-transparent paint mask sized to the preview.
                            self.mask_paint = Some(image::RgbaImage::new(mw, mh));
                            self.mask_tex = None;
                            self.mask_dirty = false;
                            self.paint_last = None;
                            self.paint_mode = false;
                            self.reset_history(); // a new photo starts a fresh undo history
                            self.region = None; // and a fresh local-edit selection
                            self.region_drag = None;
                            // View + tool state is per-photo.
                            self.zoom = 1.0;
                            self.pan = egui::vec2(0.5, 0.5);
                            self.crop_mode = false;
                            self.crop_drag = None;
                            self.sel_mask = None;
                            self.overlay_ref = None; // the reference develop belongs to ONE base
                            self.overlay_stale = true;
                            self.placing_mask = None;
                            self.place_start = None;
                            self.curve_drag = None; // curve_channel is a UI pref, keep it
                            self.wb_picking = false;
                            self.range_picking = None;
                            self.clone_mode = false;
                            self.clone_src = None;
                            self.verdict = None;
                            self.rationale.clear();
                            self.refresh_versions(); // version snapshots are per-photo
                            self.dirty = true; // render the (neutral) after
                            self.status = "ready — adjust sliders or run AI Analyze".into();
                        }
                    }
                    Err(e) => {
                        self.fail("could not open", e);
                        if !keep {
                            // A FRESH open failed: open_path already re-pointed
                            // src_path to the file that wouldn't decode, but the
                            // previous photo's variants / pixels are still live —
                            // a mismatch that would misdirect a later fit / heal /
                            // XMP (they key off src_path). Drop to a clean
                            // no-photo state so src_path and the variants can never
                            // disagree. (A preview-res re-decode failure — keep —
                            // leaves the still-open photo untouched.)
                            self.src_path = None;
                            self.variants.clear();
                            self.active = 0;
                            self.base_preview = None;
                            self.source_preview = None;
                            self.before_tex = None;
                            self.after_tex = None;
                            self.selected = None;
                        }
                    }
                }},
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
                    self.batch_progress = None; // the bar belongs to ONE batch run
                    self.done(format!("exported → {p}"));
                }
                Msg::Exported(Err(e)) => {
                    self.batch_progress = None;
                    self.fail("export failed", e);
                }
                Msg::BatchProgress { done, total } => {
                    self.batch_progress = Some((done, total));
                    self.status = format!("批量渲染 {done}/{total} → ./out …");
                }
                Msg::Segmented(res) => match res {
                    Ok((label, path)) => {
                        self.recipe.masks.push(autoshop::recipe::LocalAdjustment {
                            mask: autoshop::recipe::MaskGeometry::Bitmap {
                                path: path.to_string_lossy().into_owned(),
                            },
                            name: label.clone(),
                            ..Default::default()
                        });
                        self.sel_mask = Some(self.recipe.masks.len() - 1);
                        self.dirty = true; // committed-snapshot makes this one undo step
                        self.busy = false;
                        self.status =
                            format!("AI「{label}」蒙版已加入 — 调它的滑杆（曝光/对比/饱和…）即刻生效");
                    }
                    Err(e) => {
                        self.fail("AI 分割失败", e);
                    }
                },
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
                    Ok((img, msg, saved, kind)) => {
                        self.clear_mask();
                        match kind {
                            RetouchKind::NewGenerated => {
                                // Whole-frame reimagine → a NEW「AI 生成」variant
                                // whose base IS this raster. Auto-switch to it, so
                                // editing works on the generated pixels — a slider
                                // no longer reverts to the source develop. Its path
                                // is the reverse-fit / full-res export source.
                                self.push_variant(
                                    Variant {
                                        kind: VariantKind::Generated,
                                        recipe: EditRecipe::default(),
                                        base: Some(img),
                                        origin: Some(saved),
                                        thumb: None,
                                    },
                                    ctx,
                                );
                            }
                            RetouchKind::InPlace => {
                                // fill/heal/clone: a pixel touch-up of the CURRENT
                                // rendition — bake it into the active variant's base
                                // (so later slider edits develop OVER the retouched
                                // pixels) AND repoint its origin at the saved
                                // full-res artifact, so export / reverse-fit / a
                                // further retouch all follow the retouched pixels
                                // rather than the pre-retouch source (WYSIWYG).
                                let (mw, mh) = img.dimensions();
                                if let Some(v) = self.variants.get_mut(self.active) {
                                    v.base = Some(img.clone());
                                    v.origin = Some(saved);
                                }
                                self.before_tex = Some(ctx.load_texture(
                                    "before",
                                    to_color_image(&img),
                                    egui::TextureOptions::LINEAR,
                                ));
                                self.base_preview = Some(img);
                                // Keep the paint canvas sized to the new base (the
                                // retouch result can differ in dimensions — e.g. a
                                // non-square reimagine origin).
                                self.mask_paint = Some(image::RgbaImage::new(mw, mh));
                                self.mask_tex = None;
                                self.mask_dirty = false;
                                self.dirty = true;
                            }
                        }
                        self.done(msg);
                    }
                    Err(e) => {
                        self.fail("retouch failed", e);
                    }
                },
                Msg::Fitted(boxed) => match *boxed {
                    Ok((recipe, note)) => {
                        // The generated look, solved back into an editable recipe,
                        // becomes a NEW「反推」variant: base = the source neutral
                        // (same negative as Original), look carried by the recipe —
                        // so it is fully editable, exports XMP and renders at full
                        // resolution. Auto-switch to it.
                        self.push_variant(
                            Variant {
                                kind: VariantKind::Fitted,
                                recipe,
                                base: None,
                                origin: None,
                                thumb: None,
                            },
                            ctx,
                        );
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
            ui.add_enabled_ui(n > 0 && !self.busy, |ui| {
                if ui
                    .small_button(format!("🖼 渲染选中({n})"))
                    .on_hover_text(
                        "每张按它自己的 ./out 配方出图（没有配方则中性显影）→ \
./out/<名>.developed.*，用当前格式/长边/锐化/质量；AI Denoise 不参与批量",
                    )
                    .clicked()
                {
                    self.start_batch_render();
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
                            self.range_picking = None;
                            self.clone_mode = false;
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

        // --- 裁剪 + 拉直: recipe.crop / straighten_deg (export + XMP paths) ---
        let crop_active = self.recipe.crop.is_some() || self.recipe.straighten_deg != 0.0;
        egui::CollapsingHeader::new(section_title("裁剪 · Crop", crop_active))
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
                            self.range_picking = None;
                            self.clone_mode = false;
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
                // Straighten: rotate + auto-crop (engine rotate_straighten);
                // the preview shows exactly the export geometry.
                changed |= Self::slider(
                    ui,
                    "拉直 Straighten (°)",
                    &mut self.recipe.straighten_deg,
                    -45.0,
                    45.0,
                    0.0,
                );
                ui.label(
                    egui::RichText::new(
                        "进入后在图上拖角柄/移动裁剪框；预览、导出与 XMP 一致生效。拉直自动裁掉黑角。",
                    )
                    .weak()
                    .small(),
                );
            });

        // --- 镜头校正: manual lens corrections (gap batch C) ------------------
        let lens_active = self.recipe.lens_vignette != 0.0 || self.recipe.lens_distortion != 0.0;
        egui::CollapsingHeader::new(section_title("镜头校正 · Lens", lens_active))
            .id_salt("sec_lens")
            .default_open(false)
            .show(ui, |ui| {
                changed |= Self::slider(ui, "暗角补偿 Vignette", &mut self.recipe.lens_vignette, -100.0, 100.0, 0.0);
                changed |= Self::slider(ui, "中点 Midpoint", &mut self.recipe.lens_vignette_mid, 0.0, 100.0, 50.0);
                changed |= Self::slider(ui, "畸变校正 Distortion", &mut self.recipe.lens_distortion, -100.0, 100.0, 0.0);
                ui.label(
                    egui::RichText::new(
                        "暗角：正值提亮四角（补偿失光），负值压暗；线性光域径向增益。畸变：正值修桶形（广角外凸），负值修枕形（长焦内凹）；自动缩放填满画面，蒙版/画笔在校正后的画面上照常定位。预览/导出/XMP 一致。去紫边后续批次。",
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
                    self.range_picking = None;
                    self.clone_mode = false;
                    self.status = "在图上拖拽画出线性渐变（起点不受影响 → 终点完全生效）".into();
                }
                if ui.button("＋ 径向").on_hover_text("在图上拖拽画出椭圆范围").clicked() {
                    self.placing_mask = Some((MaskKind::Radial, None));
                    self.paint_mode = false;
                    self.crop_mode = false;
                    self.wb_picking = false;
                    self.range_picking = None;
                    self.clone_mode = false;
                    self.status = "在图上拖拽画出径向（椭圆）范围".into();
                }
            });
            // --- AI segmentation → bitmap masks (gap batch A②) ---------------
            ui.horizontal(|ui| {
                let can_seg = !self.busy && self.base_preview.is_some();
                if ui
                    .add_enabled(can_seg, egui::Button::new("🤖 AI 选主体"))
                    .on_hover_text(
                        "U²-Net 显著主体分割 → 位图蒙版（python sidecar：pip install rembg，\
                         首次运行自动下载模型到 ~/.u2net）",
                    )
                    .clicked()
                {
                    self.start_segment("subject", "主体");
                }
                if ui
                    .add_enabled(can_seg, egui::Button::new("☁ AI 选天空"))
                    .on_hover_text(
                        "SegFormer-ADE20K 天空分割 → 位图蒙版（python sidecar：pip install \
                         transformers，首次运行自动下载 ~14MB 模型）",
                    )
                    .clicked()
                {
                    self.start_segment("sky", "天空");
                }
            });
            // Mask list: click to select (shows overlay + sliders), 🗑 deletes;
            // HOVERING a row previews that mask's coverage without selecting.
            // hover_mask lives ONE frame: update() takes it each frame and this
            // list re-sets it while the cursor is on a row — so leaving the
            // panel, collapsing this section or switching photos all fall back
            // to the selection with no stale index to chase.
            // Rows are also DRAG SOURCES: drag one over another and release to
            // reorder (order is render semantics — masks stack sequentially).
            // egui clears the payload on release/Esc itself, and while a drag
            // is in flight `hovered()` is false everywhere, so the hover
            // preview pauses instead of churning the coverage overlay.
            let mut delete: Option<usize> = None;
            let mut dropped: Option<(usize, usize)> = None; // (from, insert-before)
            for i in 0..n_masks {
                ui.horizontal(|ui| {
                    let m = &self.recipe.masks[i];
                    let kind = match m.mask {
                        MaskGeometry::Linear { .. } => "线性",
                        MaskGeometry::Radial { .. } => "径向",
                        MaskGeometry::Bitmap { .. } => "位图",
                    };
                    let label = format!("{} · {}", if m.name.is_empty() { "mask" } else { &m.name }, kind);
                    let egui::InnerResponse { inner: row, response: drag } = ui
                        .dnd_drag_source(ui.id().with(("mask_row", i)), i, |ui| {
                            ui.selectable_label(self.sel_mask == Some(i), label)
                        });
                    if row.hovered() {
                        self.hover_mask = Some(i);
                    }
                    if row.clicked() {
                        self.sel_mask = if self.sel_mask == Some(i) { None } else { Some(i) };
                        self.overlay_stale = true; // coverage follows the selection
                    }
                    // A row being dragged over this one: mark the insertion
                    // edge (above/below the midline) and take the drop.
                    if let (Some(from), Some(p)) =
                        (drag.dnd_hover_payload::<usize>(), ui.ctx().pointer_interact_pos())
                    {
                        let below = p.y > drag.rect.center().y;
                        let y = if below { drag.rect.max.y } else { drag.rect.min.y };
                        ui.painter().hline(
                            drag.rect.x_range(),
                            y,
                            egui::Stroke::new(2.0, ui.visuals().selection.bg_fill),
                        );
                        let insert = if below { i + 1 } else { i };
                        if drag.dnd_release_payload::<usize>().is_some() {
                            dropped = Some((*from, insert));
                        }
                    }
                    if ui.small_button("🗑").clicked() {
                        delete = Some(i);
                    }
                });
            }
            if let Some((from, insert)) = dropped
                && insert != from
                && insert != from + 1
            {
                let (to, remap) = reorder_move(from, insert);
                let m = self.recipe.masks.remove(from);
                self.recipe.masks.insert(to, m);
                self.sel_mask = self.sel_mask.map(remap);
                self.overlay_stale = true;
                changed = true;
            }
            if let Some(i) = delete {
                self.recipe.masks.remove(i);
                self.sel_mask = match self.sel_mask {
                    Some(s) if s == i => None,
                    Some(s) if s > i => Some(s - 1),
                    other => other,
                };
                self.overlay_stale = true;
                changed = true;
            }
            // Selected mask: its full slider set.
            if let Some(i) = self.sel_mask.filter(|&i| i < self.recipe.masks.len()) {
                ui.separator();
                ui.horizontal(|ui| {
                    let m = &mut self.recipe.masks[i];
                    ui.add(egui::TextEdit::singleline(&mut m.name).desired_width(110.0).hint_text("名称"));
                    // Raster masks have no drag-to-place geometry — no 重画.
                    let kind = match m.mask {
                        MaskGeometry::Linear { .. } => Some(MaskKind::Linear),
                        MaskGeometry::Radial { .. } => Some(MaskKind::Radial),
                        MaskGeometry::Bitmap { .. } => None,
                    };
                    if let Some(kind) = kind
                        && ui.small_button("↻ 重画").on_hover_text("在图上重新拖拽这个 mask 的范围").clicked()
                    {
                        self.placing_mask = Some((kind, Some(i)));
                        self.paint_mode = false;
                        self.crop_mode = false;
                        self.wb_picking = false;
                        self.range_picking = None;
                        self.clone_mode = false;
                    }
                    if ui
                        .checkbox(&mut self.show_mask_overlay, "叠加")
                        .on_hover_text("红色半透明显示这个蒙版的实际作用范围（几何×范围×强度，快捷键 O）")
                        .changed()
                    {
                        self.overlay_stale = true;
                    }
                    // Mask ORDER is render semantics (masks stack sequentially;
                    // a later mask's range sees earlier masks' output) — so the
                    // list order is editable, not just cosmetic.
                    if ui
                        .add_enabled(i > 0, egui::Button::new("⬆").small())
                        .on_hover_text("上移（更早渲染）")
                        .clicked()
                    {
                        self.recipe.masks.swap(i, i - 1);
                        self.sel_mask = Some(i - 1);
                        self.overlay_stale = true;
                        changed = true;
                    }
                    if ui
                        .add_enabled(i + 1 < self.recipe.masks.len(), egui::Button::new("⬇").small())
                        .on_hover_text("下移（更晚渲染）")
                        .clicked()
                    {
                        self.recipe.masks.swap(i, i + 1);
                        self.sel_mask = Some(i + 1);
                        self.overlay_stale = true;
                        changed = true;
                    }
                    let m = &mut self.recipe.masks[i];
                    ui.checkbox(&mut m.inverted, "反转");
                });
                // --- Range Mask（LR 范围蒙版）: refines WHERE the geometry applies —
                // final weight = geometry × range, live in preview + export + XMP.
                {
                    let cur = match &self.recipe.masks[i].range {
                        None => 0usize,
                        Some(RangeMask::Luminance { .. }) => 1,
                        Some(RangeMask::Color { .. }) => 2,
                    };
                    let mut sel = cur;
                    ui.horizontal(|ui| {
                        ui.label("范围蒙版");
                        egui::ComboBox::from_id_salt("range_kind")
                            .selected_text(["无", "亮度", "颜色"][sel])
                            .width(70.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut sel, 0, "无");
                                ui.selectable_value(&mut sel, 1, "亮度");
                                ui.selectable_value(&mut sel, 2, "颜色");
                            });
                    });
                    if sel != cur {
                        self.recipe.masks[i].range = match sel {
                            // Full range = neutral start; narrow from there.
                            1 => Some(RangeMask::Luminance { lo_outer: 0.0, lo: 0.0, hi: 1.0, hi_outer: 1.0 }),
                            2 => Some(RangeMask::Color { r: 0.5, g: 0.5, b: 0.5, amount: 0.5, px: 0.5, py: 0.5 }),
                            _ => None,
                        };
                        if sel == 2 {
                            // Jump straight into sampling — a colour range without
                            // a picked colour selects nothing useful.
                            self.range_picking = Some(i);
                            self.paint_mode = false;
                            self.crop_mode = false;
                            self.placing_mask = None;
                            self.wb_picking = false;
                            self.clone_mode = false;
                            self.status = "颜色范围：点击图中要选取的颜色".into();
                        }
                        changed = true;
                    }
                    let picking_this = self.range_picking == Some(i);
                    let mut want_pick = false;
                    match &mut self.recipe.masks[i].range {
                        Some(RangeMask::Luminance { lo_outer, lo, hi, hi_outer }) => {
                            // GUI shows lo/hi + one symmetric feather; the recipe keeps
                            // ACR's 4-number trapezoid (asymmetric AI trapezoids show
                            // their averaged feather until a slider is touched).
                            let mut f = ((*lo - *lo_outer) + (*hi_outer - *hi)) * 0.5;
                            let mut ch = false;
                            ch |= Self::slider(ui, "亮度下限", lo, 0.0, 1.0, 0.0);
                            ch |= Self::slider(ui, "亮度上限", hi, 0.0, 1.0, 1.0);
                            ch |= Self::slider(ui, "羽化", &mut f, 0.0, 0.5, 0.1);
                            if ch {
                                if *lo > *hi {
                                    std::mem::swap(lo, hi);
                                }
                                *lo_outer = (*lo - f).max(0.0);
                                *hi_outer = (*hi + f).min(1.0);
                                changed = true;
                            }
                        }
                        Some(RangeMask::Color { r, g, b, amount, .. }) => {
                            ui.horizontal(|ui| {
                                let mut c = [*r, *g, *b];
                                if ui.color_edit_button_rgb(&mut c).changed() {
                                    [*r, *g, *b] = [c[0], c[1], c[2]];
                                    changed = true;
                                }
                                let label = if picking_this { "🎯 点击图中…" } else { "🎯 取样" };
                                if ui.small_button(label).on_hover_text("在图上点击要选取的颜色（亮暗不同的同色也会被选中）").clicked() {
                                    want_pick = true;
                                }
                            });
                            changed |= Self::slider(ui, "容差", amount, 0.0, 1.0, 0.5);
                        }
                        None => {}
                    }
                    if want_pick {
                        self.range_picking = if picking_this { None } else { Some(i) };
                        if self.range_picking.is_some() {
                            self.paint_mode = false;
                            self.crop_mode = false;
                            self.placing_mask = None;
                            self.wb_picking = false;
                            self.clone_mode = false;
                            self.status = "颜色范围：点击图中要选取的颜色".into();
                        }
                    }
                }
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

        // --- 版本: recipe snapshots ≈ LR virtual copies (gap batch G) --------
        let n_ver = self.versions.len();
        egui::CollapsingHeader::new(section_title(&format!("版本 · Versions ({n_ver})"), n_ver > 0))
            .id_salt("sec_versions")
            .default_open(false)
            .show(ui, |ui| {
                if ui
                    .button("＋ 存为版本")
                    .on_hover_text("把当前全部 develop 参数存为一个编号快照（./out/<名>.v<N>.recipe.json），随时可回")
                    .clicked()
                {
                    self.save_version();
                }
                let mut load: Option<u32> = None;
                for &n in &self.versions {
                    ui.horizontal(|ui| {
                        ui.label(format!("v{n}"));
                        if ui.small_button("载入").on_hover_text("替换当前参数（一步 Ctrl+Z 可撤销）").clicked() {
                            load = Some(n);
                        }
                    });
                }
                if let Some(n) = load {
                    self.load_version(n);
                }
                if n_ver == 0 {
                    ui.label(
                        egui::RichText::new("像 LR 虚拟副本：一张照片存多套参数（黑白版/裁剪版…），互不覆盖。")
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
        } else if self.range_picking.is_some() {
            "颜色范围 — 点击图中要选取的颜色"
        } else if self.clone_mode {
            "图章 — Alt+点击取源点 · 拖动涂要覆盖的区域"
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
                if ui
                    .selectable_label(self.show_clipping, "▲")
                    .on_hover_text("削波警告 (J)：红 = 高光溢出，蓝 = 阴影死黑（按导出像素判定）")
                    .clicked()
                {
                    self.show_clipping = !self.show_clipping;
                    self.dirty = true; // the layer is rebuilt inside redevelop
                }
                ui.label(egui::RichText::new(format!("{:.0}%", scale * 100.0)).weak().small());
                // --- preview resolution (gap batch E): 1:1 that actually
                // resolves detail. Switching re-decodes the current photo with
                // the recipe KEPT (the keep_recipe path from batch B).
                let before = self.preview_edge;
                ui.add_enabled_ui(!self.busy, |ui| {
                    egui::ComboBox::from_id_salt("preview_edge")
                        .selected_text(format!("{}px", self.preview_edge))
                        .width(64.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.preview_edge, 1280, "1280px 流畅");
                            ui.selectable_value(&mut self.preview_edge, 2560, "2560px");
                            ui.selectable_value(&mut self.preview_edge, 4096, "4096px 检查");
                        })
                        .response
                        .on_hover_text("工作预览分辨率：1280 滑杆最流畅；2560/4096 供 1:1 查合焦/噪点（每次调整更慢）");
                });
                if self.preview_edge != before
                    && let Some(p) = self.src_path.clone()
                {
                    self.keep_recipe = true; // re-decode, keep the edit
                    self.open_path(p);
                }
            });
        });

        let (rect, resp) = ui.allocate_exact_size(disp, egui::Sense::click_and_drag());
        ui.painter_at(rect).image(id, rect, uv, egui::Color32::WHITE);
        // Diagnostic layers, uv-synced with the image (hidden while comparing):
        // clipping warnings, then the selected mask's coverage on top.
        if !comparing {
            if let Some(t) = &self.clip_tex {
                ui.painter_at(rect).image(t.id(), rect, uv, egui::Color32::WHITE);
            }
            if let Some(t) = &self.mask_overlay_tex {
                ui.painter_at(rect).image(t.id(), rect, uv, egui::Color32::WHITE);
            }
        }
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

        // --- pan: middle-drag, Space + left-drag, or (the LR gesture) a plain
        // left-drag while ZOOMED IN — a zoomed-in drag means "pan" in every
        // photo editor, and requiring Space for it was a top "feels off".
        // Box-select stays reachable while zoomed via Ctrl+drag; an active
        // tool, a mask-knob hover/drag or a box drag in flight all take
        // priority over the implicit pan.
        let space = ui.input(|i| i.key_down(egui::Key::Space));
        let ctrl = ui.input(|i| i.modifiers.command);
        let tool_active = self.crop_mode
            || self.placing_mask.is_some()
            || self.wb_picking
            || self.range_picking.is_some()
            || self.clone_mode
            || self.paint_mode;
        let over_knob = self.mask_drag.is_some()
            || self.sel_mask.and_then(|i| self.recipe.masks.get(i)).is_some_and(|m| {
                let (dims, deg, dist) = self.geom_ctx();
                resp.hover_pos().is_some_and(|p| {
                    mask_handle_points(&geom_to_view(&m.mask, dims, deg, dist), xf)
                        .iter()
                        .any(|(_, hp)| hp.distance(p) <= HANDLE_HIT)
                })
            });
        let zoom_pan = self.zoom > 1.001
            && !tool_active
            && !ctrl
            && !over_knob
            && self.region_drag.is_none();
        let panning = resp.dragged_by(egui::PointerButton::Middle)
            || ((space || zoom_pan) && resp.dragged_by(egui::PointerButton::Primary));
        if panning {
            let d = resp.drag_delta();
            let ext = 1.0 / self.zoom; // visible extent in crop-window coords
            self.pan -= egui::vec2(
                d.x / rect.width().max(1.0) * ext,
                d.y / rect.height().max(1.0) * ext,
            );
        }

        // Cursor language: say what a click/drag would do right now. The pick
        // tools (WB / range / clone-source) set their own crosshair in their
        // handlers; this covers the hand for panning and the drawing tools.
        if panning {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if (space || zoom_pan) && resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        } else if resp.hovered()
            && (self.paint_mode || self.placing_mask.is_some() || self.crop_mode)
        {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
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
        } else if self.range_picking.is_some() {
            self.handle_range_pick(ui, &resp, xf);
        } else if self.clone_mode {
            self.handle_clone(ui, &resp, xf);
            self.ensure_mask_tex(ui.ctx());
            if let Some(t) = &self.mask_tex {
                ui.painter_at(rect).image(t.id(), rect, uv, egui::Color32::WHITE);
            }
        } else if self.paint_mode {
            self.handle_paint(&resp, xf);
            self.ensure_mask_tex(ui.ctx());
            if let Some(t) = &self.mask_tex {
                ui.painter_at(rect).image(t.id(), rect, uv, egui::Color32::WHITE);
            }
        } else {
            // The selected mask's on-image knobs take priority over box-select
            // (a knob hit means "edit the mask", never "start a region").
            if !self.handle_mask_edit(ui, &resp, xf) {
                self.handle_region_select(ui, &resp, xf);
            }
        }

        // Selected mask stays visualised so its sliders have visual feedback
        // (geometry is stored in the original frame → map into the view),
        // with the editing knobs on top (drag = reshape/move, handle_mask_edit).
        if !self.crop_mode
            && self.placing_mask.is_none()
            && let Some(m) = self.sel_mask.and_then(|i| self.recipe.masks.get(i))
        {
            let (dims, deg, dist) = self.geom_ctx();
            let vg = geom_to_view(&m.mask, dims, deg, dist);
            draw_mask_overlay(ui, xf, &vg);
            let p = ui.painter_at(xf.rect);
            for (h, pos) in mask_handle_points(&vg, xf) {
                let r = if h == 0 { 5.5 } else { 4.5 }; // centre knob reads bigger
                p.circle_filled(pos, r, egui::Color32::WHITE);
                p.circle_stroke(pos, r, egui::Stroke::new(1.5, ACCENT));
            }
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
        // base_preview is the ORIGINAL frame — map out of the transformed view.
        let ((bw, bh), deg, dist) = self.geom_ctx();
        let (nx, ny) = view_norm_to_orig(nx, ny, (bw, bh), deg, dist);
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

    /// Colour-range sample: click keys the pending mask's Color range to that
    /// spot. Samples a 5×5 mean of a PRE-MASK develop (this recipe with masks
    /// stripped) — the exact pixel state `apply_masks` evaluates range weights
    /// against, so the picked colour is what the engine will match. One extra
    /// preview-sized develop per click ≈ the cost of one slider tick.
    fn handle_range_pick(&mut self, ui: &egui::Ui, resp: &egui::Response, xf: ViewXform) {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
        if !resp.clicked() {
            return;
        }
        let Some(q) = resp.interact_pointer_pos() else { return };
        let Some(mi) = self.range_picking.filter(|&i| i < self.recipe.masks.len()) else {
            self.range_picking = None; // stale index (mask deleted mid-pick)
            return;
        };
        let (nx, ny) = xf.to_norm(q);
        // develop_preview works in the ORIGINAL frame — map out of the view.
        let ((bw, bh), deg, dist) = self.geom_ctx();
        let (nx, ny) = view_norm_to_orig(nx, ny, (bw, bh), deg, dist);
        let smp = {
            let Some(base) = &self.base_preview else { return };
            let mut pre = self.recipe.clone();
            pre.masks.clear();
            let rgb = autoshop::render::develop_preview(base, &pre).to_rgb8();
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
        // Keep the tolerance the user already dialled in; only re-key the colour.
        let amount = match self.recipe.masks[mi].range {
            Some(RangeMask::Color { amount, .. }) => amount,
            _ => 0.5,
        };
        self.recipe.masks[mi].range =
            Some(RangeMask::Color { r: smp[0], g: smp[1], b: smp[2], amount, px: nx, py: ny });
        self.range_picking = None;
        self.dirty = true;
        self.status = "颜色范围：已取样 — 「容差」滑杆调节选中宽度".into();
    }

    /// Box-select on the After image: drag a rectangle to target a local edit;
    /// the normalized box is folded into the AI direction so it masks exactly
    /// there (mirrors the web region→mask prompt). A plain click — or a tiny
    /// drag — clears the selection. Coordinates are full-frame normalized (the
    /// AI mask space), mapped through the view transform.
    /// On-image editing of the SELECTED mask's geometry (the LR gesture):
    /// drag an end/edge knob to reshape, the centre knob to move the whole
    /// mask — no more redraw-from-scratch via 重画. Geometry lives in the
    /// ORIGINAL frame, so every write maps the pointer back through
    /// view_norm_to_orig — the same chain placement uses. Returns true while
    /// it owns the pointer (hovering a knob or mid-drag) so box-select
    /// doesn't also react; a box-select drag already in flight keeps
    /// priority (else its live rectangle would freeze mid-air whenever the
    /// pointer crossed a knob).
    fn handle_mask_edit(&mut self, ui: &egui::Ui, resp: &egui::Response, xf: ViewXform) -> bool {
        if self.region_drag.is_some() {
            return false;
        }
        let Some(i) = self.sel_mask.filter(|&i| i < self.recipe.masks.len()) else {
            self.mask_drag = None;
            return false;
        };
        let (dims, deg, dist) = self.geom_ctx();
        let view_geom = geom_to_view(&self.recipe.masks[i].mask, dims, deg, dist);
        let handles = mask_handle_points(&view_geom, xf);
        if handles.is_empty() {
            self.mask_drag = None; // bitmap: nothing parametric to drag
            return false;
        }
        let hover_h = resp.hover_pos().and_then(|p| {
            handles.iter().find(|(_, hp)| hp.distance(p) <= HANDLE_HIT).map(|(h, _)| *h)
        });
        let orig_at = |p: egui::Pos2| {
            let (nx, ny) = xf.to_norm(p);
            view_norm_to_orig(nx, ny, dims, deg, dist)
        };
        if resp.drag_started()
            && let (Some(h), Some(p)) = (hover_h, resp.interact_pointer_pos())
        {
            self.mask_drag = Some((h, orig_at(p)));
        }
        if resp.dragged()
            && let (Some((h, last)), Some(p)) = (self.mask_drag, resp.interact_pointer_pos())
        {
            let cur = orig_at(p);
            let (dx, dy) = (cur.0 - last.0, cur.1 - last.1);
            // LR allows geometry to start off-canvas; a generous band keeps
            // knobs recoverable instead of letting them fly to infinity.
            let cl = |v: f32| v.clamp(-0.5, 1.5);
            match &mut self.recipe.masks[i].mask {
                MaskGeometry::Linear { zero_x, zero_y, full_x, full_y } => match h {
                    1 => (*zero_x, *zero_y) = (cl(cur.0), cl(cur.1)),
                    2 => (*full_x, *full_y) = (cl(cur.0), cl(cur.1)),
                    _ => {
                        *zero_x = cl(*zero_x + dx);
                        *zero_y = cl(*zero_y + dy);
                        *full_x = cl(*full_x + dx);
                        *full_y = cl(*full_y + dy);
                    }
                },
                MaskGeometry::Radial { top, left, bottom, right, .. } => {
                    const MIN_SIZE: f32 = 0.01;
                    match h {
                        1 => *left = cl(cur.0).min(*right - MIN_SIZE),
                        2 => *top = cl(cur.1).min(*bottom - MIN_SIZE),
                        3 => *right = cl(cur.0).max(*left + MIN_SIZE),
                        4 => *bottom = cl(cur.1).max(*top + MIN_SIZE),
                        _ => {
                            *left = cl(*left + dx);
                            *right = cl(*right + dx);
                            *top = cl(*top + dy);
                            *bottom = cl(*bottom + dy);
                        }
                    }
                }
                MaskGeometry::Bitmap { .. } => {}
            }
            self.mask_drag = Some((h, cur));
            self.dirty = true; // masks are develop stages — live preview
            self.overlay_stale = true;
        }
        if resp.drag_stopped() {
            self.mask_drag = None; // commit_if_settled turns the drag into ONE undo step
        }
        if self.mask_drag.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if hover_h.is_some() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
        self.mask_drag.is_some() || hover_h.is_some()
    }

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
                // The region feeds the AI's mask prompt — ORIGINAL frame space.
                let ((bw, bh), deg, dist) = self.geom_ctx();
                let map = |p: egui::Pos2| {
                    let (nx, ny) = xf.to_norm(p);
                    view_norm_to_orig(nx, ny, (bw, bh), deg, dist)
                };
                let (sn, en) = (map(s), map(e));
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
            // Stored in the original frame → back into the transformed view.
            let ((bw, bh), deg, dist) = self.geom_ctx();
            let a = orig_norm_to_view(l, t, (bw, bh), deg, dist);
            let b2 = orig_norm_to_view(rr, bb, (bw, bh), deg, dist);
            draw(
                egui::Rect::from_min_max(xf.to_screen(a.0, a.1), xf.to_screen(b2.0, b2.1))
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
        const HIT: f32 = 12.0; // handle pick radius, px — shared by drag + cursor
        let corner_pos = |c: &[f32; 4], k: u8| match k {
            0 => xf.to_screen(c[0], c[1]),
            1 => xf.to_screen(c[2], c[1]),
            2 => xf.to_screen(c[0], c[3]),
            _ => xf.to_screen(c[2], c[3]),
        };
        let pick_handle = |c: &[f32; 4], p: egui::Pos2| {
            (0..4)
                .find(|&k| corner_pos(c, k).distance(p) <= HIT)
                .or_else(|| {
                    let r = egui::Rect::from_min_max(
                        xf.to_screen(c[0], c[1]),
                        xf.to_screen(c[2], c[3]),
                    );
                    r.contains(p).then_some(4)
                })
        };
        if resp.drag_started()
            && let Some(p) = resp.interact_pointer_pos()
            && let Some(h) = pick_handle(&cur, p)
        {
            self.crop_drag = Some((h, p, cur));
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

        // Cursor affordance: name the resize direction of the handle under
        // the pointer — or of the one being DRAGGED, since the pointer can
        // lag off a corner mid-drag. Runs after show_image's generic
        // crosshair set, and cursor_icon is last-write-wins, so this
        // overrides it exactly when a handle would take the drag.
        let c = self
            .recipe
            .crop
            .map(|c| [c.left, c.top, c.right, c.bottom])
            .unwrap_or([0.0, 0.0, 1.0, 1.0]);
        let hover_handle = self
            .crop_drag
            .map(|(h, ..)| h)
            .or_else(|| resp.hover_pos().and_then(|p| pick_handle(&c, p)));
        if let Some(h) = hover_handle {
            ui.ctx().set_cursor_icon(match h {
                0 | 3 => egui::CursorIcon::ResizeNwSe, // TL/BR diagonal
                1 | 2 => egui::CursorIcon::ResizeNeSw, // TR/BL diagonal
                _ => egui::CursorIcon::Move,           // inside: move the window
            });
        }

        // --- overlay: darkened surround + thirds + handles --------------------
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
        // Mask geometry lives in the ORIGINAL frame (the engine composites
        // masks before the geometric remap) — map pointer positions out of the view.
        let (dims, deg, dist) = self.geom_ctx();
        if resp.drag_started()
            && let Some(p) = resp.interact_pointer_pos()
        {
            let (nx, ny) = xf.to_norm(p);
            self.place_start = Some(view_norm_to_orig(nx, ny, dims, deg, dist));
        }
        let Some(s) = self.place_start else { return };
        let Some(p) = resp.interact_pointer_pos() else { return };
        let e = {
            let (nx, ny) = xf.to_norm(p);
            view_norm_to_orig(nx, ny, dims, deg, dist)
        };
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
        draw_mask_overlay(ui, xf, &geom_to_view(&geom, dims, deg, dist)); // live preview
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
        let (dims, deg, dist) = self.geom_ctx();
        let Some(m) = self.mask_paint.as_mut() else { return };
        let (mw, mh) = (m.width() as f32, m.height() as f32);
        // The canvas lives in the ORIGINAL frame (fill/heal edit source
        // pixels), so pointer positions map out of the transformed view.
        let to_mask = |p: egui::Pos2| {
            let (nx, ny) = xf.to_norm(p);
            let (ox, oy) = view_norm_to_orig(nx, ny, dims, deg, dist);
            (ox * mw, oy * mh)
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

    /// Clone-stamp interaction: Alt+click picks the SOURCE point (stored in
    /// the original frame like every pixel-path coordinate); plain drags paint
    /// the target with the shared brush. The picked source stays marked with
    /// a crosshair ring so the offset is always visible.
    fn handle_clone(&mut self, ui: &egui::Ui, resp: &egui::Response, xf: ViewXform) {
        let alt = ui.input(|i| i.modifiers.alt);
        if alt {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
            if resp.clicked()
                && let Some(q) = resp.interact_pointer_pos()
            {
                let (dims, deg, dist) = self.geom_ctx();
                let (nx, ny) = xf.to_norm(q);
                self.clone_src = Some(view_norm_to_orig(nx, ny, dims, deg, dist));
                self.status = "克隆源已取样 — 画笔涂要覆盖的区域，然后「⎘ 克隆已涂区域」".into();
            }
        } else {
            self.handle_paint(resp, xf);
        }
        if let Some((sx, sy)) = self.clone_src {
            let (dims, deg, dist) = self.geom_ctx();
            let (vx, vy) = orig_norm_to_view(sx, sy, dims, deg, dist);
            let q = xf.to_screen(vx, vy);
            let p = ui.painter_at(xf.rect);
            p.circle_stroke(q, 9.0, egui::Stroke::new(2.0, PILL));
            p.line_segment(
                [q - egui::vec2(13.0, 0.0), q + egui::vec2(13.0, 0.0)],
                egui::Stroke::new(1.0, PILL),
            );
            p.line_segment(
                [q - egui::vec2(0.0, 13.0), q + egui::vec2(0.0, 13.0)],
                egui::Stroke::new(1.0, PILL),
            );
        }
    }

    /// Generative fill: regenerate the painted area (gpt-image), composite onto
    /// the source, save to ./out. Runs on a worker thread.
    fn start_fill(&mut self) {
        // Retouch the ACTIVE variant's pixels (a Generated variant → its origin
        // PNG), not the raw negative — otherwise a fill on the AI image would
        // splice in original pixels.
        let Some(path) = self.active_source_path() else { return };
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
        let edge = self.preview_edge.clamp(640, 8192); // bake at the working res, not a fixed 1280
        std::thread::spawn(move || {
            let res = (|| -> RetouchDone {
                let cfg = autoshop::config::Config::load();
                let out = autoshop::pipeline::default_out(&path, "retouch", "png");
                let mask_tmp =
                    std::env::temp_dir().join(format!("autoshop_gui_fill_{}.png", std::process::id()));
                std::fs::write(&mask_tmp, &mask_png)?;
                let r = autoshop::generative::retouch(&cfg, &path, &mask_tmp, &prompt, &quality, full_res, &out);
                let _ = std::fs::remove_file(&mask_tmp);
                r?;
                let img = autoshop::decode::load_image(&out)?.thumbnail(edge, edge);
                // InPlace: refine the current rendition — bake into the active
                // variant's base AND repoint its origin at this saved artifact
                // so export / reverse-fit / next retouch follow the fill.
                Ok((img, format!("filled → {} (更新当前变体)", out.display()), out, RetouchKind::InPlace))
            })();
            let _ = tx.send(Msg::Retouched(Box::new(res)));
        });
    }

    /// Heal: AI auto-detect (use_mask=false) or the painted mask (use_mask=true).
    /// Pixel retouch from surrounding real pixels; saves to ./out.
    fn start_heal(&mut self, use_mask: bool) {
        // Heal the ACTIVE variant's pixels (Generated → its origin PNG).
        let Some(path) = self.active_source_path() else { return };
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
        let edge = self.preview_edge.clamp(640, 8192); // bake at the working res, not a fixed 1280
        std::thread::spawn(move || {
            let res = (|| -> RetouchDone {
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
                let img = autoshop::decode::load_image(&out)?.thumbnail(edge, edge);
                // InPlace: bake into the active variant's base + repoint origin.
                Ok((img, format!("healed {} spot(s) → {}", rep.spots, out.display()), out, RetouchKind::InPlace))
            })();
            let _ = tx.send(Msg::Retouched(Box::new(res)));
        });
    }

    /// Run the clone stamp on a worker: painted target mask + the Alt+picked
    /// source point → `retouch::clone_stamp` (deterministic, no AI) → ./out
    /// pixel master shown in the After pane, exactly like heal.
    fn start_clone(&mut self) {
        // Clone within the ACTIVE variant's pixels (Generated → its origin PNG).
        let Some(path) = self.active_source_path() else { return };
        if self.busy {
            return;
        }
        let Some(src_pt) = self.clone_src else {
            self.status = "先 Alt+点击取克隆源点".into();
            return;
        };
        let Some(mask_png) = self.export_mask_png() else {
            self.status = "先用画笔涂要克隆覆盖的区域".into();
            return;
        };
        self.busy = true;
        self.status = "克隆中…（本地像素运算）".into();
        let tx = self.tx.clone();
        let full_res = self.clone_fullres;
        let edge = self.preview_edge.clamp(640, 8192); // bake at the working res, not a fixed 1280
        std::thread::spawn(move || {
            let res = (|| -> RetouchDone {
                let out = autoshop::pipeline::default_out(&path, "clone", "png");
                let mask_tmp = std::env::temp_dir()
                    .join(format!("autoshop_gui_clone_{}.png", std::process::id()));
                std::fs::write(&mask_tmp, &mask_png)?;
                let rep = autoshop::retouch::clone_stamp(&path, &mask_tmp, src_pt, full_res, &out);
                let _ = std::fs::remove_file(&mask_tmp);
                let rep = rep?;
                let img = autoshop::decode::load_image(&out)?.thumbnail(edge, edge);
                // InPlace: a pixel transplant of the current rendition — bake it
                // into the active variant's base + repoint origin at the artifact.
                Ok((img, format!("克隆 {} 处 → {}", rep.spots, out.display()), out, RetouchKind::InPlace))
            })();
            let _ = tx.send(Msg::Retouched(Box::new(res)));
        });
    }

    /// Full-frame generative re-render via gpt-image — the OPTIONAL "let GPT
    /// directly make the picture" path. Uses the Direction text as the look
    /// prompt. Unlike Analyze (a faithful parametric recipe), this REGENERATES
    /// pixels — a creative restyle (up to ~8 MP on flexible-size models, ~1.5K
    /// on older ones). The result enters the strip as a new「AI 生成」variant
    /// (`origin = Some(out)`); its saved path is the reverse-fit ("反推配方")
    /// target that turns the look back into sliders + XMP at full resolution.
    fn start_reimagine(&mut self) {
        // Always reimagine the ORIGINAL negative (src_path), never a generated
        // variant's pixels — regenerating a rendition is the double-cook path
        // the variant model exists to avoid. Each call gets a UNIQUE ./out PNG
        // so two Generated variants never alias the same origin (which would
        // cross-wire their export / reverse-fit).
        let Some(path) = self.src_path.clone() else { return };
        if self.busy {
            return;
        }
        // First FREE ./out name — probing the filesystem (not a live-variant
        // count) so delete-then-reimagine can't reuse a number whose PNG a
        // surviving variant still points at. Bounded by a hard cap.
        let out = {
            let mut n = 0u32;
            loop {
                let tag = if n == 0 { "reimagine".to_string() } else { format!("reimagine-{}", n + 1) };
                let cand = autoshop::pipeline::default_out(&path, &tag, "png");
                if !cand.exists() || n >= 999 {
                    break cand;
                }
                n += 1;
            }
        };
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
        let edge = self.preview_edge.clamp(640, 8192);
        std::thread::spawn(move || {
            let res = (|| -> RetouchDone {
                let cfg = autoshop::config::Config::load();
                // fidelity "high" keeps it recognisably the same photo.
                autoshop::generative::reimagine(&cfg, &path, &prompt, "high", &cfg.openai_image_quality, &out)?;
                let img = autoshop::decode::load_image(&out)?.thumbnail(edge, edge);
                let msg = format!("已生成「AI 生成」变体 → {} · 可继续微调或「反推配方」", out.display());
                // NewGenerated: a whole-frame rendition → a new Generated variant.
                Ok((img, msg, out, RetouchKind::NewGenerated))
            })();
            let _ = tx.send(Msg::Retouched(Box::new(res)));
        });
    }

    /// Reverse-fit ("match"): statistically solve the develop parameters that map
    /// the SOURCE neutral onto the active「AI 生成」variant — the result lands as
    /// a new「反推」variant (base = source neutral, look in the recipe), and for a
    /// RAW the XMP sidecar is written immediately. Deterministic, no API call.
    ///
    /// The base is `source_preview`, NOT `base_preview`: after a reimagine the
    /// active variant's base IS the generated raster, and fitting a rendition
    /// onto itself would recover ~neutral. Fit must map the negative → the look.
    fn start_fit(&mut self) {
        let (Some(base), Some(tgt)) = (self.source_preview.clone(), self.fit_target())
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
                    "反推完成：look 残差 {:.3}→{:.3} · 已建「反推」变体（可编辑/导 XMP/出全分辨率）",
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
        let (Some(base), Some(tgt)) = (self.source_preview.clone(), self.fit_target())
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

    /// The variant strip (版本条): one card per rendition — 原片 / AI 生成 /
    /// 反推 — with a live developed thumbnail. Click a card to switch (lossless;
    /// each variant keeps its own base + recipe), × to drop one. This is the
    /// selector that makes an AI develop a first-class, non-reverting version.
    fn variant_strip(&mut self, ui: &mut egui::Ui) {
        let mut switch_to: Option<usize> = None;
        let mut delete: Option<usize> = None;
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            ui.label(egui::RichText::new("版本").strong());
            ui.separator();
            egui::ScrollArea::horizontal().show(ui, |ui| {
                ui.horizontal(|ui| {
                    for i in 0..self.variants.len() {
                        let active = i == self.active;
                        let kind = self.variants[i].kind;
                        ui.vertical(|ui| {
                            // Developed thumbnail (or a placeholder until the
                            // variant has been developed once).
                            let resp = if let Some(t) = &self.variants[i].thumb {
                                let s = t.size_vec2();
                                let h = 52.0;
                                let w = (s.x / s.y.max(1.0) * h).clamp(30.0, 104.0);
                                let (rect, resp) =
                                    ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click());
                                let uv = egui::Rect::from_min_max(
                                    egui::pos2(0.0, 0.0),
                                    egui::pos2(1.0, 1.0),
                                );
                                if active {
                                    ui.painter().rect_filled(
                                        rect.expand(3.0),
                                        5.0,
                                        egui::Color32::from_rgba_unmultiplied(0xc9, 0xa1, 0x4a, 46),
                                    );
                                }
                                ui.painter().image(t.id(), rect, uv, egui::Color32::WHITE);
                                if active {
                                    ui.painter().rect_stroke(
                                        rect,
                                        4.0,
                                        egui::Stroke::new(2.0, PILL),
                                    );
                                }
                                resp
                            } else {
                                ui.add_sized([64.0, 52.0], egui::Button::new("…"))
                            };
                            if resp.on_hover_text("点击切到此版本（无损）").clicked() {
                                switch_to = Some(i);
                            }
                            ui.horizontal(|ui| {
                                let label = egui::RichText::new(kind.label()).small();
                                ui.label(if active { label.strong().color(PILL) } else { label });
                                // Any variant except the sole Original can be dropped.
                                if self.variants.len() > 1
                                    && kind != VariantKind::Original
                                    && ui.small_button("×").on_hover_text("删除此版本").clicked()
                                {
                                    delete = Some(i);
                                }
                            });
                        });
                        ui.add_space(6.0);
                    }
                });
            });
        });
        if let Some(i) = switch_to {
            self.switch_variant(i, ui.ctx());
        } else if let Some(i) = delete {
            self.delete_variant(i, ui.ctx());
        }
    }

    fn retouch_panel(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.heading("Retouch");

        // Whole-image generative re-render: let gpt-image DIRECTLY produce the
        // picture (the optional "GPT makes the image" path). Distinct from
        // AI Analyze, which emits a faithful parametric recipe. The result
        // becomes a new「AI 生成」variant in the strip below; the reverse-fit
        // button then closes the loop, adding a「反推」variant whose look lives
        // in an editable recipe (full-res + XMP). No more "continue from
        // master" button — each result is its own selectable variant, so a
        // slider edit can never revert or double-cook it.
        egui::CollapsingHeader::new("整图 AI 生成 · Reimagine")
            .id_salt("sec_reimagine")
            .default_open(true)
            .show(ui, |ui| {
                ui.add_enabled_ui(!self.busy, |ui| {
                    if ui
                        .button("✨ AI 生成出片")
                        .on_hover_text(
                            "用 gpt-image 直接重绘整张图（拿上方 Direction 文本当风格描述）。重绘像素=非保真；\
                             生成后自动加入底部「AI 生成」变体并切过去，可继续微调不会变回去。\
                             支持任意尺寸的模型（gpt-image-2）可达 ~8MP，其余 ~1.5K。需 OPENAI_API_KEY",
                        )
                        .clicked()
                    {
                        self.start_reimagine();
                    }
                });
                // Reverse-fit the active generated variant's look back into an
                // editable recipe — how the low-res experiment becomes a
                // full-res, XMP-able「反推」variant.
                let can_fit = self.fit_target().is_some() && self.source_preview.is_some();
                if !can_fit {
                    ui.label(
                        egui::RichText::new("先「AI 生成出片」并停在该变体上，才能反推它的配方。")
                            .weak()
                            .small(),
                    );
                }
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
            let r = ui
                .checkbox(&mut self.paint_mode, "Paint mask")
                .on_hover_text("Brush over the area; box-select is paused while on. Fill 与 Heal 共用");
            if r.changed() && self.paint_mode {
                self.clone_mode = false; // the stamp has its own paint dispatch
                self.range_picking = None; // and painting cancels a pending colour sample
            }
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

        egui::CollapsingHeader::new("仿制图章 · Clone Stamp")
            .id_salt("sec_clone")
            .default_open(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let label = if self.clone_mode { "✅ 完成" } else { "🖊 进入图章" };
                    if ui.button(label).clicked() {
                        self.clone_mode = !self.clone_mode;
                        if self.clone_mode {
                            // One canvas tool at a time; a fresh target mask.
                            self.paint_mode = false;
                            self.crop_mode = false;
                            self.placing_mask = None;
                            self.wb_picking = false;
                            self.range_picking = None;
                            self.clear_mask();
                            self.status =
                                "图章：Alt+点击取源点 → 画笔涂目标区 → 「⎘ 克隆已涂区域」".into();
                        }
                    }
                    ui.checkbox(&mut self.clone_fullres, "Full-res")
                        .on_hover_text("在全分辨率显影上克隆（慢，仅 RAW）");
                    ui.add_enabled_ui(!self.busy && self.clone_mode, |ui| {
                        if ui.button("⎘ 克隆已涂区域").clicked() {
                            self.start_clone();
                        }
                    });
                });
                ui.label(
                    egui::RichText::new(
                        "Photoshop 的仿制图章：Alt+点击取源（十字标记），画笔涂要覆盖的区域，\
                         按源点原样搬运像素（羽化边缘、不做色调匹配）。本地运算，存 ./out 像素母版。",
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
        // Hover-to-preview is frame-scoped: take last frame's target; the mask
        // list re-sets it below if the cursor is still on a row. The diff is
        // checked right before the overlay refresh at the end of update().
        let hover_prev = self.hover_mask.take();

        // Global shortcuts. Skip while a widget is focused so the Direction text
        // field keeps its own text editing / undo. Ctrl+Z/Y = undo/redo,
        // Ctrl+O = open, Ctrl+E = export, Ctrl+S = save XMP, ←/→ = walk the
        // gallery — the keyboard grammar of every desktop photo editor.
        if ctx.memory(|m| m.focused()).is_none() {
            let (mut do_undo, mut do_redo, mut do_open, mut do_export, mut do_xmp) =
                (false, false, false, false, false);
            let (mut do_escape, mut do_overlay, mut do_clip) = (false, false, false);
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
                if i.consume_key(egui::Modifiers::NONE, egui::Key::Escape) { do_escape = true; }
                if i.consume_key(egui::Modifiers::NONE, egui::Key::O) { do_overlay = true; }
                if i.consume_key(egui::Modifiers::NONE, egui::Key::J) { do_clip = true; }
            });
            if do_undo { self.undo(); }
            if do_redo { self.redo(); }
            // Esc = leave whatever on-image tool is active (the universal
            // editor exit); painted canvases/samples stay for resuming.
            if do_escape {
                let any = self.crop_mode
                    || self.placing_mask.is_some()
                    || self.wb_picking
                    || self.range_picking.is_some()
                    || self.clone_mode
                    || self.paint_mode
                    || self.region_drag.is_some();
                if any {
                    self.crop_mode = false;
                    self.placing_mask = None;
                    self.place_start = None;
                    self.wb_picking = false;
                    self.range_picking = None;
                    self.clone_mode = false;
                    self.paint_mode = false;
                    self.region_drag = None;
                    self.status = "已退出当前工具（Esc）".into();
                }
            }
            if do_overlay {
                self.show_mask_overlay = !self.show_mask_overlay;
                self.overlay_stale = true;
            }
            if do_clip {
                self.show_clipping = !self.show_clipping;
                self.dirty = true; // the layer is rebuilt inside redevelop
            }
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
            // BOTH toolbar rows wrap: a plain horizontal row CLIPS whatever
            // falls past the window edge (the "shrink the window and lose
            // the buttons" bug). horizontal_wrapped only wraps between
            // ATOMIC widget allocations — nested add_enabled_ui scopes get
            // squeezed at the row edge instead of wrapping — so the enabled
            // gating is per-widget (ui.add_enabled) rather than per-group.
            ui.horizontal_wrapped(|ui| {
                ui.heading("Autoshop");
                ui.separator();
                // Live batch-render progress (full-res develops take seconds
                // each — a bare toast at the end reads as a hang).
                if let Some((done, total)) = self.batch_progress {
                    ui.add(
                        egui::ProgressBar::new(done as f32 / total.max(1) as f32)
                            .desired_width(150.0)
                            .text(format!("批量 {done}/{total}")),
                    );
                    ui.separator();
                }
                if ui.button("Open photo…").on_hover_text("Ctrl+O · 或直接拖拽进窗口").clicked()
                    && let Some(path) = photo_file_dialog()
                {
                    self.selected = None; // a one-off file isn't a gallery selection
                    self.open_path(path);
                }
                let ready = self.src_path.is_some() && !self.busy;
                if ui.add_enabled(ready, egui::Button::new("✨ AI Analyze")).clicked() {
                    self.start_analyze();
                }
                ui.add_enabled(ready, egui::Checkbox::new(&mut self.refine, "Refine"))
                    .on_hover_text("Adjust the CURRENT edit instead of proposing from scratch");
                if ui.add_enabled(ready, egui::Button::new("Reset")).clicked() {
                    self.recipe = EditRecipe::default();
                    self.region = None;
                    self.dirty = true;
                }
                ui.separator();
                if ui
                    .add_enabled(ready && !self.undo_stack.is_empty(), egui::Button::new("↶ Undo"))
                    .on_hover_text("Ctrl+Z")
                    .clicked()
                {
                    self.undo();
                }
                if ui
                    .add_enabled(ready && !self.redo_stack.is_empty(), egui::Button::new("↷ Redo"))
                    .on_hover_text("Ctrl+Y")
                    .clicked()
                {
                    self.redo();
                }
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
            // AI direction (free text) + save options. Export SETTINGS
            // (format / size / sharpen / colour space) stay editable with no
            // photo open — they're persisted preferences; only the ACTIONS
            // (Export / Download / Save XMP) gate on a ready photo. That also
            // keeps every widget an atomic allocation so the row can wrap.
            ui.horizontal_wrapped(|ui| {
                ui.label("Direction:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.guidance)
                        .desired_width(340.0)
                        .hint_text("e.g. warmer and moodier, lift the shadows"),
                );
                let ready = self.src_path.is_some() && !self.busy;
                ui.separator();
                egui::ComboBox::from_id_salt("save_fmt")
                    .selected_text(if self.save_jpeg { "JPEG" } else { "16-bit TIFF" })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.save_jpeg, false, "16-bit TIFF");
                        ui.selectable_value(&mut self.save_jpeg, true, "JPEG");
                    });
                // --- delivery pipeline (gap batch F): resize → sharpen → quality ---
                ui.label("长边");
                egui::ComboBox::from_id_salt("exp_long_edge")
                    .selected_text(if self.exp_long_edge == 0 {
                        "原尺寸".to_string()
                    } else {
                        format!("{} px", self.exp_long_edge)
                    })
                    .width(86.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.exp_long_edge, 0, "原尺寸");
                        for px in [1600u32, 2048, 2560, 3840, 5120] {
                            ui.selectable_value(&mut self.exp_long_edge, px, format!("{px} px"));
                        }
                    });
                Self::slider(ui, "输出锐化", &mut self.exp_sharpen, 0.0, 100.0, 0.0);
                if self.save_jpeg {
                    Self::slider(ui, "JPEG 质量", &mut self.exp_quality, 60.0, 100.0, 95.0);
                }
                // --- delivery color space (gap batch D2): a real gamut
                // transform + matching embedded profile, not a tag swap.
                ui.label("色彩空间");
                const SPACES: [&str; 3] = ["sRGB（通用）", "Display P3（广色域屏）", "Adobe RGB（印刷）"];
                egui::ComboBox::from_id_salt("exp_space")
                    .selected_text(SPACES[(self.exp_space as usize).min(2)])
                    .width(170.0)
                    .show_ui(ui, |ui| {
                        for (i, name) in SPACES.iter().enumerate() {
                            ui.selectable_value(&mut self.exp_space, i as u8, *name);
                        }
                    });
                ui.checkbox(&mut self.save_denoise, "AI Denoise").on_hover_text(
                    "SCUNet AI denoise before developing — high-ISO / astro (slow, GPU; needs the python sidecar)",
                );
                if ui.add_enabled(ready, egui::Button::new("Export → ./out")).clicked() {
                    self.start_export();
                }
                if ui.add_enabled(ready, egui::Button::new("Download…")).clicked() {
                    let ext = if self.save_jpeg { "jpg" } else { "tif" };
                    // Suggest a name from the ACTIVE variant's pixel source (a
                    // Generated variant → its reimagine stem), matching what
                    // Export → ./out writes; the rendered pixels already follow it.
                    let src = self.active_source_path();
                    let stem = src
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
                if ui.add_enabled(ready, egui::Button::new("Save XMP")).clicked() {
                    self.save_xmp();
                }
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

        // Variant strip — sits directly above the status bar (registered after
        // it so it stacks on top), only when a photo is open. The selector for
        // 原片 / AI 生成 / 反推 renditions.
        if self.src_path.is_some() {
            egui::TopBottomPanel::bottom("variants")
                .exact_height(96.0)
                .show(ctx, |ui| {
                    self.variant_strip(ui);
                });
        }

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
        // The mask coverage overlay follows develop / selection / toggle /
        // hover (a changed hover target includes "left the list entirely").
        if self.hover_mask != hover_prev {
            self.overlay_stale = true;
        }
        if std::mem::take(&mut self.overlay_stale) {
            self.refresh_mask_overlay(ctx);
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
                exp_long_edge: self.exp_long_edge,
                exp_sharpen: self.exp_sharpen,
                exp_quality: self.exp_quality,
                exp_space: self.exp_space,
                preview_edge: self.preview_edge,
                show_clipping: self.show_clipping,
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

/// A unified visual theme: warm-gold accent (shared with the variant-strip
/// highlight), rounder widgets, a little more breathing room, and headings a
/// step down from egui's chunky default so section titles group instead of
/// shouting. One tasteful pass over egui's dark base — not a full reskin.
fn install_theme(ctx: &egui::Context) {
    use egui::{FontFamily, FontId, Rounding, Stroke, TextStyle};
    let mut style = (*ctx.style()).clone();
    style.visuals.selection.bg_fill =
        egui::Color32::from_rgba_unmultiplied(0xc9, 0xa1, 0x4a, 90);
    style.visuals.selection.stroke = Stroke::new(1.0, PILL);
    style.visuals.hyperlink_color = PILL;
    let rounding = Rounding::same(5.0);
    for w in [
        &mut style.visuals.widgets.noninteractive,
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
        &mut style.visuals.widgets.open,
    ] {
        w.rounding = rounding;
    }
    style.visuals.window_rounding = Rounding::same(8.0);
    style.visuals.menu_rounding = Rounding::same(6.0);
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(8.0, 4.0);
    style.spacing.interact_size.y = 24.0;
    style
        .text_styles
        .insert(TextStyle::Heading, FontId::new(17.0, FontFamily::Proportional));
    ctx.set_style(style);
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
            // Below this the wrapped toolbar rows + two side panels leave no
            // usable canvas; wrapping (not clipping) covers everything above.
            .with_min_inner_size([980.0, 620.0])
            .with_title("Autoshop")
            .with_icon(std::sync::Arc::new(app_icon())),
        ..Default::default()
    };
    eframe::run_native(
        "Autoshop",
        opts,
        Box::new(|cc| {
            install_cjk_font(&cc.egui_ctx); // CJK glyphs so Chinese labels aren't tofu
            install_theme(&cc.egui_ctx); // unified accent / spacing / rounding pass
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
    fn geometric_view_mapping_roundtrips() {
        // The two boundary maps must be exact inverses (they share the
        // engine's inscribed_dims / distort_norm formulas), and all-zero
        // controls must be the identity so no existing flow changes.
        // Interior points only: originals near the frame edge can be
        // legitimately cropped away by a strong barrel fix (no preimage).
        let dims = (1280.0, 853.0);
        assert_eq!(view_norm_to_orig(0.31, 0.77, dims, 0.0, 0.0), (0.31, 0.77));
        for deg in [0.0f32, 2.5, -7.0, 12.0] {
            for dist in [0.0f32, 60.0, -60.0, 100.0] {
                for (nx, ny) in [(0.5, 0.5), (0.35, 0.65), (0.6, 0.42)] {
                    let (vx, vy) = orig_norm_to_view(nx, ny, dims, deg, dist);
                    let (ox, oy) = view_norm_to_orig(vx, vy, dims, deg, dist);
                    assert!(
                        (ox - nx).abs() < 2e-3 && (oy - ny).abs() < 2e-3,
                        "deg {deg} dist {dist}: ({nx},{ny}) → view ({vx},{vy}) → back ({ox},{oy})"
                    );
                }
                // The centre is a fixed point at any angle + distortion.
                let (cx, cy) = view_norm_to_orig(0.5, 0.5, dims, deg, dist);
                assert!((cx - 0.5).abs() < 1e-4 && (cy - 0.5).abs() < 1e-4);
            }
        }
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

    #[test]
    fn reorder_move_remap_matches_actual_remove_insert() {
        // The remap returned by reorder_move must agree with what physically
        // happens to a vec under remove(from) + insert(to) — for EVERY
        // element, every (from, insert) pair, including the append slot
        // (insert == len). The two no-op slots are the caller's guard.
        for len in 1..=5usize {
            for from in 0..len {
                for insert in 0..=len {
                    if insert == from || insert == from + 1 {
                        continue; // no-op drop slots, skipped by the GUI
                    }
                    let mut v: Vec<usize> = (0..len).collect();
                    let (to, remap) = reorder_move(from, insert);
                    let m = v.remove(from);
                    v.insert(to, m);
                    for orig in 0..len {
                        let now = v.iter().position(|&x| x == orig).unwrap();
                        assert_eq!(
                            remap(orig),
                            now,
                            "len {len} from {from} insert {insert}: element {orig}"
                        );
                    }
                }
            }
        }
    }
}
