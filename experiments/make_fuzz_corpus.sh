#!/usr/bin/env bash
# Regenerate the committed cargo-fuzz seed corpora from a real compile.
#
# The decoders' deep paths are gated behind zlib (tag-4 profile frames) or
# valid frame shapes, which libFuzzer cannot discover by mutating random
# bytes. These seeds give it real wire records to perturb, so the CI smoke
# runs and local `cargo fuzz run` exercise `dec_plane` / RLE / delta paths.
#
# DETERMINISM: the corpus is committed and CI (job `corpus-check`) regenerates
# it and fails on any drift. The source clip is therefore encoded losslessly
# with FFV1 in a Matroska container — an x264 intermediate would depend on the
# ffmpeg version's rate control and make the corpus drift between builds. With
# FFV1 the decoded frames are bit-exact copies of the (deterministic) lavfi
# sources, and the compiler's output is thread-count independent, so the same
# bytes come out everywhere.
#
# Run from the repo root:  experiments/make_fuzz_corpus.sh
set -euo pipefail
cd "$(dirname "$0")/.."

BIN=target/release/asciline-compile
if [ ! -x "$BIN" ]; then
    echo "building release compiler first…"
    cargo build --release --bin asciline-compile
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# 3 s clip with a hard cut mid-stream: keyframes, motion deltas, skips.
# FFV1 (lossless, deterministic) in mkv — see the determinism note above.
ffmpeg -y -v error \
    -f lavfi -i 'testsrc2=size=320x180:rate=30:duration=2' \
    -f lavfi -i 'smptebars=size=320x180:rate=30:duration=1' \
    -filter_complex '[0:v][1:v]concat=n=2:v=1[v]' -map '[v]' -pix_fmt yuv420p \
    -c:v ffv1 -f matroska "$TMP/clip.mkv"

# tag-4 lossy DCT profile (cell-independent) — the deep DCT decoder paths
"$BIN" "$TMP/clip.mkv" --cols 240 --profile --qf 70 --no-quality --out "$TMP/prof" >/dev/null
# adaptive, cell=4 (text mode, 16 M colours)
"$BIN" "$TMP/clip.mkv" --cols 40 --mode 6 --tolerance 8 --no-quality --out "$TMP/adapt4" >/dev/null
# adaptive, cell=3 (pixel mode)
"$BIN" "$TMP/clip.mkv" --cols 40 --pixel --tolerance 8 --no-quality --out "$TMP/adapt3" >/dev/null

python3 - "$TMP/prof.ascf" "$TMP/adapt4.ascf" "$TMP/adapt3.ascf" <<'EOF'
import os, shutil, sys

corpus = "fuzz/corpus"
for d in ("fuzz_parse_ascf", "fuzz_adaptive_decode", "fuzz_profile_decode", "fuzz_ascf_stream"):
    shutil.rmtree(os.path.join(corpus, d), ignore_errors=True)
    os.makedirs(os.path.join(corpus, d), exist_ok=True)

def records(path):
    data = open(path, "rb").read()
    is_v2 = data[:4] in (b"ASC2", b"ASC1")  # v2 container
    off = 18 if is_v2 else 14
    recs = []
    while off + 4 <= len(data):
        n = int.from_bytes(data[off : off + 4], "big")
        off += 4
        if n == 0 or off + n > len(data):
            break
        recs.append(data[off : off + n])
        off += n
    return data, is_v2, recs

files = [
    ("fuzz_profile_decode", sys.argv[1], 4),
    ("fuzz_adaptive_decode", sys.argv[2], None),  # cell=4 seeds
    ("fuzz_adaptive_decode", sys.argv[3], None),  # cell=3 seeds
]
for target, path, want_tag in files:
    data, is_v2, recs = records(path)
    hdr = data[: 18 if is_v2 else 14]
    open(os.path.join(corpus, "fuzz_parse_ascf", os.path.basename(path) + ".hdr.bin"), "wb").write(hdr)
    for i, r in enumerate(recs):
        tag = r[4] if len(r) >= 5 else -1
        if want_tag is None:
            if 0 <= tag <= 3:
                open(os.path.join(corpus, target, f"{os.path.basename(path)}.{i:04d}.bin"), "wb").write(r)
        elif tag == want_tag:
            open(os.path.join(corpus, target, f"{i:04d}.bin"), "wb").write(r)
    # stream seeds: header + records, capped at ~4 KB (libFuzzer default max_len)
    buf = bytearray(hdr)
    for r in recs:
        if len(buf) + 4 + len(r) > 4000:
            break
        buf += len(r).to_bytes(4, "big") + r
    open(os.path.join(corpus, "fuzz_ascf_stream", os.path.basename(path) + ".bin"), "wb").write(bytes(buf))

for d in sorted(os.listdir(corpus)):
    files_ = os.listdir(os.path.join(corpus, d))
    total = sum(os.path.getsize(os.path.join(corpus, d, f)) for f in files_)
    print(f"{d}: {len(files_)} seeds, {total} bytes")
EOF
