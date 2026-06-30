//! List the model ids an OpenAI-compatible account can use (`GET {base}/models`),
//! so the Settings UI can offer a real pick-list instead of a blank text box the
//! user has to guess into. Pure read; never logs the key.

use anyhow::{anyhow, Context, Result};

/// Fetch the sorted, de-duplicated list of model ids from `{base_url}/models`.
/// `base_url` is the OpenAI-compatible API root (e.g. `https://api.openai.com/v1`).
pub fn list_models(base_url: &str, api_key: &str) -> Result<Vec<String>> {
    if api_key.trim().is_empty() {
        return Err(anyhow!("no API key — set one in Settings (save first), then fetch"));
    }
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let resp = ureq::get(&url)
        .timeout(std::time::Duration::from_secs(20)) // don't hang the UI on a dead endpoint
        .set("Authorization", &format!("Bearer {api_key}"))
        .call();
    let value: serde_json::Value = match resp {
        Ok(r) => r.into_json().context("parse /models response")?,
        Err(ureq::Error::Status(code, r)) => {
            let b = r.into_string().unwrap_or_default();
            return Err(anyhow!("models API {code}: {b}"));
        }
        Err(ureq::Error::Transport(t)) => return Err(anyhow!("transport: {t}")),
    };
    let mut ids: Vec<String> = value
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| anyhow!("no data[] in /models response: {value}"))?
        .iter()
        .filter_map(|m| m.get("id").and_then(|s| s.as_str()).map(str::to_string))
        .collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// True if `id` looks like an image-generation model (gpt-image-*, *image*).
pub fn is_image_model(id: &str) -> bool {
    id.contains("image")
}

/// True if `id` looks like a text/vision chat model (the proposer/verifier roles),
/// excluding audio/embedding/etc. variants that can't do vision-chat.
pub fn is_chat_model(id: &str) -> bool {
    let bad = ["audio", "realtime", "transcribe", "tts", "whisper", "embedding", "moderation"];
    let family = id.starts_with("gpt")
        || id.starts_with("chatgpt")
        || (id.starts_with('o') && id.as_bytes().get(1).is_some_and(u8::is_ascii_digit));
    family && !is_image_model(id) && !bad.iter().any(|b| id.contains(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_real_account_ids() {
        // Ids verified present in a live /models probe (2026-06).
        assert!(is_image_model("gpt-image-2"));
        assert!(is_image_model("gpt-image-1.5"));
        assert!(is_image_model("chatgpt-image-latest"));
        assert!(!is_chat_model("gpt-image-2")); // an image model is not a chat model

        assert!(is_chat_model("gpt-5.5"));
        assert!(is_chat_model("gpt-4o"));
        assert!(is_chat_model("o3"));
        assert!(is_chat_model("o4-mini"));
        assert!(!is_chat_model("gpt-4o-audio-preview"));
        assert!(!is_chat_model("text-embedding-3-large"));
        assert!(!is_image_model("gpt-5.5"));
    }
}
