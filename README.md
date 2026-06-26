# Autoshop

AI-assisted **automatic development of RAW photographs**. Point it at a RAW file;
an AI advisor decides the develop adjustments (exposure, white balance, tone,
colour, crop…) and a deterministic Rust engine applies them — or writes an XMP
sidecar so the edit opens as adjustable sliders in Lightroom.

The AI never touches pixels. It only emits an
[`EditRecipe`](src/recipe.rs) (a small, bounded, Lightroom-style JSON); the
engine renders from the original RAW. That keeps results reproducible,
non-destructive, auditable, and free of hallucinated detail.

See **[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)** for the full design.

## Status

Initial scaffold (Milestone **M0**): data model + CLI compile and test clean.
The decode → advise → render pipeline (M1–M3) is designed and stubbed.

```
cargo build            # builds the autoshop binary
cargo test             # 3 passing tests on the EditRecipe schema
cargo run -- recipe-schema   # print the JSON shape the AI must emit
```

## Planned CLI

```
autoshop analyze <raw> [-o recipe.json]      # decode + ask AI → recipe (no render)
autoshop apply   <raw> <recipe.json> -o out  # render from a recipe
autoshop auto    <raw> [-o out]              # analyze + apply end-to-end
autoshop recipe-schema                       # print the EditRecipe template (works today)
```

## Tech

Rust (rustc/cargo 1.94). The AI advisor shells out to the local `claude` CLI
(`-p --output-format json`), reusing your Claude Code OAuth — no API key needed.
An optional parallel GPT advisor can be wired in later for cross-checking.
