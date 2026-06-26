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
            sharpening: 0.0,
            noise_reduction: 0.0,
            straighten_deg: 0.0,
            crop: None,
            tone_curve: Vec::new(),
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

/// A local (masked) adjustment: *where* it applies (`mask`) plus the slider
/// deltas to apply inside that mask. Sliders use the SAME UI scale as the global
/// [`EditRecipe`] fields; the XMP writer converts to ACR's local scale (exposure
/// stops/4, other sliders /100). `temperature` here is a *relative* shift, not
/// Kelvin (maps to `crs:LocalTemperature`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalAdjustment {
    pub mask: MaskGeometry,
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
}

impl Default for LocalAdjustment {
    fn default() -> Self {
        Self {
            mask: MaskGeometry::Linear { zero_x: 0.5, zero_y: 0.0, full_x: 0.5, full_y: 0.5 },
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
        self.sharpening = c(self.sharpening, 0.0, 150.0);
        self.noise_reduction = c(self.noise_reduction, 0.0, 100.0);
        self.straighten_deg = c(self.straighten_deg, -45.0, 45.0);
        self.confidence = c(self.confidence, 0.0, 1.0);
        if let Some(k) = self.temperature_k {
            self.temperature_k = Some(c(k, 2000.0, 50000.0));
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
    fn masks_round_trip_and_v1_compatible() {
        // Default has no masks (v1-compatible).
        assert!(EditRecipe::default().masks.is_empty());

        let mut recipe = EditRecipe {
            masks: vec![
                LocalAdjustment {
                    mask: MaskGeometry::Linear { zero_x: 0.5, zero_y: 0.35, full_x: 0.5, full_y: 0.0 },
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
                    name: "subject".into(),
                    shadows: 15.0,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        recipe.clamp();
        assert_eq!(recipe.masks[0].highlights, -100.0); // clamped

        let json = serde_json::to_string_pretty(&recipe).unwrap();
        let back: EditRecipe = serde_json::from_str(&json).unwrap();
        assert_eq!(recipe, back);
        assert!(!recipe.is_noop()); // masks present ⇒ not a no-op

        // A v1 recipe JSON (no "masks" key) still deserializes, masks default empty.
        let v1 = r#"{ "exposure_ev": 0.5, "rationale": "x", "confidence": 0.9 }"#;
        assert!(serde_json::from_str::<EditRecipe>(v1).unwrap().masks.is_empty());
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
