FROM rust:1.87-slim AS builder
WORKDIR /build
RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs
RUN cargo build --release
RUN rm -rf src

COPY src/ src/
COPY app/ app/
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates curl && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /build/target/release/tgstate /app/tgstate
COPY --from=builder /build/app/ /app/app/

RUN mkdir -p /app/data \
    && useradd -r -u 10001 -d /app appuser \
    && chown -R appuser:appuser /app \
    && chmod 777 /app/data
ENV DATA_DIR=/app/data
ENV PORT=7860
EXPOSE 7860
USER appuser

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD curl -fsS http://127.0.0.1:7860/api/health || exit 1

LABEL org.opencontainers.image.source=https://github.com/buyi06/tgstate-rust
LABEL org.opencontainers.image.description="Telegram-based private file storage system built with Rust"
LABEL org.opencontainers.image.licenses=MIT

CMD ["./tgstate"]
