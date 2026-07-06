//! The `EditRecipe` is the contract between the *AI advisor* (which decides
//! *what* to do) and the *render engine* (which decides *how* to do it,
//! deterministically). The AI never touches pixels — it only emits one of
//! these. Every field maps to a well-understood, Lightroom/ACR-style develop
//! control so the same recipe can be (a) rendered by our own pipeline or
//! (b) serialised to an XMP sidecar that Lightroom / Camera Raw reads directly.
//!
//! Ranges follow Adobe conventions so the numbers are intuitive and portable:
//!   * sliders such as contrast/highlights/shadows: -100..=100, 0 = no change
//!   * `exposure_ev`: stops of exposure, typically -5.0..=5.0, 0.0 = no change
//!   * `temperature_k`: absolute white-balance target in Kelvin (None = as-shot)

use serde::{Deserialize, Serialize};

/// A complete, self-describing set of develop adjustments for one image.
///
/// `#[serde(default)]` means the AI may omit any field it doesn't want to
/// touch; the omitted control simply stays neutral. This keeps prompts small
/// and makes partial recipes valid.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EditRecipe {
    /// Schema version so we can evolve the contract without silently
    /// misreading old recipes.
    pub version: u32,

    // --- Tone ---------------------------------------------------------------
    /// Global exposure in stops (EV). 0.0 = unchanged.
    pub exposure_ev: f32,
    /// -100..=100
    pub contrast: f32,
    /// -100..=100 (recover blown highlights when negative)
    pub highlights: f32,
    /// -100..=100 (lift shadows when positive)
    pub shadows: f32,
    /// -100..=100 (white point)
    pub whites: f32,
    /// -100..=100 (black point)
    pub blacks: f32,

    // --- White balance ------------------------------------------------------
    /// Absolute colour temperature target in Kelvin. `None` = keep as-shot.
    pub temperature_k: Option<f32>,
    /// Green/magenta tint, -100..=100. 0 = neutral.
    pub tint: f32,

    // --- Colour / presence --------------------------------------------------
    /// -100..=100, protects already-saturated colours.
    pub vibrance: f32,
    /// -100..=100, uniform saturation.
    pub saturation: f32,
    /// -100..=100, local contrast / midtone punch.
    pub clarity: f32,
    /// -100..=100, atmospheric haze removal.
    pub dehaze: f32,

    // --- Per-colour HSL (the 8 ACR colour bands) ----------------------------
    /// Lightroom's HSL / Color mixer. Default = all bands neutral (v1-compatible).
    pub hsl: Hsl,

    // --- Colour grading (3-wheel + global) ----------------------------------
    /// Lightroom's Color Grading wheels. Default = neutral (v1-compatible).
    pub color_grade: ColorGrade,

    // --- Detail -------------------------------------------------------------
    /// 0..=150, capture sharpening amount.
    pub sharpening: f32,
    /// 0..=100, luminance noise reduction.
    pub noise_reduction: f32,

    // --- Geometry (optional) ------------------------------------------------
    /// Clockwise straighten angle in degrees, e.g. -2.5..=2.5 for horizons.
    pub straighten_deg: f32,
    /// Optional crop as normalised [0,1] coordinates of the kept region.
    pub crop: Option<Crop>,

    // --- Free-form tone curve (optional) ------------------------------------
    /// Monotonic control points on the master tone curve, input/output in
    /// 0..=255. Empty = identity curve.
    pub tone_curve: Vec<CurvePoint>,

    /// Per-channel RGB tone curves (red/green/blue), input/output 0..=255. Empty =
    /// identity. The colour-shaping companion to `tone_curve`; emitted as
    /// `crs:ToneCurvePV2012Red/Green/Blue`.
    pub red_curve: Vec<CurvePoint>,
    pub green_curve: Vec<CurvePoint>,
    pub blue_curve: Vec<CurvePoint>,

    // --- Local (masked) adjustments -----------------------------------------
    /// Local adjustments applied through gradient masks. Empty = global-only
    /// (v1-compatible). Emitted as `crs:MaskGroupBasedCorrections` in the XMP
    /// and composited by the render engine.
    pub masks: Vec<LocalAdjustment>,

    // --- Provenance (the AI explains itself) --------------------------------
    /// One or two sentences: why these adjustments, for the user to sanity-check.
    pub rationale: String,
    /// AI self-reported confidence, 0.0..=1.0. Used to gate auto-apply.
    pub confidence: f32,
}

