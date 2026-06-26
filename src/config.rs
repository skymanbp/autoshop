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
    /// Image model for generative retouch/reimagine (V2_PLAN §5). Consumed by
    /// the generative module landing next; allow it to exist until then.
    #[allow(dead_code)]
    pub openai_image_model: String,
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
        }
    }
}
