#!/usr/bin/env python3
"""AI segmentation sidecar — writes an 8-bit grayscale mask PNG (white = selected).

Companion to the Rust bridge (src/segment.rs), following the same sidecar
pattern as denoise.py: the Rust side shells out, this script does one job and
exits non-zero with a human-readable reason on stderr when it can't.

Usage:
  python segment.py --input photo.png --output mask.png --target subject|sky

Backends (weights auto-download to the USER's home cache on first run — nothing
is stored in the repo, consistent with .gitignore'ing python/weights):
  subject -> rembg, U^2-Net salient-object alpha  (pip install rembg; ~/.u2net)
  sky     -> SegFormer-B0 fine-tuned on ADE20K via transformers
             (pip install transformers torch; ~/.cache/huggingface)

The output mask is soft (the model's own alpha / class probability), which the
render engine samples bilinearly — so edges come pre-feathered.
"""

import argparse
import sys


def die(msg: str) -> None:
    print(f"segment.py: {msg}", file=sys.stderr)
    sys.exit(2)


def subject_mask(img_path: str):
    """Salient-subject alpha via rembg's default U^2-Net session."""
    try:
        from rembg import remove
    except ImportError:
        # ASCII-only: Windows consoles in legacy codepages mangle wide dashes.
        die(
            "subject segmentation needs rembg -> pip install rembg "
            "(the U^2-Net model auto-downloads to ~/.u2net on first run)"
        )
    from PIL import Image

    img = Image.open(img_path).convert("RGB")
    # only_mask=True returns the soft alpha as a single-channel PIL image.
    return remove(img, only_mask=True)


def sky_mask(img_path: str):
    """ADE20K semantic segmentation, sky-class probability as the mask."""
    try:
        import torch
        from transformers import (
            SegformerForSemanticSegmentation,
            SegformerImageProcessor,
        )
    except ImportError:
        # ASCII-only: Windows consoles in legacy codepages mangle wide dashes.
        die(
            "sky segmentation needs transformers + torch -> pip install transformers "
            "(SegFormer-B0 ADE20K, ~14 MB, auto-downloads to ~/.cache/huggingface)"
        )
    import numpy as np
    from PIL import Image

    name = "nvidia/segformer-b0-finetuned-ade-512-512"
    processor = SegformerImageProcessor.from_pretrained(name)
    model = SegformerForSemanticSegmentation.from_pretrained(name)
    model.eval()

    # Resolve the sky class from the model's own label table instead of
    # hard-coding an index — survives label-map revisions.
    sky_ids = [i for i, l in model.config.id2label.items() if "sky" in l.lower()]
    if not sky_ids:
        die(f"model {name} has no 'sky' class in id2label — cannot build a sky mask")

    img = Image.open(img_path).convert("RGB")
    with torch.no_grad():
        inputs = processor(images=img, return_tensors="pt")
        logits = model(**inputs).logits  # (1, n_classes, h/4, w/4)
        up = torch.nn.functional.interpolate(
            logits, size=(img.height, img.width), mode="bilinear", align_corners=False
        )
        probs = up.softmax(dim=1)[0]

    m = probs[int(sky_ids[0])].numpy()
    m = (m * 255.0).clip(0, 255).astype(np.uint8)
    return Image.fromarray(m, mode="L")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--input", required=True, help="source image (any PIL-readable format)")
    ap.add_argument("--output", required=True, help="mask PNG to write (8-bit grayscale)")
    ap.add_argument("--target", required=True, choices=["subject", "sky"])
    a = ap.parse_args()

    mask = subject_mask(a.input) if a.target == "subject" else sky_mask(a.input)
    mask = mask.convert("L")
    mask.save(a.output)
    print(f"segment.py: {a.target} mask -> {a.output}")


if __name__ == "__main__":
    main()
