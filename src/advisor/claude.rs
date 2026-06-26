//! Claude provider — the data-only acceptance verifier ("收货验证").
//!
//! Shells out to the local `claude` CLI in print mode, reusing the user's
//! Claude Code OAuth (no API key). Invocation and envelope shape were verified
//! live against `claude` 2.1.158 on this machine:
//!   `claude -p --bare --model <m> --output-format json "<prompt>"`
//!   → `{"type":"result","is_error":false,"result":"<model text>", ...}`
//! `--bare` is mandatory: without it the session's plugins/skills auto-load and
//! pollute `result` (and cost ~16×).

use std::process::Command;

use crate::config::Config;
use crate::decode::{Histogram, Meta};
use crate::recipe::EditRecipe;

use super::{balanced_objects, hist_summary, strip_code_fence, Advisor, AdvisorError, Verdict};

pub struct ClaudeProvider {
    bin: String,
    model: String,
}

impl ClaudeProvider {
    pub fn new(cfg: &Config) -> Self {
        Self {
            bin: cfg.claude_bin.clone(),
            model: cfg.claude_model.clone(),
        }
    }
}

impl Advisor for ClaudeProvider {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn verify(
        &self,
        recipe: &EditRecipe,
        meta: &Meta,
        hist: &Histogram,
    ) -> Result<Verdict, AdvisorError> {
        let prompt = build_verify_prompt(recipe, meta, hist)?;

        let output = Command::new(&self.bin)
            .args([
                "-p",
                "--bare",
                "--model",
                &self.model,
                "--output-format",
                "json",
            ])
            .arg(&prompt)
            .output()?;

        if !output.status.success() {
            return Err(AdvisorError::CliFailed {
                bin: self.bin.clone(),
                code: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        // The CLI envelope; we only need these fields (serde ignores the rest).
        #[derive(serde::Deserialize)]
        struct Envelope {
            is_error: bool,
            result: String,
        }
        let env: Envelope =
            serde_json::from_slice(&output.stdout).map_err(|source| AdvisorError::BadEnvelope {
                source,
                head: String::from_utf8_lossy(&output.stdout).chars().take(300).collect(),
            })?;
        if env.is_error {
            return Err(AdvisorError::ClaudeError(env.result));
        }

        // `result` is the model's text — instructed to be exactly the Verdict
        // JSON, but LLMs intermittently add a fence or a reasoning preamble. Try
        // the bare (fence-stripped) text first; otherwise scan every balanced
        // {...} object and keep the LAST one that parses as a Verdict (the
        // model's final answer, past any example/prose objects).
        let cleaned = strip_code_fence(&env.result);
        let verdict: Verdict = match serde_json::from_str::<Verdict>(cleaned) {
            Ok(v) => v,
            Err(first_err) => {
                let mut found = None;
                for cand in balanced_objects(&env.result) {
                    if let Ok(v) = serde_json::from_str::<Verdict>(cand) {
                        found = Some(v);
                    }
                }
                found.ok_or_else(|| AdvisorError::BadVerdict {
                    source: first_err,
                    got: env.result.chars().take(400).collect(),
                })?
            }
        };
        Ok(verdict)
    }
}

fn build_verify_prompt(
    recipe: &EditRecipe,
    meta: &Meta,
    hist: &Histogram,
) -> Result<String, AdvisorError> {
    let recipe_json = serde_json::to_string_pretty(recipe)?;
    let meta_json = serde_json::to_string(meta)?;
    Ok(format!(
        "You are a photo-edit QA verifier. You do NOT see the image — judge ONLY from the data below.\n\
Decide whether this proposed RAW develop recipe is safe to apply. Check, concretely:\n\
- every slider is within its documented range (exposure_ev -5..5; most sliders -100..100; sharpening 0..150; confidence 0..1);\n\
- adjustments are consistent with the metadata + histogram (e.g. do NOT brighten when highlights already clip; do NOT crush shadows that are already dark; large moves need justification);\n\
- the rationale matches the numbers and confidence is adequate to auto-apply.\n\n\
METADATA: {meta_json}\n\
HISTOGRAM: {hist}\n\
PROPOSED RECIPE:\n{recipe_json}\n\n\
Output ONLY the JSON object: no reasoning, no preamble, no markdown fence. Your entire reply must start with '{{' and end with '}}'. Shape:\n\
{{\"decision\":\"accept\"|\"revise\"|\"reject\",\"reasons\":[\"short reason\", ...],\"revised_hint\":\"a short instruction for the next attempt if revise/reject, else null\"}}",
        meta_json = meta_json,
        hist = hist_summary(hist),
        recipe_json = recipe_json,
    ))
}
