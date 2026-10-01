FROM rust:1.98-trixie@sha256:a8a5f0a1e5fe7dfe1d352591e4a1c7dd2c08fd70475cae872cf3458ba0df0546 AS source
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY src ./src
COPY migrations ./migrations
# Compiled into the binary: the web UI's files and the privacy policy / terms pages. Only these
# two documents, so editing the runbook does not invalidate the build cache.
COPY static ./static
COPY docs/privacy.md docs/terms.md ./docs/

FROM source AS build
# .cargo/config.toml keeps local Docker Desktop builds at one job; CI raises it.
ARG CARGO_BUILD_JOBS=1
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS} cargo build --locked --release \
    && cp target/release/discord-discussion-bot /app/bot

FROM source AS test
COPY tests ./tests
RUN rustup component add clippy rustfmt
RUN cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked
ENTRYPOINT ["cargo", "test", "--locked"]

FROM debian:trixie-slim@sha256:a99cfc517144bc59b1978475ec53b46ecabec7e43635402ee5b77cc54cd1b20a AS runtime-base
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home bot
WORKDIR /app
USER 10001:10001
ENTRYPOINT ["/app/bot"]
# GET /healthz (database and Discord gateway); always healthy while the web UI is off. Defined
# here rather than in compose.yaml: binaries before M2 treat `healthcheck` as a normal start, so
# compose must not run it against an image rolled back to.
HEALTHCHECK --interval=30s --timeout=10s --start-period=60s --retries=3 CMD ["/app/bot", "healthcheck"]

# Image with a binary compiled on the production VM by scripts/build-image.sh, which passes it
# as the named build context "prebuilt" (target/ is excluded from the main context).
FROM runtime-base AS runtime-prebuilt
COPY --from=prebuilt discord-discussion-bot /app/bot

# Default target: compile inside Docker (development machines and CI).
FROM runtime-base AS runtime
COPY --from=build /app/bot /app/bot
