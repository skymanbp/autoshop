//! Runtime configuration for the AI providers.
//!
//! Secrets (the OpenAI key) come **only** from the environment — loaded from a
//! gitignored `.env` via `dotenvy` if present, never from a committed file and
//! never logged. Non-secret knobs (model ids, the `claude` binary path) also
//! read from env with sensible defaults so the tool runs with zero config.
//!
//! Also loads the optional **style profile** (`out/style-profile.json`, written
//! by `autoshop eval --save-profile`) and turns it into a calibration sentence
//! the vision advisor uses to bias edits toward how *this* user actually edits.

use std::collections::BTreeMap;
use std::env;

const PROFILE_PATH: &str = "out/style-profile.json";

pub struct Config {
    /// OpenAI key for the GPT vision advisor. `None` ⇒ GPT path is unavailable
    /// and the CLI falls back to the heuristic proposer.
    pub openai_api_key: Option<String>,
    pub openai_model: String,
    pub openai_base_url: String,
    /// Path/name of the `claude` executable (reused for the verifier via OAuth).
    pub claude_bin: String,
    pub claude_model: String,
    /// Calibration sentence derived from the user's style profile, injected into
    /// the vision advisor's prompt. `None` if no profile has been generated.
    pub style_calibration: Option<String>,
}

impl Config {
    pub fn load() -> Self {
        // Load .env if present; absence is fine. Does not print the key.
        let _ = dotenvy::dotenv();
        let nonempty = |k: &str| env::var(k).ok().filter(|s| !s.trim().is_empty());
        Config {
            openai_api_key: nonempty("OPENAI_API_KEY"),
            // Default model id is a placeholder the user can override; it is not
            // verified against the live API here (no key at build time).
            openai_model: nonempty("AUTOSHOP_OPENAI_MODEL").unwrap_or_else(|| "gpt-5.5".to_string()),
            openai_base_url: nonempty("AUTOSHOP_OPENAI_BASE_URL")
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            claude_bin: nonempty("AUTOSHOP_CLAUDE_BIN").unwrap_or_else(|| "claude".to_string()),
            claude_model: nonempty("AUTOSHOP_CLAUDE_MODEL")
                .unwrap_or_else(|| "claude-sonnet-4-6".to_string()),
            style_calibration: load_calibration(PROFILE_PATH),
        }
    }
}

/// Read the style profile (field → bias, where bias = AI − user) and build a
/// calibration sentence describing how the user tends to edit. `None` if the
/// file is absent/empty or no field exceeds the noise threshold.
fn load_calibration(path: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let map: BTreeMap<String, f32> = serde_json::from_str(&text).ok()?;
    let tips: Vec<String> = map.iter().filter_map(|(f, &b)| tendency(f, b)).collect();
    if tips.is_empty() {
        return None;
    }
    Some(format!(
        "STYLE CALIBRATION — relative to a generic AI edit, THIS photographer tends to: {}. \
         Bias your recipe toward these personal tendencies.",
        tips.join("; ")
    ))
}

/// Turn one field's bias (AI − user) into a user-tendency phrase. The user's
/// tendency is the OPPOSITE of the AI bias. Returns `None` for sub-noise biases.
fn tendency(field: &str, bias: f32) -> Option<String> {
    if field == "temperature_k" {
        if bias.abs() < 150.0 {
            return None;
        }
        // bias < 0 ⇒ AI cooler ⇒ user warmer (higher Kelvin).
        let dir = if bias < 0.0 { "warmer" } else { "cooler" };
        return Some(format!("white balance ~{:.0}K {dir}", bias.abs()));
    }
    let threshold = if field == "exposure_ev" { 0.15 } else { 4.0 };
    if bias.abs() < threshold {
        return None;
    }
    // bias > 0 ⇒ AI sets higher ⇒ user sets lower.
    let dir = if bias > 0.0 { "lower" } else { "higher" };
    let mag = if field == "exposure_ev" {
        format!("{:.2} EV", bias.abs())
    } else {
        format!("{:.0}", bias.abs())
    };
    Some(format!("{} ~{mag} {dir}", pretty(field)))
}

fn pretty(field: &str) -> &str {
    match field {
        "exposure_ev" => "exposure",
        "noise_reduction" => "noise reduction",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tendency_inverts_bias_into_user_direction() {
        // AI sets highlights +28 over user → user runs ~28 LOWER.
        assert_eq!(tendency("highlights", 28.3).as_deref(), Some("highlights ~28 lower"));
        // AI cooler by 733K → user is warmer.
        assert_eq!(tendency("temperature_k", -733.0).as_deref(), Some("white balance ~733K warmer"));
        // AI under-sharpens by 15 → user sharpens ~15 higher.
        assert_eq!(tendency("sharpening", -15.0).as_deref(), Some("sharpening ~15 higher"));
        // sub-noise → skipped
        assert_eq!(tendency("contrast", 2.0), None);
        assert_eq!(tendency("exposure_ev", 0.05), None);
        assert_eq!(tendency("exposure_ev", 0.30).as_deref(), Some("exposure ~0.30 EV lower"));
    }
}
