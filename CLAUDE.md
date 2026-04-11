# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`rekos-web` is a Rust workspace that re-implements the deprecated KStars/Ekos web client. It is **not** an Ekos Live cloud client — it runs alongside KStars on the LAN as a transparent relay between KStars and a browser. The browser sees KStars exactly as if it had connected to ekoslive.com, but the traffic stays local.

Two crates:

- **`rekos-server`** (Axum/Tokio) — local relay. KStars connects *inbound* to it; browsers connect to it; it broadcasts KStars events to all browsers and forwards browser commands to the attached KStars session. It also serves the WASM frontend from `rekos-wasm/dist/`.
- **`rekos-wasm`** (Leptos 0.7 CSR + WebGPU) — browser app. Connects to the server's `/ws` and exchanges raw Ekos Live JSON `{type, payload}` messages with KStars.

`junos-web/` is legacy and unused. `kstars/` is the upstream KStars C++ source kept as a read-only reference for the Ekos Live wire format — never edit it, but grep it heavily when you need to know what KStars actually sends/accepts.

## Build & run

```bash
# One-time
rustup target add wasm32-unknown-unknown
cargo install trunk

# Build (from workspace root)
cd rekos-wasm && trunk build --release && cd ..   # outputs rekos-wasm/dist/
cargo build --release -p rekos-server

# Run (from workspace root — default DIST_DIR is rekos-wasm/dist)
./target/release/rekos-server
# Defaults: BIND_ADDR=0.0.0.0:8080, DIST_DIR=rekos-wasm/dist
# Override: --bind-addr / --dist-dir, or env vars

# Dev loop (two terminals)
cd rekos-wasm && trunk watch                       # rebuilds dist/ on change
cargo run -p rekos-server                          # serves the same dist/

# Type-check the WASM crate without building (much faster than trunk)
cargo check -p rekos-wasm --target wasm32-unknown-unknown

# Type-check the server
cargo check -p rekos-server
```

The workspace root `Cargo.toml` sets `default-members = ["rekos-server"]` so `cargo build`/`cargo run` from the root operate on the server only — `rekos-wasm` is only buildable through Trunk (or with `--target wasm32-unknown-unknown -p rekos-wasm`).

There are no tests — verification is manual: run KStars, enable Ekos Live, point it at `http://localhost:8080`, start an equipment profile (simulators are fine), open the browser to the same URL.

## Architecture

### Server (`rekos-server`)

`hub.rs` is the central state. It holds a `tokio::sync::broadcast` channel that fans KStars events out to every connected browser, plus an `Option<mpsc::Sender>` that points at the currently-attached KStars session (only one KStars can be attached at a time).

- `kstars_ws.rs` handles `GET /message/ekos` and `GET /media/ekos` (KStars connects to these as an Ekos Live "offline server"). On connect it sends KStars the `set_client_state` handshake and publishes a synthetic `new_connection_state` to the hub. Inbound text messages are broadcast to browsers verbatim.
- `proxy.rs` handles `GET /ws` (browser side). On connect it tells the new browser the current KStars-attached state, then loops: KStars events → browser, browser commands → KStars via the hub.
- `auth.rs` is a stub for `POST /api/authenticate` (no real auth — local relay only).
- `config.rs` parses `--bind-addr` / `--dist-dir` (clap, env-aware).

There is **no protocol translation** in the server. Messages flow through opaque. All Ekos Live semantics live in the WASM client.

### Frontend (`rekos-wasm`)

Leptos 0.7 CSR. Single binary, all reactive state lives in `ws.rs::DeviceStore`.

**`ws.rs` is the spine.** It owns:
- `DeviceStore` — a struct of `RwSignal`s, one per Ekos subsystem (`mount_status`, `camera_status`, `guider_status`, `focus_status`, `align_status`, `optical_trains`, `ekos_profiles`, `device_conn`, `drivers_catalog`, `devices_catalog`, …).
- `apply_ekos_event(type_str, payload)` — the giant `match` that translates incoming KStars JSON into store updates. Adding support for a new Ekos Live message means adding an arm here.
- `SendCmd = Arc<dyn Fn(String)>` — the type-erased command sink. Components dispatch raw JSON strings; do not invent strongly-typed wrappers, just use `serde_json::json!({"type": "...", "payload": {...}})`.
- `use_rekos_ws()` — opens the WebSocket to `/ws`, spawns the read/write tasks, returns `(DeviceStore, SendCmd)`.

