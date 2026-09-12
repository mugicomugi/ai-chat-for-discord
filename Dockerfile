FROM rust:1.98-bookworm AS source
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY src ./src
COPY migrations ./migrations

FROM source AS build
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --locked --release && cp target/release/discord-discussion-bot /app/bot

FROM source AS test
COPY tests ./tests
RUN rustup component add clippy rustfmt
RUN cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked
ENTRYPOINT ["cargo", "test", "--locked"]

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home bot
WORKDIR /app
COPY --from=build /app/bot /app/bot
USER 10001:10001
ENTRYPOINT ["/app/bot"]
