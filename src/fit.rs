//! Reverse-fit ("match") — derive an editable [`EditRecipe`] from a LOOK.
//!
//! Given the same shot twice — the untouched source preview and a target
//! rendition of it (the gpt-image `reimagine` output, or any finished reference
//! of the SAME frame) — solve for the develop parameters that reproduce the
//! target's tonality and colour through OUR deterministic engine. No pixels are
//! copied: the output is sliders + curves, so it applies at full sensor
//! resolution and serialises to a Lightroom XMP sidecar. This is how a low-res
//! generative experiment becomes a real, adjustable, full-resolution develop.
//!
//! Method: STATISTICS, not per-pixel regression — a generative target is NOT
//! pixel-aligned with the source (the model re-renders the frame), so only
//! distribution-level evidence is trustworthy.
//!
//!   1. **Tone** — luminance CDF matching gives a monotone map `M`; sample it at
//!      the engine's own tone knots ([`render::TONE_KNOTS_X`]) and least-squares
//!      solve the sliders against the engine's OWN basis
//!      ([`render::tone_slider_basis`]), scanning exposure (it enters the model
//!      nonlinearly). The solve carries a REAL magnitude prior (ridge +
//!      penalised model selection): the knot system is near-collinear, so
//!      without it grotesque mutually-cancelling combos (Exposure +1.5 with
//!      Contrast −97 and Shadows −100) beat tasteful ones by numerical ε —
//!      the residual curve makes their total maps indistinguishable, but the
//!      slider semantics are ruined (real-photo failure, 2026-07-07). Whatever
//!      shape the penalised sliders don't express goes into `tone_curve`
//!      control points, which the engine composes exactly on top.
//!   2. **Saturation** — global mean-chroma ratio, secant-refined through real
//!      [`render::develop_preview`] renders (closed loop, not open-loop math).
//!   3. **Colour cast** — per-channel CDF residuals → red/green/blue curves,
//!      last as the catch-all (cast-before-saturation measured worse on the
//!      haze regression — see the stage comments in [`fit_recipe`]). Accepted
//!      only through TWO gates: the aggregate look-error ratio AND the
//!      foreign-hue veto (the curves must not paint a region of the frame in
//!      hues the target holds nowhere — the 2026-07-09 violet-sky failure was
//!      cross-band invisible to the aggregate; see the veto const block).
//!
//! There is deliberately NO per-band HSL stage — per-band statistics against
//! a non-pixel-aligned generative target conflate content with style and are
//! unidentifiable (see the note in [`fit_recipe`]; it caused the 2026-07-07
//! purple-sky failure).
//!
//! Every stage fits the RESIDUAL against a fresh render of the current recipe,
//! so stage interactions are absorbed instead of compounding; the report carries
//! the honest before/after distribution error (tonal + channel means + per-band
//! hue, so a hue disaster cannot hide behind matched luma quantiles). Local
//! masks and content changes are out of scope by construction (statistics
//! cannot localise them) — the AI style-prompt path covers intent the numbers
//! cannot.

use image::DynamicImage;

use crate::recipe::{CurvePoint, EditRecipe};
use crate::render;

/// Analysis resolution (long edge). CDFs and band means are stable well below
/// this; keeping it small makes the 4 closed-loop renders interactive-fast.
const ANALYZE_EDGE: u32 = 384;
const HIST_BINS: usize = 1024;
/// Quantile clip for CDF inversion — the extreme tails of a generative render
/// are noise (a few blown/crushed pixels would otherwise own the end knots).
const P_CLIP: f32 = 0.002;
/// Cast-curve acceptance: the fitted per-channel curves must cut the hue-aware
/// look error to ≤ this fraction of the without-curves error, else they are
/// rejected as a content mismatch masquerading as a cast (see the stage-4
/// comment in [`fit_recipe`]). A true global cast slashes the error far past
/// this; a content difference only nibbles at it while damaging regions.
const CAST_ACCEPT_RATIO: f32 = 0.85;

// --- cast foreign-hue veto (the second, pixel-aligned gate) -----------------
// The aggregate ratio above is structurally blind to a CROSS-BAND hue wreck:
// rotate a small pale sky from blue into violet and its band mass lands in
// Purple/Magenta (empty in the target → the hue term's two-sided weight gate
// skips it) while draining out of Blue (below the gate in the fitted render →
// also skipped) — the hue term sees nothing, and the tonal+colour win on the
// frame-dominant region sails the curves through (real-machine canyon
// failure, 2026-07-09; reproduced by
// `warm_rock_cast_must_not_violet_the_pale_sky`).
//
// The veto exploits what the aggregate cannot: the renders WITHOUT and WITH
// the curves come from the SAME source, so "what did the curves do" is
// exact, not statistical. The verdict is by HUE DISTANCE: a pixel the curves
// leave visibly tinted at a hue ≥ [`VETO_FAR_BINS`]·15° away from EVERY hue
// the target populates is FOREIGN — reject the curves when they grow the
// frame's foreign share by ≥ [`VETO_CREATED_SHARE`].
//
// Distance is the discriminator the failure data demanded (both probed on
// the live pairs): the canyon violet lands 60-100° from everything the
// target contains, while the haze correction's imperfect residuals scatter
// pixels only 5-40° off the target's own orange/green/blue mass — so
// family-membership rules at ANY granularity mis-classify one side or the
// other (a ±15° window scored the haze fix 15% "damage"; whole-band shares
// flagged its 35-45° orange-yellow skirt as a phantom yellow family), but a
// 45° = 1.5-ACR-band radius separates them with a 20°+ margin either way.
// Measuring the DELTA against the without-render keeps pre-existing content
// mismatch (already-foreign pixels the curves didn't create) out of the
// verdict.
//
// Known non-goal: a cast that rotates a region into a hue the target DOES
// contain elsewhere (sky turned rock-red) passes this veto; that failure is
// near-band and the band-centroid hue term in `look_err` is the signal that
// sees it. The observed failure class is the foreign hue.

