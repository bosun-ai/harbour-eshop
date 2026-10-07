FROM rust:1.97.1-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
COPY --from=build /build/target/release/eshop-gateway /usr/local/bin/eshop-gateway
USER 65532:65532
HEALTHCHECK --interval=10s --timeout=12s --start-period=10s CMD ["eshop-gateway", "check-ready"]
ENTRYPOINT ["eshop-gateway"]
CMD ["serve"]
