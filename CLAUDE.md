# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`rekos-web` is a Rust workspace that re-implements the deprecated KStars/Ekos web client. It is **not** an Ekos Live cloud client — it runs alongside KStars on the LAN as a transparent relay between KStars and a browser. The browser sees KStars exactly as if it had connected to ekoslive.com, but the traffic stays local.

Two crates:

- **`rekos-server`** (Axum/Tokio) — local relay. KStars connects *inbound* to it; browsers connect to it; it broadcasts KStars events to all browsers and forwards browser commands to the attached KStars session. Also serves the WASM frontend from `rekos-wasm/dist/`.
- **`rekos-wasm`** (Leptos 0.7 CSR + WebGPU) — browser app. Connects to the server's `/ws` and exchanges raw Ekos Live JSON `{type, payload}` messages with KStars.

`kstars/` is the upstream KStars C++ source kept as a **read-only reference** for the Ekos Live wire format — never edit it, but grep it heavily when you need to know what KStars actually sends/accepts.

## Repo layout

- `rekos-server/`, `rekos-wasm/` — the two workspace crates (see Architecture below).
- `junos-web/` — sibling Trunk/Leptos crate with its **own** `Cargo.toml` (not in the workspace). Build with `cd junos-web && trunk build --release`. Has its own `CLAUDE.md` — read that before touching it.
- `kstars/` — read-only upstream KStars C++ source, kept as the authoritative reference for the Ekos Live wire format. Never edit; grep heavily.
- `scripts/` — Python tools that regenerate the binary catalogs in `rekos-wasm/public/` (`gen_catalog.py`, `gen_dso_catalog.py`, `gen_dso_sprites.py`, `download_nebulae.py`, `platesolve_nebulae.py`, …). Per user preference, run with `uv run script.py` — never bare `python3`. The generated outputs are checked in; don't regenerate them as part of unrelated code changes.
- `flake.nix` / `nix/` — Nix dev shell. Already provided; don't propose adding one.

## Frontend tabs

`rekos-wasm` is no longer planetarium-only. `components/tabs.rs` defines a `Tab` enum and `components/tab_wheel.rs` renders the tab switcher. Current tabs: `Sky` (planetarium, fullscreen behind the wheel), `Mount`, `Focus`, `Guide`, `Imaging`, `Mosaic`, `PolarAlign`, `Scheduler`. Each tab module lives directly under `components/`. The planetarium remains the most fully-featured surface; the other tabs are progressively being reintroduced — when adding to them, grow `DeviceStore` (`ws.rs`) and the `apply_ekos_event` match arm only as needed.

## Build & run

```bash
# One-time
rustup target add wasm32-unknown-unknown
cargo install trunk

# Preferred — uses the repo's justfile
just               # release build (wasm + server) then run
just build         # release build only
just check         # fast typecheck both crates (no codegen)
just dev-wasm      # `trunk watch` in rekos-wasm/
just dev-server    # `cargo run -p rekos-server`
just clean         # cargo clean + rm rekos-wasm/dist

# Manual equivalents
cd rekos-wasm && trunk build --release
cargo build --release -p rekos-server
./target/release/rekos-server
cargo check -p rekos-wasm --target wasm32-unknown-unknown
cargo check -p rekos-server
```

Workspace root `Cargo.toml` sets `default-members = ["rekos-server"]`, so `cargo build`/`cargo run` from the root operate on the server only. `rekos-wasm` is only buildable through Trunk (or `cargo check --target wasm32-unknown-unknown -p rekos-wasm`). `junos-web` is **not** in the workspace and has its own build pipeline.

### Server ports

`rekos-server` binds **two** ports by default:

- **HTTP on `:8080`** — KStars-facing. KStars' Ekos Live client connects here.
- **HTTPS on `:8443`** — browser-facing. iOS Safari requires TLS to expose WebGPU, so the browser must hit `https://<host>:8443`. A self-signed cert is auto-generated into `.certs/` on first run.

Pass `--no-https` to skip TLS for headless/CI runs. `config.rs` (clap, env-aware) parses `--bind-addr`, `--dist-dir`, and the TLS flags.

There are no unit tests — verification is manual: run KStars, enable Ekos Live, point it at `http://localhost:8080`, start an equipment profile (simulators are fine), open the browser to `https://localhost:8443` (accept the self-signed cert), click Start in Ekos, check the top status strip flips to `Ekos online` and the mount-anchored FOV reticle appears on the sky.

## Architecture

### Server (`rekos-server`)

`hub.rs` is the central state. A `tokio::sync::broadcast` channel fans KStars events out to every connected browser, plus an `Option<mpsc::Sender>` that points at the currently-attached KStars session (only one KStars can be attached at a time).

