#!/usr/bin/env python3
"""
gen_profile_vectors.py — differential test vectors for the Rust tag-4
lossy DCT profile port.

Encodes synthetic BGR frames with the ORIGINAL codec.py ProfileEncoder and
writes a binary vector file:

    'PRFV'(4) [u8 version=1][u16 BE W][u16 BE H][u32 BE n_frames]
    per frame:
        [u32 BE index][u32 BE msg_len][msg bytes][u32 BE shown_len][shown bytes]

`shown` is the reconstructed BGR frame the client holds after decoding `msg`
(the encoder's `yuv_to_bgr` of its own reconstruction).

Usage:
    python3 experiments/gen_profile_vectors.py > experiments/vectors_profile_py.bin
"""
import os
import struct
import sys

import numpy as np

sys.path.insert(0, os.environ.get("ASCILINE_REPO", "/tmp/asciline-ref"))
from codec import ProfileEncoder  # noqa: E402


def synth_bgr(w, h, n, seed=777):
    rng = np.random.default_rng(seed)
    frames = []
    for i in range(n):
        base = rng.integers(0, 256, (h, w, 3), dtype=np.uint8)
        cx = w // 2 + (i * 4) % max(w // 2, 1)
        cy = h // 2
        r = max(2, min(w, h) // 8)
        yy, xx = np.ogrid[:h, :w]
        mask = ((xx - cx) ** 2 + (yy - cy) ** 2) <= r * r
        blob = np.zeros((h, w, 3), dtype=np.uint8)
        blob[mask] = [255, 128, 0]
        frames.append(np.clip(base.astype(np.int16) + blob.astype(np.int16), 0, 255).astype(np.uint8))
    return frames


def main():
    out = sys.stdout.buffer
    cases = [
        (48, 32, 60, 70),  # crosses the KEY=48 keyframe boundary
        (64, 48, 30, 50),  # lower quality factor -> heavier quantization
        (32, 16, 12, 90),  # high quality, tiny grid
    ]
    for w, h, n, qf in cases:
        enc = ProfileEncoder(w, h, qf)
        out.write(b"PRFV" + bytes([1]) + struct.pack(">HH", w, h) + struct.pack(">I", n))
        for i, f in enumerate(synth_bgr(w, h, n)):
            f = np.ascontiguousarray(f)
            msg, shown = enc.encode(f)
            out.write(struct.pack(">I", i))
            out.write(struct.pack(">I", len(msg)))
            out.write(msg)
            s = np.ascontiguousarray(shown).tobytes()
            out.write(struct.pack(">I", len(s)))
            out.write(s)
    print(f"[gen] wrote {len(cases)} profile cases", file=sys.stderr)
    print(f"[gen] frames encoded: {sum(c[2] for c in cases)}", file=sys.stderr)


if __name__ == "__main__":
    main()
