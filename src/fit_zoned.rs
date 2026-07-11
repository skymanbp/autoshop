//! Zoned reverse-fit — the semantic-region extension of [`crate::fit`].
//!
//! The global fit is statistics over the WHOLE frame, and its gates refuse
//! regional regrades by design (fit.rs rotation budget: "true regional
//! regrades belong to the zoned fit"). This module supplies that fit: segment
//! the same semantic region (the sky) in BOTH the source and the target,
//! compare the two zones' colour statistics, and emit the difference as a
//! bitmap-masked [`LocalAdjustment`](crate::recipe::LocalAdjustment) driving
//! the engine's local dials (render.rs `apply_masks`).
//!
//! Identifiability stance (fit.rs's, one level down): zone MOMENTS (weighted
//! first moments) only — no per-zone CDFs or curves. A zone is a small,
//! soft-edged, non-pixel-aligned population; means are the only statistics
//! stable enough to trust there. The global fit must refuse per-channel
//! moves because it cannot tell a cast from content (WHERE is unknown); here
//! the mask answers WHERE, so exact per-channel gains on the zone are
//! identified — that is the entire expressiveness upgrade.
//!
//! Dial choice (measured, golden-sky pair): a palette transplant (pale-blue
//! sky → vivid gold) demands linear channel ratios of r/b ≈ 5.3×, while ANY
//! white-balance parametrisation caps near 1.9× (the full 2000–40000 K
//! blackbody range) and ±100 saturation only doubles chroma — Temp/Tint/Sat
//! physically cannot repaint. So the fit solves the move as **exact
//! per-channel linear gains** (`color_gains`, engine-rendered inside the
//! mask) with brightness split out into local exposure (the tone LUT's soft
//! shoulder handles it more gracefully than a raw linear gain would).
//! Saturation stays closed-loop through real renders ([`zone_sat_step`]),
//! matching the global fit's philosophy.

use std::path::Path;

use anyhow::{Context, Result};
use image::{DynamicImage, GrayImage};

use crate::fit::{self, FitReport};
use crate::recipe::{LocalAdjustment, MaskGeometry, MaskRole};
use crate::render;
use crate::segment::{segment_file, SegmentOpts};

/// A zone must cover at least this weighted share of ITS frame on BOTH sides
/// to carry trustworthy moments (a real sky measures 10–40%; segmentation
/// misses and boundary slivers sit far below).
pub(crate) const MIN_ZONE_SHARE: f32 = 0.03;
/// Conservative local-exposure budget: ±2.5 EV covers any real sky-to-sky
/// brightness gap; a larger demand means the zones do not correspond.
const ZONE_EV_LIMIT: f32 = 2.5;
/// Local saturation shares the global fit's model cap (fit.rs stage 3).
const ZONE_SAT_LIMIT: f32 = 60.0;
/// Acceptance: the zone-local error ([`zone_err`]) must fall to ≤ this
/// fraction of its pre-correction value. The correction is judged on ITS
/// zone, not on the frame-global `look_err` — measured on the real pair
/// (2026-07-09, _DSC9621 × reimagine-5): the sky correction landed the zone
/// moments almost exactly on the target's (zone error 0.507 → 0.015) while
/// the FRAME-global metric moved 0.1768 → 0.1792, because the generative
/// target holds ~3× more sky area than the source (the composition differs —
/// no zone repaint can reconcile frame-level distributions) and a correct
/// blue→gold repaint migrates band mass, which the worst-band hue term can
/// only read as damage. A frame-global gate therefore vetoes exactly the
/// correction this module exists to make.
const ZONE_ACCEPT_RATIO: f32 = 0.5;
/// Insurance bound: the mask cannot touch pixels outside its raster (engine
/// guarantee, pinned by the rocks-bit-equal test), so the only frame-global
/// drift a correct zone repaint can cause is metric-visible band migration
/// inside its own region. Allow that small, measured drift (+0.0024 on the
/// real pair) but refuse anything larger — a big global regression means the
/// mask is NOT the region we thought it was.
const ZONE_GLOBAL_REGRESSION_TOL: f32 = 0.02;

/// Mask-weighted first moments of one zone.
pub(crate) struct ZoneMoments {
    /// Weighted mean Rec.601 luma of the LINEAR-light channels (EV math needs
    /// linear; the engine's exact transfer curve via `srgb_to_linear`).
    pub luma_lin: f32,
    /// Weighted mean per-channel LINEAR values (colour gains act here).
    pub mean_lin: [f32; 3],
    /// Weighted mean HSV-style chroma (max−min) in sRGB — the same definition
    /// the global fit's `mean_chroma` uses, so the two stages agree on what
    /// "saturated" means.
    pub chroma: f32,
    /// Weighted zone share of the frame, Σw / n.
    pub share: f32,
}

