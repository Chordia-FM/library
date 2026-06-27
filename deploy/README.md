# Running a Chordia library with Docker Compose

Self-host your music with the published image. Your library pairs to the Chordia Hub and streams audio
directly to clients. The Hub never sees your audio.

> See also the top-level [`SELF_HOSTING.md`](../../SELF_HOSTING.md) for the binary/systemd install,
> adding music, and the in-process TLS option.

## 1. Configure

```bash
cd deploy
cp .env.example .env                                # set MUSIC_DIR to your music folder
cp chordia-library.prod.example.toml chordia-library.toml
# edit chordia-library.toml -> set hub_endpoint to your PUBLIC https URL (see below)
```

## 2. Make it reachable (pick one)

Browsers need a real-CA HTTPS URL (they can't pin self-signed certs).

**A. Cloudflare Tunnel (recommended): no inbound ports, no cert management.**
```bash
docker compose -f compose.prod.yaml up -d
cloudflared tunnel --url http://localhost:8443      # prints https://<random>.trycloudflare.com
```
Put that https URL in `chordia-library.toml` as `hub_endpoint` (for a permanent URL, use a Named
Tunnel + DNS CNAME). The library re-advertises it to the Hub on every heartbeat.

**B. Public VPS with your own domain: nginx + Let's Encrypt edge overlay.**
```bash
# set LIBRARY_DOMAIN + CERTBOT_EMAIL in .env, point DNS at this host, then:
bash init-letsencrypt.sh
docker compose -f compose.prod.yaml -f compose.edge.yaml up -d
```
Set `hub_endpoint = "https://<LIBRARY_DOMAIN>"` in the toml.

## 3. Pair

```bash
docker compose -f compose.prod.yaml logs library     # find the printed setup URL
```
Open the setup URL through your public HTTPS endpoint, sign in with your Chordia account, and pick the
folders to index. Pairing credentials persist in the `library-data` volume, so keep it.

## Notes

- **Music** is mounted read-write at `/data/music` (the library sandboxes its folder browser to that
  subtree) so Organize / Dedupe can run when you enable them
  (both opt-in, off by default, so nothing is touched until you do; append `:ro` in `compose.prod.yaml`
  to keep it untouched). Point `MUSIC_DIR` at a folder of music (any format `symphonia` decodes); the
  setup flow scans it.
- **ffmpeg** is bundled in the image (needed only for the lower quality tiers; the `Original` tier is
  bit-perfect passthrough).
- **Persistence:** the `library-data` volume holds the SQLite index, transcode cache, and pairing
  credentials. Back it up or you'll re-pair and re-scan.
- **Updating:** `docker compose -f compose.prod.yaml pull && docker compose -f compose.prod.yaml up -d`.
  Migrations apply automatically at startup.
