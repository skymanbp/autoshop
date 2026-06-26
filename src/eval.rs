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

/// Read a single `crs:<key>="<value>"` numeric attribute, tolerating a leading
/// `+` (ACR writes `"+22"`). `None` if the key is absent or unparizable.
pub(crate) fn crs_f32(xmp: &str, key: &str) -> Option<f32> {
    let needle = format!("crs:{key}=\"");
    let start = xmp.find(&needle)? + needle.len();
    let rest = &xmp[start..];
    let end = rest.find('"')?;
    rest[..end].trim().trim_start_matches('+').parse::<f32>().ok()
}

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

#[derive(Default, Clone, Copy)]
struct Acc {
    sum_abs: f64,
    sum_signed: f64,
    n: u32,
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

    for (i, raw) in pairs.iter().take(limit).enumerate() {
        print!("[{}/{}] {} ... ", i + 1, pairs.len().min(limit), pipeline::stem(raw));
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let xmp_text = std::fs::read_to_string(raw.with_extension("xmp"))
            .with_context(|| format!("read user xmp for {}", raw.display()))?;
        let user = parse_user_xmp(&xmp_text);
        let (ai, _verdict) = match pipeline::produce_recipe(raw, &cfg, false, None) {
            Ok(v) => v,
            Err(e) => {
                println!("FAILED: {e}");
                continue;
            }
        };
        for (name, user_val) in &user.fields {
            if let (Some(u), Some(a)) = (user_val, ai_field(&ai, name)) {
                let d = (a - u) as f64;
                let e = acc.entry(name).or_default();
                e.sum_abs += d.abs();
                e.sum_signed += d;
                e.n += 1;
            }
        }
        evaluated += 1;
        println!("done");
    }

    if evaluated == 0 {
        println!("No photos evaluated.");
        return Ok(());
    }

    println!("\n=== AI vs your edits ({evaluated} photo(s)) ===");
    println!("{:<16} {:>4} {:>12} {:>14}", "field", "n", "mean|Δ|", "bias(AI−you)");
    for name in order {
        if let Some(a) = acc.get(name)
            && a.n > 0 {
                let mae = a.sum_abs / a.n as f64;
                let bias = a.sum_signed / a.n as f64;
                println!("{:<16} {:>4} {:>12.2} {:>+14.2}", name, a.n, mae, bias);
            }
    }
    println!(
        "\nInterpretation: positive bias = AI sets this higher than you do. Large mean|Δ| = AI \
         and you disagree a lot on this control. Use these to calibrate the advisor prompt."
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
}
