# Security

## Trust model

`asciline-server` is a **LAN / localhost streaming server**: it decodes local
media and streams it to browser clients. It is not a multi-tenant service:

- Anyone who can reach the bound address can **watch and control playback**
  (stream the queue, seek, reinit, change filters).
- There is **no TLS**: audio/video and the optional token travel in plaintext.
  Put the server behind a TLS-terminating reverse proxy (Caddy/nginx) if it
  must cross untrusted networks.

## What is enforced

| Control | Detail |
|---|---|
| Default bind | `127.0.0.1` — loopback only unless `--host 0.0.0.0` is passed |
| WS origin check | Non-matching `Origin` headers are rejected (cross-site WebSocket hijack); non-browser clients (no Origin) are allowed |
| Optional token | `--token <secret>` makes `/ws`, `/audio`, `/scrub`, `/scrub_sprite` require `?token=<secret>`. The original browser client does not send one, so enable it only when you can append the token to the URLs. |
| Connection cap | `--max-clients N` (default 8): each WebSocket client owns an ffmpeg child, a decode thread and encode work, so unbounded sockets are a resource-exhaustion vector. Overflow connections get `503`. |
| ffmpeg cap | `--max-ffmpeg N` (default 4): bounds concurrent `/audio` transcodes and scrub-sprite builds. |
| Static files | `/static/*` serves only a whitelist (`app.js`, `style.css`, `codec.js`) — no path traversal. |
| Liveness | `GET /healthz` → `200 {"status":"ok","clients":{...}}` for orchestrators / the Docker healthcheck. |

## Untrusted input

The decoders parse attacker-controllable data (`asciline-player` opens
arbitrary `.ascf` files):

- zlib decompression is capped at 64 MiB (decompression bombs fail instead of
  exhausting memory).
- RLE runs are bounds-checked and the expanded output is capped (a truncated
  run used to panic with an out-of-bounds slice).
- The tag-4 profile decoder rejects keyframes declaring grids over 4 M pixels
  (a crafted header previously requested a multi-GB allocation).
- `asciline-player` caps per-record lengths before allocating; grid `--cols`/
  `--rows` (including playlist overrides) are clamped to 2000.
- `tests/fuzz_malformed.rs` property-tests every parse entry point with
  arbitrary bytes (proptest, runs in CI): **no input may panic a decoder**.

Media files are decoded by the `ffmpeg`/`ffprobe` binaries, invoked with argv
(no shell interpolation); ffmpeg's own robustness is your defense there.

## Logging

`RUST_LOG=info asciline-server ...` enables tracing output (lifecycle,
connection rejections, shutdown); per-frame console logs (`[PLAYING]`, `[BW]`)
stay on stdout.

## Reporting

Please open an issue at <https://github.com/KidIkaros/asciline-rust> with the
input that triggered the problem.
