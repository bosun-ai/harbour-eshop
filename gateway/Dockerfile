FROM rust:1.88-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release

FROM debian:bookworm-slim
COPY --from=build /src/target/release/eshop-gateway /usr/local/bin/eshop-gateway
USER 65532:65532
EXPOSE 8002
ENTRYPOINT ["eshop-gateway"]
CMD ["serve"]
