FROM rust:1.97.1-slim-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src/ ./src/
RUN cargo build --locked --release

FROM debian:bookworm-slim
COPY --from=build /build/target/release/eshop-gateway /usr/local/bin/eshop-gateway
USER 65534:65534
EXPOSE 8002
ENTRYPOINT ["/usr/local/bin/eshop-gateway"]