- `kstars_ws.rs` handles `GET /message/ekos` and `GET /media/ekos` (KStars connects to these as an Ekos Live "offline server"). On connect it sends KStars the `set_client_state` handshake (required — KStars drops every outbound event until it receives that) and publishes a synthetic `new_connection_state {connected:true}` to the hub. Inbound text is broadcast to browsers verbatim; binary media frames are decoded from the 512-byte metadata header plus JPEG/FITS payload and re-emitted as `new_preview_image`.
- `proxy.rs` handles `GET /ws` (browser side). On connect it tells the new browser the current KStars-attached state, then loops: KStars events → browser, browser commands → KStars via the hub.
- `auth.rs` is a stub for `POST /api/authenticate` (no real auth — local relay only).
- `config.rs` parses `--bind-addr`, `--dist-dir`, and the HTTPS/TLS flags (see "Server ports" above).

There is **no protocol translation** in the server. Messages flow through opaque. All Ekos Live semantics live in the WASM client.

### Frontend (`rekos-wasm`)

Leptos 0.7 CSR. Entry point `main.rs` → `App()` → tab wheel + active tab. Module layout:

- `ws.rs` — the WebSocket spine. Owns `DeviceStore`, `apply_ekos_event()`, `use_rekos_ws()`, and the cross-referencing Effects that derive `telescope_settings` from `scopes ∩ trains`. Also fires per-device retry loops via `spawn_retry_property()` for `CCD_INFO` and `EQUATORIAL_EOD_COORD`.
- `compat.rs` — flat snapshot types (`MountSnapshot`, `CameraSnapshot`, `SiteSnapshot`, `SolveSnapshot`) derived from `DeviceStore`. The sky module imports these, not `DeviceStore`.
- `main.rs` — wires catalogs, site location, language, the Leptos contexts required by `sky/actions.rs` (`MountDeviceCtx`, `CameraDeviceCtx`, `AlignDefaultsCtx`, `AlignSolveRadiusCtx`, `ServiceBusyCtx`, `MosaicPlannerCtx`), and the tab shell. The top status strip (position `fixed`, `pointer-events:none`) shows WS state + mount RA/Dec + active FOV in arcmin.
- `components/tabs.rs`, `components/tab_wheel.rs` — tab enum and wheel switcher.
- `components/sky/` — planetarium. Dual-canvas renderer (WebGPU bottom + Canvas2D overlay, fallback to all-Canvas2D). See below.
- `components/{mount,focus,imaging,polar_align,scheduler,mosaic_tab}.rs`, `components/guide/` — other tabs. Each takes only the signals it needs plus `SendCmd`; never `DeviceStore` whole.
- `astro.rs` / `coords.rs` / `ephemeris.rs` — equatorial↔horizontal math (Julian date, GMST/LST, precession to/from J2000, `fov_deg(focal, sensor_px, pixel_um)`), and ephemerides for solar-system bodies. Correct — reuse, do not reimplement.
- `catalog.rs`, `dso_catalog.rs`, `nebulae.rs` — async fetchers for the binary blobs in `public/`.
- `gpu.rs` + `shaders/` — WebGPU compute pipeline.
- `i18n.rs` — string table (EN/FR). Has many unused strings; don't gratuitously prune.

`SendCmd = Arc<dyn Fn(String) + Send + Sync>` — type-erased command sink. Components dispatch raw JSON strings via `send(serde_json::json!({"type":"…","payload":{…}}).to_string())`. Do not introduce a typed command enum.

### Planetarium (`components/sky/`)

The most fully-featured surface. Treat as stable — make targeted edits when adding overlays or interactions; don't rewrite. Structure:

- `mod.rs` — `SkyTab` component, canvas/GPU setup, event loop, localStorage persistence (`sky_center_alt`, `sky_center_az`, `sky_fov_radius`, `sky_follow_mount`, `sky_focal_override`).
- `render.rs` — Canvas2D overlay: grid, horizon, constellations (falls back from GPU), DSO labels, nebulae thumbnails, `render_center_fov()` and `render_mount_fov()` — the two FOV rectangles. Both call `astro::fov_deg` with `RenderParams.{fl, cam_pixel_size_um, cam_sensor_width, cam_sensor_height, rotation_deg, mount_ra_h, mount_dec_deg}`.
- `controls.rs` — right-panel render toggles + focal length override input.
- `search.rs` — catalog object search.
- `actions.rs` — right-click context menu + confirm popup that dispatches `mount_goto_rade` and `align_solve`. Imports `MountDeviceCtx`, `CameraDeviceCtx`, `AlignDefaultsCtx`, `AlignSolveRadiusCtx`, `ServiceBusyCtx` from the crate root — these newtypes live in `main.rs` and must be provided. `MosaicPlannerCtx` (also in `main.rs`) drives the Pick-on-Sky flow that hands a center off to the Mosaic tab.

## Ekos Live wire format

JSON `{"type": "...", "payload": {...}}` over WebSocket. Authoritative references:

- **`kstars/kstars/ekos/ekoslive/commands.h`** — the enum of ~200 message types. Name list.
- **`kstars/kstars/ekos/ekoslive/message.cpp`** — handlers. Read this first when extending the client. Especially `processTextMessage()` (top of the big command switch), `processDeviceCommands()` (line 1652), `updateMountCoords()` in `manager.cpp:3173`.
- **`kstars/kstars/indi/indistd.cpp`** — `numberToJson`, `switchToJson`, `textToJson`. This is how `device_property_get` / `device_property_set` payloads are serialized.

