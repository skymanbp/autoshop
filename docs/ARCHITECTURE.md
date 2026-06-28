# Autoshop — Architecture

> Status: **implemented**. The full decode → advise → verify → render pipeline
> ships, plus the web UI, AI denoise (SCUNet sidecar), the PNG/TIFF baked-source
> mode, style retrieval, XMP sidecars (global + local masks), experimental
> generative edits, and an optional pixel-**heal** retouch mode (§4.7). 27 unit
> tests pass. This document describes the design; a few
> historical **[verify]** notes are left in place for provenance.
>
> Confirmed by the user (2026-06-25): Sony `.ARW`; output = XMP sidecar **and**
> rendered file (XMP-first); two AI roles behind one unified provider framework —
> **vision model (GPT) does image processing**, **Claude does non-image analysis
> + acceptance verification**.
>
> Since shipped, two *opt-in* pixel-level features were added alongside the
> parametric core: **AI denoise** (a Python/SCUNet GPU sidecar, run before
> tone/sharpen) and a **baked-source mode** (edit an already-exported PNG/TIFF,
> e.g. one denoised in Lightroom — auto-detected by file type).

## 1. The core idea

The expensive, judgement-heavy part of developing a RAW photo is *deciding what
to change* (this sky is blown, those shadows are crushed, the white balance is
2°°too cool, straighten the horizon). The mechanical part is *applying* those
decisions. So we split exactly there:

```
  RAW ─► decode + features ─► [vision advisor] ─► EditRecipe ─► [Claude verify] ─► render engine
   .ARW    preview+EXIF+hist     GPT (image)        JSON          QA / accept       │
                                                                                    ▼
                                                            XMP sidecar  +  rendered image
```

**The AI never touches a single pixel.** The vision advisor receives a small
preview image + metadata and returns an [`EditRecipe`](../src/recipe.rs) — a
bounded set of Lightroom/ACR-style develop controls. A deterministic Rust engine
renders from the original RAW using that recipe. Key benefits:

- **Reproducibility** — same recipe + same RAW ⇒ byte-identical output.
- **Non-destructiveness** — the recipe is a tiny JSON; originals are never modified.
- **Auditability** — every recipe carries a `rationale` + `confidence`.
- **Lightroom interop** — the recipe serialises to an XMP sidecar, so the edit
  shows up as adjustable sliders in your catalog.
- **No hallucinated pixels** — the AI can only turn the same knobs a human would.

## 2. The `EditRecipe` contract

Defined and unit-tested in [`src/recipe.rs`](../src/recipe.rs). Adobe-convention
ranges (sliders −100..=100, exposure in stops, temperature in Kelvin). Every
field is `#[serde(default)]`, so an advisor emits only the controls it moves.
`EditRecipe::clamp()` defends the render engine; `confidence` gates auto-apply.

Run `cargo run -- recipe-schema` to print the exact JSON shape — this same text
is the advisor's required output format.

## 3. The unified AI provider framework (统一 API 框架)

A single Rust trait abstracts *all* AI calls so providers are interchangeable and
transport-agnostic (an HTTP API, or shelling out to the `claude` CLI). Two roles,
each independently configurable in the in-app **Settings (⚙)** panel:

| Role | Provider options | Sees pixels? | Job |
|------|------------------|--------------|-----|
| **Image advisor** (图像) | **API only** — OpenAI-compatible vision (default `gpt-5.5`) | **yes** (preview) | Look at the photo → emit an `EditRecipe`. The `claude` CLI has no image input in print mode, so this role can't use OAuth. |
| **Analyst / verifier** (分析) | **OAuth** (`claude` CLI, default model `opus`) **or** API (OpenAI-compatible chat) | **no** (data only) | Reason over EXIF/histogram; **acceptance-verify** the recipe (ranges sane? consistent with metadata & intent? confidence adequate?) and flag/veto bad recipes. |

> The verifier judges at the **data level** — recipe + histogram/clipping stats +
> the advisor's rationale — *without* re-doing vision.

Sketch (final shape):

```rust
trait Advisor {                      // one trait, many providers
    fn propose(&self, img: &Preview, meta: &Meta) -> Result<EditRecipe>;   // image role
    fn verify(&self, recipe: &EditRecipe, meta: &Meta) -> Result<Verdict>; // analyst role
}
// propose: OpenAiProvider (HTTP vision)        |  HeuristicProposer (no-key baseline)
// verify:  ClaudeProvider (claude CLI -p OAuth) |  OpenAiVerifier (HTTP chat, OpenAI-compatible)
```

Provider/model/key selection lives in `autoshop.local.json` (written by the
Settings panel) and/or `.env` — both gitignored; the local file overrides env.
The OAuth analysis path reuses Claude Code OAuth — **no API key needed**; the
image path and the API analysis path each need an OpenAI-compatible key.

## 4. Components & milestones

