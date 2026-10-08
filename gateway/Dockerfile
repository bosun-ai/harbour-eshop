FROM rust:1.97-bookworm AS build
WORKDIR /src/gateway
COPY gateway/Cargo.toml gateway/Cargo.lock ./
COPY gateway/src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
COPY --from=build /src/gateway/target/release/eshop-gateway /usr/local/bin/eshop-gateway
USER 65534:65534
EXPOSE 8002
ENTRYPOINT ["eshop-gateway"]
