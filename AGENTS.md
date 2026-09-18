# AGENTS.md

## Scope

- `junos-web` is a LAN-only KStars/Ekos Live relay plus browser UI, not an Ekos Live cloud client.
- Workspace crates are `junos-server` and `junos-web`; root `Cargo.toml` has `default-members = ["junos-server"]`, so plain `cargo build`/`cargo run` only targets the server.
- `kstars/` is read-only upstream C++ reference for the Ekos Live wire format; grep it, never edit it.
- Licensed GPL-3.0-or-later (`LICENSE`, and the `license` field in both crate manifests); bundled Stellarium-derived assets are GPL-2.0-or-later.

## Commands

- One-time local setup: `just install` adds `wasm32-unknown-unknown`, installs `trunk`, and downloads `junos-web/bin/tailwindcss` used by Trunk.
- Full release build: `just build` runs `cd junos-web && trunk build --release`, then `cargo build --release -p junos-server`.
- Build and run: `just` or `just run`; the server serves `junos-web/dist` by default.
- Fast verification: `just check` runs `cargo check -p junos-web --target wasm32-unknown-unknown` followed by `cargo check -p junos-server`.
- Dev loop: run `just dev-wasm` and `just dev-server` in separate terminals.
- There are no repo tests currently; use `just check` plus manual KStars/browser verification for behavior changes.
- Nix shell exists (`nix develop`); do not propose adding another dev shell.

## Runtime Defaults

- `junos-server` binds HTTP `0.0.0.0:8080` for KStars and HTTPS `0.0.0.0:8443` for browsers.
- iOS Safari needs `https://<host>:8443` for WebGPU; `--no-https` is only for headless/CI-style runs.
- TLS certs are auto-generated into `.certs/`, or overridden with `--tls-cert`/`--tls-key` and env vars.
- `--dist-dir`/`DIST_DIR` defaults to `junos-web/dist`.
- `--captures-dir`/`CAPTURES_DIR` backs the Files tab and sandboxes `/api/files/*`; fallback is `$HOME/Pictures` (only if it already exists), then cwd.
- `--dso-tile-dir`/`DSO_TILE_DIR` defaults to `.cache/dso_tiles` and backs `/api/dso_tiles/*`; a missing dir is not an error (empty index).
- Packaged installs differ: `packaging/arch/junos-web.service` runs KStars-facing HTTP on **8090**, not 8080.

## Architecture

- The server does no protocol translation: KStars JSON and media frames flow through mostly opaque.
- Server entrypoint is `junos-server/src/main.rs`; central relay state is `hub.rs`.
- KStars connects inbound on `/message/ekos` and `/media/ekos`; browsers connect on `/ws`.
- `kstars_ws.rs` must send `{"type":"set_client_state","payload":{"state":true}}` on KStars connect; KStars otherwise silently drops outbound events.
- WASM entrypoint is `junos-web/src/main.rs`; WebSocket state and event dispatch are under `junos-web/src/ws/`.
- `DeviceStore` lives in `ws/store.rs`; add inbound Ekos handling in `apply_ekos_event` and expose sky-facing derived data through `compat.rs`.
- Components dispatch raw JSON strings via `SendCmd = Arc<dyn Fn(String) + Send + Sync>`; do not introduce a typed command enum.
- The `Tab` enum is in `junos-web/src/main.rs`; render order is `TABS` in `components/tab_wheel.rs` (shared with the desktop `components/tab_bar.rs`). Twelve tabs: `Profiles`, `Sky`, `Mount`, `Focus`, `Imaging`, `Files`, `PolarAlign`, `Guide`, `Scheduler`, `Mosaic`, `FlatCal`, `Devices`.
- Tab routing (mount/dismount) is `components/tabs.rs`; `SkyTab` stays mounted and hidden on tab switch to preserve WebGPU/catalog state, while other tabs mount lazily.
- Tab components should receive only needed signals plus `SendCmd`, not the whole `DeviceStore`, except the central `TabContent` wiring layer.

## Ekos Wire-Format Pitfalls

- Read `kstars/kstars/ekos/ekoslive/commands.h`, `message.cpp`, and `kstars/kstars/indi/indistd.cpp` before adding or changing protocol messages.
- `new_connection_state.connected` only means KStars is attached; many KStars commands are ignored until `online: true`, so prime gated requests from `online`, not `connected`.
- `processDeviceCommands` silently drops INDI commands if the device is not registered yet; use the existing `spawn_retry_property` pattern for new device property reads/subscriptions.
- `new_mount_state` payloads are partial and come from multiple code paths; merge fields defensively instead of replacing whole mount state.
- Idle mounts may not emit RA/Dec until `EQUATORIAL_EOD_COORD` is fetched directly; keep the mount coordinate retry/refresh behavior.
- FOV data sources are split: focal length/aperture from `get_scopes`, active devices from first `train_get_all` train, sensor size/pixel size from camera `CCD_INFO`.
- `train_settings_get` is not a hardware-spec source; do not parse focal length or pixel size from it.

## Frontend And Assets

- Trunk pre-build calls `junos-web/bin/tailwindcss`; if missing, `just install` or `just setup-tailwind` is required before `trunk build`.
- Tailwind scans `junos-web/src/**/*.rs`; shared design tokens are in `junos-web/styles/tokens.css` and mapped in `tailwind.config.js`.
- `junos-web/index.html` links only `tokens.css`, `base.css`, generated `tailwind.css`, and `responsive.css`; keep Trunk copy directives for checked-in catalogs.
- Static catalogs in `junos-web/public/` are checked in; do not regenerate or re-encode them for unrelated changes.
- Of the `scripts/`, only `prefetch_dso_tiles.py`, `feather_nebulae.py` and `gen_sky_tiles.py` are PEP-723 self-contained — run those with `uv run`, never bare `python3`. The rest need their own deps, and several still target the removed `stars-web/` tree; treat the checked-in outputs as authoritative.
- `astro.rs`, `coords.rs`, and `ephemeris.rs` contain the shared sky math; reuse them instead of reimplementing coordinate/FOV logic.

## Leptos/Rust Conventions

- Leptos props and callbacks commonly need `Send + Sync`; prefer `Arc` for command callbacks and clone before moving into closures.
- For raw Ekos commands, build strings with `serde_json::json!(...).to_string()` unless a static literal already exists.
- Long-standing `unused_variables`/`dead_code` warnings exist, especially in `i18n`; do not clean them up opportunistically.
- French and English comments both exist; match nearby style.
- For frontend styling, prefer existing Tailwind utilities/tokens over new inline DOM styles; Canvas2D paint strings are not DOM CSS and can stay inline.

## Manual Verification

- Start KStars, point Ekos Live offline server to `http://localhost:8080` (8090 for a packaged install), start an equipment profile, then open `https://localhost:8443`.
- Expected smoke test: top status becomes `Ekos online`, browser `/ws` connects, and the sky view shows the mount-anchored FOV reticle when mount/camera data is available.
