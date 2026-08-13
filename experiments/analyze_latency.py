#!/usr/bin/env python3
"""Join server + client per-frame latency logs and report a percentile breakdown.

Usage:
    python3 experiments/analyze_latency.py <server.log> <client.log>

Both logs are four whitespace-separated columns, one line per *sent* frame:

    server.log : frame_index  t_read  t_encode  t_send
    client.log : frame_index  t_recv  t_decode  t_render

Timestamps are monotonic-clock values converted to wall-clock nanoseconds at
process start, so logs from processes on the same host are directly comparable
(see `LatencyLog` in src/server.rs / src/bin/render.rs).

Stages (all in milliseconds, per frame):
    encode : t_encode - t_read   (map + codec on the blocking pool)
    wire   : t_recv   - t_send   (socket + client scheduling)
    decode : t_decode - t_recv   (CodecDecoder/ProfileDecoder)
    render : t_render - t_decode (rasterize + PPM write)
    total  : t_render - t_read   (frame-in -> display, end to end)

The report also lists frames only one side saw (skipped/dropped by backpressure
or a partial capture), and the effective fps over the joined span.
"""

import sys


def load(path):
    """Return {frame_index: (a, b, c)} for well-formed lines."""
    out = {}
    with open(path) as f:
        for line in f:
            parts = line.split()
            if len(parts) != 4:
                continue
            try:
                idx = int(parts[0])
                out[idx] = tuple(int(v) for v in parts[1:4])
            except ValueError:
                continue
    return out


def percentile(sorted_vals, p):
    """Linear-interpolated percentile of an ascending list (p in 0..1)."""
    if not sorted_vals:
        return 0.0
    k = (len(sorted_vals) - 1) * p
    lo = int(k)
    hi = min(lo + 1, len(sorted_vals) - 1)
    return sorted_vals[lo] + (sorted_vals[hi] - sorted_vals[lo]) * (k - lo)


def ms(ns):
    return ns / 1e6


def report(name, vals):
    if not vals:
        print(f"  {name:<30} (no frames)")
        return
    s = sorted(vals)
    mean = sum(vals) / len(vals)
    print(
        f"  {name:<30} n={len(vals):>4}  "
        f"p50={ms(percentile(s, 0.50)):7.2f} ms  "
        f"p95={ms(percentile(s, 0.95)):7.2f} ms  "
        f"p99={ms(percentile(s, 0.99)):7.2f} ms  "
        f"max={ms(max(vals)):7.2f} ms"
    )


def main():
    if len(sys.argv) != 3:
        print(__doc__)
        sys.exit(2)
    server = load(sys.argv[1])
    client = load(sys.argv[2])

    common = sorted(set(server) & set(client))
    only_server = sorted(set(server) - set(client))
    only_client = sorted(set(client) - set(server))

    if not common:
        print("no frames in common between the two logs — nothing to report")
        sys.exit(1)

    stages = {
        "encode (map+codec)": [],
        "wire (send->recv)": [],
        "decode": [],
        "render (raster+write)": [],
        "total (frame-in->display)": [],
    }
    for idx in common:
        tr, te, ts = server[idx]
        rc, rd, rr = client[idx]
        stages["encode (map+codec)"].append(te - tr)
        stages["wire (send->recv)"].append(rc - ts)
        stages["decode"].append(rd - rc)
        stages["render (raster+write)"].append(rr - rd)
        stages["total (frame-in->display)"].append(rr - tr)

    span_ns = max(client[i][2] for i in common) - min(server[i][0] for i in common)
    fps = len(common) / (span_ns / 1e9) if span_ns > 0 else 0.0

    print(f"joined frames : {len(common)}")
    print(f"server only   : {len(only_server)}  (skipped/dropped or not yet sent)")
    print(f"client only   : {len(only_client)}  (first/last frames, partial capture)")
    print(f"effective fps : {fps:.1f} over the joined span")
    print()
    print("per-frame latency by stage (ms):")
    for name, vals in stages.items():
        report(name, vals)

    # the worst frames — where scene cuts / stalls show up
    worst = sorted(common, key=lambda i: client[i][2] - server[i][0], reverse=True)[:5]
    print()
    print("worst frames by total latency (index, total ms):")
    for i in worst:
        print(f"  frame {i:>4}: {ms(client[i][2] - server[i][0]):.2f} ms")


if __name__ == "__main__":
    main()
