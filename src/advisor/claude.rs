//! Claude provider — the data-only acceptance verifier ("收货验证").
//!
//! Shells out to the local `claude` CLI in print mode, reusing the user's
//! Claude Code OAuth (no API key). Invocation and envelope shape were verified
//! live against `claude` 2.1.210 on this machine:
//!   `claude -p --setting-sources "" --strict-mcp-config
//!    --disable-slash-commands --model <m> --output-format json "<prompt>"`
//!   → `{"type":"result","is_error":false,"result":"<model text>", ...}`
//! Isolation flags instead of `--bare`: since at least 2.1.210, `--bare`
//! documents "Anthropic auth is strictly ANTHROPIC_API_KEY or apiKeyHelper via
//! --settings (OAuth and keychain are never read)" — under `--bare` this
//! provider can never bill the user's subscription (it fails "Not logged in",
//! or worse, silently bills a stray API key). The three flags above reproduce
//! what `--bare` protected against — plugins/skills/hooks auto-loading into
//! the session (0 plugins enabled, 0 hooks registered, no user skills, clean
//! stderr; measured 2026-07-17) — while keeping the stored OAuth usable.

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
            // NOT `--bare`: it never reads the stored OAuth login (see the
            // module docs) — these three flags give the same isolation while
            // keeping the subscription auth.
            "--setting-sources",
            "",
            "--strict-mcp-config",
            "--disable-slash-commands",
            "--model",
            &self.model,
            "--output-format",
            "json",
        ])
        .arg(&prompt);
        // This provider bills the user's Claude subscription via the stored
        // OAuth login by design. A stray ANTHROPIC_API_KEY in the inherited
        // environment (e.g. a machine-wide env var meant for other tools)
        // takes precedence over that login and silently re-routes billing to
        // metered API credits — measured live 2026-07-17: with the key present
        // the identical invocation fails 400 "Credit balance is too low";
        // without it, the subscription answers. Strip it from the child.
        cmd.env_remove("ANTHROPIC_API_KEY");
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

        // The CLI envelope; we only need these fields (serde ignores the rest).
        #[derive(serde::Deserialize)]
        struct Envelope {
            is_error: bool,
            result: String,
        }

        if !output.status.success() {
            // A failed headless run usually exits 1 with an EMPTY stderr and
            // the real reason inside the stdout JSON envelope ("Credit balance
            // is too low", "Not logged in · Please run /login", …) — measured
            // 2026-07-17. Surface that text instead of a blank CliFailed.
            if let Ok(env) = serde_json::from_slice::<Envelope>(&output.stdout)
                && env.is_error
            {
                return Err(AdvisorError::ClaudeError(env.result));
            }
            let mut stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if stderr.is_empty() {
                let head: String =
                    String::from_utf8_lossy(&output.stdout).chars().take(300).collect();
                stderr = format!("(empty stderr) stdout: {head}");
            }
            return Err(AdvisorError::CliFailed {
                bin: self.bin.clone(),
                code: output.status.code(),
                stderr,
            });
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
