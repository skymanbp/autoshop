//! The AI advisor layer — the unified provider framework (统一 API 框架).
//!
//! One [`Advisor`] trait, two roles (see `docs/ARCHITECTURE.md` §3):
//!   * **propose** — a vision model looks at the preview and emits an
//!     [`EditRecipe`] (GPT in production; [`HeuristicProposer`] as a no-key
//!     baseline).
//!   * **verify** — Claude, data-only, acceptance-checks the recipe before it
//!     is applied (the "收货验证" role), via the `claude` CLI over OAuth.
//!
//! M1 is synchronous: a single image flows propose → verify sequentially, so we
//! avoid the cost/complexity of an async runtime + `async_trait`. Concurrency
//! (batch, or parallel GPT/Claude) can move this to async later if needed.

mod claude;
mod heuristic;
mod openai;

pub use claude::ClaudeProvider;
pub use heuristic::HeuristicProposer;
pub use openai::OpenAiProvider;

use crate::decode::{Histogram, Meta};
use crate::recipe::EditRecipe;

/// JPEG preview bytes handed to a vision advisor.
pub struct Preview {
    pub jpeg: Vec<u8>,
}

/// The verifier's decision on a proposed recipe.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Accept,
    Revise,
    Reject,
}

/// Acceptance-verification outcome (the analyst/verifier role).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Verdict {
    pub decision: Decision,
    #[serde(default)]
    pub reasons: Vec<String>,
    /// When `Revise`/`Reject`, a short instruction for the next propose round.
    #[serde(default)]
    pub revised_hint: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AdvisorError {
    #[error("missing config: {0}")]
    Missing(String),
    #[error("subprocess io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{bin} exited {code:?}: {stderr}")]
    CliFailed { bin: String, code: Option<i32>, stderr: String },
    #[error("claude reported is_error: {0}")]
    ClaudeError(String),
    #[error("claude CLI envelope was not JSON ({source}); first bytes: {head:?}")]
    BadEnvelope { source: serde_json::Error, head: String },
    #[error("claude's verdict was not valid JSON ({source}); got: {got:?}")]
    BadVerdict { source: serde_json::Error, got: String },
    #[error("http {status}: {body}")]
    Http { status: u16, body: String },
    #[error("http transport: {0}")]
    Transport(String),
    #[error("advisor '{0}' does not serve this role")]
    Unsupported(&'static str),
}

/// One AI advisor. A provider implements the role(s) it serves; the unserved
/// role returns [`AdvisorError::Unsupported`] rather than panicking, so a single
/// registry can hold mixed providers.
pub trait Advisor {
    fn name(&self) -> &'static str;

    /// Image role: preview + features → recipe. `hint` carries the verifier's
    /// revision instruction on a second round (ignored by providers that can't
    /// use it).
    fn propose(
        &self,
        _img: &Preview,
        _meta: &Meta,
        _hist: &Histogram,
        _hint: Option<&str>,
    ) -> Result<EditRecipe, AdvisorError> {
        Err(AdvisorError::Unsupported(self.name()))
    }

    /// Whether this provider can act on a revision `hint` (gates the revise loop).
    fn supports_revision(&self) -> bool {
        false
    }

    /// Analyst role: data-only acceptance check of a proposed recipe.
    fn verify(
        &self,
        _recipe: &EditRecipe,
        _meta: &Meta,
        _hist: &Histogram,
    ) -> Result<Verdict, AdvisorError> {
        Err(AdvisorError::Unsupported(self.name()))
    }
}

/// Compact, prompt-friendly histogram summary (the full 4×256 bins are too
/// large and noisy to put in a prompt). Reports clipping, mean luma, and a
/// 16-bucket luma distribution as percentages.
pub fn hist_summary(h: &Histogram) -> String {
    let total: u64 = h.luma.iter().map(|&v| v as u64).sum::<u64>().max(1);
    let weighted: u64 = h
        .luma
        .iter()
        .enumerate()
        .map(|(i, &v)| i as u64 * v as u64)
        .sum();
    let mean = weighted as f32 / total as f32;

    // 256 -> 16 buckets, each as % of pixels.
    let mut buckets = [0u64; 16];
    for (i, &v) in h.luma.iter().enumerate() {
        buckets[i / 16] += v as u64;
    }
    let dist: Vec<String> = buckets
        .iter()
        .map(|&b| format!("{:.0}", 100.0 * b as f32 / total as f32))
        .collect();

    format!(
        "mean_luma={mean:.0}/255, clip_black={:.2}%, clip_white={:.2}%, luma_16buckets_pct=[{}]",
        h.clip_black_pct,
        h.clip_white_pct,
        dist.join(","),
    )
}

/// Strip a leading/trailing markdown code fence if the model wrapped its JSON,
/// then return the inner text. Idempotent for already-bare JSON.
pub(crate) fn strip_code_fence(s: &str) -> &str {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix("```") {
        // Drop an optional language tag on the first line, and the trailing ```.
        let rest = rest.splitn(2, '\n').nth(1).unwrap_or(rest);
        rest.trim().strip_suffix("```").unwrap_or(rest).trim()
    } else {
        t
    }
}

/// Extract the first balanced top-level JSON object/array from text that may
/// have prose around it (LLMs intermittently preface or wrap their JSON, so a
/// bare `from_str` can flake). String contents and escapes are respected so
/// braces inside strings don't throw off the depth count. `None` if no opener.
pub(crate) fn extract_json_value(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{' || b == b'[')?;
    let (open, close) = if bytes[start] == b'{' { (b'{', b'}') } else { (b'[', b']') };
    let (mut depth, mut in_str, mut esc) = (0i32, false, false);
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
        } else if b == b'"' {
            in_str = true;
        } else if b == open {
            depth += 1;
        } else if b == close {
            depth -= 1;
            if depth == 0 {
                return Some(&s[start..=i]);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_handles_prose_and_fences() {
        let bare = r#"{"decision":"accept","reasons":[]}"#;
        assert_eq!(extract_json_value(bare), Some(bare));
        assert_eq!(
            extract_json_value("Here is my verdict:\n```json\n{\"a\":1}\n```\nDone."),
            Some(r#"{"a":1}"#)
        );
        // braces inside a string must not end the object early
        let tricky = r#"prefix {"reasons":["has } brace","ok"]} suffix"#;
        assert_eq!(extract_json_value(tricky), Some(r#"{"reasons":["has } brace","ok"]}"#));
        assert_eq!(extract_json_value("no json here"), None);
    }
}
