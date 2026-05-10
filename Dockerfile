# syntax=docker/dockerfile:1.7
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive \
    CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

# apt cache mounts: keep /var/cache/apt and /var/lib/apt across builds.
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    rm -f /etc/apt/apt.conf.d/docker-clean \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        curl \
        git \
        pkg-config \
        libssl-dev \
        openssl \
        just

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --default-toolchain stable --profile minimal

WORKDIR /work
COPY . .

# Cache cargo registry/git and the workspace target dir across builds.
# The target/ cache is shared between `just install` (cargo install trunk)
# and `just build`. After the build we copy the release binary out of the
# cache mount so it survives in the final image layer.

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/work/target,id=junos-target \
    --mount=type=cache,target=/work/junos-web/target,id=junos-web-target \
    just install 
    

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/work/target,id=junos-target \
    --mount=type=cache,target=/work/junos-web/target,id=junos-web-target \
    just build \
    && mkdir -p /out \
    && cp target/release/junos-server /out/junos-server

RUN just gen-cert

# Move the cached binary into a stable path inside the image.
RUN mkdir -p target/release && mv /out/junos-server target/release/junos-server

EXPOSE 8080 8443
CMD ["./target/release/junos-server"]