impl Default for EditRecipe {
    fn default() -> Self {
        Self {
            version: 1,
            exposure_ev: 0.0,
            contrast: 0.0,
            highlights: 0.0,
            shadows: 0.0,
            whites: 0.0,
            blacks: 0.0,
            temperature_k: None,
            tint: 0.0,
            vibrance: 0.0,
            saturation: 0.0,
            clarity: 0.0,
            dehaze: 0.0,
            hsl: Hsl::default(),
            color_grade: ColorGrade::default(),
            sharpening: 0.0,
            noise_reduction: 0.0,
            straighten_deg: 0.0,
            crop: None,
            tone_curve: Vec::new(),
            red_curve: Vec::new(),
            green_curve: Vec::new(),
            blue_curve: Vec::new(),
            masks: Vec::new(),
            rationale: String::new(),
            confidence: 0.0,
        }
    }
}

/// Normalised crop rectangle. All values in [0.0, 1.0], with (0,0) at the
/// top-left of the full frame.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Crop {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// A single point on the master tone curve.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CurvePoint {
    /// Input value, 0..=255.
    pub input: u8,
    /// Output value, 0..=255.
    pub output: u8,
}

/// The 8 ACR colour bands, in the order the [`Hsl`] arrays are indexed. These
/// map 1:1 to Lightroom's `crs:{Hue,Saturation,Luminance}AdjustmentRed` … keys.
pub const HSL_BANDS: [&str; 8] =
    ["Red", "Orange", "Yellow", "Green", "Aqua", "Blue", "Purple", "Magenta"];

/// Per-colour HSL adjustments — Lightroom's HSL / Color mixer. Each array is
/// indexed by [`HSL_BANDS`] (red, orange, yellow, green, aqua, blue, purple,
/// magenta). Values -100..=100, 0 = no change: `hue` rotates a band, `saturation`
/// changes its intensity, `luminance` its brightness. This is the single biggest
/// "look" control the global sliders cannot express (e.g. teal-foliage / orange-skin).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Hsl {
    pub hue: [f32; 8],
    pub saturation: [f32; 8],
    pub luminance: [f32; 8],
}

impl Default for Hsl {
    fn default() -> Self {
        Self { hue: [0.0; 8], saturation: [0.0; 8], luminance: [0.0; 8] }
    }
}

impl Hsl {
    /// True when every band on every axis is neutral (lets the render + XMP skip it).
    pub fn is_neutral(&self) -> bool {
        self.hue.iter().chain(&self.saturation).chain(&self.luminance).all(|&v| v == 0.0)
    }

    /// Clamp every band to the documented -100..=100 range.
    pub fn clamp(&mut self) {
        for arr in [&mut self.hue, &mut self.saturation, &mut self.luminance] {
            for v in arr.iter_mut() {
                *v = v.clamp(-100.0, 100.0);
            }
        }
    }
}

/// Lightroom's Color Grading (the 3-wheel + global model that supersedes Split
/// Toning). Each tonal region (shadow/midtone/highlight) plus a global wheel gets
/// a `hue` (0..=360°), `sat` (0..=100), and `lum` (-100..=100). `blending` (0..=100,
/// default 50) sets how much the regions overlap; `balance` (-100..=100) shifts the
/// shadow/highlight split. Default = neutral. ACR XMP convention (verified against
/// the user's own sidecar): shadow/highlight hue+sat round-trip via the legacy
/// `crs:SplitToning*` keys, everything else via `crs:ColorGrade*`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ColorGrade {
    pub shadow_hue: f32,
    pub shadow_sat: f32,
    pub shadow_lum: f32,
    pub midtone_hue: f32,
    pub midtone_sat: f32,
    pub midtone_lum: f32,
    pub highlight_hue: f32,
    pub highlight_sat: f32,
    pub highlight_lum: f32,
    pub global_hue: f32,
    pub global_sat: f32,
    pub global_lum: f32,
    pub blending: f32,
    pub balance: f32,
}

impl Default for ColorGrade {
    fn default() -> Self {
        Self {
            shadow_hue: 0.0, shadow_sat: 0.0, shadow_lum: 0.0,
            midtone_hue: 0.0, midtone_sat: 0.0, midtone_lum: 0.0,
            highlight_hue: 0.0, highlight_sat: 0.0, highlight_lum: 0.0,
            global_hue: 0.0, global_sat: 0.0, global_lum: 0.0,
            blending: 50.0, // ACR default
            balance: 0.0,
        }
    }
}

