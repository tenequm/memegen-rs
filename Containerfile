# syntax=docker/dockerfile:1
# memegen-rs container image. Builds the Rust server and bakes the template
# corpus in, so the image is fully self-contained and durable.
# Must run on linux/amd64 (Cloudflare Containers requirement).

# ---- build stage ----
FROM rust:1.95-slim-bookworm AS build
WORKDIR /src

# Cache dependency compilation separately from source changes.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

# Real source.
COPY src ./src
COPY assets ./assets
RUN touch src/main.rs && cargo build --release --locked

# ---- runtime stage ----
FROM debian:bookworm-slim AS runtime
WORKDIR /app

# ca-certificates lets the `custom?background=<https url>` fetch succeed
# regardless of which rustls root set is compiled in.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/memegen-rs /usr/local/bin/memegen-rs

# Template corpus baked in (background images + config.yml per folder).
COPY templates ./templates

ENV PORT=5005 \
    MEMEGEN_TEMPLATES_DIR=/app/templates

EXPOSE 5005
CMD ["memegen-rs"]
