# syntax=docker/dockerfile:1

# ---------------------------------------------------------------- build
FROM rust:1-slim-bookworm AS build

WORKDIR /app
# Only what the server needs. The SDK packages and examples are not part of
# the image, so a change to them does not invalidate this layer.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates

# Every statement is a runtime `sqlx::query`, never a `query!` macro, so the
# build needs no database. Migrations are embedded by `sqlx::migrate!`.
# `target/` lives in a cache mount, so the binary is copied out in the same RUN.
RUN --mount=type=cache,id=otto-flags-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=otto-flags-target,target=/app/target,sharing=locked \
    cargo build --release -p flags-server \
    && cp target/release/flags-server /usr/local/bin/flags-server

# ---------------------------------------------------------------- runtime
FROM debian:bookworm-slim AS runtime

# Postgres over TLS and every call to the platform verify against these roots.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Nothing here writes to the filesystem or binds a privileged port.
RUN useradd --create-home --uid 10001 --shell /usr/sbin/nologin flags
USER flags

COPY --from=build /usr/local/bin/flags-server /usr/local/bin/flags-server

ENV FLAGS_BIND=0.0.0.0:8080 \
    FLAGS_LOG_FORMAT=json \
    RUST_LOG=info

EXPOSE 8080

# Exec form: the process is PID 1 and receives SIGTERM directly, so graceful
# shutdown (and the final usage flush) actually runs on every deploy.
ENTRYPOINT ["/usr/local/bin/flags-server"]