/// Target pixels feeding the hue-support bins must clear this chroma —
/// deliberately BELOW the 0.06 band-stats gate so a pale sky still testifies.
const VETO_SUPPORT_CHROMA: f32 = 0.03;
/// Below this many chromatic target pixels there is no reliable hue evidence
/// (e.g. a monochrome target) — the veto stands down.
const VETO_MIN_TARGET_CHROMATIC: usize = 500;
/// A pixel is "visibly tinted" at/above this chroma and enters the foreign
/// census. Below the renderer's HSL fade-in (0.05), but a contiguous 0.04
/// tint over a sky-sized region is visible.
const VETO_TINT_CHROMA: f32 = 0.04;
/// A 15° hue bin is "populated" when it holds ≥ this share of the target's
/// chromatic mass. Chroma noise spread over 24 bins stays well under this;
/// any hue region the target actually contains clears it severalfold.
const VETO_SUPPORT_BIN_MIN: f32 = 0.015;
/// Foreign radius in 15° bins: ±3 bins = ±45° = 1.5 ACR bands. The canyon
/// violet sits 60°+ from all target hues; the haze residuals sit ≤ 40° from
/// the target's own families — 45° splits them with margin on both sides.
const VETO_FAR_BINS: usize = 3;
/// Foreign frame-share the curves must CREATE (with − without) to be
/// rejected: 5% of the frame is a REGION (the canyon sky measures ~12-15%),
/// not boundary speckle (the haze pair measures ≈ 0.04%).
const VETO_CREATED_SHARE: f32 = 0.05;

/// The fit outcome: the recipe plus the distribution error (mean |Δ| over luma
/// quantiles and channel means, 0 = identical look) before and after.
pub struct FitReport {
    pub recipe: EditRecipe,
    pub err_before: f32,
    pub err_after: f32,
}

/// Fit an [`EditRecipe`] mapping `src` (untouched preview) onto the look of
/// `target` (a rendition of the same frame). Deterministic, no network.
pub fn fit_recipe(src: &DynamicImage, target: &DynamicImage) -> FitReport {
    let s_img = src.thumbnail(ANALYZE_EDGE, ANALYZE_EDGE);
    let t_img = target.thumbnail(ANALYZE_EDGE, ANALYZE_EDGE);
    let sp = pixels_of(&s_img);
    let tp = pixels_of(&t_img);
    let err_before = look_err(&sp, &tp);

    let mut recipe = EditRecipe::default();

    // --- 1) tone: exposure scan × linear solve on the engine's knot basis ----
    // Tone evidence comes from NEAR-NEUTRAL pixels: saturated pixels clip
    // channels at the gamut ceiling under chroma scaling, so their luma lands
    // short of the tone map and would bias the solve (measured: one polluted
    // knot skews contrast by tens of points). Greys carry clean evidence.
    let s_cdf = tone_cdf(&sp);
    let t_cdf = tone_cdf(&tp);
    let tone_map = |x: f32| quantile(&t_cdf, cdf_at(&s_cdf, x).clamp(P_CLIP, 1.0 - P_CLIP));
    let (ev, sliders) = fit_tone_sliders(&tone_map);
    recipe.exposure_ev = round2(ev);
    recipe.contrast = round1(sliders[0] * 100.0);
    recipe.highlights = round1(sliders[1] * 100.0);
    recipe.shadows = round1(sliders[2] * 100.0);
    recipe.whites = round1(sliders[3] * 100.0);
    recipe.blacks = round1(sliders[4] * 100.0);

    // --- 2) residual master curve (composed on top of the sliders) -----------
    recipe.tone_curve = residual_tone_curve(&recipe, &tone_map);

    // --- 3) global saturation, secant-refined through the real engine --------
    // Saturation stays BEFORE the cast curves: channel CDFs of a desaturated
    // render differ from the target's even with zero cast (each channel's
    // distribution is compressed toward luma), so fitting the cast first
    // would express chroma expansion through per-channel curves — and
    // per-channel curves rotate hue. Saturating first may amplify a latent
    // cast, but stage 5 fits the cast residual CLOSED-LOOP on the saturated
    // render, so it is measured and removed rather than compounded.
    let t_chroma = mean_chroma(&tp);
    for _ in 0..2 {
        let cur = pixels_of(&render::develop_preview(&s_img, &recipe));
        let c_chroma = mean_chroma(&cur);
        if c_chroma < 1e-4 {
            break;
        }
        let step = ((t_chroma / c_chroma - 1.0) * 100.0).clamp(-40.0, 40.0);
        if step.abs() < 1.0 {
            break;
        }
        recipe.saturation = round1((recipe.saturation + step).clamp(-60.0, 60.0));
    }

    // --- 4) per-channel colour-cast curves — the catch-all, LAST so its
    // closed-loop residual sees every earlier stage's composed output
    // (cast-before-saturation was tried and measured worse on the haze
    // regression: chroma expansion leaks into the curves, which rotate hue).
    //
    // The curves model a GLOBAL cast (one monotone map per channel). That
    // model is exactly right for uniform casts (haze tint, WB drift) and
    // exactly wrong when the colour residual is CONTENT (a generative
    // target's rocks simply ARE warmer than its sky): then the fitted map
    // drags every region — measured on the real pair, the red lift that
    // warmed the frame-dominant rocks turned the pale sky violet (and the
    // neutral-only-evidence variant, also tried, cooled the warm distance
    // haze instead). The two worlds are told apart by VALIDATION, not by
    // evidence filtering: accept the curves only if they improve the
    // hue-aware look error by a clear margin — a global map that truly
    // explains the residual slashes the error (the haze regression), while
    // a content mismatch yields a marginal "improvement" bought by regional
    // hue damage the metric's hue term partially sees. Marginal gain does
    // not earn regional risk: keep the recipe clean instead.
    //
    // Deliberately NO per-band HSL fitting. It was tried (centroid hue
    // deltas + sat/luma ratios per ACR band, correspondence-gated) and it is
    // what wrecked the real-photo fit (2026-07-07): against a generative,
    // non-pixel-aligned target, a band's centroid delta conflates CONTENT
    // difference with style, and an honest-looking 13° in-gate delta applied
    // as a whole-band rotation turns brown rock olive and a pale sky
    // lavender. Per-band intent is statistically unidentifiable here — like
    // local masks, it belongs to the AI style-prompt path, not to
    // distribution matching.
    let cur = pixels_of(&render::develop_preview(&s_img, &recipe));
    let err_without = look_err(&cur, &tp);
    recipe.red_curve = residual_channel_curve(&cur, &tp, 0);
    recipe.green_curve = residual_channel_curve(&cur, &tp, 1);
    recipe.blue_curve = residual_channel_curve(&cur, &tp, 2);
    if !(recipe.red_curve.is_empty()
        && recipe.green_curve.is_empty()
        && recipe.blue_curve.is_empty())
    {
        let with_px = pixels_of(&render::develop_preview(&s_img, &recipe));
        // Two gates, both must pass: the aggregate ratio (a marginal win does
        // not earn regional risk) and the phantom-family veto (a large
        // aggregate win does not earn a wrecked region the aggregate is
        // cross-band-blind to — the veto only ever rejects, never rescues).
        let ratio_fail = look_err(&with_px, &tp) > err_without * CAST_ACCEPT_RATIO;
        if ratio_fail || cast_paints_foreign_hues(&cur, &with_px, &tp) {
            recipe.red_curve = Vec::new();
            recipe.green_curve = Vec::new();
            recipe.blue_curve = Vec::new();
        }
    }

    // --- report ---------------------------------------------------------------
    let final_px = pixels_of(&render::develop_preview(&s_img, &recipe));
    let err_after = look_err(&final_px, &tp);
    recipe.rationale = format!(
        "Reverse-fit from a target rendition (statistical match; the target is not \
         pixel-aligned, so local masks and per-band HSL are not recovered): luma-CDF \
         → tone sliders {}, chroma → saturation, per-channel cast curves. Residual \
         look error {err_before:.3} → {err_after:.3}.",
        if recipe.tone_curve.is_empty() { "(no residual curve)" } else { "+ residual tone curve" },
    );
    recipe.confidence = (1.0 - err_after * 6.0).clamp(0.25, 0.95);
    recipe.clamp();
    FitReport { recipe, err_before, err_after }
}

