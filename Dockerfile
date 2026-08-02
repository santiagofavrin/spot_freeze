# syntax=docker/dockerfile:1

# Current stable Rust on the current stable Debian suite.
# All Linux deps are pure Rust or dlopen-based (wayland-client dlopen backend,
# xkbcommon-dl), so the builder needs no system C libraries.
FROM rust:1.97.1-slim-trixie AS base
WORKDIR /app

# Warm the dependency cache (including the linux target-gated deps, resolved
# automatically because the builder is Linux) so manifest-only layers stay
# cheap and source-only changes recompile just the local crate.
FROM base AS deps
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && cargo build --locked \
    && cargo build --release --locked \
    && rm -rf src

FROM deps AS test
COPY src ./src
COPY tests ./tests
# `assets/` holds files embedded at compile time via include_bytes! (the
# legend's Inter font faces), so the crate cannot build without it.
COPY assets ./assets
# BuildKit COPY keeps the (older) git mtimes; touch sources so cargo rebuilds
# the local crate instead of reusing the deps stage's dummy-lib artifacts.
RUN find src tests assets -type f -exec touch {} + \
    && cargo test --locked
CMD ["cargo", "test", "--locked"]

FROM deps AS build
COPY src ./src
COPY tests ./tests
COPY assets ./assets
RUN find src tests assets -type f -exec touch {} + \
    && cargo build --release --locked \
    && mkdir -p /out \
    && cp target/release/spotfreeze /out/spotfreeze

# Bare binary for `docker build --target export --output type=local,dest=target/docker .`
FROM scratch AS export
COPY --from=build /out/spotfreeze /spotfreeze

# Default target stays the usable builder image; the scratch export stage
# above is opt-in via --target export.
FROM build
