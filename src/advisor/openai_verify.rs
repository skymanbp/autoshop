//! OpenAI-compatible **API** verifier — the analysis role when its provider is
//! `api` instead of the OAuth `claude` CLI. Uses the standard Chat Completions
//! endpoint (`/chat/completions`), so it also works with any OpenAI-compatible
//! server (point `analysis_base_url` at it). Text-only: like the Claude verifier,
//! it judges the recipe from data, never the image.

use serde_json::{json, Value};

use crate::config::Config;
use crate::decode::{Histogram, Meta};
use crate::recipe::EditRecipe;

use super::{balanced_objects, build_verify_prompt, strip_code_fence, Advisor, AdvisorError, Verdict};

pub struct OpenAiVerifier {
    api_key: Option<String>,
    model: String,
    base_url: String,
}

impl OpenAiVerifier {
    pub fn new(cfg: &Config) -> Self {
        Self {
            api_key: cfg.analysis_api_key.clone(),
            model: cfg.analysis_model.clone(),
            base_url: cfg.analysis_base_url.clone(),
        }
    }
}

impl Advisor for OpenAiVerifier {
    fn name(&self) -> &'static str {
        "openai-verify"
    }

    fn verify(
        &self,
        recipe: &EditRecipe,
        meta: &Meta,
        hist: &Histogram,
    ) -> Result<Verdict, AdvisorError> {
        let key = self.api_key.as_ref().ok_or_else(|| {
            AdvisorError::Missing(
                "analysis API key (set it in Settings, or AUTOSHOP_ANALYSIS_API_KEY)".into(),
            )
        })?;
        let prompt = build_verify_prompt(recipe, meta, hist)?;
        let body = json!({
            "model": self.model,
            "messages": [{ "role": "user", "content": prompt }],
            "temperature": 0
        });
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let resp = ureq::post(&url)
            .set("Authorization", &format!("Bearer {key}"))
            .set("Content-Type", "application/json")
            .send_json(body);

        let value: Value = match resp {
            Ok(r) => r.into_json().map_err(|e| AdvisorError::Transport(e.to_string()))?,
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                return Err(AdvisorError::Http { status: code, body });
            }
            Err(ureq::Error::Transport(t)) => return Err(AdvisorError::Transport(t.to_string())),
        };

        let text = value
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .ok_or_else(|| {
                AdvisorError::Transport(format!(
                    "no choices[0].message.content in chat response: {value}"
                ))
            })?;

        // The model is told to emit bare Verdict JSON; tolerate a fence/preamble
        // exactly like the Claude path (try bare, then the last balanced object).
        let cleaned = strip_code_fence(text);
        match serde_json::from_str::<Verdict>(cleaned) {
            Ok(v) => Ok(v),
            Err(first_err) => {
                let mut found = None;
                for cand in balanced_objects(text) {
                    if let Ok(v) = serde_json::from_str::<Verdict>(cand) {
                        found = Some(v);
                    }
                }
                found.ok_or_else(|| AdvisorError::BadVerdict {
                    source: first_err,
                    got: text.chars().take(400).collect(),
                })
            }
        }
    }
}
