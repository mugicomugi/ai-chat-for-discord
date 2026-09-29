FROM rust:1.98-trixie@sha256:a8a5f0a1e5fe7dfe1d352591e4a1c7dd2c08fd70475cae872cf3458ba0df0546 AS source
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY src ./src
COPY migrations ./migrations

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

FROM debian:trixie-slim@sha256:a99cfc517144bc59b1978475ec53b46ecabec7e43635402ee5b77cc54cd1b20a AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home bot
WORKDIR /app
COPY --from=build /app/bot /app/bot
USER 10001:10001
ENTRYPOINT ["/app/bot"]
