#!/usr/bin/env python3
"""Autoshop AI-denoise sidecar.

Real-photo denoiser for high-ISO / astro / low-light frames, in the spirit of
ACR/Lightroom "Denoise". Uses SCUNet (cszn/SCUNet) `color_real` weights — a
Swin-Conv-UNet trained on a *practical* (real sensor) degradation model, so it
targets the noise you actually get from a camera, not synthetic Gaussian.

The Rust engine (src/render.rs) hands us a developed/linear RGB image as a
16-bit PNG or TIFF; we denoise the pixels and write the result back at the same
bit depth. The AI never decides *edits* here — it only cleans noise; the rest of
the develop pipeline (tone/colour/sharpen) runs in Rust afterward.

Design choices (all verified in this environment 2026-06-26):
  * torch 2.8 + CUDA on an RTX 4060 Ti  -> GPU inference.
  * cv2 reads/writes 16-bit TIFF/PNG    -> no tifffile needed.
  * einops present; timm is shimmed      -> SCUNet's network file loads verbatim.
  * 60 MP is too big for one forward pass -> overlap-tiled inference, feathered.

Usage:
    python denoise.py --input in.tif --output out.tif
        [--model color_real_psnr|color_real_gan] [--strength 0..1]
        [--tile 512] [--overlap 32] [--cache DIR] [--fp16] [--cpu]

Exit code 0 on success; non-zero with a message on stderr otherwise.
"""
import argparse
import importlib.util
import os
import sys
import types
import warnings

warnings.filterwarnings("ignore")  # silence requests/urllib3 version warnings only

import numpy as np

_BASE = "https://github.com/cszn/KAIR/releases/download/v1.0"
WEIGHT_URLS = {
    # Blind real-noise models (best for actual high-ISO / astro frames).
    "color_real_psnr": f"{_BASE}/scunet_color_real_psnr.pth",
    "color_real_gan": f"{_BASE}/scunet_color_real_gan.pth",
    # Non-blind AWGN models, trained for a fixed noise level (15/25/50 on 0..255).
    # Handy as explicit strength tiers when the noise is closer to synthetic.
    "color_15": f"{_BASE}/scunet_color_15.pth",
    "color_25": f"{_BASE}/scunet_color_25.pth",
    "color_50": f"{_BASE}/scunet_color_50.pth",
}
NETWORK_URL = "https://raw.githubusercontent.com/cszn/SCUNet/main/models/network_scunet.py"


def log(msg):
    print(f"[denoise] {msg}", file=sys.stderr, flush=True)


def _download(url, dest):
    import requests

    log(f"downloading {os.path.basename(dest)} ...")
    tmp = dest + ".part"
    with requests.get(url, stream=True, timeout=60) as r:
        r.raise_for_status()
        total = int(r.headers.get("Content-Length", 0))
        done = 0
        with open(tmp, "wb") as f:
            for chunk in r.iter_content(chunk_size=1 << 20):
                f.write(chunk)
                done += len(chunk)
                if total:
                    pct = 100 * done / total
                    print(f"\r[denoise]   {done >> 20}/{total >> 20} MB ({pct:4.1f}%)",
                          end="", file=sys.stderr, flush=True)
    print("", file=sys.stderr)
    os.replace(tmp, dest)


def _install_timm_shim():
    """SCUNet's network file imports trunc_normal_ + DropPath from timm. Those are
    init-only / inference-noop, so a tiny shim satisfies the import without pulling
    timm (which would drag in torchvision). State-dict keys are unaffected."""
    import torch.nn as nn

    def trunc_normal_(tensor, mean=0.0, std=1.0, a=-2.0, b=2.0):
        return nn.init.trunc_normal_(tensor, mean, std, a, b)

    class DropPath(nn.Module):
        def __init__(self, drop_prob=0.0):
            super().__init__()
            self.drop_prob = drop_prob

        def forward(self, x):  # identity at inference (eval mode)
            return x

    timm = types.ModuleType("timm")
    models = types.ModuleType("timm.models")
    layers = types.ModuleType("timm.models.layers")
    layers.trunc_normal_ = trunc_normal_
    layers.DropPath = DropPath
    sys.modules.setdefault("timm", timm)
    sys.modules.setdefault("timm.models", models)
    sys.modules["timm.models.layers"] = layers

    # thop is a FLOPs counter the network file imports only for its __main__
    # self-test; a no-op stub satisfies the top-level import without the dep.
    thop = types.ModuleType("thop")
    thop.profile = lambda *a, **k: (0, 0)
    sys.modules.setdefault("thop", thop)


def load_model(model_name, cache_dir, device):
    import torch

    os.makedirs(cache_dir, exist_ok=True)
    net_path = os.path.join(cache_dir, "network_scunet.py")
    if not os.path.exists(net_path):
        _download(NETWORK_URL, net_path)
    weight_path = os.path.join(cache_dir, f"scunet_{model_name}.pth")
    if not os.path.exists(weight_path):
        if model_name not in WEIGHT_URLS:
            raise SystemExit(f"unknown model '{model_name}'")
        _download(WEIGHT_URLS[model_name], weight_path)

    _install_timm_shim()
    spec = importlib.util.spec_from_file_location("network_scunet", net_path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)

    # color_real models: 3-channel, dim=64, 7 stages of depth 4 (KAIR test config).
    model = mod.SCUNet(in_nc=3, config=[4, 4, 4, 4, 4, 4, 4], dim=64)
    state = torch.load(weight_path, map_location="cpu")
    model.load_state_dict(state, strict=True)
    model.eval().to(device)
    return model