/// Moments of the zone selected by `weights` (one weight per pixel, [0,1] —
/// a decoded segmentation mask, or anything else). Zero-weight pixels cost
/// nothing. A degenerate mask (Σw ≈ 0) returns `share == 0.0` and neutral
/// moments — callers gate on [`MIN_ZONE_SHARE`] anyway.
pub(crate) fn zone_moments(px: &[[f32; 3]], weights: &[f32]) -> ZoneMoments {
    debug_assert_eq!(px.len(), weights.len());
    let mut w_total = 0.0f64;
    let mut luma = 0.0f64;
    let mut mean = [0.0f64; 3];
    let mut chroma = 0.0f64;
    for (p, &w) in px.iter().zip(weights) {
        if w <= 0.0 {
            continue;
        }
        let w = w as f64;
        w_total += w;
        let lin = [
            render::srgb_to_linear(p[0]),
            render::srgb_to_linear(p[1]),
            render::srgb_to_linear(p[2]),
        ];
        luma += w * (0.299 * lin[0] + 0.587 * lin[1] + 0.114 * lin[2]) as f64;
        for c in 0..3 {
            mean[c] += w * lin[c] as f64;
        }
        chroma += w * (p[0].max(p[1]).max(p[2]) - p[0].min(p[1]).min(p[2])) as f64;
    }
    if w_total <= 0.0 {
        return ZoneMoments { luma_lin: 0.0, mean_lin: [0.0; 3], chroma: 0.0, share: 0.0 };
    }
    ZoneMoments {
        luma_lin: (luma / w_total) as f32,
        mean_lin: [
            (mean[0] / w_total) as f32,
            (mean[1] / w_total) as f32,
            (mean[2] / w_total) as f32,
        ],
        chroma: (chroma / w_total) as f32,
        share: (w_total / px.len().max(1) as f64) as f32,
    }
}

/// The coarse zone correction: local exposure + the exact per-channel linear
/// gains the engine's mask stage renders. Saturation is deliberately NOT
/// here — it is closed-loop by construction (see [`zone_sat_step`]).
pub(crate) struct ZoneDials {
    pub exposure_ev: f32,
    pub color_gains: [f32; 3],
}

/// Solve the dials that move the source zone's moments onto the target
/// zone's. Pure moment math, no renders, and EXACT for the moments by
/// construction:
///
/// * **exposure** — linear-luma ratio in EV. Brightness rides the local tone
///   LUT (soft shoulder) instead of a raw gain, so bright zone texture rolls
///   off instead of clipping.
/// * **color_gains** — the remaining brightness-normalised per-channel
///   demand `(tgt/src) / 2^EV` in linear light: exactly the ratios the
///   engine multiplies in (`apply_masks`), exactly what a WB dial cannot
///   express (see the module doc).
pub(crate) fn fit_zone_dials(src: &ZoneMoments, tgt: &ZoneMoments) -> ZoneDials {
    let exposure_ev = (tgt.luma_lin.max(1e-5) / src.luma_lin.max(1e-5))
        .log2()
        .clamp(-ZONE_EV_LIMIT, ZONE_EV_LIMIT);
    let bright = 2.0f32.powf(exposure_ev);
    let mut color_gains = [1.0f32; 3];
    for (c, gain) in color_gains.iter_mut().enumerate() {
        let want = tgt.mean_lin[c].max(1e-5) / src.mean_lin[c].max(1e-5);
        // Same legal range recipe::clamp enforces (0 would kill a channel).
        *gain = (want / bright).clamp(0.05, 8.0);
    }
    ZoneDials { exposure_ev, color_gains }
}

/// One closed-loop saturation step: the same mean-chroma chase as the global
/// fit's stage 3 (per-step ±40; the caller clamps the accumulated value with
/// [`clamp_zone_sat`]), fed with the zone chroma MEASURED on a real render of
/// the current recipe — open-loop chroma math after a recolour is not
/// trustworthy (the gains change chroma by themselves). Returns the step to
/// ADD to the current local saturation; `None` when converged (< 1 point) or
/// when the zone carries no chroma evidence.
pub(crate) fn zone_sat_step(cur_chroma: f32, tgt_chroma: f32) -> Option<f32> {
    if cur_chroma < 1e-4 {
        return None;
    }
    let step = ((tgt_chroma / cur_chroma - 1.0) * 100.0).clamp(-40.0, 40.0);
    if step.abs() < 1.0 {
        return None;
    }
    Some(step)
}

/// Clamp an accumulated local saturation to the zone model cap.
pub(crate) fn clamp_zone_sat(v: f32) -> f32 {
    v.clamp(-ZONE_SAT_LIMIT, ZONE_SAT_LIMIT)
}

/// Zone-local look distance: mean |Δ| of the linear channel means plus the
/// chroma gap — the moments the fit steers, measured where the mask acts.
/// This is the yardstick the zoned do-no-harm judges by (see
/// [`ZONE_ACCEPT_RATIO`] for why the frame-global `look_err` cannot be).
pub(crate) fn zone_err(a: &ZoneMoments, b: &ZoneMoments) -> f32 {
    let mean: f32 =
        a.mean_lin.iter().zip(&b.mean_lin).map(|(x, y)| (x - y).abs()).sum::<f32>() / 3.0;
    mean + (a.chroma - b.chroma).abs()
}

/// Mask-weighted luma CDF of a zone (sRGB Rec.601 luma, the same domain as
/// the global fit's tone stage). Drives the WITHIN-zone tone solve: a zone
/// can match the target's linear MEAN and still read far darker (the real
/// pair's land: the target holds sunlit mesa tops plus deep canyon shadows —
/// a few bright pixels dominate a linear mean, while perception follows the
/// distribution). Zones correspond semantically, so quantile-to-quantile
/// mapping is identified here — unlike the per-band statistics fit.rs bans
/// on the WHOLE frame, where region correspondence is unknown.
pub(crate) fn zone_luma_cdf(px: &[[f32; 3]], weights: &[f32]) -> Vec<f32> {
    const BINS: usize = 1024;
    let mut hist = vec![0.0f32; BINS];
    let mut total = 0.0f32;
    for (p, &w) in px.iter().zip(weights) {
        if w <= 0.0 {
            continue;
        }
        let l = 0.299 * p[0] + 0.587 * p[1] + 0.114 * p[2];
        hist[(l.clamp(0.0, 1.0) * (BINS - 1) as f32).round() as usize] += w;
        total += w;
    }
    let total = total.max(1e-6);
    let mut acc = 0.0f32;
    for h in hist.iter_mut() {
        acc += *h;
        *h = acc / total;
    }
    hist
}

