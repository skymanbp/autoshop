# Autoshop — Architecture

> Status: **initial scaffold**. The data model + CLI surface compile and test
> clean; the decode → advise → render pipeline is designed here and stubbed in
> code. Items marked **[verify]** are assumptions to validate during
> implementation, not established facts.

## 1. The core idea

The expensive, judgement-heavy part of developing a RAW photo is *deciding what
to change* (this sky is blown, those shadows are crushed, the white balance is
2°°too cool, straighten the horizon). The mechanical part is *applying* those
decisions. So we split exactly there:

```
  RAW file ──► decode + feature extraction ──► AI advisor ──► EditRecipe (JSON)
                                                                   │
                                       deterministic render engine ◄┘
                                                   │
                                  output image  +  XMP sidecar (for Lightroom)
```

**The AI never touches a single pixel.** It receives a small preview image plus
metadata (histogram, EXIF, clipping stats) and returns an
[`EditRecipe`](../src/recipe.rs) — a bounded set of Lightroom/ACR-style develop
controls. A deterministic Rust engine renders from the original RAW using that
recipe. This is the key design decision because it gives us:

- **Reproducibility** — same recipe + same RAW ⇒ byte-identical output.
- **Non-destructiveness** — the recipe is a tiny JSON; originals are never modified.
- **Auditability** — every recipe carries a `rationale` and a `confidence`; you
  can see *why* and gate auto-apply on confidence.
- **Lightroom interop** — the same recipe serialises to an XMP sidecar, so the
  AI's decisions show up as adjustable sliders in your existing catalog.
- **No hallucinated pixels** — the AI can't invent detail; it can only turn the
  same knobs a human would.

## 2. The `EditRecipe` contract

Defined and unit-tested in [`src/recipe.rs`](../src/recipe.rs). Adobe-convention
ranges (sliders −100..=100, exposure in stops, temperature in Kelvin). Every
field is `#[serde(default)]`, so the AI emits only the controls it wants to move
and omits the rest. `EditRecipe::clamp()` defends the render engine against
out-of-range values; `confidence` gates auto-apply.

Run `cargo run -- recipe-schema` to print the exact JSON shape — this same text
is handed to the AI as its required output format.

## 3. Components & milestones

| ID | Component | Crate/tool (candidate) | Status |
|----|-----------|------------------------|--------|
| M0 | Data model + CLI scaffold | `clap`, `serde`, `serde_json`, `anyhow`, `thiserror` | **done** |
| M1 | RAW decode + feature extraction | `rawloader` + `imagepipe` (pure Rust) **[verify]**, fallback `libraw` via FFI | planned |
| M1 | AI advisor (Claude) | shell out to `claude -p --output-format json` | planned |
| M2 | Deterministic render engine | `image`, custom tone/colour ops | planned |
| M2 | XMP sidecar writer | hand-rolled XML (ACR `crs:` namespace) | planned |
| M3 | `auto` end-to-end + batch | rayon for parallel batch | planned |
| M4 | Optional parallel GPT advisor | OpenAI HTTP API (needs key) | optional |
| M5 | UI / Photoshop plugin | TBD (egui? Tauri? PS UXP?) | later |

### 3.1 RAW decode (M1)

Primary candidate: **`rawloader`** (decodes sensor data for a wide camera range)
+ **`imagepipe`** (demosaic + basic pipeline), both pure-Rust by the same author
— no C toolchain needed on Windows. **[verify]** that the user's specific camera
bodies/formats are supported; if not, fall back to **`libraw`** (C, gold-standard
coverage) through FFI. We also extract:

- the embedded JPEG preview (fast, already white-balanced) to send to the AI,
- a downscaled linear render for histogram/clipping analysis,
- EXIF (camera, lens, ISO, shutter, aperture, as-shot WB) via e.g. `kamadak-exif`.

### 3.2 AI advisor (M1) — reusing Claude Code OAuth, no API key

The Rust binary shells out to the locally-installed `claude` CLI in print mode:

```
claude -p --output-format json --model <opus|sonnet> --append-system-prompt <advisor-prompt> "<task + image path + metadata>"
```

Verified present in CLI v2.1.158: `--print`, `--output-format`, `--model`,
`--append-system-prompt`. This reuses the user's existing Claude Code
subscription/OAuth — **no separate `ANTHROPIC_API_KEY` to manage**.

**[verify]** the exact mechanism for getting the *preview image* in front of the
model in non-interactive `-p` mode (candidates: reference the preview's absolute
path in the prompt so Claude reads it; or base64-embed). This is the single
biggest open question for M1 and will be settled empirically against a real file
before building on it.

The advisor's system prompt pins the output to the `EditRecipe` JSON schema and
forbids prose outside the JSON, so parsing is robust.

### 3.3 Render engine (M2)

Applies the recipe deterministically: white-balance → exposure → tone (high/low,
whites/blacks, curve) → colour (vibrance/saturation/HSL) → local (clarity/dehaze)
→ detail (sharpen/NR) → geometry (straighten/crop). Outputs 16-bit TIFF (master)
and/or 8-bit JPEG (share), format chosen by output extension.

### 3.4 XMP sidecar (M2)

The same recipe written as an ACR/Lightroom `.xmp` sidecar (`crs:` namespace
keys like `Exposure2012`, `Contrast2012`, `Temperature`). Dropping this next to
the RAW makes the AI's edit appear as fully-adjustable sliders in Lightroom —
ideal for the "AI does 90%, I nudge the last 10%" workflow.

### 3.5 Parallel GPT advisor (M4, optional)

Run a second advisor (OpenAI) concurrently for cross-checking / ensembling. Two
recipes can be diffed, averaged, or presented side-by-side. Requires an OpenAI
key (kept in `.env` / `autoshop.local.toml`, both gitignored). Off by default.

## 4. Why Rust

Cross-platform, no GC pauses on large-image pipelines, first-class image crates,
trivial single-binary distribution, and easy `std::process` shell-out to the
`claude` CLI. Toolchain in use: rustc/cargo **1.94.1** (verified locally).

## 5. Open questions (need user input)

1. **Image library path** — location of original RAWs + corresponding finished
   edits. The finished edits are gold: they're a style reference / eval set so
   we can measure "does the AI match how *you* would have developed this?".
2. **Camera bodies / RAW formats** in use → decides decode backend (3.1).
3. **Output target** — rendered files, XMP sidecars for Lightroom, or both.
4. **Second (GPT) advisor** — wire in now or defer.