impl ColorGrade {
    /// True when no wheel tints or lifts (sat + lum all zero) — render + XMP skip it.
    /// `blending`/`balance` alone do nothing without a saturated or lifted wheel.
    pub fn is_neutral(&self) -> bool {
        [
            self.shadow_sat, self.shadow_lum, self.midtone_sat, self.midtone_lum,
            self.highlight_sat, self.highlight_lum, self.global_sat, self.global_lum,
        ]
        .iter()
        .all(|&v| v == 0.0)
    }

    /// Clamp every wheel to its documented range (hue 0..360, sat 0..100, lum/balance
    /// -100..100, blending 0..100).
    pub fn clamp(&mut self) {
        for h in [&mut self.shadow_hue, &mut self.midtone_hue, &mut self.highlight_hue, &mut self.global_hue] {
            *h = h.rem_euclid(360.0);
        }
        for s in [&mut self.shadow_sat, &mut self.midtone_sat, &mut self.highlight_sat, &mut self.global_sat] {
            *s = s.clamp(0.0, 100.0);
        }
        for l in [&mut self.shadow_lum, &mut self.midtone_lum, &mut self.highlight_lum, &mut self.global_lum] {
            *l = l.clamp(-100.0, 100.0);
        }
        self.blending = self.blending.clamp(0.0, 100.0);
        self.balance = self.balance.clamp(-100.0, 100.0);
    }
}

/// A local (masked) adjustment: *where* it applies (`mask`) plus the slider
/// deltas to apply inside that mask. Sliders use the SAME UI scale as the global
/// [`EditRecipe`] fields; the XMP writer converts to ACR's local scale (exposure
/// stops/4, other sliders /100). `temperature` here is a *relative* shift, not
/// Kelvin (maps to `crs:LocalTemperature`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalAdjustment {
    pub mask: MaskGeometry,
    /// Optional Range Mask refinement intersected with `mask` (final weight =
    /// geometry × range). `None` = pure geometry (v1-compatible).
    pub range: Option<RangeMask>,
    /// Human label → `crs:CorrectionName` / `crs:MaskName`.
    pub name: String,
    /// Master opacity 0.0..=1.0 → `crs:CorrectionAmount` + `crs:MaskValue`.
    pub amount: f32,
    /// Invert the mask region → `crs:MaskInverted`.
    pub inverted: bool,
    pub exposure_ev: f32,
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub whites: f32,
    pub blacks: f32,
    pub clarity: f32,
    pub dehaze: f32,
    pub texture: f32,
    pub saturation: f32,
    /// Relative warm/cool shift (NOT Kelvin) → `crs:LocalTemperature`.
    pub temperature: f32,
    pub tint: f32,
    /// Local luminance noise reduction, 0..=100 → `crs:LocalLuminanceNoise`.
    /// For "this region is noisy" requests; smooths only inside the mask.
    pub noise_reduction: f32,
}

impl Default for LocalAdjustment {
    fn default() -> Self {
        Self {
            mask: MaskGeometry::Linear { zero_x: 0.5, zero_y: 0.0, full_x: 0.5, full_y: 0.5 },
            range: None,
            name: String::new(),
            amount: 1.0,
            inverted: false,
            exposure_ev: 0.0,
            contrast: 0.0,
            highlights: 0.0,
            shadows: 0.0,
            whites: 0.0,
            blacks: 0.0,
            clarity: 0.0,
            dehaze: 0.0,
            texture: 0.0,
            saturation: 0.0,
            temperature: 0.0,
            tint: 0.0,
            noise_reduction: 0.0,
        }
    }
}

/// Where a local adjustment applies. Coordinates are normalised to the frame and
/// MAY fall outside [0,1] for gradients (matching ACR's geometry).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MaskGeometry {
    /// Linear gradient — the zero→full vector sets direction + falloff width.
    /// Maps to ACR `What="Mask/Gradient"`.
    Linear { zero_x: f32, zero_y: f32, full_x: f32, full_y: f32 },
    /// Radial/elliptical gradient. Maps to ACR `What="Mask/CircularGradient"`.
    Radial {
        top: f32,
        left: f32,
        bottom: f32,
        right: f32,
        feather: f32,
        roundness: f32,
        flipped: bool,
    },
}

