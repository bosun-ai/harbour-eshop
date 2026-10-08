FROM rust:1.97.1-bookworm AS build
WORKDIR /gateway
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release

FROM debian:bookworm-slim
COPY --from=build /gateway/target/release/eshop-gateway /usr/local/bin/eshop-gateway
USER 65532:65532
EXPOSE 8002
ENTRYPOINT ["/usr/local/bin/eshop-gateway"]
