FROM rust:1-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release --locked

FROM debian:bookworm-slim
COPY --from=build /build/target/release/eshop-gateway /usr/local/bin/eshop-gateway
USER 65534:65534
EXPOSE 8002
ENTRYPOINT ["/usr/local/bin/eshop-gateway"]
