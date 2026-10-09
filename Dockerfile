# Harbour is not packaged by current Debian/Ubuntu releases, so build it from source.
FROM debian:bookworm-slim AS harbour
ARG HARBOUR_REF=529b0d42939610a13da1572cd7861da6f9fa2d47
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential ca-certificates curl libssl-dev \
 && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL "https://github.com/harbour/core/archive/${HARBOUR_REF}.tar.gz" | tar -xz -C /opt \
 && mv /opt/core-* /opt/harbour-src \
 && make -C /opt/harbour-src -j"$(nproc)" install HB_INSTALL_PREFIX=/opt/harbour \
 && rm -rf /opt/harbour-src

FROM harbour AS build
WORKDIR /src
COPY app/ ./
COPY boundary/ /boundary/
RUN /opt/harbour/bin/hbmk2 eshop.prg /boundary/selector.prg /boundary/process.prg /boundary/ownership.prg -static -o/src/eshop

FROM rust:1-bookworm AS rust-build
WORKDIR /slices
COPY slices/ ./
RUN sh build-bins.sh

FROM build AS test-build
COPY tests/ /tests/
RUN /opt/harbour/bin/hbmk2 eshop.prg /boundary/selector.prg /boundary/process.prg /tests/ownership.prg -static -o/src/eshop-test \
 && /opt/harbour/bin/hbmk2 /tests/native.prg /boundary/selector.prg /boundary/process.prg -w3 -es2 -gtstd -static -o/src/native-test -workdir=/coverage -inc -cflag=--coverage -ldflag=--coverage

FROM rust-build AS rust-test-build
RUN cargo build --locked --release --example hello

FROM test-build AS boundary-coverage
COPY --from=rust-test-build /slices/target/release/examples/hello /opt/eshop-slices/hello
RUN cd /coverage \
 && gcov selector.c process.c \
 && /src/native-test \
 && gcov selector.c process.c

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends openssl libssl3 \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY app/ ./
COPY --from=build /src/eshop ./eshop
COPY docker-entrypoint.sh /usr/local/bin/
EXPOSE 8002
ENTRYPOINT ["docker-entrypoint.sh"]

FROM runtime AS slices
COPY --from=rust-build /packaged-slices/ /opt/eshop-slices/

FROM slices AS boundary-test
COPY --from=test-build /src/eshop-test ./eshop
COPY --from=test-build /src/native-test /usr/local/bin/native-test
COPY --from=rust-test-build /slices/target/release/examples/hello /opt/eshop-slices/hello

# Keep the default target legacy-only, irrespective of optional slice packaging.
FROM runtime AS legacy
