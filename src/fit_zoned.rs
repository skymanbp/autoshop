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

use crate::render;

/// A zone must cover at least this weighted share of ITS frame on BOTH sides
/// to carry trustworthy moments (a real sky measures 10–40%; segmentation
/// misses and boundary slivers sit far below).
pub(crate) const MIN_ZONE_SHARE: f32 = 0.03;
/// Conservative local-exposure budget: ±2.5 EV covers any real sky-to-sky
/// brightness gap; a larger demand means the zones do not correspond.
const ZONE_EV_LIMIT: f32 = 2.5;
/// Local saturation shares the global fit's model cap (fit.rs stage 3).
const ZONE_SAT_LIMIT: f32 = 60.0;

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
    for c in 0..3 {
        let want = tgt.mean_lin[c].max(1e-5) / src.mean_lin[c].max(1e-5);
        // Same legal range recipe::clamp enforces (0 would kill a channel).
        color_gains[c] = (want / bright).clamp(0.05, 8.0);
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
        for c in 0..3 {
            let total = d.color_gains[c] * bright;
            assert!(
                (total - g_true[c]).abs() < 5e-3,
                "channel {c}: recovered {total} vs true {}",
                g_true[c]
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
                name: "反推·天空".into(),
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
}