// --------------------------------------------------------------------------
// tone solve
// --------------------------------------------------------------------------

/// Magnitude prior for the tone solve. The 5-slider knot system is
/// near-collinear (contrast vs shadows/highlights, whites vs the shoulder), so
/// unpenalised least squares happily returns huge mutually-cancelling sliders
/// whose TOTAL map ties a tasteful solution to within numerical ε — and the
/// residual curve erases even that difference. The prior makes slider
/// magnitude itself part of the cost, so "Exposure +1.5, Contrast −97,
/// Shadows −100" loses to the mild solve it was shadowing. Units: basis
/// authorities are O(0.2–0.34), knot residuals O(0.1); 0.02 prices a pegged
/// slider (s=1) like a ~0.14 luma miss at one knot — strong enough to kill
/// cancellation combos, weak enough that genuinely-needed big moves survive
/// (the roundtrip test pins recovery of a real ±25-point recipe).
const TONE_PRIOR: f64 = 0.02;

/// Scan exposure (nonlinear in the model) and, for each candidate, solve the 5
/// linear sliders (contrast/highlights/shadows/whites/blacks, in the basis
/// order of [`render::tone_slider_basis`]) by RIDGE least squares over the 8
/// knots; keep the (ev, sliders) minimising the PENALISED clamped-solution
/// score `SSE + TONE_PRIOR·Σs²` — the same prior in the solve and in the
/// model selection, so the exposure scan cannot smuggle the degeneracy back.
fn fit_tone_sliders(tone_map: &impl Fn(f32) -> f32) -> (f32, [f32; 5]) {
    let targets: Vec<f32> = render::TONE_KNOTS_X.iter().map(|&x| tone_map(x)).collect();
    let basis: Vec<[f32; 5]> =
        render::TONE_KNOTS_X.iter().map(|&x| render::tone_slider_basis(x)).collect();

    let mut best = (0.0f32, [0.0f32; 5], f32::INFINITY);
    let mut ev = -3.0f32;
    while ev <= 3.0 + 1e-6 {
        // Residual after the exposure component, then ridge normal equations.
        let resid: Vec<f64> = render::TONE_KNOTS_X
            .iter()
            .zip(&targets)
            .map(|(&x, &t)| (t - render::tone_exposure_curve(x, ev)) as f64)
            .collect();
        let mut ata = [[0.0f64; 5]; 5];
        let mut atb = [0.0f64; 5];
        for (b, r) in basis.iter().zip(&resid) {
            for i in 0..5 {
                for j in 0..5 {
                    ata[i][j] += b[i] as f64 * b[j] as f64;
                }
                atb[i] += b[i] as f64 * r;
            }
        }
        for (i, row) in ata.iter_mut().enumerate() {
            row[i] += TONE_PRIOR; // ridge = the magnitude prior (see const doc)
        }
        let sol = solve5(ata, atb);
        let s: [f32; 5] = std::array::from_fn(|i| (sol[i] as f32).clamp(-1.0, 1.0));
        let penalty: f64 = s.iter().map(|&v| TONE_PRIOR * v as f64 * v as f64).sum();
        let score: f64 = basis
            .iter()
            .zip(&resid)
            .map(|(b, r)| {
                let fit: f64 = (0..5).map(|i| b[i] as f64 * s[i] as f64).sum();
                (r - fit) * (r - fit)
            })
            .sum::<f64>()
            + penalty;
        if (score as f32) < best.2 {
            best = (ev, s, score as f32);
        }
        ev += 0.05;
    }
    (best.0, best.1)
}

