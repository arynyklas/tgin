FROM rust:1.99.0-trixie AS builder
WORKDIR /app
COPY . .
RUN cargo build --release --locked

FROM debian:trixie-slim
RUN apt-get update && \
    apt-get install -y ca-certificates && \
    rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/tgin-bench /usr/local/bin/tgin-bench
EXPOSE 8090
CMD ["tgin-bench", "serve"]
