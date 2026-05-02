FROM rust:1.95.0-bookworm AS builder
WORKDIR /app
RUN apt-get update && \
    apt-get install -y pkg-config libssl-dev && \
    rm -rf /var/lib/apt/lists/*
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && \
    apt-get install -y libssl3 ca-certificates && \
    rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/tgin-bench /usr/local/bin/tgin-bench
CMD ["tail", "-f", "/dev/null"]