/// Lightroom's Range Mask: a per-pixel refinement INTERSECTED with the mask's
/// geometry (final weight = geometry × range), so a sky gradient can affect only
/// the bright pixels, or only the blues. Serialised to XMP as a second
/// `Mask/RangeMask` component inside `crs:CorrectionMasks` — structure verified
/// against the user's own Lightroom sidecars (e.g. `_DSC9245.xmp` LumRange,
/// `_DSC9303.xmp` PointModels).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RangeMask {
    /// Select by luminance: full weight inside [lo, hi], smooth ramps over
    /// lo_outer→lo and hi→hi_outer. All four in 0..=1, non-decreasing —
    /// exactly ACR's `crs:LumRange="lo_outer lo hi hi_outer"`.
    Luminance { lo_outer: f32, lo: f32, hi: f32, hi_outer: f32 },
    /// Select pixels whose chromaticity (brightness-independent colour) is near
    /// the reference `r,g,b` (0..=1 sRGB). `amount` 0..=1 widens the tolerance
    /// (ACR `crs:ColorAmount`, LR default 0.5). `(px, py)` is the normalised
    /// sample point in the ORIGINAL frame — cosmetic, for LR's sample marker.
    Color { r: f32, g: f32, b: f32, amount: f32, px: f32, py: f32 },
}

// `clamp` is used by the render engine + advisors; `is_noop` is not yet wired to
// a call site (auto-apply gate), so allow it to exist warning-free.
#[allow(dead_code)]
impl EditRecipe {
    /// Clamp every slider into its documented legal range. The AI is
    /// instructed to stay in range, but we never trust the input blindly —
    /// an out-of-range value would otherwise corrupt the render downstream.
    pub fn clamp(&mut self) {
        let c = |v: f32, lo: f32, hi: f32| v.clamp(lo, hi);
        self.exposure_ev = c(self.exposure_ev, -5.0, 5.0);
        self.contrast = c(self.contrast, -100.0, 100.0);
        self.highlights = c(self.highlights, -100.0, 100.0);
        self.shadows = c(self.shadows, -100.0, 100.0);
        self.whites = c(self.whites, -100.0, 100.0);
        self.blacks = c(self.blacks, -100.0, 100.0);
        self.tint = c(self.tint, -100.0, 100.0);
        self.vibrance = c(self.vibrance, -100.0, 100.0);
        self.saturation = c(self.saturation, -100.0, 100.0);
        self.clarity = c(self.clarity, -100.0, 100.0);
        self.dehaze = c(self.dehaze, -100.0, 100.0);
        self.hsl.clamp();
        self.color_grade.clamp();
        self.sharpening = c(self.sharpening, 0.0, 150.0);
        self.noise_reduction = c(self.noise_reduction, 0.0, 100.0);
        self.straighten_deg = c(self.straighten_deg, -45.0, 45.0);
        self.confidence = c(self.confidence, 0.0, 1.0);
        if let Some(k) = self.temperature_k {
            // Ceiling matches the render engine's blackbody fit validity (kelvin_to_rgb
            // is documented + re-clamped to 1000..40000); keep one source of truth so
            // the recipe never carries a Kelvin the engine would silently re-clamp.
            self.temperature_k = Some(c(k, 2000.0, 40000.0));
        }
        // Clamp each local adjustment to the same UI ranges as the globals.
        for m in self.masks.iter_mut() {
            m.amount = m.amount.clamp(0.0, 1.0);
            m.exposure_ev = m.exposure_ev.clamp(-5.0, 5.0);
            for v in [
                &mut m.contrast, &mut m.highlights, &mut m.shadows, &mut m.whites,
                &mut m.blacks, &mut m.clarity, &mut m.dehaze, &mut m.texture,
                &mut m.saturation, &mut m.temperature, &mut m.tint,
            ] {
                *v = (*v).clamp(-100.0, 100.0);
            }
            m.noise_reduction = m.noise_reduction.clamp(0.0, 100.0);
            // Range mask invariants: everything in 0..=1, and the luminance
            // trapezoid non-decreasing (lo_outer ≤ lo ≤ hi ≤ hi_outer) so the
            // render's ramps and ACR's LumRange both stay well-formed.
            match &mut m.range {
                Some(RangeMask::Luminance { lo_outer, lo, hi, hi_outer }) => {
                    let a = lo.clamp(0.0, 1.0);
                    let b = hi.clamp(0.0, 1.0);
                    let (a, b) = if a <= b { (a, b) } else { (b, a) };
                    *lo = a;
                    *hi = b;
                    *lo_outer = lo_outer.clamp(0.0, a);
                    *hi_outer = hi_outer.clamp(b, 1.0);
                }
                Some(RangeMask::Color { r, g, b, amount, px, py }) => {
                    for v in [r, g, b, amount, px, py] {
                        *v = v.clamp(0.0, 1.0);
                    }
                }
                None => {}
            }
        }
    }

