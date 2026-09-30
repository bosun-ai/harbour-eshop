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
RUN /opt/harbour/bin/hbmk2 eshop.prg -static -o/src/eshop

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends openssl libssl3 \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY app/ ./
COPY --from=build /src/eshop ./eshop
COPY docker-entrypoint.sh /usr/local/bin/
EXPOSE 8002
ENTRYPOINT ["docker-entrypoint.sh"]
