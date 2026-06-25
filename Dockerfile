# Multi-stage build for the self-hosted library server.
# Includes ffmpeg in the runtime image for the (opt-in) transcode tiers.

FROM rust:1-bookworm AS builder
WORKDIR /build
COPY . .
RUN cargo build --release --locked

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates ffmpeg \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/chordia-library /usr/local/bin/chordia-library
# Music is mounted read-only; data_dir holds the SQLite index + cache + credentials.
VOLUME ["/music", "/data"]
EXPOSE 8443
ENV CHORDIA_LIBRARY_CONFIG=/data/chordia-library.toml
ENTRYPOINT ["chordia-library"]
