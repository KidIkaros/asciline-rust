#!/usr/bin/env python3
"""
bench_python.py — per-frame cost of the Python mapping + adaptive encode stage
(the numpy + codec.py equivalent of the Rust bench_map_encode test), so the
Rust port's speed-up is measured against the same work, not just asserted.

Run: python3 experiments/bench_python.py
"""
import os
import sys
import time

import numpy as np

sys.path.insert(0, os.environ.get("ASCILINE_REPO", "/tmp/asciline-ref"))
from codec import encode_frame, DEFAULT_LEVEL  # noqa: E402

PALETTE = list(" `.-':_,^=;><+!rc*/z?sLTv)J7(|Fi{C}fI31tlu[neoZ5Yxjya]2ESwqkP6h9d4VpOGbUAKXHm8RD#$Bg0MNWQ%&@")
CHAR_LUT = np.array([ord(c) for c in PALETTE], dtype=np.uint8)


def produce(rgb, frame_buf, prev, i, n):
    gray = cv2_gray(rgb)
    indices = (gray.astype(np.uint16) * (n - 1)) // 255
    np.clip(indices, 0, n - 1, out=indices)
    frame_buf[:, :, 0] = CHAR_LUT[indices]
    frame_buf[:, :, 1:] = rgb
    msg, shown = encode_frame(frame_buf, prev, i, level=DEFAULT_LEVEL, tolerance=0)
    return msg, shown


def cv2_gray(rgb):
    # matches cv2.cvtColor BGR2GRAY coefficients (0.299/0.587/0.114)
    return (
        0.299 * rgb[..., 0] + 0.587 * rgb[..., 1] + 0.114 * rgb[..., 2]
    ).astype(np.uint8)


def bench(cols, rows, frames):
    n = len(PALETTE)
    rng = np.random.default_rng(7)
    rgb = rng.integers(0, 256, (rows, cols, 3), dtype=np.uint8)
    frame_buf = np.empty((rows, cols, 4), dtype=np.uint8)
    prev = None
    for i in range(10):  # warmup
        _, prev = produce(rgb, frame_buf, prev, i, n)
    prev = None
    t0 = time.perf_counter()
    for i in range(frames):
        _, prev = produce(rgb, frame_buf, prev, i, n)
    per = (time.perf_counter() - t0) / frames
    print(f"{cols:>4}x{rows:<4} {cols*rows:>6} cells | {per*1e6:>7.1f} µs/frame (map+encode) | {1/per:>7.0f} fps ceiling")


if __name__ == "__main__":
    print("python3 + numpy + codec.py (single-threaded, GIL held):")
    for c, r, f in [(80, 23, 1000), (200, 56, 1000), (240, 67, 1000), (480, 135, 300)]:
        bench(c, r, f)
