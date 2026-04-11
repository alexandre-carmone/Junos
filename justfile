set shell := ["bash", "-cu"]

default: run

# Install all toolchain bits needed to build and run rekos.
# Assumes `rustup` and `cargo` are already on PATH.
install:
    rustup target add wasm32-unknown-unknown
    cargo install --locked trunk

# Rebuild WASM frontend (release) and server, then run the server.
run: build
    ./target/release/rekos-server

# Full release build: trunk (wasm) + cargo (server).
build: build-wasm build-server

build-wasm:
    cd rekos-wasm && trunk build --release

build-server:
    cargo build --release -p rekos-server

# Fast typecheck (no codegen) for both crates.
check:
    cargo check -p rekos-wasm --target wasm32-unknown-unknown
    cargo check -p rekos-server

# Dev loop: trunk watch in one terminal, `just dev-server` in another.
dev-wasm:
    cd rekos-wasm && trunk watch

dev-server:
    cargo run -p rekos-server

clean:
    cargo clean
    rm -rf rekos-wasm/dist
