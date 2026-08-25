# Multi-stage build for the self-hosted library server.
# Includes ffmpeg in the runtime image for the (opt-in) transcode tiers.
#
# POLYREPO: this crate depends on the sibling crate `chordia-contracts` (../contracts) via a path
# dependency, so build with the WORKSPACE ROOT as the context (the folder that holds library/ and
# contracts/ side by side), not the library/ folder. The CI image job checks out contracts next to
# library/ and builds with `context: .` / `file: library/Dockerfile`.

FROM rust:1-bookworm AS builder
WORKDIR /build
COPY contracts/ ./contracts/
COPY library/ ./library/
WORKDIR /build/library
# No --locked: the contracts checkout floats in the sibling model.
#
# Cache the cargo registry + target dir across builds (BuildKit). Without this, the `COPY library/`
# layer above invalidates on ANY source edit and every dependency recompiles from scratch — minutes
# per rebuild for a three-line change. With it, only the changed crates rebuild.
#
# The binary is copied OUT inside this RUN on purpose: a cache mount is not part of the image layer,
# so `target/` does not exist for a later `COPY --from=builder`.
#
# `sharing=locked` is NOT optional, and removing it breaks the deploy rather than slowing it.
# A cache mount with no `id=` is keyed by its TARGET PATH, so every Dockerfile in this workspace
# that mounts /usr/local/cargo/registry shares one mount - and `docker compose up --build backend
# frontend` builds them CONCURRENTLY. Cargo's package-cache lock lives at
# /usr/local/cargo/.package-cache, which is outside the mounted directory, so each container takes
# its own lock and neither sees the other; two cargo processes then unpack into one registry and
# race. The symptom is a build that dies with `failed to unpack package ...: failed to open
# .cargo-ok: File exists (os error 17)` and stays dead until the cache mount is pruned.
#
# Locked rather than separate `id=`s on purpose: the point of sharing the registry is downloading
# the crates once. Separate ids would double the disk and re-download everything per image.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
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
