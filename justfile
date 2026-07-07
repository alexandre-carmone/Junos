set shell := ["bash", "-cu"]

default: run

# Install all toolchain bits needed to build and run junos.
# Assumes `rustup` and `cargo` are already on PATH.
install: setup-tailwind
    rustup target add wasm32-unknown-unknown
    cargo install --locked trunk

# Rebuild WASM frontend (release) and server, then run the server.
run: build
    ./target/release/junos-server

# Full release build: trunk (wasm) + cargo (server).
build: build-wasm build-server

build-wasm: ensure-trunk
    cd junos-web && trunk build --release

build-server:
    cargo build --release -p junos-server

# Fast typecheck (no codegen) for both crates.
check:
    cargo check -p junos-web --target wasm32-unknown-unknown
    cargo check -p junos-server

# Dev loop: trunk watch in one terminal, `just dev-server` in another.
dev-wasm: ensure-trunk
    cd junos-web && trunk watch

# Make sure `trunk` is on PATH; if missing, add ~/.cargo/bin or install it.
ensure-trunk:
    if ! command -v trunk >/dev/null 2>&1; then \
        if [ -x "$HOME/.cargo/bin/trunk" ]; then \
            echo "trunk found in ~/.cargo/bin but not on PATH; add it to your shell rc:"; \
            echo '  export PATH="$HOME/.cargo/bin:$PATH"'; \
            exit 1; \
        else \
            echo "trunk not found; installing via cargo..."; \
            cargo install --locked trunk; \
        fi; \
    fi

# The server binds two ports by default: HTTP on 8080 (KStars-facing) and
# HTTPS on 8443 (browser-facing — required by iOS Safari for WebGPU). A
# self-signed cert is auto-generated into .certs/ on first run. Pass
# --no-https to skip TLS for headless/CI runs.
dev-server:
    cargo run -p junos-server

clean:
    cargo clean
    rm -rf junos-web/dist

# Generate a self-signed TLS cert into .certs/ covering localhost + the
# host's first non-loopback IPv4. Same shape the server would auto-create
# on first run; useful for pre-seeding (e.g. inside a Docker image).
gen-cert:
    mkdir -p .certs
    HOST_IP=$(hostname -I 2>/dev/null | awk '{print $1}'); \
    SAN="DNS:localhost,IP:127.0.0.1$([ -n "$HOST_IP" ] && echo ,IP:$HOST_IP)"; \
    openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
        -subj "/CN=junos-web" \
        -addext "subjectAltName=$SAN" \
        -keyout .certs/key.pem -out .certs/cert.pem
    @echo "Wrote .certs/cert.pem and .certs/key.pem"

# Build the Arch Linux package (x86_64 or aarch64) from the local git tree in an
# Arch container. Needs Docker with network access to Arch mirrors. Output lands
# in packaging/arch/*.pkg.tar.zst. Pass `arm64` to cross-build for Raspberry Pi
# (via qemu; slow — prefer `makepkg -si` natively on the Pi). Default: amd64.
arch-pkg arch="amd64":
    ./packaging/arch/build-local.sh {{arch}}

# Download the Tailwind v3 standalone binary (re-run to upgrade).
setup-tailwind:
    mkdir -p junos-web/bin
    curl -sLo junos-web/bin/tailwindcss \
      https://github.com/tailwindlabs/tailwindcss/releases/download/v3.4.17/tailwindcss-linux-x64
    chmod +x junos-web/bin/tailwindcss
