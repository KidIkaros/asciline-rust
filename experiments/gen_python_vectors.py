#!/usr/bin/env python3
"""
gen_python_vectors.py — differential test vectors for the Rust port.

Encodes synthetic framebuffers with the ORIGINAL Python codec.py (from the
ASCILINE repo) and writes a binary vector file:

    [u8 version=1][u8 cell_bytes][u32 BE cols][u32 BE rows][u32 BE n_frames]
    per frame:
        [u32 BE index][u32 BE msg_len][msg bytes][u32 BE plain_len][plain bytes]

`plain` is the full framebuffer the client should hold after decoding `msg`
(the Python encoder's "shown frame", lossless here so == the true frame).

The ORIGINAL codec.py is vendored at experiments/vendor/ (pinned, see the
header there), so this runs hermetically with only numpy installed:

Usage:
    python3 experiments/gen_python_vectors.py > experiments/vectors_python.bin
    # override the codec.py location with ASCILINE_REPO=/path/to/repo
"""
import os
import struct
import sys

import numpy as np

sys.path.insert(0, os.environ.get("ASCILINE_REPO", os.path.join(os.path.dirname(__file__), "vendor")))
from codec import encode_frame, DEFAULT_LEVEL  # noqa: E402

PALETTE = list(" `.-':_,^=;><+!rc*/z?sLTv)J7(|Fi{C}fI31tlu[neoZ5Yxjya]2ESwqkP6h9d4VpOGbUAKXHm8RD#$Bg0MNWQ%&@")


def build_ascii_fb(rgb):
    """Mirror the Rust mapper: [char_byte, R, G, B] cells from an RGB frame."""
    n = len(PALETTE)
    gray = (77 * rgb[..., 0].astype(np.int32) + 150 * rgb[..., 1].astype(np.int32) + 29 * rgb[..., 2].astype(np.int32)) >> 8
    indices = (gray.astype(np.uint16) * (n - 1)) // 255
    np.clip(indices, 0, n - 1, out=indices)
    fb = np.zeros((*gray.shape, 4), dtype=np.uint8)
    fb[..., 0] = np.array([ord(c) for c in PALETTE], dtype=np.uint8)[indices]
    fb[..., 1:] = rgb  # RGB order, matching the server's bgr[:, :, ::-1]
    return fb


def build_pixel_fb(rgb):
    """Mirror the Rust pixel mapper: BGR cells."""
    return rgb[..., ::-1]


def synth_frames(cols, rows, n_frames, seed=1234):
    rng = np.random.default_rng(seed)
    frames = []
    for i in range(n_frames):
        base = rng.integers(0, 256, (rows, cols, 3), dtype=np.uint8)
        # add a moving bright blob so chars + colors change between frames
        cx, cy = cols // 2 + (i * 3) % max(cols // 2, 1), rows // 2
        r = max(1, min(cols, rows) // 6)
        yy, xx = np.ogrid[:rows, :cols]
        mask = ((xx - cx) ** 2 + (yy - cy) ** 2) <= r * r
        blob = np.zeros((rows, cols, 3), dtype=np.uint8)
        blob[mask] = [255, 255, 0]
        frames.append(np.clip(base.astype(np.int16) + blob.astype(np.int16), 0, 255).astype(np.uint8))
    return frames


def synth_delta_frames(cols, rows, n_frames, seed=77):
    """Mostly-static content: ONE fixed random base + a small moving blob, so
    only a tiny fraction of cells change per frame and the DELTA encoding wins
    (the random-per-frame noise in `synth_frames` never emits deltas)."""
    rng = np.random.default_rng(seed)
    base = rng.integers(0, 256, (rows, cols, 3), dtype=np.uint8)
    frames = []
    for i in range(n_frames):
        f = base.copy()
        cx = cols // 2 + (i * 2) % max(cols // 2, 1)
        cy = rows // 2
        yy, xx = np.ogrid[:rows, :cols]
        mask = ((xx - cx) ** 2 + (yy - cy) ** 2) <= 4
        f[mask] = [255, 255, 0]
        frames.append(f)
    return frames


def main():
    out = sys.stdout.buffer
    cases = [
        ("ascii4", 4, 40, 12, 30),
        ("ascii4", 4, 23, 7, 60),   # covers keyframe boundary (48)
        ("pixel3", 3, 32, 18, 40),
        ("pixel3", 3, 16, 9, 10),
        ("ascii4-delta", 4, 24, 8, 60),  # mostly-static → DELTA frames
        ("pixel3-delta", 3, 32, 18, 60), # mostly-static → DELTA frames
    ]
    for name, cell, cols, rows, n in cases:
        frames = (
            synth_delta_frames(cols, rows, n)
            if name.endswith("-delta")
            else synth_frames(cols, rows, n)
        )
        enc_prev = None
        out.write(b"RSTV" + bytes([1, cell]) + struct.pack(">III", cols, rows, n))
        if name.startswith("ascii"):
            fps = [build_ascii_fb(f) for f in frames]
        else:
            fps = [build_pixel_fb(f) for f in frames]
        for i, fb in enumerate(fps):
            msg, shown = encode_frame(fb, enc_prev, i, level=DEFAULT_LEVEL, tolerance=0)
            enc_prev = shown
            out.write(struct.pack(">I", i))
            out.write(struct.pack(">I", len(msg)))
            out.write(msg)
            plain = np.ascontiguousarray(shown).tobytes()
            out.write(struct.pack(">I", len(plain)))
            out.write(plain)
    print(f"[gen] wrote {len(cases)} cases", file=sys.stderr)
    print(f"[gen] frames encoded: {sum(c[3] for c in cases)}", file=sys.stderr)


if __name__ == "__main__":
    main()