    /// Taste guardrail for **AI-proposed** recipes (never manual edits): keep a
    /// finished develop from over-cooking the tone. Two rules:
    ///  1. **Couple highlight recovery to the white point.** Pulling Highlights
    ///     negative without raising Whites drags specular whites (sea foam, clouds,
    ///     sun glints) to grey — so recovery lifts Whites proportionally. This is the
    ///     principled "keep whites white", at the recipe layer (the renderer stays
    ///     faithful; it does not override the recipe).
    ///  2. **Soft-cap over-aggressive tone moves** toward a tasteful ceiling with a
    ///     smooth knee (not a hard clip), so Highlights/Shadows asymptote near ±70
    ///     and Whites/Blacks near ±45 instead of slamming to the ±100 schema bound.
    pub fn temper(&mut self) {
        // Smoothly compress a magnitude past `knee` toward `ceil` (C1-continuous at
        // the knee; asymptotes to `ceil`, so |out| < ceil always). Identity below knee.
        fn soft_cap(v: f32, knee: f32, ceil: f32) -> f32 {
            let a = v.abs();
            if a <= knee {
                return v;
            }
            let span = ceil - knee;
            let excess = a - knee;
            v.signum() * (knee + span * (excess / (excess + span)))
        }
        // Couple recovery to whites BEFORE soft-capping (uses the original strength).
        if self.highlights < 0.0 {
            self.whites = self.whites.max((-self.highlights * 0.3).min(50.0));
        }
        self.highlights = soft_cap(self.highlights, 50.0, 70.0);
        self.shadows = soft_cap(self.shadows, 50.0, 70.0);
        self.whites = soft_cap(self.whites, 30.0, 45.0);
        self.blacks = soft_cap(self.blacks, 30.0, 45.0);
        // Same restraint on each local mask's tone sliders.
        for m in self.masks.iter_mut() {
            if m.highlights < 0.0 {
                m.whites = m.whites.max((-m.highlights * 0.3).min(50.0));
            }
            m.highlights = soft_cap(m.highlights, 50.0, 70.0);
            m.shadows = soft_cap(m.shadows, 50.0, 70.0);
            m.whites = soft_cap(m.whites, 30.0, 45.0);
            m.blacks = soft_cap(m.blacks, 30.0, 45.0);
        }
    }

