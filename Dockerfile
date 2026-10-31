# Tolk as a fully static (musl) binary in an image without a distribution.
#
# The build stage runs on the build machine's own architecture and
# cross-compiles with cargo-zigbuild (Zig as C compiler and linker for every
# target), so no platform needs emulation:
#   docker buildx build --platform linux/amd64,linux/arm64 .
# Supported: linux/amd64, arm64, arm/v7, 386, ppc64le and riscv64.
#
# `--target binary --output type=local,dest=dist` exports just the binary,
# the same file the image contains.
#
# Base image and tools are pinned by version and digest or checksum.
FROM --platform=$BUILDPLATFORM rust:1.99.1-trixie@sha256:09027f873d45b8ecd1dc86f1fa13b7a62b875c961f017a0e331267b350430137 AS build
ARG BUILDARCH
ARG TARGETARCH
ARG TARGETVARIANT

# Zig and cargo-zigbuild for the build machine, checked against pinned
# checksums (update version and checksums together).
ARG ZIG_VERSION=0.17.0
ARG ZIGBUILD_VERSION=0.24.0
RUN set -eu; \
    case "$BUILDARCH" in \
        amd64) zig_arch=x86_64; \
               zig_sha=1cbe9df9f27e6b78d14ccbca43b6703a404ef79ef1c463de901d7f088d4e2026; \
               zigbuild_target=x86_64-unknown-linux-musl; \
               zigbuild_sha=52be2cb85ad3875682564a29bac1832b2ab3db68b1c504765c92d5249cc9b96f ;; \
        arm64) zig_arch=aarch64; \
               zig_sha=9e8d11661d4ae3bd57702a3832781e23ad151dde5798e16a5ccd503f65234ff8; \
               zigbuild_target=aarch64-unknown-linux-gnu; \
               zigbuild_sha=de42626985f428033078dfc12ac8a59ee713f8e5627235ec883b1c899bee895c ;; \
        *) echo "unsupported build machine $BUILDARCH" >&2; exit 1 ;; \
    esac; \
    cd /tmp; \
    curl -sSfL -o zig.tar.xz \
        "https://ziglang.org/download/$ZIG_VERSION/zig-$zig_arch-linux-$ZIG_VERSION.tar.xz"; \
    echo "$zig_sha  zig.tar.xz" | sha256sum --check --strict; \
    mkdir /opt/zig; \
    tar -xJf zig.tar.xz -C /opt/zig --strip-components=1; \
    ln -s /opt/zig/zig /usr/local/bin/zig; \
    curl -sSfL -o zigbuild.tar.xz \
        "https://github.com/rust-cross/cargo-zigbuild/releases/download/v$ZIGBUILD_VERSION/cargo-zigbuild-$zigbuild_target.tar.xz"; \
    echo "$zigbuild_sha  zigbuild.tar.xz" | sha256sum --check --strict; \
    tar -xJf zigbuild.tar.xz -C /usr/local/bin --strip-components=1 \
        "cargo-zigbuild-$zigbuild_target/cargo-zigbuild"; \
    rm zig.tar.xz zigbuild.tar.xz

# The Rust target per target platform.
RUN set -eu; \
    case "$TARGETARCH$TARGETVARIANT" in \
        amd64)         triple=x86_64-unknown-linux-musl ;; \
        arm64|arm64v8) triple=aarch64-unknown-linux-musl ;; \
        armv7)         triple=armv7-unknown-linux-musleabihf ;; \
        386)           triple=i686-unknown-linux-musl ;; \
        ppc64le)       triple=powerpc64le-unknown-linux-musl ;; \
        riscv64)       triple=riscv64gc-unknown-linux-musl ;; \
        *) echo "unsupported platform $TARGETARCH$TARGETVARIANT" >&2; exit 1 ;; \
    esac; \
    rustup target add "$triple"; \
    echo "$triple" > /rust-target

# The commit shown by `tolk --version` and in the startup log.
ARG GIT_SHA=""
ENV GIT_SHA=${GIT_SHA}

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,id=target-$TARGETARCH$TARGETVARIANT \
    set -eu; \
    target="$(cat /rust-target)"; \
    cargo zigbuild --release --locked --target "$target"; \
    cp "target/$target/release/tolk" /tolk

# The root filesystem of the image: the binary, the trust store for the JMAP
# server's certificate, and a writable /tmp (SQLite's temporary files).
RUN set -eu; \
    mkdir -p /rootfs/usr/local/bin /rootfs/etc/ssl/certs /rootfs/tmp; \
    cp /tolk /rootfs/usr/local/bin/tolk; \
    cp /etc/ssl/certs/ca-certificates.crt /rootfs/etc/ssl/certs/; \
    chmod 1777 /rootfs/tmp

# Just the binary, for `--target binary --output type=local,dest=...`.
FROM scratch AS binary
COPY --from=build /tolk /tolk

FROM scratch
COPY --from=build /rootfs /
LABEL org.opencontainers.image.title="Tolk" \
      org.opencontainers.image.description="Exchange ActiveSync for JMAP mail servers" \
      org.opencontainers.image.licenses="AGPL-3.0-only" \
      org.opencontainers.image.source="https://github.com/RBogie/tolk-jmap"
USER 1000:1000
ENV LISTEN_ADDR=0.0.0.0:8080
EXPOSE 8080
# Checked every 2 s while starting: Traefik only routes a healthy container.
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --start-interval=2s CMD ["tolk", "healthcheck"]
CMD ["tolk"]
