//! OpenAI provider — the GPT **vision** advisor (image → `EditRecipe`).
//!
//! Uses the Responses API with a strict `json_schema` so the model can only
//! return our recipe shape, and sends the preview as a base64 `input_image`
//! (request shape per `docs/M1_PLAN.md` §3).
//!
//! ⚠️ UNTESTED against the live API — no `OPENAI_API_KEY` was available at build
//! time, so the request/response shapes are written from the documented API but
//! not yet round-tripped. `propose` returns [`AdvisorError::Missing`] until a key
//! is configured; the first real call must be validated (and likely tweaked)
//! against OpenAI's current Responses API.

use base64::Engine;
use serde_json::{json, Value};

use crate::config::Config;
use crate::decode::{Histogram, Meta};
use crate::recipe::EditRecipe;

use super::{hist_summary, strip_code_fence, Advisor, AdvisorError, Preview};

pub struct OpenAiProvider {
    api_key: Option<String>,
    model: String,
    base_url: String,
}

impl OpenAiProvider {
    pub fn new(cfg: &Config) -> Self {
        Self {
            api_key: cfg.openai_api_key.clone(),
            model: cfg.openai_model.clone(),
            base_url: cfg.openai_base_url.clone(),
        }
    }
}

impl Advisor for OpenAiProvider {
    fn name(&self) -> &'static str {
        "openai"
    }

    fn propose(
        &self,
        img: &Preview,
        meta: &Meta,
        hist: &Histogram,
        reference: Option<&str>,
        guidance: Option<&str>,
        hint: Option<&str>,
    ) -> Result<EditRecipe, AdvisorError> {
        let key = self
            .api_key
            .as_ref()
            .ok_or_else(|| AdvisorError::Missing("OPENAI_API_KEY".into()))?;

        let b64 = base64::engine::general_purpose::STANDARD.encode(&img.jpeg);
        let meta_json = serde_json::to_string(meta).map_err(AdvisorError::Json)?;
        let mut instruction = format!(
            "You are a master photo-edit colourist. Look at this RAW preview and its \
metadata/histogram and return an EditRecipe that develops it into a FINISHED \
photograph — a 成片 — not a flat, 'safe' tweak, but also NOT an over-cooked one. A finished \
develop COMMITS to a clear look: set ONE primary tonal anchor — EITHER a moderate Contrast slider \
OR a 3-5 point `tone_curve` forming a gentle S (placed black point, bright shoulder), NOT both at \
full strength (if the tone_curve already makes an S, keep Contrast modest, and vice versa) — then \
place the white and black points and shape colour toward what the scene wants. \
CALIBRATE THE STRENGTH of the grade to a tasteful, restrained finished look; and when a REFERENCE \
of this photographer's own past edits is provided below, MATCH its level of contrast, tonal depth \
and saturation — do NOT exceed it. A committed grade is not a maximal one. Concretely: place the \
black and white points deliberately but do NOT slam them (avoid crushing blacks or blowing whites \
past the reference habit), and use vibrance, saturation and clarity SPARINGLY — only as much as the \
reference shows; stacked vibrance+saturation+clarity reads as over-processed. Stay well inside the \
documented ranges (they are safety bounds, not a target). \
Use the `masks` array PROACTIVELY to dodge and burn like a darkroom print: even with NO explicit \
user request, add 1-2 local masks to lift the subject, hold back a hot sky, or deepen distracting \
corners when it makes the photo read better. Masks are tonal/colour adjustments through gradient \
masks — never painting, generating, or adding content. If a global edit alone achieves the look, \
leave masks empty. Prefer a linear gradient (kind=linear; zero_* = start edge, full_* = end edge, \
in 0..1 frame coords) for skies/horizons/foregrounds; radial (kind=radial) for subjects/vignettes. \
When the USER DIRECTION names a SPECIFIC AREA (e.g. 'that corner', 'the sky', 'the subject', \
'top-left', 'this part is too noisy', 'brighten her face') translate it into a mask placed over \
THAT area and set the relevant local sliders — including local `noise_reduction` (0..100) for a \
noisy region. Use 1-3 masks for such localized requests. \
Local slider values use the same scale as the globals. METADATA: {meta_json}  HISTOGRAM: {hist}",
            meta_json = meta_json,
            hist = hist_summary(hist),
        );
        if let Some(rf) = reference {
            instruction.push_str("  ");
            instruction.push_str(rf);
        }
        if let Some(g) = guidance {
            instruction.push_str("  USER DIRECTION (a specific request from the photographer — \
follow it closely): ");
            instruction.push_str(g);
        }
        if let Some(h) = hint {
            instruction.push_str(&format!("  REVISION NOTE from the verifier: {h}"));
        }

        let body = json!({
            "model": self.model,
            "input": [{
                "role": "user",
                "content": [
                    { "type": "input_text", "text": instruction },
                    { "type": "input_image",
                      "image_url": format!("data:image/jpeg;base64,{b64}"),
                      "detail": "high" }
                ]
            }],
            "text": { "format": {
                "type": "json_schema",
                "name": "edit_recipe",
                "strict": true,
                "schema": edit_recipe_schema()
            }}
        });

        let url = format!("{}/responses", self.base_url.trim_end_matches('/'));
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
            Err(ureq::Error::Transport(t)) => {
                return Err(AdvisorError::Transport(t.to_string()))
            }
        };

        let recipe_json = extract_output_text(&value).ok_or_else(|| AdvisorError::Transport(
            "could not locate structured output in OpenAI response (shape mismatch — see openai.rs)".into(),
        ))?;
        let mut recipe: EditRecipe = serde_json::from_str(strip_code_fence(&recipe_json))?;
        recipe.clamp(); // never trust the model's ranges
        Ok(recipe)
    }
}

