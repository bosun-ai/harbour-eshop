FROM rust:1-bookworm AS build
WORKDIR /gateway
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release --locked

FROM debian:bookworm-slim
COPY --from=build /gateway/target/release/eshop-gateway /usr/local/bin/eshop-gateway
USER 65534:65534
EXPOSE 8002
HEALTHCHECK --interval=10s --timeout=12s CMD ["eshop-gateway", "healthcheck"]
ENTRYPOINT ["eshop-gateway"]