    /// Returns true if the recipe leaves the image essentially untouched —
    /// useful to detect a "the AI declined to edit" no-op result.
    pub fn is_noop(&self) -> bool {
        *self == EditRecipe::default()
            // ignore provenance fields when judging "did it actually edit?"
            || EditRecipe {
                rationale: String::new(),
                confidence: 0.0,
                ..self.clone()
            } == EditRecipe::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temper_lifts_white_point_and_soft_caps_extremes() {
        // The reported over-cooked recipe: strong −Highlights with low Whites greys
        // the foam. temper() lifts the white point and softens the extremes, WITHOUT
        // touching a modest recipe's committed tone moves.
        let mut hot = EditRecipe { highlights: -78.81, whites: 10.27, shadows: 95.0, ..Default::default() };
        hot.temper();
        assert!(hot.whites >= 23.0, "recovery must lift the white point: whites={}", hot.whites);
        assert!(hot.highlights >= -65.0, "highlights not tempered: {}", hot.highlights);
        assert!(hot.highlights <= -55.0, "highlights over-tempered (lost commitment): {}", hot.highlights);
        assert!(hot.shadows >= 55.0 && hot.shadows < 70.0, "shadows soft-cap off: {}", hot.shadows);

        // A modest recipe keeps its tone moves; recovery still nudges whites a touch.
        let mut mild = EditRecipe { highlights: -30.0, shadows: 20.0, whites: 5.0, ..Default::default() };
        mild.temper();
        assert_eq!(mild.highlights, -30.0, "modest highlights must pass through");
        assert_eq!(mild.shadows, 20.0, "modest shadows must pass through");
        assert!(mild.whites >= 9.0, "modest recovery still protects speculars: {}", mild.whites);
    }

    #[test]
    fn masks_round_trip_and_v1_compatible() {
        // Default has no masks (v1-compatible).
        assert!(EditRecipe::default().masks.is_empty());

        let mut recipe = EditRecipe {
            masks: vec![
                LocalAdjustment {
                    mask: MaskGeometry::Linear { zero_x: 0.5, zero_y: 0.35, full_x: 0.5, full_y: 0.0 },
                    // Luminance range with a deliberately ill-formed trapezoid:
                    // clamp must sort lo/hi and pin the outers around them.
                    range: Some(RangeMask::Luminance { lo_outer: 0.9, lo: 0.8, hi: 0.5, hi_outer: 0.2 }),
                    name: "sky".into(),
                    exposure_ev: -0.4,
                    highlights: -200.0, // out of range → clamp pulls to -100
                    ..Default::default()
                },
                LocalAdjustment {
                    mask: MaskGeometry::Radial {
                        top: 0.3, left: 0.35, bottom: 0.7, right: 0.65,
                        feather: 0.5, roundness: 0.0, flipped: false,
                    },
                    range: Some(RangeMask::Color { r: 0.9, g: 0.6, b: 0.2, amount: 1.7, px: 0.5, py: 0.5 }),
                    name: "subject".into(),
                    shadows: 15.0,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        recipe.clamp();
        assert_eq!(recipe.masks[0].highlights, -100.0); // clamped
        // Luminance trapezoid re-ordered to lo_outer ≤ lo ≤ hi ≤ hi_outer.
        assert_eq!(
            recipe.masks[0].range,
            Some(RangeMask::Luminance { lo_outer: 0.5, lo: 0.5, hi: 0.8, hi_outer: 0.8 })
        );
        // Color amount clamped into 0..=1.
        match recipe.masks[1].range {
            Some(RangeMask::Color { amount, .. }) => assert_eq!(amount, 1.0),
            other => panic!("color range lost in clamp: {other:?}"),
        }

        let json = serde_json::to_string_pretty(&recipe).unwrap();
        let back: EditRecipe = serde_json::from_str(&json).unwrap();
        assert_eq!(recipe, back);
        assert!(!recipe.is_noop()); // masks present ⇒ not a no-op

        // A v1 recipe JSON (no "masks" key) still deserializes, masks default empty.
        let v1 = r#"{ "exposure_ev": 0.5, "rationale": "x", "confidence": 0.9 }"#;
        assert!(serde_json::from_str::<EditRecipe>(v1).unwrap().masks.is_empty());
        // A mask WITHOUT a "range" key (pre-range recipes) defaults to None.
        let old_mask = r#"{ "masks": [ { "name": "sky" } ] }"#;
        assert_eq!(serde_json::from_str::<EditRecipe>(old_mask).unwrap().masks[0].range, None);
    }

    #[test]
    fn round_trips_through_json() {
        let mut recipe = EditRecipe {
            exposure_ev: 0.35,
            contrast: 12.0,
            highlights: -40.0,
            shadows: 25.0,
            temperature_k: Some(5600.0),
            vibrance: 18.0,
            crop: Some(Crop { left: 0.05, top: 0.0, right: 0.95, bottom: 1.0 }),
            tone_curve: vec![
                CurvePoint { input: 0, output: 8 },
                CurvePoint { input: 255, output: 247 },
            ],
            rationale: "Slightly underexposed; recovered sky, lifted shadows.".into(),
            confidence: 0.82,
            ..Default::default()
        };
        recipe.clamp();

        let json = serde_json::to_string_pretty(&recipe).unwrap();
        let back: EditRecipe = serde_json::from_str(&json).unwrap();
        assert_eq!(recipe, back);
    }

    #[test]
    fn omitted_fields_default_to_neutral() {
        // The AI emits only the controls it cares about.
        let json = r#"{ "exposure_ev": 0.5, "rationale": "brighten", "confidence": 0.9 }"#;
        let recipe: EditRecipe = serde_json::from_str(json).unwrap();
        assert_eq!(recipe.exposure_ev, 0.5);
        assert_eq!(recipe.contrast, 0.0); // defaulted
        assert_eq!(recipe.temperature_k, None);
        assert_eq!(recipe.version, 1);
    }

    #[test]
    fn clamp_pulls_out_of_range_values_back() {
        let mut recipe = EditRecipe { contrast: 999.0, exposure_ev: -42.0, ..Default::default() };
        recipe.clamp();
        assert_eq!(recipe.contrast, 100.0);
        assert_eq!(recipe.exposure_ev, -5.0);
    }
}