/// JSON Schema for [`EditRecipe`] in OpenAI strict mode: every property listed
/// in `required`, `additionalProperties:false`, optionals expressed as nullable.
/// Mirrors `src/recipe.rs` — keep in sync if the recipe changes.
fn edit_recipe_schema() -> Value {
    // Closure (not a single Value) so the schema can be reused across the
    // nested object schemas without move issues.
    let num = || json!({"type": "number"});

    // MaskGeometry tagged enum (#[serde(tag="kind")]) → anyOf of the two
    // variants; each is strict (all props required, additionalProperties:false).
    let mask_geometry = json!({
        "anyOf": [
            {"type": "object", "additionalProperties": false,
             "required": ["kind","zero_x","zero_y","full_x","full_y"],
             "properties": {"kind": {"type": "string", "enum": ["linear"]},
                "zero_x": num(), "zero_y": num(), "full_x": num(), "full_y": num()}},
            {"type": "object", "additionalProperties": false,
             "required": ["kind","top","left","bottom","right","feather","roundness","flipped"],
             "properties": {"kind": {"type": "string", "enum": ["radial"]},
                "top": num(), "left": num(), "bottom": num(), "right": num(),
                "feather": num(), "roundness": num(), "flipped": {"type": "boolean"}}}
        ]
    });
    let local_adjustment = json!({
        "type": "object", "additionalProperties": false,
        "required": ["mask","name","amount","inverted","exposure_ev","contrast","highlights",
            "shadows","whites","blacks","clarity","dehaze","texture","saturation","temperature","tint",
            "noise_reduction"],
        "properties": {
            "mask": mask_geometry,
            "name": {"type": "string"}, "amount": num(), "inverted": {"type": "boolean"},
            "exposure_ev": num(), "contrast": num(), "highlights": num(), "shadows": num(),
            "whites": num(), "blacks": num(), "clarity": num(), "dehaze": num(),
            "texture": num(), "saturation": num(), "temperature": num(), "tint": num(),
            "noise_reduction": num()
        }
    });
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["version","exposure_ev","contrast","highlights","shadows","whites","blacks",
            "temperature_k","tint","vibrance","saturation","clarity","dehaze","sharpening",
            "noise_reduction","straighten_deg","crop","tone_curve","masks","rationale","confidence"],
        "properties": {
            "version": {"type": "integer"},
            "exposure_ev": num(), "contrast": num(), "highlights": num(), "shadows": num(),
            "whites": num(), "blacks": num(),
            "temperature_k": {"type": ["number","null"]}, "tint": num(),
            "vibrance": num(), "saturation": num(), "clarity": num(), "dehaze": num(),
            "sharpening": num(), "noise_reduction": num(), "straighten_deg": num(),
            "crop": {"type": ["object","null"], "additionalProperties": false,
                "required": ["left","top","right","bottom"],
                "properties": {"left": num(), "top": num(), "right": num(), "bottom": num()}},
            "tone_curve": {"type": "array", "items": {"type": "object",
                "additionalProperties": false, "required": ["input","output"],
                "properties": {"input": {"type": "integer"}, "output": {"type": "integer"}}}},
            "masks": {"type": "array", "items": local_adjustment},
            "rationale": {"type": "string"},
            "confidence": num()
        }
    })
}

/// Pull the model's text out of a Responses-API reply (convenience field first,
/// then walk `output[].content[]`).
fn extract_output_text(v: &Value) -> Option<String> {
    if let Some(s) = v.get("output_text").and_then(Value::as_str) {
        return Some(s.to_string());
    }
    for item in v.get("output")?.as_array()? {
        if let Some(content) = item.get("content").and_then(Value::as_array) {
            for c in content {
                if c.get("type").and_then(Value::as_str) == Some("output_text")
                    && let Some(s) = c.get("text").and_then(Value::as_str) {
                        return Some(s.to_string());
                    }
            }
        }
    }
    None
}
