//! Runtime configuration for the AI providers.
//!
//! Secrets (the OpenAI key) come **only** from the environment — loaded from a
//! gitignored `.env` via `dotenvy` if present, never from a committed file and
//! never logged. Non-secret knobs (model ids, the `claude` binary path) also
//! read from env with sensible defaults so the tool runs with zero config.
//!
//! Style is handled by similarity retrieval (`src/style.rs`), not by a global
//! calibration baked in here.

use std::env;

pub struct Config {
    /// OpenAI key for the GPT vision advisor + image edits. `None` ⇒ those paths
    /// are unavailable and the CLI falls back to the heuristic proposer.
    pub openai_api_key: Option<String>,
    pub openai_model: String,
    pub openai_base_url: String,
    /// Path/name of the `claude` executable (reused for the verifier via OAuth).
    pub claude_bin: String,
    pub claude_model: String,
    /// Image model for generative retouch/reimagine (V2_PLAN §5).
    pub openai_image_model: String,
    /// Python interpreter for the AI-denoise sidecar (`python/denoise.py`).
    pub python_bin: String,
    /// SCUNet weight set: color_real_psnr (blind, default) / color_real_gan /
    /// color_15 / color_25 / color_50 (non-blind AWGN tiers).
    pub denoise_model: String,
    /// Path to the denoise sidecar script (resolved from cwd by default).
    pub denoise_script: String,
    /// Where the sidecar caches its model weights.
    pub denoise_cache: String,
}

impl Config {
    pub fn load() -> Self {
        // Load .env if present; absence is fine. Does not print the key.
        let _ = dotenvy::dotenv();
        let nonempty = |k: &str| env::var(k).ok().filter(|s| !s.trim().is_empty());
        Config {
            openai_api_key: nonempty("OPENAI_API_KEY"),
            openai_model: nonempty("AUTOSHOP_OPENAI_MODEL").unwrap_or_else(|| "gpt-5.5".to_string()),
            openai_base_url: nonempty("AUTOSHOP_OPENAI_BASE_URL")
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            claude_bin: nonempty("AUTOSHOP_CLAUDE_BIN").unwrap_or_else(|| "claude".to_string()),
            claude_model: nonempty("AUTOSHOP_CLAUDE_MODEL")
                .unwrap_or_else(|| "claude-sonnet-4-6".to_string()),
            openai_image_model: nonempty("AUTOSHOP_OPENAI_IMAGE_MODEL")
                .unwrap_or_else(|| "gpt-image-1.5".to_string()),
            python_bin: nonempty("AUTOSHOP_PYTHON").unwrap_or_else(|| "python".to_string()),
            denoise_model: nonempty("AUTOSHOP_DENOISE_MODEL")
                .unwrap_or_else(|| "color_real_psnr".to_string()),
            denoise_script: nonempty("AUTOSHOP_DENOISE_SCRIPT")
                .unwrap_or_else(|| "python/denoise.py".to_string()),
            denoise_cache: nonempty("AUTOSHOP_DENOISE_CACHE")
                .unwrap_or_else(|| "python/weights".to_string()),
        }
    }
}
