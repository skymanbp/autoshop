//! Heuristic proposer — a no-AI baseline used when `OPENAI_API_KEY` is unset.
//!
//! It is NOT a placeholder stub: it derives a sane recipe from the histogram
//! (exposure toward a midtone target, highlight/shadow recovery on clipping)
//! so the full propose → verify chain runs and produces something reviewable
//! today, before the GPT vision advisor is wired with a key. It ignores the
//! image content (no vision) and the revision hint.

use crate::decode::{Histogram, Meta};
use crate::recipe::EditRecipe;

use super::{Advisor, AdvisorError, Preview};

pub struct HeuristicProposer;

impl Advisor for HeuristicProposer {
    fn name(&self) -> &'static str {
        "heuristic"
    }

    fn propose(
        &self,
        _img: &Preview,
        _meta: &Meta,
        hist: &Histogram,
        _reference: Option<&str>,
        _guidance: Option<&str>,
        _hint: Option<&str>,
    ) -> Result<EditRecipe, AdvisorError> {
        let total: u64 = hist.luma.iter().map(|&v| v as u64).sum::<u64>().max(1);
        let weighted: u64 = hist
            .luma
            .iter()
            .enumerate()
            .map(|(i, &v)| i as u64 * v as u64)
            .sum();
        let mean = (weighted as f32 / total as f32).max(1.0); // 0..255

        let mut r = EditRecipe::default();

        // Nudge exposure toward a midtone target of ~118/255, capped to ±1.5 EV.
        // Deadband: leave exposure untouched for sub-0.15-stop corrections — a
        // near-neutral frame doesn't need a trivial (and visually pointless)
        // nudge whose sign looks arbitrary.
        let ev_raw = (118.0_f32 / mean).log2().clamp(-1.5, 1.5);
        r.exposure_ev = if ev_raw.abs() < 0.15 {
            0.0
        } else {
            (ev_raw * 10.0).round() / 10.0
        };

        // Recover blown highlights / lifted-but-clipped blacks proportionally.
        if hist.clip_white_pct > 0.5 {
            r.highlights = -(hist.clip_white_pct * 8.0).min(70.0);
            r.whites = -(hist.clip_white_pct * 4.0).min(40.0);
        }
        if hist.clip_black_pct > 1.0 {
            r.shadows = (hist.clip_black_pct * 6.0).min(60.0);
        }

        // Mild, conservative default "presence".
        r.contrast = 8.0;
        r.vibrance = 8.0;
        r.clarity = 4.0;

        r.rationale = format!(
            "Heuristic baseline (no AI vision; OPENAI_API_KEY unset). mean_luma={mean:.0}/255, \
             clip black/white={:.1}%/{:.1}% → exposure {:+.1}EV, highlights {:.0}, shadows {:.0}.",
            hist.clip_black_pct, hist.clip_white_pct, r.exposure_ev, r.highlights, r.shadows,
        );
        r.confidence = 0.4;
        r.clamp();
        Ok(r)
    }
}
