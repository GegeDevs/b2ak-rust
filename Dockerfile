# syntax=docker/dockerfile:1

# Alpine/musl keeps both the builder and the final image small. The
# `magiskboot` binary extracted at runtime from the Magisk APK is statically
# linked, so it runs unchanged on musl.
FROM rust:1.89-alpine AS builder

WORKDIR /build

# `ring` (via rustls) and the bundled SQLite used by grammers-session need a C
# toolchain and CMake. Alpine's Rust image already ships the musl target.
RUN apk add --no-cache build-base cmake

# Cache dependencies in their own layer. The dummy main keeps Cargo happy
# without invalidating the dependency build whenever `src/` changes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
    && printf 'fn main() {}\n' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release --locked

# ---- Runtime stage ---------------------------------------------------------
FROM alpine:3.24

# git: cloning the AnyKernel repository
# ca-certificates: Telegram, GitHub and gofile.io HTTPS
RUN apk add --no-cache git ca-certificates

COPY --from=builder /build/target/release/b2ak /usr/local/bin/b2ak

# Session database, usage.json, cached magiskboot and per-job workspaces all
# live here, so it must be a persistent volume.
RUN mkdir -p /app/workspace
ENV WORKSPACE_DIR=/app/workspace \
    RUST_LOG=info

VOLUME ["/app/workspace"]

CMD ["b2ak"]