/// Gaussian elimination with partial pivoting for the 5×5 normal equations.
fn solve5(mut a: [[f64; 5]; 5], mut b: [f64; 5]) -> [f64; 5] {
    for c in 0..5 {
        let mut p = c;
        for r in c + 1..5 {
            if a[r][c].abs() > a[p][c].abs() {
                p = r;
            }
        }
        a.swap(c, p);
        b.swap(c, p);
        if a[c][c].abs() < 1e-12 {
            continue;
        }
        let pivot = a[c]; // copy of the pivot row ([f64; 5] is Copy)
        for r in c + 1..5 {
            let f = a[r][c] / pivot[c];
            for k in c..5 {
                a[r][k] -= f * pivot[k];
            }
            b[r] -= f * b[c];
        }
    }
    let mut x = [0.0f64; 5];
    for c in (0..5).rev() {
        let mut acc = b[c];
        for k in c + 1..5 {
            acc -= a[c][k] * x[k];
        }
        x[c] = if a[c][c].abs() < 1e-12 { 0.0 } else { acc / a[c][c] };
    }
    x
}

/// Whatever tonal shape the sliders could not express, as `tone_curve` control
/// points. The engine composes `tone_curve` AFTER the knot spline `S`, so the
/// exact residual curve is `M ∘ S⁻¹` — i.e. points `(S(x), M(x))`. Monotone by
/// construction (both `S` and `M` are monotone); skipped when the residual is
/// within tolerance everywhere.
fn residual_tone_curve(recipe: &EditRecipe, tone_map: &impl Fn(f32) -> f32) -> Vec<CurvePoint> {
    debug_assert!(recipe.tone_curve.is_empty(), "fit the residual before setting a curve");
    let lut = render::build_tone_lut(recipe);
    const XS: [f32; 9] = [0.0, 0.10, 0.25, 0.40, 0.50, 0.66, 0.82, 0.92, 1.0];
    let mut max_dev = 0.0f32;
    let mut pts: Vec<CurvePoint> = Vec::with_capacity(XS.len());
    let (mut prev_in, mut prev_out) = (-1i32, 0i32);
    for &x in &XS {
        let sx = render::sample_lut(&lut, x); // engine output before the residual curve
        let y = tone_map(x).clamp(0.0, 1.0); // desired output
        max_dev = max_dev.max((y - sx).abs());
        let input = (sx * 255.0).round() as i32;
        let output = ((y * 255.0).round() as i32).max(prev_out); // keep monotone
        if input <= prev_in {
            continue; // spline outputs can quantise together at the ends
        }
        pts.push(CurvePoint { input: input as u8, output: output as u8 });
        (prev_in, prev_out) = (input, output);
    }
    if max_dev < 0.015 {
        Vec::new() // the sliders already express the map — keep the recipe clean
    } else {
        pts
    }
}

// --------------------------------------------------------------------------
// colour residuals
// --------------------------------------------------------------------------

/// Per-band accumulator: weight, circular hue (sin/cos), HSL sat + luma.
#[derive(Clone, Copy, Default)]
struct BandStat {
    w: f64,
    sin: f64,
    cos: f64,
    s: f64,
    l: f64,
}

/// Accumulate chroma-gated band statistics with the SAME partition of unity the
/// renderer uses ([`render::bracket_bands`]), so the fit and the engine agree on
/// what "the blue band" is. Returns the per-band stats and the chromatic total.
fn band_stats(px: &[[f32; 3]]) -> ([BandStat; 8], f64) {
    let mut bands = [BandStat::default(); 8];
    let mut total = 0.0f64;
    for p in px {
        let chroma = p[0].max(p[1]).max(p[2]) - p[0].min(p[1]).min(p[2]);
        if chroma < 0.06 {
            continue; // matches the renderer's chroma gate: near-grey carries no hue evidence
        }
        let (h, s, l) = render::rgb_to_hsl(p[0], p[1], p[2]);
        let (b0, b1, w1) = render::bracket_bands(h * 360.0, &render::HSL_CENTERS);
        let ang = (h * std::f32::consts::TAU) as f64;
        for (bi, w) in [(b0, 1.0 - w1 as f64), (b1, w1 as f64)] {
            let b = &mut bands[bi];
            b.w += w;
            b.sin += w * ang.sin();
            b.cos += w * ang.cos();
            b.s += w * s as f64;
            b.l += w * l as f64;
        }
        total += 1.0;
    }
    (bands, total)
}

/// Residual per-channel CDF map (current render → target) as a channel curve —
/// the colour-cast catch-all (white balance shift, split toning the wheels/HSL
/// didn't express). Skipped when the channel already matches within tolerance.
fn residual_channel_curve(cur: &[[f32; 3]], tgt: &[[f32; 3]], ch: usize) -> Vec<CurvePoint> {
    let c_cdf = channel_cdf(cur, ch);
    let t_cdf = channel_cdf(tgt, ch);
    const XS: [f32; 5] = [0.0, 0.25, 0.50, 0.75, 1.0];
    let mut max_dev = 0.0f32;
    let mut pts: Vec<CurvePoint> = Vec::with_capacity(XS.len());
    let (mut prev_in, mut prev_out) = (-1i32, 0i32);
    for &x in &XS {
        let y = quantile(&t_cdf, cdf_at(&c_cdf, x).clamp(P_CLIP, 1.0 - P_CLIP)).clamp(0.0, 1.0);
        max_dev = max_dev.max((y - x).abs());
        let input = (x * 255.0).round() as i32;
        let output = ((y * 255.0).round() as i32).max(prev_out);
        if input <= prev_in {
            continue;
        }
        pts.push(CurvePoint { input: input as u8, output: output as u8 });
        (prev_in, prev_out) = (input, output);
    }
    if max_dev < 0.012 {
        Vec::new()
    } else {
        pts
    }
}

