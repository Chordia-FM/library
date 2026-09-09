# Multi-stage build for the self-hosted library server.
# Includes ffmpeg in the runtime image for the (opt-in) transcode tiers and the Discord bot's
# fallback decoder.
#
# POLYREPO: this crate depends on the sibling crates `chordia-contracts` (../contracts) and
# `chordia-i18n` (../i18n) via path dependencies, so build with the WORKSPACE ROOT as the context
# (the folder that holds library/, contracts/ and i18n/ side by side), not the library/ folder. The
# CI image job checks both out next to library/ and builds with `context: .` /
# `file: library/Dockerfile`.

FROM rust:1-bookworm AS builder
# cmake: the Discord bot's Opus encoder (libopus, via `libopus_sys`) is built from source.
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY contracts/ ./contracts/
COPY i18n/ ./i18n/
COPY library/ ./library/
WORKDIR /build/library
# No --locked: the contracts and i18n checkouts float in the sibling model.
#
# Cache the cargo registry + target dir across builds (BuildKit). Without this, the `COPY library/`
# layer above invalidates on ANY source edit and every dependency recompiles from scratch — minutes
# per rebuild for a three-line change. With it, only the changed crates rebuild.
#
# The binary is copied OUT inside this RUN on purpose: a cache mount is not part of the image layer,
# so `target/` does not exist for a later `COPY --from=builder`.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/library/target \
    cargo build --release \
    && cp target/release/chordia-library /build/chordia-library

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ffmpeg \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/chordia-library /usr/local/bin/chordia-library
# Music is mounted read-only; data_dir holds the SQLite index + cache + credentials.
VOLUME ["/music", "/data"]
EXPOSE 8443
ENV CHORDIA_LIBRARY_CONFIG=/data/chordia-library.toml
ENTRYPOINT ["chordia-library"]
