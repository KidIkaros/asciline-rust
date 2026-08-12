#!/usr/bin/env python3
"""Count profile keyframes/inter frames in an .ascf file.

.ascf layout: 18-byte header, then repeated [len u32 BE][msg].
msg = [frame_index u32 BE][tag u8][zlib(payload)]. Tag 4 (TAG_PROFILE):
the decompressed payload's first byte is ftype: 0 keyframe, 1 inter.
"""
import struct
import sys
import zlib


def count(path):
    data = open(path, "rb").read()
    off = 18  # skip header
    key = inter = 0
    key_frames = []
    while off + 4 <= len(data):
        (n,) = struct.unpack(">I", data[off : off + 4])
        off += 4
        msg = data[off : off + n]
        off += n
        if len(msg) < 6 or msg[4] != 4:
            continue  # adaptive or malformed; not profile
        idx = struct.unpack(">I", msg[0:4])[0]
        try:
            ftype = zlib.decompress(msg[5:])[0]
        except zlib.error:
            continue
        if ftype == 0:
            key += 1
            key_frames.append(idx)
        else:
            inter += 1
    print(
        f"{path}: {key} keyframes (at {key_frames}) + {inter} inter"
        f" = {key + inter} profile frames"
    )


if __name__ == "__main__":
    count(sys.argv[1])
