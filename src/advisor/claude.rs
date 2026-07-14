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

use super::{balanced_objects, build_verify_prompt, strip_code_fence, Advisor, AdvisorError, Verdict};

pub struct ClaudeProvider {
    bin: String,
    model: String,
}

impl ClaudeProvider {
    pub fn new(cfg: &Config) -> Self {
        Self {
            bin: cfg.claude_bin.clone(),
            model: cfg.analysis_model.clone(),
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

        let mut cmd = Command::new(&self.bin);
        cmd.args([
            "-p",
            "--bare",
            "--model",
            &self.model,
            "--output-format",
            "json",
        ])
        .arg(&prompt);
        // Run the child from a neutral cwd. Headless `claude` treats its cwd as
        // the workspace: if that directory carries a `.claude/settings.json`
        // (any project checkout) and the workspace was never trusted
        // interactively, the CLI errors out — "Ignoring N permissions.allow
        // entries … this workspace has not been trusted" + exit 1 — and the
        // whole analysis fails. Verified live 2026-07-14: identical invocation
        // from `D:/Projects/Autoshop` prints the trust error, from a fresh temp
        // dir it does not. The verifier is a pure stdin/stdout call that never
        // touches files, so the temp dir (always present) is a safe workspace.
        cmd.current_dir(std::env::temp_dir());
        // Don't flash a console window when the windowed GUI spawns this CLI child.
        crate::hide_child_console(&mut cmd);
        let output = cmd.output()?;

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