// --------------------------------------------------------------------------
// orchestration
// --------------------------------------------------------------------------

/// The zoned reverse-fit: the global [`fit::fit_recipe`] first, then a
/// sky-to-sky zone correction on top — segment the sky in BOTH images
/// (`seg`, the same sidecar the GUI's mask panel uses), compare zone moments,
/// attach a Bitmap-masked [`LocalAdjustment`] when it measurably helps.
///
/// `mask_path` is where the SOURCE sky mask lands (the recipe references it;
/// use the GUI convention `out/<stem>.mask-sky.png` so the mask panel shows
/// the same raster). GRACEFUL BY CONTRACT: segmentation missing/failing, a
/// degenerate sky, or a correction that does not improve the look all fall
/// back to the plain global fit with an honest rationale note — never an
/// error, because the global fit in hand is already a valid result.
pub fn fit_recipe_zoned(
    src: &DynamicImage,
    target: &DynamicImage,
    seg: &SegmentOpts,
    mask_path: &Path,
) -> FitReport {
    let mut report = fit::fit_recipe(src, target);
    match segment_both(src, target, seg, mask_path) {
        Ok((src_mask, tgt_mask)) => {
            attach_zones(src, target, &mut report, &src_mask, &tgt_mask, mask_path);
        }
        Err(e) => {
            report.recipe.rationale.push_str(&format!(
                " Zoned sky fit unavailable ({e:#}) — global fit only.",
            ));
        }
    }
    report
}

/// Run the segmentation sidecar on both images. The source mask persists at
/// `mask_path` (the recipe references it); the target's inputs/mask are
/// temporary siblings, removed before returning. Any failure aborts the
/// whole zoned attempt — the caller degrades to the global fit.
fn segment_both(
    src: &DynamicImage,
    target: &DynamicImage,
    seg: &SegmentOpts,
    mask_path: &Path,
) -> Result<(GrayImage, GrayImage)> {
    let sibling = |suffix: &str| -> std::path::PathBuf {
        let mut s = mask_path.as_os_str().to_owned();
        s.push(suffix);
        s.into()
    };
    let tmp_src = sibling(".src-in.png");
    let tmp_tgt = sibling(".tgt-in.png");
    let tmp_tgt_mask = sibling(".tgt-mask.png");
    let run = || -> Result<(GrayImage, GrayImage)> {
        src.to_rgb8().save(&tmp_src).context("write segmentation input (source)")?;
        target.to_rgb8().save(&tmp_tgt).context("write segmentation input (target)")?;
        segment_file(seg, &tmp_src, mask_path).context("segment source sky")?;
        segment_file(seg, &tmp_tgt, &tmp_tgt_mask).context("segment target sky")?;
        let sm = image::open(mask_path).context("read source sky mask")?.to_luma8();
        let tm = image::open(&tmp_tgt_mask).context("read target sky mask")?.to_luma8();
        Ok((sm, tm))
    };
    let out = run();
    for p in [&tmp_src, &tmp_tgt, &tmp_tgt_mask] {
        std::fs::remove_file(p).ok();
    }
    out
}

/// The post-segmentation half (separable so tests drive it with hand-built
/// masks, no python): correct the SKY zone, then the LAND zone — the same
/// raster reused with `inverted = true`, so one segmentation buys the whole
/// frame (the first real-pair render showed why land is not optional: the
/// distant haze-terrain outside the sky mask kept its global-fit blue and
/// clashed against the repainted gold sky as a hard halo). Each zone is
/// gated independently; the raster file is removed only when NO zone kept
/// it. Requires a VALID sky partition first: an empty/degenerate sky mask
/// makes "land" mean "everything", which would just be a weaker-gated
/// re-run of the global fit.
fn attach_zones(
    src: &DynamicImage,
    target: &DynamicImage,
    report: &mut FitReport,
    src_mask: &GrayImage,
    tgt_mask: &GrayImage,
    mask_path: &Path,
) {
    let s_img = src.thumbnail(fit::ANALYZE_EDGE, fit::ANALYZE_EDGE);
    let t_img = target.thumbnail(fit::ANALYZE_EDGE, fit::ANALYZE_EDGE);
    let tgt_px = fit::pixels_of(&t_img);
    let (aw, ah) = {
        let c = render::develop_preview(&s_img, &report.recipe);
        (c.width(), c.height())
    };
    let sw = mask_weights(src_mask, aw, ah);
    let tw = mask_weights(tgt_mask, t_img.width(), t_img.height());
    // Partition validity — judged on the raw mask shares (Σw/n), before any
    // zone-specific gating.
    let share = |w: &[f32]| w.iter().sum::<f32>() / w.len().max(1) as f32;
    let (s_share, t_share) = (share(&sw), share(&tw));
    if !(MIN_ZONE_SHARE..=1.0 - MIN_ZONE_SHARE).contains(&s_share)
        || !(MIN_ZONE_SHARE..=1.0 - MIN_ZONE_SHARE).contains(&t_share)
    {
        report.recipe.rationale.push_str(&format!(
            " Zoned fit skipped: no usable sky partition (sky covers {:.0}% \
             of the source frame, {:.0}% of the target's).",
            s_share * 100.0,
            t_share * 100.0,
        ));
        std::fs::remove_file(mask_path).ok();
        return;
    }
    let swl: Vec<f32> = sw.iter().map(|w| 1.0 - w).collect();
    let twl: Vec<f32> = tw.iter().map(|w| 1.0 - w).collect();
    let sky =
        attach_one_zone(&s_img, &tgt_px, report, &sw, &tw, mask_path, MaskRole::ZoneSky, false);
    let land =
        attach_one_zone(&s_img, &tgt_px, report, &swl, &twl, mask_path, MaskRole::ZoneLand, true);
    if !sky && !land {
        std::fs::remove_file(mask_path).ok();
    }
}

