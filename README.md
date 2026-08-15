# skymanbp's Autoshop

[![License](https://img.shields.io/github/license/skymanbp/autoshop)](LICENSE)
[![Latest release](https://img.shields.io/github/v/release/skymanbp/autoshop?label=release)](https://github.com/skymanbp/autoshop/releases)

AI-assisted, deterministic RAW development — a GPT-based vision advisor proposes Lightroom-style EditRecipes and a Rust engine deterministically renders reproducible 16‑bit masters and Lightroom-compatible XMP sidecars.

Quickstart

```bash
# build (Linux/macOS/Windows via cargo)
cargo build --release

# end-to-end example (one-liner)
# after building, run the CLI from target/release or use the released binary
target/release/autoshop auto "photo.ARW" --guidance "warm, lift shadows"
```

Core features

- Deterministic Rust rendering from original RAW → 16‑bit TIFF masters
- GPT vision advisor emits a bounded, auditable EditRecipe (JSON) — AI never touches pixels in the main path
- Writes Lightroom-compatible XMP sidecars so edits are adjustable in Lightroom/ACR
- Optional AI denoise, pixel heal and generative retouch (all opt-in)
- Desktop native GUI and a small local web UI for browsing & adjusting
- Batch processing, style retrieval, reverse-fit (look matching), and eval tools

Docs & support

- Detailed architecture and design: docs/ARCHITECTURE.md
- AI denoise helper: python/denoise.py
- For developer notes and tests: run `cargo test`

How to report bugs or request features

- Open an issue and choose the appropriate template (bug or feature).
- For bugs include: steps to reproduce, minimal input image or EXIF, exact command used, and output / error text.

Stable metadata & topics (suggested)

Suggested repo description (set in Settings):
Autoshop — AI-assisted deterministic RAW development: GPT vision advisor + Rust rendering → 16‑bit masters + Lightroom-compatible XMP.

Suggested topics (paste into repo Topics):
raw photography image-processing rust denoise lightroom xmp gui ai photo-editing

License

This repository is licensed under the Apache-2.0 License. See LICENSE for details.

--------

If you want a concise Chinese translation, see README.zh.md in this branch.