### Critical pitfalls — *read these before adding features*

1. **Two gates, not one.** `new_connection_state` carries `{connected, online}`. `rekos-server`'s synthetic event only sets `connected`. KStars' real event after profile start sets `online: true`. Many endpoints (`get_devices`, `get_states`, `get_scopes` in most call sites, `process*Commands`) gate on `getEkosStartingStatus() == Success` and are silently dropped before that — see `message.cpp:264, 291`. `ws.rs` prime requests fire on `online=true`, not `connected=true`, for this reason.

2. **`m_ClientState`.** KStars' `Node::sendResponse` (`node.cpp:156`) drops every outbound event if the remote peer hasn't sent `{"type":"set_client_state","payload":{"state":true}}`. `rekos-server/src/kstars_ws.rs` sends this on connect; don't remove it.

3. **`processDeviceCommands` silent drop.** `message.cpp:1664` — `if (!INDIListener::findDevice(device, …)) return;`. If the INDI driver for a device isn't registered yet when you send `device_property_get` / `device_property_set` / `device_property_subscribe`, the command is dropped with no reply and the subscription is not recorded. This is why `ws.rs::spawn_retry_property` exists: it keeps firing subscribe+get for up to 60 s until the expected data actually lands in the store.

4. **Mount coordinates come from a timer, not a signal.** `kstars/indi/indimount.cpp:244` — `updateCoordinatesTimer.start()` is only called after the first `processNumber(EQUATORIAL_EOD_COORD)` arrives from INDI. For idle drivers that don't push until something changes (e.g. Telescope Simulator at rest), `new_mount_state` carries `{status, target, …}` but **no RA/Dec** until the user triggers movement. The retry fetches `EQUATORIAL_EOD_COORD` directly via `device_property_get` to short-circuit this.

5. **`new_mount_state` is sent from multiple places.** Coord updates (full payload `{ra, de, ra0, de0, az, at, ha, …}`, throttled to 1 s in `message.cpp:2552`) vs status-only updates (`{status}`, `{target}`, `{pierSide}`, …). Match arms must tolerate partial payloads.

### Where FOV inputs actually live

- **Focal length + aperture** → `get_scopes` (OAL scope DB, keyed by `name`). Cross-reference the active train's `scope` field against this list. *Not* in `train_settings_get`.
- **Pixel size + sensor WxH** → INDI `CCD_INFO` number property on the camera device. Fetch via `device_property_get {device, property:"CCD_INFO"}`. Element names: `CCD_MAX_X`, `CCD_MAX_Y`, `CCD_PIXEL_SIZE_X/_Y` (fallback `CCD_PIXEL_SIZE`).
- **`train_settings_get` is a red herring** — it returns `OpticalTrainSettings` (a map keyed by module-enum IDs like `"0"`, `"1"`, storing per-module configs), not hardware specs. Don't add arms that parse `focalLength`/`pixelSize` from it; those fields will never exist.
- **Active train** → `train_get_all`, take `trains[0]`. Fields: `{id, name, mount, camera, scope, guider}`.

### Adding a new device control

1. Find the message in `commands.h` and read its handler in `message.cpp` to learn the payload schema. If it's an INDI property, read `indistd.cpp::{numberToJson, switchToJson, textToJson}` for the exact wire shape (compact vs non-compact).
2. If KStars *sends* it, add a match arm in `ws.rs::apply_ekos_event` and add fields to the relevant `*StatusData` struct.
3. If the browser *sends* it, dispatch via `send(serde_json::json!({…}).to_string())`. For INDI properties that may arrive before the driver is registered, use the `spawn_retry_property` pattern.
4. To expose new state to the planetarium, plumb it through `compat.rs` (the `*Snapshot` types consumed by `SkyTab`).

## Static assets

`rekos-wasm/public/` contains binary catalogs — `junos.bin` (star catalog), `dso.bin` (deep-sky catalog), `nebulae.json` + `nebulae/` (thumbnails). Trunk copies these into `dist/`. They are checked in — do not regenerate or re-encode them as part of code changes. The Python regen tools live in `scripts/` (run with `uv run`).

## Code style observed in this codebase

- French and English comments coexist; mirror the surrounding file.
- Commands are dispatched as raw JSON strings; do not introduce a typed command enum.
- Arc-clone `SendCmd` aggressively before moving it into closures.
- IDE-reported `unused_variables`/`dead_code` warnings are long-standing background — don't gratuitously fix them while doing unrelated work. The `i18n` string table in particular has many unused entries that stay intentionally.
- Tab components should take only the specific signals they need plus `SendCmd`, never `DeviceStore` whole. Use Leptos context only for values that need to cross many components (see `*Ctx` newtypes in `main.rs`).
