# DEPLOYMENT.md — running asciline-server in production

`asciline-server` is a single static binary + an `ffmpeg` dependency. It ships
no TLS, no database, and no state on disk — deployment is "run the binary next
to a media folder, put a reverse proxy in front". Three supported modes:

| Mode | Best for | What you need |
|---|---|---|
| [systemd](#1-systemd-bare-metal) | one host, no containers | `deploy/asciline.service` |
| [Docker Compose](#2-docker-compose) | containerized hosts, easy upgrades | `deploy/docker-compose.yml` |
| [Caddy reverse proxy](#3-caddy-tls-reverse-proxy) | public TLS termination (any mode) | `deploy/Caddyfile` |

All modes assume the binary is at `/usr/local/bin/asciline-server` and ffmpeg
is on `PATH` (Docker handles this for you).

---

## Security defaults (read this first)

From [SECURITY.md](SECURITY.md):

- The server binds **127.0.0.1:8000 by default** — it is not exposed to the
  network until you say so.
- WebSocket upgrades check the browser `Origin` header (anti-CSWSH).
- `--max-clients` caps concurrent WebSocket clients (each owns an ffmpeg child
  + decode thread); overflow gets a `503`.
- `--max-ffmpeg` caps concurrent ffmpeg spawns for `/audio` + scrub builds.
- `--token SECRET` (or `ASCILINE_TOKEN` env) requires `?token=SECRET` on
  `/ws`, `/audio`, `/scrub`, `/scrub_sprite`.
- `/healthz` is unauthenticated liveness for orchestrators.

**The original browser client does not send a token.** If you enable auth, the
UI URL becomes `https://host/?token=SECRET` (the client appends it to the
WebSocket/audio requests) — or terminate auth at a reverse proxy instead.

### Choosing the exposure surface

- **Private / localhost only:** bind `127.0.0.1`, no token, no proxy.
- **LAN without TLS:** bind `0.0.0.0` + `--token`, firewalled to the LAN.
- **Public internet:** bind `127.0.0.1`, put Caddy (TLS) in front, and either
  pass `--token` or let Caddy's `basic_auth` guard the routes.

Never bind `0.0.0.0` without a token. There is no built-in TLS.

---

## 1. systemd (bare metal)

```bash
# one-time
sudo useradd --system --create-home --shell /usr/sbin/nologin asciline
sudo mkdir -p /var/lib/asciline/videos /etc/asciline
sudo chown -R asciline:asciline /var/lib/asciline
sudo install -m 0644 deploy/asciline.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now asciline
```

Drop media files into `/var/lib/asciline/videos/`. If you use auth, set the
secret once (permissions 0600 so only root reads it):

```bash
echo 'ASCILINE_TOKEN=change-me' | sudo tee /etc/asciline/env > /dev/null
sudo chmod 600 /etc/asciline/env
```

Edit `/etc/systemd/system/asciline.service` to tune
`--cols` / `--mode` / `--max-clients`, then `systemctl daemon-reload && systemctl restart asciline`.

Useful commands:

```bash
journalctl -u asciline -f          # logs (RUST_LOG=info by default in the unit)
curl -s http://127.0.0.1:8765/healthz   # {"status":"ok","in_use":N,"max":M}
systemctl status asciline
```

## 2. Docker Compose

```bash
cd deploy
cp .env.example .env      # edit ASCILINE_TOKEN, ports, tune flags
docker compose up -d --build
docker compose logs -f asciline
```

The image already contains ffmpeg, runs as a non-root user, and has a
`/healthz` healthcheck. Media goes in `deploy/videos/`. The compose file binds
the container to **127.0.0.1 only** — put Caddy in front for public access.

Upgrade: `git pull && docker compose up -d --build` (or pull a tagged image /
release tarball and `docker build`).

## 3. Caddy (TLS reverse proxy)

Public-facing setup:

1. Run asciline-server bound to `127.0.0.1:8765` (systemd unit or compose
   both do this).
2. Put `deploy/Caddyfile` at `/etc/caddy/Caddyfile`, replace
   `asciline.example.com` with your domain, `caddy run --config ...`.
3. Caddy terminates TLS and proxies WebSocket automatically.

For auth you have two compatible options:

- **App-level (recommended for simplicity):** uncomment the `--token` in the
  unit/compose; clients use `https://host/?token=SECRET`.
- **Proxy-level:** use Caddy's `basic_auth` on the routes and keep the app
  unauthenticated. WS handshakes carry the `Authorization` header, which
  `basic_auth` checks before the upgrade — this works with the stock client
  only if it is modified to send credentials; prefer app-level auth.

## Tuning knobs

| Flag | Default | When to change |
|---|---|---|
| `--cols N` | 200 text / 450 pixel | Larger grid = finer picture, more CPU + bandwidth |
| `--mode 1-6` | 1 | 6 = 16 M colours (heaviest) |
| `--fps N` | source rate (no cap) | Cap upstream bandwidth / CPU: `--fps 30` |
| `--max-clients` | 8 | RAM: each client runs an ffmpeg child + decode thread |
| `--max-ffmpeg` | 4 | Concurrent `/audio` + thumbnail transcodes |
| `--quality {lossless,high,balanced,low}` | lossless | Lossy deltas shrink bandwidth a lot |
| `RUST_LOG=info` | off | `info` = lifecycle, `debug` = per-frame detail |

A rough capacity model: each client ≈ one ffmpeg decode process + one rayon
map/encode job. `--max-clients 8` on a 4-core box is comfortable; scale up with
cores, down with memory pressure. Monitor `/healthz` `in_use`/`max` and the
`[+]` connect lines in the logs.

## Upgrades & rollback

Releases are tagged `v*`; each tag carries a tarball with the three binaries,
`web/`, checksums, and `BUILD_INFO.txt`. Rollback = reinstall the previous
tarball's binaries (config lives outside the binary). The `.ascf` files
produced by the compiler are forward/backward compatible across the 0.x
series (documented container version in the 18-byte header).