/// The set of 15°-hue bins FOREIGN to the target: farther than
/// [`VETO_FAR_BINS`] bins (circularly) from every bin holding ≥
/// [`VETO_SUPPORT_BIN_MIN`] of the target's chromatic mass. `None` when the
/// target has fewer than [`VETO_MIN_TARGET_CHROMATIC`] chromatic pixels — no
/// reliable hue testimony, the veto stands down.
fn foreign_hue_bins(tp: &[[f32; 3]]) -> Option<[bool; 24]> {
    let mut mass = [0.0f32; 24];
    let mut n = 0usize;
    for p in tp {
        let chroma = p[0].max(p[1]).max(p[2]) - p[0].min(p[1]).min(p[2]);
        if chroma < VETO_SUPPORT_CHROMA {
            continue;
        }
        let (h, _s, _l) = render::rgb_to_hsl(p[0], p[1], p[2]);
        mass[((h * 24.0) as usize).min(23)] += 1.0;
        n += 1;
    }
    if n < VETO_MIN_TARGET_CHROMATIC {
        return None;
    }
    let populated: Vec<usize> =
        (0..24).filter(|&k| mass[k] / n as f32 >= VETO_SUPPORT_BIN_MIN).collect();
    let mut foreign = [true; 24];
    for (k, f) in foreign.iter_mut().enumerate() {
        for &p in &populated {
            let fwd = (k as isize - p as isize).rem_euclid(24) as usize;
            if fwd.min(24 - fwd) <= VETO_FAR_BINS {
                *f = false;
                break;
            }
        }
    }
    Some(foreign)
}

/// Fraction of the frame visibly tinted at a hue foreign to the target
/// (chroma ≥ [`VETO_TINT_CHROMA`], hue in a foreign bin).
fn foreign_share(px: &[[f32; 3]], foreign: &[bool; 24]) -> f32 {
    let mut cnt = 0usize;
    for p in px {
        let chroma = p[0].max(p[1]).max(p[2]) - p[0].min(p[1]).min(p[2]);
        if chroma < VETO_TINT_CHROMA {
            continue;
        }
        let (h, _s, _l) = render::rgb_to_hsl(p[0], p[1], p[2]);
        if foreign[((h * 24.0) as usize).min(23)] {
            cnt += 1;
        }
    }
    cnt as f32 / px.len().max(1) as f32
}

/// Did the cast curves paint a REGION of the frame in hues the target holds
/// nowhere (≥ [`VETO_FAR_BINS`]·15° from all its populated hue mass)?
/// `cur`/`with_px` render the SAME source, so the share DELTA is exactly the
/// curves' own work — pre-existing content mismatch cancels out.
fn cast_paints_foreign_hues(cur: &[[f32; 3]], with_px: &[[f32; 3]], tp: &[[f32; 3]]) -> bool {
    let Some(foreign) = foreign_hue_bins(tp) else {
        return false;
    };
    foreign_share(with_px, &foreign) - foreign_share(cur, &foreign) >= VETO_CREATED_SHARE
}

// --------------------------------------------------------------------------
// statistics primitives
// --------------------------------------------------------------------------

fn pixels_of(img: &DynamicImage) -> Vec<[f32; 3]> {
    img.to_rgb8()
        .pixels()
        .map(|p| [p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0])
        .collect()
}

fn luma601(p: &[f32; 3]) -> f32 {
    0.299 * p[0] + 0.587 * p[1] + 0.114 * p[2]
}

fn cdf_from_values(values: impl Iterator<Item = f32>, n_hint: usize) -> Vec<f32> {
    let mut hist = vec![0.0f32; HIST_BINS];
    let mut n = 0usize;
    for v in values {
        let i = ((v.clamp(0.0, 1.0)) * (HIST_BINS - 1) as f32).round() as usize;
        hist[i] += 1.0;
        n += 1;
    }
    let total = (n.max(n_hint.min(1)) as f32).max(1.0);
    let mut acc = 0.0f32;
    for h in hist.iter_mut() {
        acc += *h;
        *h = acc / total;
    }
    hist
}

fn luma_cdf(px: &[[f32; 3]]) -> Vec<f32> {
    cdf_from_values(px.iter().map(luma601), px.len())
}

/// Near-neutral gate shared by the tone and cast evidence. Gated on HSV
/// saturation ((max−min)/max), which is INVARIANT under pure luminance
/// scaling — so the same pixels qualify in the source and in its tone-mapped
/// target (an absolute-chroma gate is not: dark colours slip under it in the
/// source and leave it once brightened, skewing the two CDFs against each
/// other). Near-black counts as neutral.
fn is_neutralish(p: &[f32; 3]) -> bool {
    let mx = p[0].max(p[1]).max(p[2]);
    let mn = p[0].min(p[1]).min(p[2]);
    mx < 0.04 || (mx - mn) / mx < 0.15
}

/// Luma CDF over near-neutral pixels only — the clean tone evidence (see the
/// call site). Falls back to every pixel when the frame is too colourful to
/// leave a reliable neutral sample (< 5 %, or < 512 px).
fn tone_cdf(px: &[[f32; 3]]) -> Vec<f32> {
    let neutral: Vec<f32> = px.iter().filter(|p| is_neutralish(p)).map(luma601).collect();
    if neutral.len() >= (px.len() / 20).max(512) {
        let n = neutral.len();
        cdf_from_values(neutral.into_iter(), n)
    } else {
        luma_cdf(px)
    }
}

fn channel_cdf(px: &[[f32; 3]], ch: usize) -> Vec<f32> {
    cdf_from_values(px.iter().map(|p| p[ch]), px.len())
}

/// F(x): fraction of pixels ≤ x (linear interp between bins).
fn cdf_at(cdf: &[f32], x: f32) -> f32 {
    let pos = x.clamp(0.0, 1.0) * (cdf.len() - 1) as f32;
    let i = pos.floor() as usize;
    if i >= cdf.len() - 1 {
        return cdf[cdf.len() - 1];
    }
    let t = pos - i as f32;
    cdf[i] * (1.0 - t) + cdf[i + 1] * t
}