| ID | Component | Crate/tool (actual) | Status |
|----|-----------|---------------------|--------|
| M0 | Data model + CLI scaffold | `clap`, `serde`, `serde_json`, `anyhow`, `thiserror` | **done** |
| M1 | RAW decode + features (Sony ARW) | **`rawler` 0.7.2** (preview + EXIF + WB) | **done** |
| M1 | Unified provider framework + GPT advisor + Claude verifier | `ureq` (HTTP) + `claude` CLI | **done** |
| M2 | Deterministic render engine | `image`, custom tone/colour/WB/clarity/NR/sharpen ops | **done** |
| M2 | XMP sidecar writer (ACR `crs:`, global + local masks) | hand-rolled XML | **done** |
| M3 | `auto` end-to-end + batch | sequential batch (resumes by skipping done `.xmp`) | **done** |
| M4 | Style retrieval + eval harness (your edits as ground truth) | k-NN over EXIF+histogram; per-field MAE/bias | **done** |
| M5 | Local web UI | `tiny_http` + vanilla JS (gallery, live before/after) | **done** |
| V2 | AI denoise (high-ISO/astro) | Python sidecar → **SCUNet** on GPU, called from Rust | **done** |
| V2 | Baked-source mode (edit exported PNG/TIFF) | extension dispatch; develop runs on loaded pixels | **done** |
| V2 | Generative reimagine / retouch | OpenAI Images (`gpt-image-*`) | **done (experimental)** |
| V2 | Pixel retouch / heal (spot removal) | deterministic heal engine + vision spot-detect ([`src/retouch.rs`](../src/retouch.rs)) | **done (experimental)** |

### 4.1 RAW decode (M1)

Backed by **`rawler` 0.7.2** (chosen over the now-frozen `rawloader` for current
Sony body coverage + embedded preview + full EXIF; see [`src/decode.rs`](../src/decode.rs)).
It extracts the embedded JPEG preview (for the vision advisor + UI), a downscaled
histogram with clipping stats, and EXIF (camera/lens/ISO/shutter/aperture/
as-shot WB). Baked sources (PNG/TIFF/JPEG) skip this and load directly via the
`image` crate with neutral metadata.

### 4.2 Vision advisor — image processing (M1)

A vision-capable OpenAI model receives the preview + metadata and returns an
`EditRecipe` (JSON-schema-constrained output). Exact model id, request shape
(base64 vs URL), and structured-output mechanism are **[verify]** in M1 and
pinned in config — not hardcoded from memory.

### 4.3 Claude analyst / verifier (M1)

The verifier (analysis role) runs one of two providers (set in Settings):
**OAuth** — the `claude` CLI for non-image reasoning + acceptance verification via
`claude -p --bare --output-format json --model <opus>` (default `opus`; flags
verified present in CLI v2.1.158), reusing Claude Code OAuth — no API key; or
**API** — an OpenAI-compatible chat endpoint (`OpenAiVerifier`, `/chat/completions`)
sharing the same data-only prompt. Either returns a `Verdict` (accept / revise /
reject + reasons). A rejected recipe can trigger one revision round with the
vision advisor.

### 4.4 Render engine (M2)

Applies the recipe deterministically: white-balance → exposure → tone → colour →
local (clarity/dehaze) → detail → geometry. Outputs 16-bit TIFF (master) and/or
8-bit JPEG (share).

### 4.5 XMP sidecar (M2) — primary deliverable

The recipe written as an ACR/Lightroom `.xmp` sidecar (`crs:` keys like
`Exposure2012`, `Contrast2012`, `Temperature`, `ToneCurvePV2012`). Dropped next
to the `.ARW`, the AI's edit appears as fully-adjustable sliders in Lightroom —
the "AI does 90%, I nudge the last 10%" workflow.

### 4.6 Style / eval harness (M4)

The user's **finished edits** are ground truth. If they're Lightroom XMP/develop
settings, diff the AI recipe against them; if they're exported JPEGs, compare the
AI render perceptually. Lets us measure "does the AI match *how the user*
develops a shot?" and tune the advisor prompt accordingly.

### 4.7 Pixel retouch / heal (optional) — V2

A third, opt-in editing mode (`autoshop heal`, or the UI's **修图 · 去瑕疵** panel),
distinct from BOTH the parametric path (which never touches pixels) and the
generative path (which *synthesises* them). It does traditional **spot-healing**:
small defects (dust, sensor spots, blemishes, specks) are removed by sampling
SURROUNDING REAL pixels and blending them over the defect with a mean-corrected,
feathered patch (the "heal" vs "clone" distinction). By construction the engine
only ever copies / shifts / averages pixels that ALREADY exist — it never invents
content, so this stays *retouching, not generation* (the hard design constraint).

Targeting is hybrid: a vision model auto-detects small spots
([`detect_spots`](../src/retouch.rs), constrained by prompt + schema to small
spot-removals) and/or the user paints regions in the UI
([`plan_from_mask`](../src/retouch.rs) → connected components → circular targets);
both feed the deterministic [`heal_image`](../src/retouch.rs) engine. Donors are
auto-searched (the in-bounds neighbour whose surroundings best match the spot's
border) unless an explicit source offset is given. Output is a pixel master in
./out — **non-XMP** (pixel edits don't serialise to ACR). Runs on the embedded
preview by default; `--full-res` heals the full-sensor develop (slow, RAW only).

## 5. Why Rust

Cross-platform, no GC pauses on large-image pipelines, first-class image crates,
single-binary distribution, trivial `std::process` shell-out to `claude`.
Toolchain in use: rustc/cargo **1.94.1** (verified locally).

## 6. Open questions

| # | Question | Status |
|---|----------|--------|
| 1 | **Image library path** (originals + finished edits) | **OPEN — needed for M1** |
| 2 | Camera / RAW format | resolved: Sony `.ARW` |
| 3 | Output target | resolved: XMP sidecar **+** rendered, XMP-first |
| 4 | AI roles | resolved: GPT=image, Claude=non-image+verify, unified framework |
| 5 | Exact meaning of Claude's "收货验证" (data-level vs pixel-level) | assumed data-level (§3) — confirm |
| 6 | How to feed the preview to the GPT vision API; `crs:` key set for ARW | **[verify]** in M1 (research underway) |