**`main.rs` wires everything.** It builds the store, derives compatibility snapshots via `compat.rs`, sets up Leptos contexts (`MountDeviceCtx`, `CameraDeviceCtx`, `DeviceConnCtx`, `ServiceBusyCtx`, `AlignDefaultsCtx`, `AlignSolveRadiusCtx`), and hands signals to each tab component. Tabs do **not** receive `DeviceStore` whole — they receive only the signals they need (e.g. `MountTab` gets `mount_status: RwSignal<...>`).

**Tabs live in `components/`** (mount, camera, focus, internal_guide, align, polar_align, scheduler, dust_cap, filter_wheel, flat_calibrator, devices, profiles, sky, log, indiserver). Each is a self-contained `#[component]` function. The `connect_button` component is reusable across device tabs and reads `DeviceConnCtx` from context to render Connect/Disconnect against the INDI `CONNECTION` switch property.

**The planetarium (`components/sky/`)** is a dual-canvas renderer:
- Bottom canvas: WebGPU compute (`gpu.rs` + `shaders/`) — projects `catalog.rs` star data and constellation lines on the GPU. The packed star buffer is built once at startup from `public/junos.bin`.
- Top canvas: Canvas2D overlay (`render.rs`) — grid, horizon, DSO labels (`dso_catalog.rs` + `public/dso.bin`), nebulae thumbnails (`nebulae.rs` + `public/nebulae/`), FOV reticle, mount-position crosshair.
- Falls back to all-Canvas2D when WebGPU is unavailable.
- `astro.rs` and `coords.rs` hold the equatorial↔horizontal coordinate math (Julian date, GMST/LST, precession to/from J2000). The math is correct — don't reimplement it, reuse it.

### Ekos Live wire format

The protocol is JSON `{"type": "...", "payload": {...}}` over WebSocket. The full command vocabulary is enumerated in `kstars/kstars/ekos/ekoslive/commands.h` (~200 message types). Server-side handling lives in `kstars/kstars/ekos/ekoslive/message.cpp` — when extending the client, **read that file first** to know the exact field names KStars sends and accepts.

Critical pitfalls observed in this codebase:

- KStars only answers `get_devices`, `get_states`, `train_settings_get`, `get_scopes`, etc. **after** a profile has been started (`getEkosStartingStatus() == Success`, see `message.cpp:264`). Requests sent before that are silently dropped. The Devices tab refetches on `connection.connected` flips and exposes a manual ⟳ Refresh.
- `get_profiles` returns `{selectedProfile, profiles}`, **not** a bare array. Field names follow `kstars/auxiliary/profileinfo.cpp::toJson()`.
- Profile start uses `profile_start` with `payload.name` (not `id`).
- Ekos Live has **no** high-level "connect device" command. Connection is toggled by writing the INDI `CONNECTION` switch property via `device_property_set` with `elements: [{name: "CONNECT" | "DISCONNECT", value: "On"}]`. The `ConnectButton` component implements exactly this.
- `new_connection_state` is sent both by the rekos-server (synthetic, on KStars attach/detach) and by KStars itself (real, when Ekos starts). Treat `connected=true` as "we can talk to KStars" and gate further work on it.

### Adding a new device control

1. Find the message in `kstars/ekos/ekoslive/commands.h` and read its handler in `kstars/ekos/ekoslive/message.cpp` to learn the payload schema.
2. If KStars *sends* it, add a match arm in `ws.rs::apply_ekos_event` and add fields to the relevant `*StatusData` struct in `ws.rs`.
3. If the browser *sends* it, add a button or input to the relevant tab component and dispatch via `send(serde_json::json!({...}).to_string())`.
4. To expose new state to the planetarium, plumb it through `compat.rs` (the `*Snapshot` types are flat, derived projections of `DeviceStore` consumed by `SkyTab`).

### Static assets

`rekos-wasm/public/` contains binary catalogs (`junos.bin` star catalog, `dso.bin` deep-sky catalog, `nebulae.json` + `nebulae/` thumbnails). Trunk copies these into `dist/`. They are checked in — do not regenerate or re-encode them as part of code changes.

## Code style observed in this codebase

- French comments and English comments coexist; mirror the surrounding file.
- Tab components do not take `DeviceStore` as a prop — they take only the specific signals they need, plus `SendCmd`. Use Leptos context only when the value needs to cross many tabs (see `*Ctx` newtypes in `main.rs`).
- Commands are dispatched as raw JSON strings; do not introduce a typed command enum.
- Arc-clone `SendCmd` aggressively before moving into closures (camera.rs uses `send2`…`send13` — that's the existing pattern).
- IDE-reported `unused_variables`/`dead_code` warnings are a long-standing background; don't gratuitously fix them while doing unrelated work.