/// Fit + gate ONE zone; returns whether its correction was kept. The zone is
/// measured on a fresh render of the CURRENT recipe (including any zone
/// already attached), so corrections stack the way the engine renders them.
/// Judged by the ZONE-LOCAL error (see [`ZONE_ACCEPT_RATIO`] for the
/// measured reason the frame-global metric cannot be the judge), with the
/// frame-global error as a bounded-drift insurance only.
#[allow(clippy::too_many_arguments)] // internal seam; a struct would just rename the args
fn attach_one_zone(
    s_img: &DynamicImage,
    tgt_px: &[[f32; 3]],
    report: &mut FitReport,
    sw: &[f32],
    tw: &[f32],
    mask_path: &Path,
    role: MaskRole,
    inverted: bool,
) -> bool {
    // `label` drives the rationale prose; it's the zone's stable ASCII tag, so
    // the text stays English/identical regardless of the GUI's display language.
    let label = role.tag();
    let cur_px = fit::pixels_of(&render::develop_preview(s_img, &report.recipe));
    let ms = zone_moments(&cur_px, sw);
    let mt = zone_moments(tgt_px, tw);
    if ms.share < MIN_ZONE_SHARE || mt.share < MIN_ZONE_SHARE {
        report.recipe.rationale.push_str(&format!(
            " The {label} zone covers too little of the frame (source {:.0}%, \
             target {:.0}%) — skipped.",
            ms.share * 100.0,
            mt.share * 100.0,
        ));
        return false;
    }
    let d = fit_zone_dials(&ms, &mt);
    let round1 = |v: f32| (v * 10.0).round() / 10.0;
    let round2 = |v: f32| (v * 100.0).round() / 100.0;
    report.recipe.masks.push(LocalAdjustment {
        mask: MaskGeometry::Bitmap { path: mask_path.to_string_lossy().into_owned() },
        // Identity lives in `role`, not the (empty) display name — the GUI
        // derives a localised label from the role. `name` stays default ("").
        role,
        amount: 1.0,
        inverted,
        color_gains: Some(d.color_gains.map(round2)),
        ..Default::default()
    });
    // Within-zone tone: the zone's brightness/contrast is a DISTRIBUTION,
    // not a mean (see [`zone_luma_cdf`] — the real pair's land matched the
    // linear mean and still rendered far darker than the target). Map the
    // zone's weighted luma CDF onto the target zone's and solve the engine's
    // own local tone sliders from it — the same basis + magnitude prior as
    // the global stage 1. This SUPERSEDES the moment-EV from fit_zone_dials
    // (which now only normalises the gains): brightness lives here, on the
    // render of the gains-only mask so the solve sees the recoloured zone.
    //
    // IDENTIFIABILITY GUARD: a quantile map out of a near-uniform source
    // zone is degenerate — a monotone map cannot spread a luma spike into
    // the target's wide distribution, and the slider solve goes wild on the
    // violent pseudo-map instead (measured, real pair: the flat hazy sky
    // drew exposure −0.70 and its zone residual went 0.016 → 0.108). Below
    // an IQR floor, fall back to the moment-EV and leave the tone flat.
    {
        let rp = fit::pixels_of(&render::develop_preview(s_img, &report.recipe));
        let s_cdf = zone_luma_cdf(&rp, sw);
        let src_iqr = fit::quantile(&s_cdf, 0.75) - fit::quantile(&s_cdf, 0.25);
        let m = report.recipe.masks.last_mut().expect("zone mask just pushed");
        if src_iqr >= 0.05 {
            let t_cdf = zone_luma_cdf(tgt_px, tw);
            let tone_map = |x: f32| {
                fit::quantile(&t_cdf, fit::cdf_at(&s_cdf, x).clamp(fit::P_CLIP, 1.0 - fit::P_CLIP))
            };
            let (ev, sliders) = fit::fit_tone_sliders(&tone_map);
            m.exposure_ev = round2(ev.clamp(-ZONE_EV_LIMIT, ZONE_EV_LIMIT));
            m.contrast = round1(sliders[0] * 100.0);
            m.highlights = round1(sliders[1] * 100.0);
            m.shadows = round1(sliders[2] * 100.0);
            m.whites = round1(sliders[3] * 100.0);
            m.blacks = round1(sliders[4] * 100.0);
        } else {
            m.exposure_ev = round2(d.exposure_ev.clamp(-ZONE_EV_LIMIT, ZONE_EV_LIMIT));
        }
    }
    // Closed-loop zone saturation on real renders (the gains change chroma
    // by themselves — only a render knows where the zone landed).
    for _ in 0..2 {
        let rp = fit::pixels_of(&render::develop_preview(s_img, &report.recipe));
        let zone_chroma = zone_moments(&rp, sw).chroma;
        let Some(step) = zone_sat_step(zone_chroma, mt.chroma) else { break };
        let m = report.recipe.masks.last_mut().expect("zone mask just pushed");
        let next = clamp_zone_sat((m.saturation + step).round());
        if next == m.saturation {
            break;
        }
        m.saturation = next;
    }
    let zoned_px = fit::pixels_of(&render::develop_preview(s_img, &report.recipe));
    let zoned_err = fit::look_err(&zoned_px, tgt_px);
    let zone_before = zone_err(&ms, &mt);
    let zone_after = zone_err(&zone_moments(&zoned_px, sw), &mt);
    if zone_after <= zone_before * ZONE_ACCEPT_RATIO
        && zoned_err <= report.err_after + ZONE_GLOBAL_REGRESSION_TOL
    {
        let m = report.recipe.masks.last().expect("zone mask just pushed");
        let g = m.color_gains.unwrap_or([1.0; 3]);
        report.recipe.rationale.push_str(&format!(
            " Zoned {label} correction attached ({label}-to-{label} moments → \
             local exposure {:+.2} EV, colour gains [{:.2} {:.2} {:.2}], \
             saturation {:+.0}): zone residual {:.3} → {:.3}. The correction \
             is a BITMAP mask — rendered in-app; the Lightroom sidecar \
             carries the global fit only (classic XMP cannot hold raster \
             masks).",
            m.exposure_ev, g[0], g[1], g[2], m.saturation, zone_before, zone_after,
        ));
        // Honest composition note: when the zone covers very different frame
        // shares on the two sides, the GLOBAL distributions cannot fully
        // reconcile no matter how well the zone matches (the real pair's
        // sky: 7.5% vs 22.6%).
        let (lo, hi) = (ms.share.min(mt.share), ms.share.max(mt.share));
        if hi > 2.0 * lo {
            report.recipe.rationale.push_str(&format!(
                " Note: the {label} zone covers {:.0}% of the source frame \
                 but {:.0}% of the target's — the compositions differ, so the \
                 overall distribution residual stays where the global fit \
                 left it.",
                ms.share * 100.0,
                mt.share * 100.0,
            ));
        }
        report.err_after = zoned_err;
        report.recipe.confidence = (1.0 - zoned_err * 6.0).clamp(0.25, 0.95);
        true
    } else {
        report.recipe.masks.pop();
        report.recipe.rationale.push_str(&format!(
            " Zoned {label} correction dropped: zone residual {:.3} → {:.3} \
             (needs ≤ {:.0}% of the original) with frame-global drift \
             {:+.3} (tolerance {:+.3}).",
            zone_before,
            zone_after,
            ZONE_ACCEPT_RATIO * 100.0,
            zoned_err - report.err_after,
            ZONE_GLOBAL_REGRESSION_TOL,
        ));
        false
    }
}