def _tile_window(th, tw, overlap):
    """Linear feather window so overlapping tiles blend without seams."""
    wy = np.ones(th, dtype=np.float32)
    wx = np.ones(tw, dtype=np.float32)
    if overlap > 0:
        ramp = np.linspace(0, 1, overlap, dtype=np.float32)
        wy[:overlap] = np.minimum(wy[:overlap], ramp)
        wy[-overlap:] = np.minimum(wy[-overlap:], ramp[::-1])
        wx[:overlap] = np.minimum(wx[:overlap], ramp)
        wx[-overlap:] = np.minimum(wx[-overlap:], ramp[::-1])
    # Floor to a small positive value: the ramp reaches 0 at the very edge, and an
    # image-border pixel covered by only one tile would otherwise divide by ~0 and
    # turn black. With a floor, a lone tile normalises to exactly the model output,
    # while interior seams (where a neighbour has ~full weight) still blend cleanly.
    return np.clip(np.outer(wy, wx), 1e-3, 1.0)


def denoise(model, img, device, tile=512, overlap=32, fp16=False):
    """img: float32 HxWx3 in [0,1]. Returns denoised float32 HxWx3 in [0,1]."""
    import torch

    h, w, _ = img.shape
    acc = np.zeros((h, w, 3), dtype=np.float32)
    wsum = np.zeros((h, w, 1), dtype=np.float32)
    step = max(1, tile - overlap)
    ys = list(range(0, max(1, h - overlap), step)) if h > tile else [0]
    xs = list(range(0, max(1, w - overlap), step)) if w > tile else [0]

    autocast = torch.autocast(device_type=device.split(":")[0], dtype=torch.float16) \
        if fp16 and device.startswith("cuda") else _nullctx()

    with torch.no_grad():
        for y in ys:
            for x in xs:
                y0, x0 = y, x
                y1, x1 = min(y0 + tile, h), min(x0 + tile, w)
                y0, x0 = max(0, y1 - tile), max(0, x1 - tile)  # keep full tile near edges
                patch = img[y0:y1, x0:x1, :]
                t = torch.from_numpy(patch.transpose(2, 0, 1)).unsqueeze(0).to(device)
                with autocast:
                    out = model(t)  # SCUNet pads to /64 internally
                out = out.squeeze(0).float().clamp(0, 1).cpu().numpy().transpose(1, 2, 0)
                win = _tile_window(y1 - y0, x1 - x0, overlap)[:, :, None]
                acc[y0:y1, x0:x1, :] += out * win
                wsum[y0:y1, x0:x1, :] += win
    wsum[wsum == 0] = 1.0
    return acc / wsum


class _nullctx:
    def __enter__(self):
        return self

    def __exit__(self, *a):
        return False


def main():
    ap = argparse.ArgumentParser(description="Autoshop AI denoise (SCUNet)")
    ap.add_argument("--input", required=True)
    ap.add_argument("--output", required=True)
    ap.add_argument("--model", default="color_real_psnr", choices=list(WEIGHT_URLS))
    ap.add_argument("--strength", type=float, default=1.0, help="0..1 blend with original")
    ap.add_argument("--tile", type=int, default=512)
    ap.add_argument("--overlap", type=int, default=32)
    ap.add_argument("--cache", default=os.path.join(os.path.dirname(__file__), "weights"))
    ap.add_argument("--fp16", action="store_true")
    ap.add_argument("--cpu", action="store_true")
    args = ap.parse_args()

    import cv2
    import torch

    device = "cpu" if args.cpu or not torch.cuda.is_available() else "cuda:0"
    log(f"device={device} model={args.model} strength={args.strength}")

    raw = cv2.imread(args.input, cv2.IMREAD_UNCHANGED)
    if raw is None:
        raise SystemExit(f"cannot read image: {args.input}")
    if raw.ndim == 2:
        raw = cv2.cvtColor(raw, cv2.COLOR_GRAY2BGR)
    if raw.shape[2] == 4:
        raw = raw[:, :, :3]
    is16 = raw.dtype == np.uint16
    maxv = 65535.0 if is16 else 255.0
    rgb = cv2.cvtColor(raw, cv2.COLOR_BGR2RGB).astype(np.float32) / maxv

    model = load_model(args.model, args.cache, device)
    log(f"input {rgb.shape[1]}x{rgb.shape[0]} ; denoising ...")
    den = denoise(model, rgb, device, tile=args.tile, overlap=args.overlap, fp16=args.fp16)

    s = float(np.clip(args.strength, 0.0, 1.0))
    if s < 1.0:
        den = s * den + (1.0 - s) * rgb

    out = np.clip(den * maxv + 0.5, 0, maxv).astype(np.uint16 if is16 else np.uint8)
    out = cv2.cvtColor(out, cv2.COLOR_RGB2BGR)
    ok = cv2.imwrite(args.output, out)
    if not ok:
        raise SystemExit(f"cannot write image: {args.output}")
    log(f"wrote {args.output} ({'16-bit' if is16 else '8-bit'})")


if __name__ == "__main__":
    main()
