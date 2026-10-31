# The build stage runs on the build machine's own architecture and
# cross-compiles, so building an amd64 image on Apple Silicon needs no
# emulation: docker build --platform linux/amd64 .
# Base images are pinned by version and digest; update both together (the
# runtime's glibc must be at least the build image's).
FROM --platform=$BUILDPLATFORM rust:1.99.1-trixie@sha256:09027f873d45b8ecd1dc86f1fa13b7a62b875c961f017a0e331267b350430137 AS build
ARG TARGETARCH
ARG BUILDARCH
RUN if [ "$TARGETARCH" != "$BUILDARCH" ]; then \
        apt-get update \
        && apt-get install -y --no-install-recommends crossbuild-essential-$TARGETARCH \
        && rm -rf /var/lib/apt/lists/*; \
    fi
RUN case "$TARGETARCH" in \
        amd64) echo x86_64-unknown-linux-gnu ;; \
        arm64) echo aarch64-unknown-linux-gnu ;; \
        *) echo "unsupported TARGETARCH $TARGETARCH" >&2; exit 1 ;; \
    esac > /rust-target \
    && rustup target add "$(cat /rust-target)"
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc

# The commit shown by `tolk --version` and in the startup log.
ARG GIT_SHA=""
ENV GIT_SHA=${GIT_SHA}

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    target="$(cat /rust-target)" \
    && cargo build --release --locked --target "$target" \
    && cp "target/$target/release/tolk" /tolk

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
