# rekos-web

A local, browser-based KStars/Ekos web client.
It runs **alongside KStars on your LAN** and acts as a transparent relay
between KStars and your browser — nothing is sent to the cloud.

## How it works

```
   KStars (Ekos Live "offline server")
         │  ws://localhost:8080/message/ekos
         │  ws://localhost:8080/media/ekos
         ▼
   ┌─────────────────────┐
   │     rekos-server    │   Axum + Tokio relay
   │  (Rust, native)     │   serves the WASM frontend at /
   └─────────────────────┘
         ▲
         │  ws://localhost:8080/ws
         │
   Browser ── rekos-wasm (Leptos + WebGPU planetarium)
```

- **`rekos-server`** — a tiny Axum relay. KStars connects *into* it as if it
  were `ekoslive.com`. Browsers connect to `/ws` and exchange raw Ekos Live
  JSON messages with KStars. The server also serves the compiled WASM
  frontend from `rekos-wasm/dist/`.
- **`rekos-wasm`** — the browser app. Leptos 0.7 CSR with a WebGPU sky view.
  Speaks the Ekos Live wire format directly (`{type, payload}` JSON).

The server does **no protocol translation** — messages flow through opaque.
All Ekos Live semantics live in the WASM client.

## Install

One-time setup (requires `rustup` and `cargo` already on PATH):

```bash
just install
```

This adds the `wasm32-unknown-unknown` Rust target and installs `trunk`.

## Build & run

```bash
just              # release build (wasm + server) then run
just build        # release build only, no run
just check        # fast typecheck of both crates
just dev-wasm     # `trunk watch` for the frontend
just dev-server   # `cargo run -p rekos-server`
just clean        # cargo clean + rm rekos-wasm/dist
```

By default the server listens on `http://localhost:8080`.

## Using it with KStars

1. Start KStars and open **Ekos**.
2. In Ekos Live settings, point the **offline server** at
   `http://localhost:8080`.
3. Start your equipment profile (simulators are fine for testing).
4. Open `http://localhost:8080` in your browser.
5. The top status strip should flip to **Ekos online** and the
   mount-anchored FOV reticle should appear on the sky view.
