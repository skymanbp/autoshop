//! Eval harness — how close is the AI's edit to the user's own?
//!
//! For RAWs that have a sibling `.xmp` (the user's ACR/Lightroom develop
//! settings = ground truth), run the AI advisor and compare global slider
//! values. Reports per-field **mean absolute error** (how far off) and **mean
//! signed bias** AI−user (which direction the AI leans). That bias is the
//! tuning signal: e.g. "AI contrast runs +8 hotter than you" → nudge the prompt.
//!
//! XMP is parsed by plain text scan of `crs:Key="value"` (the values are
//! attributes on rdf:Description; verified against the user's real DSC08724.xmp).
//! No exiftool needed.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::Config;
use crate::pipeline;
use crate::recipe::EditRecipe;

// The `crs:` attribute scanner lives beside the XMP writer it inverts (xmp.rs,
// where the sidecar READER also uses it); re-exported so this module's tests
// and `style.rs` keep their `eval::crs_f32` path.
pub(crate) use crate::xmp::crs_f32;

/// Comparable global develop values from a user XMP, mapped into our recipe's
/// units (e.g. crs Sharpness 0..100 → recipe sharpening 0..150).
struct UserEdit {
    fields: Vec<(&'static str, Option<f32>)>,
}

fn parse_user_xmp(xmp: &str) -> UserEdit {
    UserEdit {
        fields: vec![
            ("exposure_ev", crs_f32(xmp, "Exposure2012")),
            ("contrast", crs_f32(xmp, "Contrast2012")),
            ("highlights", crs_f32(xmp, "Highlights2012")),
            ("shadows", crs_f32(xmp, "Shadows2012")),
            ("whites", crs_f32(xmp, "Whites2012")),
            ("blacks", crs_f32(xmp, "Blacks2012")),
            ("tint", crs_f32(xmp, "Tint")),
            ("vibrance", crs_f32(xmp, "Vibrance")),
            ("saturation", crs_f32(xmp, "Saturation")),
            ("clarity", crs_f32(xmp, "Clarity2012")),
            ("dehaze", crs_f32(xmp, "Dehaze")),
            // crs Sharpness is 0..100; recipe sharpening is 0..150.
            ("sharpening", crs_f32(xmp, "Sharpness").map(|s| s * 1.5)),
            ("noise_reduction", crs_f32(xmp, "LuminanceSmoothing")),
            ("temperature_k", crs_f32(xmp, "Temperature")),
        ],
    }
}

/// The AI recipe's value for the same named field (None = field not set, e.g.
/// temperature_k left as-shot).
fn ai_field(r: &EditRecipe, name: &str) -> Option<f32> {
    Some(match name {
        "exposure_ev" => r.exposure_ev,
        "contrast" => r.contrast,
        "highlights" => r.highlights,
        "shadows" => r.shadows,
        "whites" => r.whites,
        "blacks" => r.blacks,
        "tint" => r.tint,
        "vibrance" => r.vibrance,
        "saturation" => r.saturation,
        "clarity" => r.clarity,
        "dehaze" => r.dehaze,
        "sharpening" => r.sharpening,
        "noise_reduction" => r.noise_reduction,
        "temperature_k" => return r.temperature_k,
        _ => return None,
    })
}

/// Parse an ACR tone-curve `<rdf:Seq>` of `<rdf:li>x, y</rdf:li>` points (each
/// 0..255 input,output) for the given crs tag (e.g. "ToneCurvePV2012"). Empty
/// vec if the tag is absent. The master tone curve is the single biggest "look"
/// control that the flat-slider comparison above was completely blind to.
fn parse_tone_curve(xmp: &str, tag: &str) -> Vec<(f32, f32)> {
    let open = format!("<crs:{tag}>");
    let close = format!("</crs:{tag}>");
    let Some(s) = xmp.find(&open) else { return Vec::new() };
    let body = &xmp[s + open.len()..];
    let Some(e) = body.find(&close) else { return Vec::new() };
    let body = &body[..e];
    let mut pts = Vec::new();
    for chunk in body.split("<rdf:li>").skip(1) {
        let Some(end) = chunk.find("</rdf:li>") else { continue };
        let mut it = chunk[..end].split(',');
        if let (Some(xs), Some(ys)) = (it.next(), it.next())
            && let (Ok(x), Ok(y)) = (xs.trim().parse::<f32>(), ys.trim().parse::<f32>())
        {
            pts.push((x, y));
        }
    }
    pts
}

/// The AI recipe's master tone curve as the same (input,output) point list.
fn ai_tone_curve_points(r: &EditRecipe) -> Vec<(f32, f32)> {
    r.tone_curve.iter().map(|p| (p.input as f32, p.output as f32)).collect()
}

/// Build a 256-entry [0..255]→[0..255] LUT from tone-curve control points
/// (piecewise-linear, clamped at the ends). Identity if fewer than 2 points.
fn curve_lut(points: &[(f32, f32)]) -> [f32; 256] {
    let mut lut = [0f32; 256];
    if points.len() < 2 {
        for (i, v) in lut.iter_mut().enumerate() {
            *v = i as f32;
        }
        return lut;
    }
    let mut pts = points.to_vec();
    pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    for (i, v) in lut.iter_mut().enumerate() {
        *v = interp255(&pts, i as f32);
    }
    lut
}

fn interp255(pts: &[(f32, f32)], x: f32) -> f32 {
    if x <= pts[0].0 {
        return pts[0].1;
    }
    let last = pts[pts.len() - 1];
    if x >= last.0 {
        return last.1;
    }
    for w in pts.windows(2) {
        let ((x0, y0), (x1, y1)) = (w[0], w[1]);
        if x >= x0 && x <= x1 {
            let t = if (x1 - x0).abs() < 1e-6 { 0.0 } else { (x - x0) / (x1 - x0) };
            return y0 + (y1 - y0) * t;
        }
    }
    x
}

/// How much a curve lifts the black point (output at input 0; 0 = pinned black).
fn curve_black_lift(lut: &[f32; 256]) -> f32 {
    lut[0]
}

/// S-curve strength: how much the curve brightens the quarter-highlight (input
/// 191) AND darkens the quarter-shadow (input 64) relative to identity. >0 adds
/// contrast (an S); <0 flattens; ~0 is identity/linear.
fn curve_s_strength(lut: &[f32; 256]) -> f32 {
    (lut[191] - 191.0) - (lut[64] - 64.0)
}

/// RMS difference between two 0..255 LUTs (same scale as the curve values).
fn curve_rmse(a: &[f32; 256], b: &[f32; 256]) -> f64 {
    let mut s = 0f64;
    for i in 0..256 {
        let d = (a[i] - b[i]) as f64;
        s += d * d;
    }
    (s / 256.0).sqrt()
}

/// The user's master tone-curve SHAPE from an XMP, summarised as
/// `(black_lift, s_strength)` for the style library — `None` if they drew no
/// curve. Stores the shape, not the raw point list (averaging point lists is
/// mush). Reused by `style.rs` so the curve metric has one definition.
pub(crate) fn user_curve_shape(xmp: &str) -> Option<(f32, f32)> {
    let pts = parse_tone_curve(xmp, "ToneCurvePV2012");
    if pts.len() < 2 {
        return None;
    }
    let lut = curve_lut(&pts);
    Some((curve_black_lift(&lut), curve_s_strength(&lut)))
}

#[derive(Default, Clone, Copy)]
struct Acc {
    sum_abs: f64,
    sum_signed: f64,
    n: u32,
    /// Times the user used this control but the AI left it neutral/omitted it —
    /// a real miss the old both-set gate dropped silently.
    omit: u32,
}

pub fn run(dir: &Path, limit: usize) -> Result<()> {
    let cfg = Config::load();
    let raws = pipeline::find_raws(dir)?;
    let pairs: Vec<_> = raws
        .iter()
        .filter(|r| r.with_extension("xmp").exists())
        .collect();
    println!(
        "found {} RAW(s); {} have a sibling .xmp (your edits). Evaluating {}.",
        raws.len(),
        pairs.len(),
        pairs.len().min(limit)
    );
    if pairs.is_empty() {
        println!("Nothing to evaluate — no .xmp sidecars next to the RAWs in this folder.");
        return Ok(());
    }

    // Field order for the report (matches parse_user_xmp).
    let order = [
        "exposure_ev", "contrast", "highlights", "shadows", "whites", "blacks", "tint",
        "vibrance", "saturation", "clarity", "dehaze", "sharpening", "noise_reduction",
        "temperature_k",
    ];
    let mut acc: BTreeMap<&str, Acc> = BTreeMap::new();
    let mut evaluated = 0u32;
    // Master-tone-curve comparison (the look control the flat sliders miss),
    // accumulated only over photos where YOU drew a curve.
    let (mut curve_n, mut sum_curve_rmse) = (0u32, 0f64);
    let (mut sum_user_lift, mut sum_ai_lift) = (0f64, 0f64);
    let (mut sum_user_s, mut sum_ai_s) = (0f64, 0f64);

    for (i, raw) in pairs.iter().take(limit).enumerate() {
        print!("[{}/{}] {} ... ", i + 1, pairs.len().min(limit), pipeline::stem(raw));
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let xmp_text = std::fs::read_to_string(raw.with_extension("xmp"))
            .with_context(|| format!("read user xmp for {}", raw.display()))?;
        let user = parse_user_xmp(&xmp_text);
        // style_strength = 0: eval measures the raw AI proposal vs your edits, so
        // it must NOT pull toward your historical style (that would bias the gap).
        let (ai, _verdict) = match pipeline::produce_recipe(raw, &cfg, false, None, None, 0.0) {
            Ok(v) => v,
            Err(e) => {
                println!("FAILED: {e}");
                continue;
            }
        };
        for (name, user_val) in &user.fields {
            // Only judge controls YOU actually used; skip ones you left neutral.
            let u = match user_val {
                Some(u) => *u,
                None => continue,
            };
            let eps = if *name == "exposure_ev" { 0.05 } else { 0.5 };
            let e = acc.entry(name).or_default();
            match ai_field(&ai, name) {
                Some(a) => {
                    let d = (a - u) as f64;
                    e.sum_abs += d.abs();
                    e.sum_signed += d;
                    e.n += 1;
                    // You moved it; the AI parked it at neutral → a miss.
                    if u.abs() > eps && a.abs() <= eps {
                        e.omit += 1;
                    }
                }
                // AI left the field unset (e.g. WB as-shot) while you set it: a
                // real omission the old both-set gate dropped without counting.
                None => {
                    if u.abs() > eps {
                        e.omit += 1;
                    }
                }
            }
        }

        // --- master tone curve: did the AI commit to a curve like you did? ---
        let user_curve = parse_tone_curve(&xmp_text, "ToneCurvePV2012");
        if user_curve.len() >= 2 {
            let ulut = curve_lut(&user_curve);
            let alut = curve_lut(&ai_tone_curve_points(&ai));
            curve_n += 1;
            sum_curve_rmse += curve_rmse(&ulut, &alut);
            sum_user_lift += curve_black_lift(&ulut) as f64;
            sum_ai_lift += curve_black_lift(&alut) as f64;
            sum_user_s += curve_s_strength(&ulut) as f64;
            sum_ai_s += curve_s_strength(&alut) as f64;
        }

        evaluated += 1;
        println!("done");
    }

    if evaluated == 0 {
        println!("No photos evaluated.");
        return Ok(());
    }

    println!("\n=== AI vs your edits ({evaluated} photo(s)) ===");
    println!("{:<16} {:>4} {:>10} {:>13} {:>8}", "field", "n", "mean|Δ|", "bias(AI−you)", "AI-omit");
    for name in order {
        let Some(a) = acc.get(name) else { continue };
        if a.n == 0 && a.omit == 0 {
            continue;
        }
        if a.n > 0 {
            let mae = a.sum_abs / a.n as f64;
            let bias = a.sum_signed / a.n as f64;
            println!("{:<16} {:>4} {:>10.2} {:>+13.2} {:>8}", name, a.n, mae, bias, a.omit);
        } else {
            // You used it, the AI never engaged it — no Δ to report, just the miss.
            println!("{:<16} {:>4} {:>10} {:>13} {:>8}", name, a.n, "—", "—", a.omit);
        }
    }

    // --- master tone curve summary -------------------------------------------
    if curve_n > 0 {
        let avg = |s: f64| s / curve_n as f64;
        println!("\n=== Master tone curve (you drew one on {curve_n} photo(s)) ===");
        println!("  black lift (output@0):  you {:>6.1}   AI {:>6.1}", avg(sum_user_lift), avg(sum_ai_lift));
        println!("  S-strength (contrast):  you {:>6.1}   AI {:>6.1}", avg(sum_user_s), avg(sum_ai_s));
        println!("  curve RMSE (AI vs you): {:>6.1}   (0 = identical, on the 0..255 scale)", avg(sum_curve_rmse));
        if avg(sum_user_s).abs() > 4.0 && avg(sum_ai_s).abs() < avg(sum_user_s).abs() * 0.5 {
            println!("  → the AI's curve is much flatter than yours: it is omitting the S-curve that gives your photos their contrast.");
        }
    } else {
        println!("\n(no master tone curve in your XMPs to compare)");
    }

    // --- aggregate gap score -------------------------------------------------
    // Mean fractional divergence across the controls you used, each MAE
    // normalised by a sensible full-scale, plus the tone-curve term. One number
    // to watch move as the advisor improves.
    let range_of = |name: &str| -> f64 {
        match name {
            "exposure_ev" => 5.0,
            "sharpening" => 150.0,
            "temperature_k" => 2000.0,
            _ => 100.0,
        }
    };
    let (mut frac_sum, mut frac_n) = (0f64, 0u32);
    for name in order {
        if let Some(a) = acc.get(name)
            && a.n > 0
        {
            frac_sum += (a.sum_abs / a.n as f64) / range_of(name);
            frac_n += 1;
        }
    }
    if curve_n > 0 {
        frac_sum += (sum_curve_rmse / curve_n as f64) / 255.0;
        frac_n += 1;
    }
    let gap = if frac_n > 0 { 100.0 * frac_sum / frac_n as f64 } else { 0.0 };
    println!(
        "\nOverall gap score: {gap:.1}%  (mean per-control divergence incl. tone curve; lower = closer to your look)"
    );
    println!(
        "Interpretation: positive bias = AI sets this higher than you do; large mean|Δ| = you \
         disagree a lot on that control; AI-omit = times you used a control the AI ignored. Use \
         these to calibrate the advisor prompt."
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Values copied from the user's real DSC08724.xmp (read this session).
    const SAMPLE: &str = r#"<rdf:Description
        crs:Temperature="5650" crs:Tint="+13" crs:Exposure2012="0.00"
        crs:Contrast2012="+22" crs:Highlights2012="+7" crs:Shadows2012="-6"
        crs:Whites2012="0" crs:Blacks2012="0" crs:Clarity2012="0" crs:Dehaze="+18"
        crs:Vibrance="+5" crs:Saturation="+13" crs:Sharpness="40"
        crs:LuminanceSmoothing="0">"#;

    fn get(u: &UserEdit, k: &str) -> Option<f32> {
        u.fields.iter().find(|(n, _)| *n == k).and_then(|(_, v)| *v)
    }

    #[test]
    fn parses_real_crs_values() {
        let u = parse_user_xmp(SAMPLE);
        assert_eq!(get(&u, "exposure_ev"), Some(0.0));
        assert_eq!(get(&u, "contrast"), Some(22.0));
        assert_eq!(get(&u, "shadows"), Some(-6.0));
        assert_eq!(get(&u, "dehaze"), Some(18.0));
        assert_eq!(get(&u, "temperature_k"), Some(5650.0));
        assert_eq!(get(&u, "sharpening"), Some(60.0)); // 40 * 1.5
        assert_eq!(crs_f32(SAMPLE, "Nonexistent"), None);
    }

    // A user S-curve: black lifted to 12, quarter-shadow pulled down (64→50),
    // quarter-highlight pushed up (191→210), white pinned.
    const CURVE_XMP: &str = r#"<crs:ToneCurvePV2012>
 <rdf:Seq>
  <rdf:li>0, 12</rdf:li>
  <rdf:li>64, 50</rdf:li>
  <rdf:li>191, 210</rdf:li>
  <rdf:li>255, 255</rdf:li>
 </rdf:Seq>
</crs:ToneCurvePV2012>"#;

    #[test]
    fn parses_tone_curve_and_measures_shape() {
        let pts = parse_tone_curve(CURVE_XMP, "ToneCurvePV2012");
        assert_eq!(pts.len(), 4);
        assert_eq!(pts[0], (0.0, 12.0));
        assert_eq!(pts[2], (191.0, 210.0));

        let lut = curve_lut(&pts);
        assert!((lut[0] - 12.0).abs() < 0.5, "black lifted to 12: {}", lut[0]);
        assert!((lut[255] - 255.0).abs() < 0.5, "white pinned: {}", lut[255]);
        assert!(curve_black_lift(&lut) > 10.0, "reads the black lift");
        // S-strength = (210-191) - (50-64) = 19 - (-14) = 33 > 0 (an S).
        assert!(curve_s_strength(&lut) > 20.0, "reads as an S: {}", curve_s_strength(&lut));

        // Identity curve has ~0 lift and ~0 strength, and differs from the S.
        let id = curve_lut(&[]);
        assert!(curve_s_strength(&id).abs() < 0.5);
        assert!(curve_rmse(&lut, &id) > 5.0, "an S-curve is far from identity");

        // Absent tag → empty (so the eval simply skips the curve comparison).
        assert!(parse_tone_curve("<x:y/>", "ToneCurvePV2012").is_empty());
    }
}