/// Per-pixel mask weights for an analysis frame of `w`×`h` — the SAME
/// normalisation and bilinear sampling the engine's mask stage uses
/// (`render::sample_gray_norm` with x/w, y/h), so the moments are measured
/// exactly where the render will apply them.
fn mask_weights(mask: &GrayImage, w: u32, h: u32) -> Vec<f32> {
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        for x in 0..w {
            out.push(render::sample_gray_norm(mask, x as f32 / w as f32, y as f32 / h as f32));
        }
    }
    out
}

// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zone_moments_use_only_the_weighted_pixels() {
        // Two distinct populations; a binary mask must reproduce the selected
        // population's stats exactly, and `share` must count the weights.
        let px = [
            [0.8f32, 0.2, 0.2], // red-ish (masked out)
            [0.8, 0.2, 0.2],
            [0.2, 0.2, 0.8], // blue-ish (selected)
            [0.2, 0.2, 0.8],
        ];
        let m = zone_moments(&px, &[0.0, 0.0, 1.0, 1.0]);
        assert!((m.share - 0.5).abs() < 1e-6, "share {}", m.share);
        let b_lin = render::srgb_to_linear(0.8);
        let d_lin = render::srgb_to_linear(0.2);
        assert!((m.mean_lin[2] - b_lin).abs() < 1e-6, "blue mean {}", m.mean_lin[2]);
        assert!((m.mean_lin[0] - d_lin).abs() < 1e-6, "red mean {}", m.mean_lin[0]);
        assert!((m.chroma - 0.6).abs() < 1e-6, "chroma {}", m.chroma);
        // Soft weights: half-weight pixels still average to the same MEANS
        // (weights normalise out) but halve the share.
        let soft = zone_moments(&px, &[0.0, 0.0, 0.5, 0.5]);
        assert!((soft.mean_lin[2] - b_lin).abs() < 1e-6);
        assert!((soft.share - 0.25).abs() < 1e-6, "soft share {}", soft.share);
        // Degenerate mask: share 0, no NaNs.
        let dead = zone_moments(&px, &[0.0; 4]);
        assert_eq!(dead.share, 0.0);
        assert!(dead.luma_lin == 0.0 && dead.chroma == 0.0);
    }

    #[test]
    fn zone_dials_recover_a_known_channel_transform() {
        // Forward-transform a zone with known per-channel linear gains, then
        // ask the fit to recover them from moments alone. The TOTAL demand
        // (gains × 2^EV) must reproduce the true gains exactly — the EV/gain
        // SPLIT is a rendering choice, the product is the identified move.
        let g_true = [1.9f32, 1.1, 0.45];
        let src: Vec<[f32; 3]> = vec![
            [0.30, 0.35, 0.45],
            [0.40, 0.42, 0.50],
            [0.25, 0.30, 0.38],
            [0.35, 0.38, 0.46],
        ];
        let lin = |c: f32| render::srgb_to_linear(c);
        let srgb = |c: f32| {
            if c <= 0.0031308 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
        };
        let tgt: Vec<[f32; 3]> = src
            .iter()
            .map(|p| {
                let mut q = [0.0f32; 3];
                for c in 0..3 {
                    q[c] = srgb((lin(p[c]) * g_true[c]).clamp(0.0, 1.0));
                }
                q
            })
            .collect();
        let w = vec![1.0f32; src.len()];
        let ms = zone_moments(&src, &w);
        let mt = zone_moments(&tgt, &w);
        let d = fit_zone_dials(&ms, &mt);
        let bright = 2.0f32.powf(d.exposure_ev);
        for (c, &truth) in g_true.iter().enumerate() {
            let total = d.color_gains[c] * bright;
            assert!(
                (total - truth).abs() < 5e-3,
                "channel {c}: recovered {total} vs true {truth}"
            );
        }
        // The brightness split itself must be sane (the transform brightens
        // red, dims blue — net luma slightly up).
        assert!(d.exposure_ev.abs() < 1.0, "ev {}", d.exposure_ev);
    }

    #[test]
    fn zone_dials_are_neutral_for_matching_zones() {
        let px: Vec<[f32; 3]> = vec![[0.6, 0.63, 0.67], [0.55, 0.58, 0.62]];
        let w = vec![1.0f32; px.len()];
        let m1 = zone_moments(&px, &w);
        let m2 = zone_moments(&px, &w);
        let d = fit_zone_dials(&m1, &m2);
        assert!(d.exposure_ev.abs() < 0.01, "ev {}", d.exposure_ev);
        for c in 0..3 {
            assert!((d.color_gains[c] - 1.0).abs() < 0.01, "gain {c}: {}", d.color_gains[c]);
        }
        assert!(zone_sat_step(m1.chroma, m2.chroma).is_none(), "sat must converge");
    }

    #[test]
    fn zone_dials_turn_a_pale_sky_golden_through_the_engine() {
        // The acceptance geometry of the real failure (_DSC9621 ×
        // reimagine-5, batch #2): hazy pale-BLUE sky, vivid GOLD target sky
        // (the fixtures fit.rs's rotation-gate tests pin). The zoned dials,
        // applied through the engine's bitmap-mask recolour stage, must land
        // the sky in the target's warm family — exactly the regrade the
        // global fit refuses by design — and leave the rocks equal to the
        // control render. (A Temp/Tint-only variant of this test was tried
        // first and could NOT pass: WB gains cap at r/b ≈ 1.9× where this
        // repaint demands ≈ 5.3× — that measurement is why color_gains
        // exists.)
        use crate::recipe::{EditRecipe, LocalAdjustment, MaskGeometry};
        use image::{DynamicImage, GrayImage, RgbImage};

        let (w, h) = (16u32, 16u32);
        let sky_src = [0.60f32, 0.63, 0.67]; // hazy pale blue (hue ≈ 214°)
        let sky_tgt = [0.92f32, 0.72, 0.48]; // vivid gold
        let rock = [0.55f32, 0.45, 0.35];
        let build = |sky: [f32; 3]| -> DynamicImage {
            let img = RgbImage::from_fn(w, h, |_, y| {
                let p = if y >= 12 { sky } else { rock };
                image::Rgb(p.map(|c| (c * 255.0).round() as u8))
            });
            DynamicImage::ImageRgb8(img)
        };
        let src = build(sky_src);
        let tgt = build(sky_tgt);
        // Binary sky mask on disk — the production carrier (Bitmap geometry).
        std::fs::create_dir_all("out").ok();
        let mask_path = "out/_zoned_dials_test_mask.png";
        GrayImage::from_fn(w, h, |_, y| image::Luma([if y >= 12 { 255u8 } else { 0 }]))
            .save(mask_path)
            .unwrap();

        let px_of = |img: &DynamicImage| -> Vec<[f32; 3]> {
            img.to_rgb8()
                .pixels()
                .map(|p| [p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0])
                .collect()
        };
        let weights: Vec<f32> = (0..w * h).map(|i| if i / w >= 12 { 1.0 } else { 0.0 }).collect();
        let ms = zone_moments(&px_of(&src), &weights);
        let mt = zone_moments(&px_of(&tgt), &weights);
        assert!(ms.share >= MIN_ZONE_SHARE && mt.share >= MIN_ZONE_SHARE);
        let d = fit_zone_dials(&ms, &mt);
        assert!(
            d.color_gains[0] > 1.2 && d.color_gains[2] < 0.6,
            "blue→gold demands strong warm gains: {:?}",
            d.color_gains
        );

        let recipe = EditRecipe {
            masks: vec![LocalAdjustment {
                mask: MaskGeometry::Bitmap { path: mask_path.into() },
                role: MaskRole::ZoneSky,
                amount: 1.0,
                exposure_ev: d.exposure_ev,
                color_gains: Some(d.color_gains),
                ..Default::default()
            }],
            ..Default::default()
        };
        let out = px_of(&render::develop_preview(&src, &recipe));
        let control = px_of(&render::develop_preview(&src, &EditRecipe::default()));
        let sky_i = (14 * w + 8) as usize;
        let rock_i = (4 * w + 8) as usize;
        // Sky: source has b > r (blue); the zoned render must land it in the
        // target's warm family (r > g > b) with a clear warm margin, near
        // the target colour (the EV rides the tone LUT's shoulder, so exact
        // equality is not expected — family + proximity is the contract).
        let sky = out[sky_i];
        assert!(sky[0] > sky[2] + 0.10, "sky must turn warm (r >> b): {sky:?}");
        assert!(sky[0] > sky[1] && sky[1] > sky[2], "gold orders r > g > b: {sky:?}");
        for c in 0..3 {
            assert!(
                (sky[c] - sky_tgt[c]).abs() < 0.25,
                "sky channel {c} far from the target: {sky:?} vs {sky_tgt:?}"
            );
        }
        // Rocks: outside the mask — must match the control render.
        for c in 0..3 {
            assert!(
                (out[rock_i][c] - control[rock_i][c]).abs() < 1e-4,
                "rocks must be untouched: {:?} vs {:?}",
                out[rock_i],
                control[rock_i]
            );
        }
        std::fs::remove_file(mask_path).ok();
    }

    // ---- orchestration ----------------------------------------------------

    use image::{DynamicImage, GrayImage, RgbImage};

    /// The golden-pair toy geometry shared by the orchestration tests:
    /// identical warm rocks (top 12 rows), only the sky differs.
    fn zoned_pair() -> (DynamicImage, DynamicImage, GrayImage) {
        let (w, h) = (16u32, 16u32);
        let build = |sky: [f32; 3]| -> DynamicImage {
            let img = RgbImage::from_fn(w, h, |_, y| {
                let p = if y >= 12 { sky } else { [0.55f32, 0.45, 0.35] };
                image::Rgb(p.map(|c| (c * 255.0).round() as u8))
            });
            DynamicImage::ImageRgb8(img)
        };
        let sky_mask =
            GrayImage::from_fn(w, h, |_, y| image::Luma([if y >= 12 { 255u8 } else { 0 }]));
        (build([0.60, 0.63, 0.67]), build([0.92, 0.72, 0.48]), sky_mask)
    }

    #[test]
    fn zoned_orchestration_attaches_the_sky_mask_and_improves_the_zone() {
        let (src, tgt, sky_mask) = zoned_pair();
        std::fs::create_dir_all("out").ok();
        let mask_path = Path::new("out/_zoned_orch_mask.png");
        sky_mask.save(mask_path).unwrap();
        let mut report = fit::fit_recipe(&src, &tgt);
        let err_global = report.err_after;
        attach_zones(&src, &tgt, &mut report, &sky_mask, &sky_mask, mask_path);
        assert!(
            report.recipe.masks.iter().any(|m| m.role == MaskRole::ZoneSky && !m.inverted),
            "sky correction must attach: {}",
            report.recipe.rationale
        );
        // The zoned gate judges each ZONE; frame-global error is only bounded
        // (the insurance tolerance, once per attached zone), never required
        // to improve.
        let bound = err_global
            + ZONE_GLOBAL_REGRESSION_TOL * report.recipe.masks.len() as f32;
        assert!(
            report.err_after <= bound,
            "zoned err {} exceeded the insurance bound {bound}",
            report.err_after
        );
        assert!(
            report.recipe.rationale.contains("Zoned sky correction attached"),
            "rationale must document the zone: {}",
            report.recipe.rationale
        );
        assert!(
            report.recipe.rationale.contains("global fit only"),
            "rationale must carry the XMP honesty note: {}",
            report.recipe.rationale
        );
        std::fs::remove_file(mask_path).ok();
    }

    #[test]
    fn zoned_orchestration_corrects_the_land_through_the_inverted_raster() {
        // The first real-pair render's lesson: repainting ONLY the sky leaves
        // everything outside the mask with the global look — on the real
        // pair a blue haze band clashed against the new gold sky. The land
        // zone reuses the SAME raster inverted; when the target's land
        // differs too (muted vs vivid warm), both zones must attach.
        let (w, h) = (16u32, 16u32);
        let build = |sky: [f32; 3], rock: [f32; 3]| -> DynamicImage {
            let img = RgbImage::from_fn(w, h, |_, y| {
                let p = if y >= 12 { sky } else { rock };
                image::Rgb(p.map(|c| (c * 255.0).round() as u8))
            });
            DynamicImage::ImageRgb8(img)
        };
        // Muted hazy land → bright vivid warm land (the real pair's demand).
        let src = build([0.60, 0.63, 0.67], [0.45, 0.42, 0.40]);
        let tgt = build([0.92, 0.72, 0.48], [0.80, 0.50, 0.28]);
        let sky_mask =
            GrayImage::from_fn(w, h, |_, y| image::Luma([if y >= 12 { 255u8 } else { 0 }]));
        std::fs::create_dir_all("out").ok();
        let mask_path = Path::new("out/_zoned_orch_land_mask.png");
        sky_mask.save(mask_path).unwrap();
        let mut report = fit::fit_recipe(&src, &tgt);
        attach_zones(&src, &tgt, &mut report, &sky_mask, &sky_mask, mask_path);
        assert!(
            report.recipe.masks.iter().any(|m| m.role == MaskRole::ZoneSky && !m.inverted),
            "sky zone must attach: {}",
            report.recipe.rationale
        );
        let land = report
            .recipe
            .masks
            .iter()
            .find(|m| m.role == MaskRole::ZoneLand)
            .unwrap_or_else(|| panic!("land zone must attach: {}", report.recipe.rationale));
        assert!(land.inverted, "the land zone rides the INVERTED sky raster");
        assert!(
            report.recipe.rationale.contains("Zoned land correction attached"),
            "rationale must document the land zone: {}",
            report.recipe.rationale
        );
        // Render check: a land pixel must move toward the vivid warm target.
        let out = render::develop_preview(&src, &report.recipe).to_rgb8();
        let p = out.get_pixel(8, 4);
        let (r, b) = (p[0] as f32 / 255.0, p[2] as f32 / 255.0);
        assert!(r > b + 0.10, "land must turn warm (r >> b): {p:?}");
        std::fs::remove_file(mask_path).ok();
    }

    #[test]
    fn zoned_fit_survives_a_composition_share_mismatch() {
        // The real-pair failure geometry (2026-07-09): the generative target
        // holds ~3× more sky than the source, so the FRAME-global look_err
        // barely moves (or drifts up) when the zone is repainted correctly —
        // the first gate (frame-global improvement) dropped a correction
        // whose zone moments landed almost exactly on the target's (measured
        // zone residual 0.507 → 0.015, global drift +0.0024). The zone-local
        // gate must attach it; the rationale must surface the composition
        // difference honestly.
        let (w, h) = (16u32, 16u32);
        let build = |sky: [f32; 3], sky_rows: u32| -> DynamicImage {
            let img = RgbImage::from_fn(w, h, |_, y| {
                let p = if y >= h - sky_rows { sky } else { [0.55f32, 0.45, 0.35] };
                image::Rgb(p.map(|c| (c * 255.0).round() as u8))
            });
            DynamicImage::ImageRgb8(img)
        };
        let mask_of = |sky_rows: u32| {
            GrayImage::from_fn(w, h, |_, y| {
                image::Luma([if y >= h - sky_rows { 255u8 } else { 0 }])
            })
        };
        // Source: 2 sky rows (12.5%). Target: 6 gold rows (37.5%) — 3× more.
        let src = build([0.60, 0.63, 0.67], 2);
        let tgt = build([0.92, 0.72, 0.48], 6);
        let (sm, tm) = (mask_of(2), mask_of(6));
        std::fs::create_dir_all("out").ok();
        let mask_path = Path::new("out/_zoned_orch_share_mask.png");
        sm.save(mask_path).unwrap();
        let mut report = fit::fit_recipe(&src, &tgt);
        attach_zones(&src, &tgt, &mut report, &sm, &tm, mask_path);
        assert!(
            report.recipe.rationale.contains("Zoned sky correction attached"),
            "the zone gate must attach a correct repaint despite the share \
             mismatch: {}",
            report.recipe.rationale
        );
        assert!(
            report.recipe.rationale.contains("compositions differ"),
            "rationale must surface the share mismatch: {}",
            report.recipe.rationale
        );
        std::fs::remove_file(mask_path).ok();
    }

    #[test]
    fn zoned_orchestration_skips_a_degenerate_sky() {
        // An empty sky mask must skip BOTH zones: without a valid sky
        // partition, "land" would mean "everything" — a weaker-gated re-run
        // of the global fit, not a semantic zone.
        let (src, tgt, _) = zoned_pair();
        let empty = GrayImage::from_pixel(16, 16, image::Luma([0u8]));
        let mask_path = Path::new("out/_zoned_orch_empty_mask.png");
        let mut report = fit::fit_recipe(&src, &tgt);
        attach_zones(&src, &tgt, &mut report, &empty, &empty, mask_path);
        assert!(report.recipe.masks.is_empty(), "no mask on a degenerate partition");
        assert!(
            report.recipe.rationale.contains("no usable sky partition"),
            "rationale must say why: {}",
            report.recipe.rationale
        );
    }

    #[test]
    fn zoned_fit_degrades_gracefully_without_python() {
        // A missing/broken python must yield the plain global fit plus an
        // honest note — never an error (the graceful-fallback contract).
        let (src, tgt, _) = zoned_pair();
        let seg = SegmentOpts {
            python_bin: "autoshop-test-no-such-python".into(),
            // Must EXIST so the failure exercised is the launch, not the
            // script check.
            script: "Cargo.toml".into(),
            target: "sky".into(),
        };
        std::fs::create_dir_all("out").ok();
        let mask_path = Path::new("out/_zoned_orch_nopython_mask.png");
        let report = fit_recipe_zoned(&src, &tgt, &seg, mask_path);
        assert!(report.recipe.masks.is_empty(), "fallback must not attach masks");
        assert!(
            report.recipe.rationale.contains("global fit only"),
            "rationale must explain the fallback: {}",
            report.recipe.rationale
        );
        // The temporary segmentation inputs must not survive the fallback.
        for suffix in [".src-in.png", ".tgt-in.png", ".tgt-mask.png"] {
            let mut p = mask_path.as_os_str().to_owned();
            p.push(suffix);
            assert!(
                !Path::new(&p).exists(),
                "temp file {suffix} leaked past the fallback"
            );
        }
    }
}
