# The build stage runs on the build machine's own architecture and
# cross-compiles, so no platform needs emulation, e.g.
#   docker buildx build --platform linux/amd64,linux/arm64 .
# Every platform of the runtime image is supported (see the table below)
# except linux/arm/v5, which lacks the 64-bit atomics that Tolk and some of
# its dependencies need.
# Base images are pinned by version and digest; update both together (the
# runtime's glibc must be at least the build image's).
FROM --platform=$BUILDPLATFORM rust:1.99.1-trixie@sha256:09027f873d45b8ecd1dc86f1fa13b7a62b875c961f017a0e331267b350430137 AS build
ARG TARGETARCH
ARG TARGETVARIANT
# Per target platform: the Rust target, the Debian architecture and the GCC
# prefix that cross-compiles (and links) for it.
RUN set -eu; \
    case "$TARGETARCH$TARGETVARIANT" in \
        amd64)         triple=x86_64-unknown-linux-gnu;      deb=amd64;   gcc=x86_64-linux-gnu ;; \
        arm64|arm64v8) triple=aarch64-unknown-linux-gnu;     deb=arm64;   gcc=aarch64-linux-gnu ;; \
        armv7)         triple=armv7-unknown-linux-gnueabihf; deb=armhf;   gcc=arm-linux-gnueabihf ;; \
        386)           triple=i686-unknown-linux-gnu;        deb=i386;    gcc=i686-linux-gnu ;; \
        ppc64le)       triple=powerpc64le-unknown-linux-gnu; deb=ppc64el; gcc=powerpc64le-linux-gnu ;; \
        riscv64)       triple=riscv64gc-unknown-linux-gnu;   deb=riscv64; gcc=riscv64-linux-gnu ;; \
        s390x)         triple=s390x-unknown-linux-gnu;       deb=s390x;   gcc=s390x-linux-gnu ;; \
        *) echo "unsupported platform $TARGETARCH$TARGETVARIANT" >&2; exit 1 ;; \
    esac; \
    if [ "$deb" != "$(dpkg --print-architecture)" ]; then \
        apt-get update; \
        apt-get install -y --no-install-recommends "crossbuild-essential-$deb"; \
        rm -rf /var/lib/apt/lists/*; \
    fi; \
    rustup target add "$triple"; \
    { echo "export CARGO_BUILD_TARGET=$triple"; \
      echo "export CARGO_TARGET_$(echo "$triple" | tr 'a-z-' 'A-Z_')_LINKER=$gcc-gcc"; \
      echo "export CC_$(echo "$triple" | tr - _)=$gcc-gcc"; } > /build-env

# The commit shown by `tolk --version` and in the startup log.
ARG GIT_SHA=""
ENV GIT_SHA=${GIT_SHA}

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,id=target-$TARGETARCH$TARGETVARIANT \
    . /build-env \
    && cargo build --release --locked \
    && cp "target/$CARGO_BUILD_TARGET/release/tolk" /tolk

FROM debian:trixie-20261027-slim@sha256:f6b08b09a05725f9431502d3b5714f5c4576e82c1a84fa1debe0c24a7e0c3ec4
# Trust store for the JMAP server's certificate, from the build image: the
# bundle is the same on every architecture, and copying it keeps this stage
# free of RUN steps, so cross-building needs no emulation.
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
LABEL org.opencontainers.image.title="Tolk" \
      org.opencontainers.image.description="Exchange ActiveSync for JMAP mail servers" \
      org.opencontainers.image.licenses="AGPL-3.0-only" \
      org.opencontainers.image.source="https://github.com/RBogie/tolk-jmap"
COPY --from=build /tolk /usr/local/bin/tolk
USER 1000:1000
ENV LISTEN_ADDR=0.0.0.0:8080
EXPOSE 8080
# Checked every 2 s while starting: Traefik only routes a healthy container.
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --start-interval=2s CMD ["tolk", "healthcheck"]
CMD ["tolk"]
