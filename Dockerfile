# syntax=docker/dockerfile:1

FROM rust:1.97-slim-trixie AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked -p eve-server --bins \
 && cp target/release/eve-server target/release/fakeclient /usr/local/bin/

FROM debian:trixie-slim
COPY --from=build /usr/local/bin/eve-server /usr/local/bin/fakeclient /usr/local/bin/
COPY crates/eve-marshal/tests/data/strings.txt /app/strings.txt
RUN useradd --system --no-create-home rmr && mkdir -p /app/dumps && chown rmr /app/dumps
USER rmr
WORKDIR /app
ENV RMR_BIND=0.0.0.0:26000 \
    RMR_WEB=0.0.0.0:8080 \
    RMR_STRINGS=/app/strings.txt \
    RMR_DUMP_DIR=/app/dumps
EXPOSE 26000 8080
CMD ["eve-server"]
