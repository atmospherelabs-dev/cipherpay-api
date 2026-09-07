FROM rust:1.97.1-bookworm AS builder
WORKDIR /app
RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates libssl3 curl && rm -rf /var/lib/apt/lists/* && useradd --system --uid 10001 cipherpay && mkdir /data && chown cipherpay /data
COPY --from=builder /app/target/release/cipherpay /usr/local/bin/cipherpay
USER cipherpay
WORKDIR /data
ENV API_HOST=0.0.0.0 DATABASE_URL=sqlite:/data/cipherpay.db
EXPOSE 3080
HEALTHCHECK CMD curl --fail --silent http://127.0.0.1:3080/api/health || exit 1
CMD ["cipherpay"]