/// Q(p): the value at quantile `p` (inverse CDF, linear interp within the bin).
fn quantile(cdf: &[f32], p: f32) -> f32 {
    let n = cdf.len();
    let mut lo = 0usize;
    let mut hi = n - 1;
    while lo < hi {
        let mid = (lo + hi) / 2;
        if cdf[mid] < p {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    // Interpolate within the step from the previous bin for a smooth inverse.
    if lo == 0 {
        return 0.0;
    }
    let (c0, c1) = (cdf[lo - 1], cdf[lo]);
    let t = if c1 > c0 { ((p - c0) / (c1 - c0)).clamp(0.0, 1.0) } else { 1.0 };
    ((lo - 1) as f32 + t) / (n - 1) as f32
}

fn mean_chroma(px: &[[f32; 3]]) -> f32 {
    if px.is_empty() {
        return 0.0;
    }
    let sum: f32 =
        px.iter().map(|p| p[0].max(p[1]).max(p[2]) - p[0].min(p[1]).min(p[2])).sum();
    sum / px.len() as f32
}

/// One scalar "how different do these look" — mean |Δ| over 21 luma quantiles
/// (60 %), the 3 channel means (20 %), and the weight-averaged per-band
/// centroid hue disagreement (20 %). 0 = identical distributions. The hue
/// term exists because matched luma quantiles + channel MEANS can hide a
/// full-blown hue disaster (a purple sky and a blue one can share all four
/// global numbers — exactly how the 2026-07-07 real-photo failure reported
/// err 0.034 / confidence 0.80 for an unusable render).
fn look_err(a: &[[f32; 3]], b: &[[f32; 3]]) -> f32 {
    let (ca, cb) = (luma_cdf(a), luma_cdf(b));
    let mut tonal = 0.0f32;
    let mut n = 0.0f32;
    for i in 0..=20 {
        let p = (i as f32 / 20.0).clamp(P_CLIP, 1.0 - P_CLIP);
        tonal += (quantile(&ca, p) - quantile(&cb, p)).abs();
        n += 1.0;
    }
    tonal /= n;
    let mean = |px: &[[f32; 3]], ch: usize| -> f32 {
        if px.is_empty() {
            return 0.0;
        }
        px.iter().map(|p| p[ch]).sum::<f32>() / px.len() as f32
    };
    let colour = (0..3).map(|ch| (mean(a, ch) - mean(b, ch)).abs()).sum::<f32>() / 3.0;
    // Per-band centroid hue disagreement — the WORST qualifying band, not a
    // weighted mean: one region with wrecked hue ruins a photo no matter how
    // small its area share (a lavender sky over perfect rocks), and an
    // area-weighted mean lets exactly that hide (measured: the violet-sky
    // curves slipped through the cast-acceptance gate on the mean variant).
    // |Δ| saturates at 60° so a fully-wrecked band reads 1.
    let (sa, ta) = band_stats(a);
    let (sb, tb) = band_stats(b);
    let mut hue = 0.0f32;
    if ta >= 1.0 && tb >= 1.0 {
        for i in 0..8 {
            let (x, y) = (&sa[i], &sb[i]);
            if x.w / ta < 0.015 || y.w / tb < 0.015 {
                continue;
            }
            let mut d = y.sin.atan2(y.cos).to_degrees() - x.sin.atan2(x.cos).to_degrees();
            while d > 180.0 {
                d -= 360.0;
            }
            while d < -180.0 {
                d += 360.0;
            }
            hue = hue.max((d.abs().min(60.0) / 60.0) as f32);
        }
    }
    0.6 * tonal + 0.2 * colour + 0.2 * hue
}

fn round1(v: f32) -> f32 {
    (v * 10.0).round() / 10.0
}
fn round2(v: f32) -> f32 {
    (v * 100.0).round() / 100.0
}

// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use image::RgbImage;

    /// Synthetic frame with real tonal + chromatic coverage: a neutral luma ramp
    /// plus orange / blue / green ramps (192×128 — analysis-sized already).
    fn synth() -> DynamicImage {
        let (w, h) = (192u32, 128u32);
        let mut img = RgbImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let l = x as f32 / (w - 1) as f32;
                let p = match y * 4 / h {
                    0 => [l, l, l],
                    1 => [l, l * 0.6, l * 0.2],
                    2 => [l * 0.2, l * 0.7, l],
                    _ => [l * 0.3, l, l * 0.4],
                };
                img.put_pixel(x, y, image::Rgb(p.map(|c| (c * 255.0).round() as u8)));
            }
        }
        DynamicImage::ImageRgb8(img)
    }

    #[test]
    fn identity_fit_is_near_neutral() {
        let img = synth();
        let rep = fit_recipe(&img, &img);
        let r = &rep.recipe;
        assert!(r.exposure_ev.abs() < 0.06, "exposure {}", r.exposure_ev);
        for (name, v) in [
            ("contrast", r.contrast),
            ("highlights", r.highlights),
            ("shadows", r.shadows),
            ("whites", r.whites),
            ("blacks", r.blacks),
            ("saturation", r.saturation),
        ] {
            assert!(v.abs() < 6.0, "{name} should stay near 0, got {v}");
        }
        assert!(rep.err_after < 0.02, "identity residual {}", rep.err_after);
    }

    #[test]
    fn roundtrip_recovers_tone_and_saturation() {
        // Render a KNOWN recipe through the real engine, then fit it back.
        let src = synth();
        let mut truth = EditRecipe {
            exposure_ev: 0.35,
            contrast: 18.0,
            highlights: -25.0,
            whites: 12.0,
            saturation: 15.0,
            ..Default::default()
        };
        truth.clamp();
        let target = render::develop_preview(&src, &truth);
        let rep = fit_recipe(&src, &target);
        let r = &rep.recipe;
        // The luma CDF of the target IS the engine's own tone map of the source,
        // so the solve must land close (exposure/slider trade-offs allowed).
        assert!((r.exposure_ev - 0.35).abs() < 0.20, "exposure {}", r.exposure_ev);
        assert!(r.contrast > 3.0 && r.contrast < 45.0, "contrast {}", r.contrast);
        assert!(r.highlights < -8.0 && r.highlights > -50.0, "highlights {}", r.highlights);
        assert!(r.saturation > 5.0 && r.saturation < 30.0, "saturation {}", r.saturation);
        // And the fitted recipe must actually reproduce the look through the engine.
        assert!(
            rep.err_after < (rep.err_before * 0.5).max(0.012),
            "residual {} vs before {}",
            rep.err_after,
            rep.err_before
        );
    }

    #[test]
    fn hazy_to_clean_fit_stays_sane() {
        // Regression for the 2026-07-07 real-photo failure: fitting a
        // low-contrast, low-chroma, blue-cast base toward a clean punchy
        // target produced mutually-cancelling pegged tone sliders
        // (Exposure +1.5 / Contrast −97 / Shadows −100), pegged per-band hue
        // rotations (+45) and a purple sky — while the old metric reported
        // "improved". The prior, the stage order and the correspondence gate
        // must keep every fitted control in its sane regime.
        let clean = synth();
        let mut haze = EditRecipe {
            exposure_ev: -0.3,
            contrast: -45.0,
            blacks: 40.0,
            saturation: -40.0,
            // a shadow-weighted blue cast at realistic haze strength (the
            // midpoint pin keeps it out of the highlights, like real haze)
            blue_curve: vec![
                CurvePoint { input: 0, output: 25 },
                CurvePoint { input: 128, output: 132 },
                CurvePoint { input: 255, output: 255 },
            ],
            ..Default::default()
        };
        haze.clamp();
        let base = render::develop_preview(&clean, &haze);
        let rep = fit_recipe(&base, &clean);
        let r = &rep.recipe;
        assert!(
            r.contrast > -20.0 && r.contrast.abs() < 90.0,
            "degenerate contrast {}",
            r.contrast
        );
        assert!(
            r.shadows.abs() < 90.0 && r.whites.abs() < 90.0 && r.blacks.abs() < 90.0,
            "pegged tone sliders: sh {} wh {} bl {}",
            r.shadows,
            r.whites,
            r.blacks
        );
        assert!(r.exposure_ev.abs() <= 1.0, "runaway exposure {}", r.exposure_ev);
        // NOTE deliberately no "slider not pegged" assertion for hue: the
        // correspondence gate already rejects mismatched populations, and a
        // genuine in-gate rotation larger than the engine's ±13.5° range
        // legitimately clamps. What must hold is the RESULT (below): no band
        // of the fitted render lands tens of degrees off the target.
        assert!(
            rep.err_after < rep.err_before,
            "fit made the look worse: {} -> {}",
            rep.err_before,
            rep.err_after
        );
        // The decisive invariant: render the fitted recipe and check every
        // populated band's centroid hue against the target — the purple-sky
        // failure class means some band lands tens of degrees off.
        let fitted = pixels_of(&render::develop_preview(&base, &rep.recipe));
        let (fb, ftot) = band_stats(&fitted);
        let (tb, ttot) = band_stats(&pixels_of(&clean));
        let mut worst = 0.0f64;
        for i in 0..8 {
            let (x, y) = (&fb[i], &tb[i]);
            if x.w / ftot < 0.015 || y.w / ttot < 0.015 {
                continue;
            }
            let mut d = y.sin.atan2(y.cos).to_degrees() - x.sin.atan2(x.cos).to_degrees();
            while d > 180.0 {
                d -= 360.0;
            }
            while d < -180.0 {
                d += 360.0;
            }
            worst = worst.max(d.abs());
        }
        assert!(worst < 15.0, "a band's hue is still {worst:.1}° off after the fit");
    }

    /// 192×128 canyon: 15.6% neutral ramp (tone evidence — without it the
    /// pale sky is the only `is_neutralish` population and the tone solve
    /// degenerates), 68.8% warm-rock ramp, 15.6% pale-blue sky. `warm` = the
    /// region-graded target: rocks red-lifted (`l^0.7`), ramp + sky IDENTICAL
    /// to the source — the grade a global cast cannot express without
    /// collateral damage.
    fn canyon(warm: bool) -> DynamicImage {
        let (w, h) = (192u32, 128u32);
        let mut img = RgbImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let l = 0.15 + 0.80 * x as f32 / (w - 1) as f32;
                let p = if y < 16 {
                    [l, l, l] // neutral ramp
                } else if y < 112 {
                    // Rocks: the warm grade is a pure RED OFFSET — a hue move
                    // toward orange that symmetric chroma expansion (the
                    // saturation stage) cannot express, so the residual lands
                    // squarely on the red channel curve, as in the real photo.
                    let r = if warm { (0.85 * l + 0.18).min(1.0) } else { 0.85 * l };
                    [r, 0.52 * l, 0.30 * l]
                } else {
                    [0.64, 0.68, 0.73] // pale blue sky, hue ≈ 213°, chroma 0.09
                };
                img.put_pixel(
                    x,
                    y,
                    image::Rgb(p.map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8)),
                );
            }
        }
        DynamicImage::ImageRgb8(img)
    }

    /// The veto's discriminator, pinned on both live cases: it must NOT fire
    /// on the haze pair (whose accepted correction rotates pixels only INTO
    /// the target's own hue families — measured foreign-share delta ≈ 0.000)
    /// and MUST fire on the canyon cast (which paints ~12% of the frame in
    /// hues ≥ 45° from everything the target contains). The end-to-end
    /// verdicts live in `hazy_to_clean_fit_stays_sane` and
    /// `warm_rock_cast_must_not_violet_the_pale_sky`; this pins the primitive
    /// so a threshold tweak that flips one side fails HERE with numbers.
    #[test]
    fn foreign_hue_veto_separates_haze_from_canyon() {
        // Canyon: rebuild stage 4's exact inputs (fit minus its cast curves →
        // `cur`; curves re-derived and rendered → `with`).
        let src = canyon(false);
        let tgt = canyon(true);
        let s2 = src.thumbnail(ANALYZE_EDGE, ANALYZE_EDGE);
        let tp2 = pixels_of(&tgt.thumbnail(ANALYZE_EDGE, ANALYZE_EDGE));
        let mut pre = fit_recipe(&src, &tgt).recipe;
        pre.red_curve = Vec::new();
        pre.green_curve = Vec::new();
        pre.blue_curve = Vec::new();
        let cur2 = pixels_of(&render::develop_preview(&s2, &pre));
        let mut with = pre.clone();
        with.red_curve = residual_channel_curve(&cur2, &tp2, 0);
        with.green_curve = residual_channel_curve(&cur2, &tp2, 1);
        with.blue_curve = residual_channel_curve(&cur2, &tp2, 2);
        assert!(
            !with.red_curve.is_empty(),
            "premise broken: the canyon pair no longer provokes a red cast curve"
        );
        let with2 = pixels_of(&render::develop_preview(&s2, &with));
        let cf = foreign_hue_bins(&tp2).expect("canyon target has chromatic mass");
        let created = foreign_share(&with2, &cf) - foreign_share(&cur2, &cf);
        assert!(
            cast_paints_foreign_hues(&cur2, &with2, &tp2),
            "veto must fire on the canyon cast (foreign share created {created:.4})"
        );
        assert!(created > 2.0 * VETO_CREATED_SHARE, "margin eroded: created {created:.4}");

        // Haze: same reconstruction on the accepted correction.
        let clean = synth();
        let mut haze = EditRecipe {
            exposure_ev: -0.3,
            contrast: -45.0,
            blacks: 40.0,
            saturation: -40.0,
            blue_curve: vec![
                CurvePoint { input: 0, output: 25 },
                CurvePoint { input: 128, output: 132 },
                CurvePoint { input: 255, output: 255 },
            ],
            ..Default::default()
        };
        haze.clamp();
        let base = render::develop_preview(&clean, &haze);
        let s_img = base.thumbnail(ANALYZE_EDGE, ANALYZE_EDGE);
        let tp = pixels_of(&clean.thumbnail(ANALYZE_EDGE, ANALYZE_EDGE));
        let rec = fit_recipe(&base, &clean).recipe; // veto active: curves survive
        assert!(
            !(rec.red_curve.is_empty() && rec.green_curve.is_empty() && rec.blue_curve.is_empty()),
            "the haze correction's cast curves must survive the veto"
        );
        let mut pre_h = rec.clone();
        pre_h.red_curve = Vec::new();
        pre_h.green_curve = Vec::new();
        pre_h.blue_curve = Vec::new();
        let cur_h = pixels_of(&render::develop_preview(&s_img, &pre_h));
        let with_h = pixels_of(&render::develop_preview(&s_img, &rec));
        assert!(
            !cast_paints_foreign_hues(&cur_h, &with_h, &tp),
            "veto must NOT fire on the haze correction"
        );
    }

    #[test]
    fn warm_rock_cast_must_not_violet_the_pale_sky() {
        // Regression for the 2026-07-09 real-machine canyon failure: the target
        // warms the frame-dominant rocks and keeps the small pale sky blue. The
        // channel-CDF cast stage answers the rocks' demand with a global red
        // lift whose 5-knot interpolation drags the sky's red up too → violet
        // sky. The aggregate acceptance gate passed it because the rotated sky
        // is CROSS-BAND invisible to the hue term (mass lands in Purple/Magenta
        // — empty in the target — and drains out of Blue: the two-sided band
        // gate skips both). The pixel-aligned hue-damage veto must reject the
        // curves; saturation alone (hue-preserving) then matches the chroma.
        let src = canyon(false);
        let tgt = canyon(true);
        let rep = fit_recipe(&src, &tgt);
        // Render the fitted recipe and audit the sky region (rows y ≥ 108).
        let out = render::develop_preview(&src, &rep.recipe).to_rgb8();
        let (mut sin, mut cos, mut n) = (0.0f64, 0.0f64, 0.0f64);
        for y in 108..128 {
            for x in 0..192 {
                let p = out.get_pixel(x, y);
                let (r, g, b) =
                    (p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0);
                if r.max(g).max(b) - r.min(g).min(b) < 0.03 {
                    continue; // desaturated sky pixels carry no hue verdict
                }
                let hue = render::rgb_to_hsl(r, g, b).0 as f64 * std::f64::consts::TAU;
                sin += hue.sin();
                cos += hue.cos();
                n += 1.0;
            }
        }
        if n > 0.0 {
            let mean = sin.atan2(cos).to_degrees().rem_euclid(360.0);
            let d = (mean - 213.0 + 540.0).rem_euclid(360.0) - 180.0;
            assert!(
                d.abs() < 30.0,
                "sky hue drifted to {mean:.0}° (Δ{d:.0}°) — a violet/purple cast leaked through"
            );
        }
        // And rejecting the cast must not have made the overall fit worse.
        assert!(
            rep.err_after <= rep.err_before + 0.01,
            "fit made the look worse: {} -> {}",
            rep.err_before,
            rep.err_after
        );
    }

    #[test]
    fn quantile_and_cdf_are_inverse_on_a_ramp() {
        let px: Vec<[f32; 3]> = (0..4096)
            .map(|i| {
                let v = i as f32 / 4095.0;
                [v, v, v]
            })
            .collect();
        let cdf = luma_cdf(&px);
        for &x in &[0.1f32, 0.25, 0.5, 0.75, 0.9] {
            let p = cdf_at(&cdf, x);
            let back = quantile(&cdf, p);
            assert!((back - x).abs() < 0.01, "x={x} → p={p} → {back}");
        }
    }
}
