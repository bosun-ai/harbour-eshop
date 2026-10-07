FROM rust:1.97.1-bookworm AS build
WORKDIR /build
COPY gateway/ ./gateway/
RUN cargo build --locked --release --manifest-path gateway/Cargo.toml

FROM debian:bookworm-slim
COPY --from=build /build/gateway/target/release/eshop-gateway /usr/local/bin/eshop-gateway
USER 65532:65532
EXPOSE 8002
ENTRYPOINT ["eshop-gateway"]
