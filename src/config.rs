//! Runtime configuration for the AI providers.
//!
//! Resolution order (later wins): built-in defaults → environment (a gitignored
//! `.env` via `dotenvy`) → the UI-written local file `autoshop.local.json`
//! (also gitignored). Secrets (API keys) come only from `.env` or that local
//! file, never from a committed source and never logged.
//!
//! Two AI roles (see `docs/ARCHITECTURE.md` §3), each independently configurable:
//!   * **analysis** (the verifier) — provider `oauth` (the `claude` CLI, no key)
//!     or `api` (an OpenAI-compatible chat endpoint).
//!   * **image** (the vision proposer) — `api` only: the `claude` CLI has no
//!     image input in print mode, so this role needs an OpenAI-compatible vision
//!     endpoint (point `image_base_url` anywhere that speaks the API).
//!
//! Style is handled by similarity retrieval (`src/style.rs`), not a global
//! calibration baked in here.

use std::env;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The UI-written / hand-edited local config. Every field is optional; a present
/// value overrides the environment. Lives in [`local_settings_path`] (gitignored).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct LocalSettings {
    pub analysis_provider: Option<String>,
    pub analysis_model: Option<String>,
    pub analysis_api_key: Option<String>,
    pub analysis_base_url: Option<String>,
    pub image_api_key: Option<String>,
    pub image_model: Option<String>,
    pub image_base_url: Option<String>,
    pub image_gen_model: Option<String>,
}

/// Path to the local settings file (cwd-relative, gitignored).
pub fn local_settings_path() -> PathBuf {
    PathBuf::from("autoshop.local.json")
}

/// Read the local settings file, if present. A missing or malformed file yields
/// defaults (we never block startup on it).
pub fn load_local_settings() -> LocalSettings {
    let p = local_settings_path();
    match std::fs::read_to_string(&p) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => LocalSettings::default(),
    }
}

/// Persist the local settings file (the POST /api/settings target).
pub fn save_local_settings(s: &LocalSettings) -> std::io::Result<PathBuf> {
    let p = local_settings_path();
    std::fs::write(&p, serde_json::to_string_pretty(s).unwrap_or_default())?;
    Ok(p)
}

pub struct Config {
    // --- image role: the vision proposer (OpenAI-compatible API only) ---------
    /// API key for the image (vision) role + generative edits. `None` ⇒ the
    /// proposer falls back to the heuristic baseline.
    pub openai_api_key: Option<String>,
    pub openai_model: String,
    pub openai_base_url: String,
    /// Image model for generative retouch/reimagine (V2_PLAN §5).
    pub openai_image_model: String,
    /// Output quality tier for generative edits: low | medium | high | auto.
    pub openai_image_quality: String,

    // --- analysis role: the verifier (oauth = claude CLI, or api = OpenAI) -----
    /// `"oauth"` (default; the `claude` CLI) or `"api"` (OpenAI-compatible chat).
    pub analysis_provider: String,
    /// Model for the analysis role: a `claude` alias/id for oauth (default
    /// `opus`), or a chat model id for api.
    pub analysis_model: String,
    /// Path/name of the `claude` executable (oauth analysis, reuses Claude OAuth).
    pub claude_bin: String,
    /// API key + base for the `api` analysis provider (independent of the image key).
    pub analysis_api_key: Option<String>,
    pub analysis_base_url: String,

    // --- AI denoise sidecar ---------------------------------------------------
    /// Python interpreter for the AI-denoise sidecar (`python/denoise.py`).
    pub python_bin: String,
    /// SCUNet weight set (color_real_psnr default; see python/denoise.py).
    pub denoise_model: String,
    pub denoise_script: String,
    pub denoise_cache: String,

    /// How strongly to lean on the user's historical edit style, 0.0..1.0.
    pub style_strength: f32,
}

impl Config {
    pub fn load() -> Self {
        // .env first (absence is fine; never prints the key), then the local file.
        let _ = dotenvy::dotenv();
        let nonempty = |k: &str| env::var(k).ok().filter(|s| !s.trim().is_empty());
        let local = load_local_settings();
        // local-file value wins over env; `pick` returns the first non-empty.
        let pick = |file: &Option<String>, e: Option<String>, default: &str| -> String {
            file.as_ref()
                .filter(|s| !s.trim().is_empty())
                .cloned()
                .or(e)
                .unwrap_or_else(|| default.to_string())
        };
        let pick_opt = |file: &Option<String>, e: Option<String>| -> Option<String> {
            file.as_ref()
                .filter(|s| !s.trim().is_empty())
                .cloned()
                .or(e)
        };

        let default_base = "https://api.openai.com/v1";
        Config {
            openai_api_key: pick_opt(&local.image_api_key, nonempty("OPENAI_API_KEY")),
            openai_model: pick(&local.image_model, nonempty("AUTOSHOP_OPENAI_MODEL"), "gpt-5.5"),
            openai_base_url: pick(&local.image_base_url, nonempty("AUTOSHOP_OPENAI_BASE_URL"), default_base),
            openai_image_model: pick(
                &local.image_gen_model,
                nonempty("AUTOSHOP_OPENAI_IMAGE_MODEL"),
                "gpt-image-1.5",
            ),
            openai_image_quality: nonempty("AUTOSHOP_IMAGE_QUALITY").unwrap_or_else(|| "high".to_string()),

            analysis_provider: pick(
                &local.analysis_provider,
                nonempty("AUTOSHOP_ANALYSIS_PROVIDER"),
                "oauth",
            ),
            analysis_model: pick(
                &local.analysis_model,
                nonempty("AUTOSHOP_ANALYSIS_MODEL").or_else(|| nonempty("AUTOSHOP_CLAUDE_MODEL")),
                "opus",
            ),
            claude_bin: nonempty("AUTOSHOP_CLAUDE_BIN").unwrap_or_else(|| "claude".to_string()),
            analysis_api_key: pick_opt(&local.analysis_api_key, nonempty("AUTOSHOP_ANALYSIS_API_KEY")),
            analysis_base_url: pick(
                &local.analysis_base_url,
                nonempty("AUTOSHOP_ANALYSIS_BASE_URL"),
                default_base,
            ),

            python_bin: nonempty("AUTOSHOP_PYTHON").unwrap_or_else(|| "python".to_string()),
            denoise_model: nonempty("AUTOSHOP_DENOISE_MODEL")
                .unwrap_or_else(|| "color_real_psnr".to_string()),
            denoise_script: nonempty("AUTOSHOP_DENOISE_SCRIPT")
                .unwrap_or_else(|| "python/denoise.py".to_string()),
            denoise_cache: nonempty("AUTOSHOP_DENOISE_CACHE")
                .unwrap_or_else(|| "python/weights".to_string()),
            style_strength: nonempty("AUTOSHOP_STYLE_STRENGTH")
                .and_then(|s| s.parse::<f32>().ok())
                .unwrap_or(0.3)
                .clamp(0.0, 1.0),
        }
    }

    /// True if the analysis role is configured to use the OpenAI-compatible API.
    pub fn analysis_is_api(&self) -> bool {
        self.analysis_provider.eq_ignore_ascii_case("api")
    }
}
