//! Runtime configuration for the AI providers.
//!
//! Secrets (the OpenAI key) come **only** from the environment — loaded from a
//! gitignored `.env` via `dotenvy` if present, never from a committed file and
//! never logged. Non-secret knobs (model ids, the `claude` binary path) also
//! read from env with sensible defaults so the tool runs with zero config.

use std::env;

pub struct Config {
    /// OpenAI key for the GPT vision advisor. `None` ⇒ GPT path is unavailable
    /// and the CLI falls back to the heuristic proposer.
    pub openai_api_key: Option<String>,
    pub openai_model: String,
    pub openai_base_url: String,
    /// Path/name of the `claude` executable (reused for the verifier via OAuth).
    pub claude_bin: String,
    pub claude_model: String,
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
        }
    }
}
