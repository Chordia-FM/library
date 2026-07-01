# library - Chordia

> The lightweight, self-hosted server you run at home or in the cloud. It scans your music,
> indexes it, and streams it **bit-perfect** directly to authorized clients, so your audio never
> touches the Hub.

[![CI](https://img.shields.io/badge/CI-passing-brightgreen)](#)
[![Release](https://img.shields.io/badge/release-0.1.0-blue)](#)
[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-orange)](./LICENSE)

## Overview

A single static binary. It pairs once with your global account, scans the folders you point it
at, indexes metadata + fingerprints into local SQLite, and serves audio over authenticated HTTP
Range. It also relays tracks for DJ rooms and buffers scrobbles when the Hub is unreachable.

Each physical server can host several **logical libraries** (e.g. "Hi-Fi Archive", "Family
Collection") and share each independently. See the
[topology](https://github.com/chordia-fm/contracts/blob/main/docs/ARCHITECTURE.md#1-topology).

## Architecture

- **Stack:** Rust · Axum 0.8 · Tokio · SQLx · SQLite.
- **Responsibilities:** pairing · scan + watch (with full re-index + progress) · metadata +
  fingerprint · catalog API · bit-perfect Range streaming · listener-controlled quality tiers ·
  own-copy match · edition + canonical-artist sync · opt-in acquisition · DJ relay ·
  offline scrobble buffer/forward · directory heartbeat.
- **Talks to:** the Hub (HTTPS control), clients (HTTPS Range), and peer libraries (relay pulls).
- **Security:** every request carries a Hub-signed capability token, validated **offline** against
  the Hub JWKS. Clients pin the server's TLS fingerprint advertised in the Hub directory.

## Connectivity

The server is **directly reachable**: open/forward `bind_port` on your router. There is no Hub
relay of normal streams. If you can't port-forward, front it with a tunnel (Cloudflare Tunnel /
Tailscale), which is documented but not required.

## Quick start

```bash
rustup update stable          # needs rustc >= 1.85
cp config/chordia-library.example.toml chordia-library.toml   # edit paths
cargo run                     # scans, indexes, serves on bind_port
curl localhost:8443/health    # -> ok
```

## Configuration

TOML file (path via `CHORDIA_LIBRARY_CONFIG`, default `./chordia-library.toml`). See
[`config/chordia-library.example.toml`](./config/chordia-library.example.toml).

## Acquisition (opt-in)

Off by default. When enabled, the library executes download jobs you trigger from the Manager: it
searches your own Prowlarr for a missing release, scores the results against your quality profile
(highest quality wins), hands the pick to your qBittorrent, then imports the finished files into the
target library so they index and sync like anything else. Chordia ships no indexers or trackers; you
supply your own, and you are responsible for the legality of what you acquire. Configure it under
`[acquisition]` (see the example config); a remote or shared seedbox is supported via
`remote_path`/`local_path`.

## Development

```bash
cargo test
cargo clippy -- -D warnings
sqlx migrate run --database-url sqlite://data/library.sqlite
```

## License

AGPL-3.0, see [LICENSE](./LICENSE).
