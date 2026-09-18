# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`junos-web` is a Rust workspace that re-implements the deprecated KStars/Ekos web client. It is **not** an Ekos Live cloud client — it runs alongside KStars on the LAN as a transparent relay between KStars and a browser. The browser sees KStars exactly as if it had connected to ekoslive.com, but the traffic stays local.

Two crates:

- **`junos-server`** (Axum/Tokio) — local relay. KStars connects *inbound* to it; browsers connect to it; it broadcasts KStars events to all browsers and forwards browser commands to the attached KStars session. Also serves the WASM frontend from `junos-web/dist/`.
- **`junos-web`** (Leptos 0.7 CSR + WebGPU) — browser app. Connects to the server's `/ws` and exchanges raw Ekos Live JSON `{type, payload}` messages with KStars.

`kstars/` is the upstream KStars C++ source kept as a **read-only reference** for the Ekos Live wire format — never edit it, but grep it heavily when you need to know what KStars actually sends/accepts.

## Repo layout

- `junos-server/`, `junos-web/` — the two workspace crates (see Architecture below).
- `kstars/` — read-only upstream KStars C++ source, kept as the authoritative reference for the Ekos Live wire format. Never edit; grep heavily.
- `scripts/` — two generators. `prefetch_dso_tiles.py` is PEP-723 self-contained (`uv run scripts/prefetch_dso_tiles.py`) and writes the offline tile cache; `gen_dso_catalog.py` is plain `python3` and writes `junos-web/public/dso.bin` from `openngc.csv`. There is **no generator for `junos.bin`** — the star catalog is checked in and authoritative. Don't regenerate outputs as part of unrelated code changes.
- `packaging/` — Arch Linux package (`packaging/arch/`), portable tarball (`packaging/portable/`), and CI notes. `.github/workflows/` has `ci.yml` (mirrors `just check`) and `release.yml` (tag-triggered two-arch build).
- `flake.nix` / `nix/` — Nix dev shell, packages, and the `services.junos-web` NixOS module. Already provided; don't propose adding one.

## Frontend tabs

`junos-web` is no longer planetarium-only. The `Tab` enum lives in `main.rs` (**not** `components/tabs.rs`, which only holds the `TabContent` router). Render order is `TABS` in `components/tab_wheel.rs`, used by both the mobile wheel and the desktop strip (`components/tab_bar.rs`):

`Profiles`, `Sky`, `Mount`, `Focus`, `Imaging`, `Files`, `PolarAlign`, `Guide`, `Scheduler`, `Mosaic`, `FlatCal`, `Devices` — 12 tabs. Displayed labels come from `i18n` (`tab_*` keys); `PolarAlign` renders as "Polar Align" and `FlatCal` as "Flat Cal".

Each tab module lives directly under `components/` (some as directories: `guide/`, `imaging/`, `files/`, `scheduler/`, `sky/`). `SkyTab` is kept mounted (`display:none` when inactive) so its WebGPU context and catalog state survive tab switches; the other tabs are lazy via `<Show>`. All 12 are substantial — none is a stub any more. When extending one, grow `DeviceStore` (`ws/store.rs`) and the `apply_ekos_event` match arm only as needed.

## Build & run

```bash
# One-time
rustup target add wasm32-unknown-unknown
cargo install trunk

# Preferred — uses the repo's justfile
just               # release build (wasm + server) then run
just build         # release build only
just check         # fast typecheck both crates (no codegen)
just dev-wasm      # `trunk watch` in junos-web/
just dev-server    # `cargo run -p junos-server`
just clean         # cargo clean + rm junos-web/dist

# Manual equivalents
cd junos-web && trunk build --release
cargo build --release -p junos-server
./target/release/junos-server
cargo check -p junos-web --target wasm32-unknown-unknown
cargo check -p junos-server
```

Workspace root `Cargo.toml` sets `default-members = ["junos-server"]`, so `cargo build`/`cargo run` from the root operate on the server only. `junos-web` is only buildable through Trunk (or `cargo check --target wasm32-unknown-unknown -p junos-web`).

### Server ports

`junos-server` binds **two** ports by default:

- **HTTP on `:8080`** — KStars-facing. KStars' Ekos Live client connects here.
- **HTTPS on `:8443`** — browser-facing. iOS Safari requires TLS to expose WebGPU, so the browser must hit `https://<host>:8443`. A self-signed cert is auto-generated into `.certs/` on first run.

Pass `--no-https` to skip TLS for headless/CI runs. `config.rs` (clap, env-aware) parses `--http-addr`, `--https-addr`, `--dist-dir`, and the TLS flags.

`just test` runs the server-side unit tests (`cargo test -p junos-server`). `junos-web` is wasm-only — wgpu's webgpu backend does not build for the host — so its tests need a wasm runner and are not in that recipe. Everything else is verified manually: run KStars, enable Ekos Live, point it at `http://localhost:8080`, start an equipment profile (simulators are fine), open the browser to `https://localhost:8443` (accept the self-signed cert), click Start in Ekos, check the top status strip flips to `Ekos online` and the mount-anchored FOV reticle appears on the sky.

## Architecture

### Server (`junos-server`)

`hub.rs` is the central state. A `tokio::sync::broadcast` channel fans KStars events out to every connected browser, plus an `Option<mpsc::Sender>` that points at the currently-attached KStars session (only one KStars can be attached at a time).

- `kstars_ws.rs` handles `GET /message/ekos` and `GET /media/ekos` (KStars connects to these as an Ekos Live "offline server"). On connect it sends KStars the `set_client_state` handshake (required — KStars drops every outbound event until it receives that) and publishes a synthetic `new_connection_state {connected:true}` to the hub. Inbound text is broadcast to browsers verbatim; binary media frames are decoded from the 512-byte metadata header plus JPEG/FITS payload and re-emitted as `new_preview_image`.
- `proxy.rs` handles `GET /ws` (browser side). On connect it tells the new browser the current KStars-attached state, then loops: KStars events → browser, browser commands → KStars via the hub.
- `auth.rs` is a stub for `POST /api/authenticate` (no real auth — local relay only).
- `config.rs` parses the clap `Config` (all long-only flags, each with an env var): `--http-addr`/`HTTP_ADDR`, `--https-addr`/`HTTPS_ADDR`, `--dist-dir`/`DIST_DIR`, `--captures-dir`/`CAPTURES_DIR`, `--dso-tile-dir`/`DSO_TILE_DIR`, `--tls-cert`, `--tls-key`, `--no-https`. Also `resolved_captures_dir()` / `resolved_dso_tile_dir()`.
- `tls.rs` — cert/key resolution and self-signed generation into `.certs/` (`CN=junos-dev`, SANs = localhost + 127.0.0.1 + every non-loopback IPv4). Supplying only one of `--tls-cert`/`--tls-key` is a fatal error.
- `files.rs` (+ `starfind.rs`) — the Files tab's backend: `/api/files/{list,meta,thumb,raw,download,rename,delete,resolve,tilt}`, FITS header parsing, thumbnailing, and a star detector used by the tilt/aberration analyzer. Sandboxed to the resolved captures dir by canonicalize checks.
- `apps.rs` — `/api/apps/{launch,stop,state}`: spawn and monitor KStars or PHD2 on the server host.
- `dso_tiles.rs` — serves the offline DSO tile cache at `/api/dso_tiles/*` (see "Offline DSO tiles" below).
- `main.rs` — also serves `GET /api/config`, which reports the resolved captures dir. The frontend reads it once into `CaptureDirCtx` so the sequencer forms default to a folder the Files tab can browse.
- `skysurvey.rs` — `/api/skysurvey`, a same-origin hips2fits proxy. Route still registered but no longer used by the framing path.

There is **no protocol translation** in the server. Messages flow through opaque. All Ekos Live semantics live in the WASM client.

### Frontend (`junos-web`)

Leptos 0.7 CSR. Entry point `main.rs` → `App()` → tab wheel + active tab. Module layout:

- `ws/` — the WebSocket spine (`mod.rs` owns `use_junos_ws()` and the cross-referencing Effects that derive `telescope_settings` from `scopes ∩ trains`; `store.rs` owns `DeviceStore` and `apply_ekos_event()`; `retry.rs` owns `spawn_retry_property()`, fired per device for `CCD_INFO` and `EQUATORIAL_EOD_COORD`; `types.rs` the payload structs). `ws_helpers.rs` sits alongside it.
- `compat.rs` — flat snapshot types (`MountSnapshot`, `CameraSnapshot`, `SiteSnapshot`, `SolveSnapshot`) derived from `DeviceStore`. The sky module imports these, not `DeviceStore`.
- `main.rs` — wires catalogs, site location, language, the Leptos contexts `sky/actions.rs` reads (`ServiceBusyCtx`, `SchedulerPrefillCtx`, `FramingCtx`, `MosaicPlannerCtx`), and the tab shell. Also defines `debug_log!`, a `leptos::logging::log!` that compiles out of release builds. The top status strip (position `fixed`, `pointer-events:none`) shows WS state + mount RA/Dec + active FOV in arcmin.
- `components/tabs.rs` — the `TabContent` router (mount/dismount policy). `components/tab_wheel.rs` (mobile wheel, owns `TABS`) and `components/tab_bar.rs` (desktop strip) are the two switchers; `components/tab_wheel_icons.rs` has the per-tab icons.
- `components/sky/` — planetarium. Dual-canvas renderer (WebGPU bottom + Canvas2D overlay, fallback to all-Canvas2D). See below.
- `components/{mount,focus,polar_align,mosaic_tab,flat_cal,devices,profiles}.rs` and `components/{guide,imaging,files,scheduler}/` — the other tabs. Each takes only the signals it needs plus `SendCmd`; never `DeviceStore` whole.
- `components/{branding,coord_input,dialog_modal,sequence_editor}.rs` — shared pieces, not tabs. `dialog_modal.rs` surfaces KStars' blocking `dialog_get_info` prompts.
- `dso_tiles.rs` — offline tile index fetcher (`/api/dso_tiles/index.json`) and `DsoTileIndex::find_overlapping`, consumed by the Framing Assistant.
- `astro.rs` / `coords.rs` / `ephemeris.rs` — equatorial↔horizontal math (Julian date, GMST/LST, precession to/from J2000, `fov_deg(focal, sensor_px, pixel_um)`), and ephemerides for solar-system bodies. Correct — reuse, do not reimplement.
- `catalog.rs`, `dso_catalog.rs` — async fetchers for `public/junos.bin` and `public/dso.bin`.
- `dom.rs` — `event_target_value` / `event_target_checked`, the only two DOM-event readers; every tab uses these rather than rolling its own.
- `i18n/` — module dir (`mod.rs` + `en.json` + `fr.json`, embedded via `include_str!`). EN default, FR selected via the `EN`/`FR` pill in the tab bar/wheel and persisted to localStorage `junos_lang`; there is no browser auto-detect. The `translations!` macro declares the schema — a key missing from one language **panics** on first use. Many unused strings; don't gratuitously prune.

`SendCmd = Arc<dyn Fn(String) + Send + Sync>` — type-erased command sink. Components dispatch raw JSON strings via `send(serde_json::json!({"type":"…","payload":{…}}).to_string())`. Do not introduce a typed command enum.

### Planetarium (`components/sky/`)

The most fully-featured surface. Treat as stable — make targeted edits when adding overlays or interactions; don't rewrite. Structure:

- `mod.rs` — `SkyTab` component, canvas/GPU setup, event loop, localStorage persistence (`sky_center_alt`, `sky_center_az`, `sky_fov_radius`, `sky_follow_mount`, `sky_dso_mag_limit`, plus one key per render toggle).
- `render/` — Canvas2D overlay (`mod.rs`, `layer.rs`, `params.rs`, `pipeline.rs` + one module per layer in `render/layers/`: stars, dso, grids, ground, zenith, constellation_names, center_crosshair, mount_crosshair, fov_reticle, solve_marker, slew_trail, solar_system, mosaic, scheduler_jobs, info_overlay). Draws grid, horizon, constellations (falls back from GPU), DSO labels, `render_center_fov()` and `render_mount_fov()` — the two FOV rectangles. Both call `astro::fov_deg` with `RenderParams.{fl, cam_pixel_size_um, cam_sensor_width, cam_sensor_height, rotation_deg, mount_ra_h, mount_dec_deg}`.
- `controls.rs` — right-panel render toggles + focal length override input.
- `search.rs` — catalog object search.
- `actions.rs` — right-click (or 500 ms long-press) context menu with four actions: `mount_goto_rade`, goto-then-`align_solve`, Add to Scheduler (`SchedulerPrefillCtx`), and Framing assistant (`FramingCtx`). Reads `ServiceBusyCtx` (to disable Goto / Goto & Align while a device is busy), `SchedulerPrefillCtx` and `FramingCtx` from the crate root — these newtypes live in `main.rs` and must be provided. `MosaicPlannerCtx` (also in `main.rs`) drives the Pick-on-Sky flow that hands a center off to the Mosaic tab.
- `framing.rs` — the Framing Assistant modal (opened only from `actions.rs`, not a tab). See "Offline DSO tiles" below.
- `hud.rs`, `picking.rs`, `info_popup.rs`, `object_search.rs`, `dso_index.rs`, `dso_render.rs`, `dso_shape.rs`, `solar_render.rs`, `utils.rs`, `gpu/`, `shaders/*.wgsl` — the remaining pieces.

## Ekos Live wire format

JSON `{"type": "...", "payload": {...}}` over WebSocket. Authoritative references:

- **`kstars/kstars/ekos/ekoslive/commands.h`** — the enum of ~200 message types. Name list.
- **`kstars/kstars/ekos/ekoslive/message.cpp`** — handlers. Read this first when extending the client. Especially `processTextMessage()` (top of the big command switch), `processDeviceCommands()` (line 1652), `updateMountCoords()` in `manager.cpp:3173`.
- **`kstars/kstars/indi/indistd.cpp`** — `numberToJson`, `switchToJson`, `textToJson`. This is how `device_property_get` / `device_property_set` payloads are serialized.

### Critical pitfalls — *read these before adding features*

1. **Two gates, not one.** `new_connection_state` carries `{connected, online}`. `junos-server`'s synthetic event only sets `connected`. KStars' real event after profile start sets `online: true`. Many endpoints (`get_devices`, `get_states`, `get_scopes` in most call sites, `process*Commands`) gate on `getEkosStartingStatus() == Success` and are silently dropped before that — see `message.cpp:264, 291`. `ws/` prime requests fire on `online=true`, not `connected=true`, for this reason.

2. **`m_ClientState`.** KStars' `Node::sendResponse` (`node.cpp:156`) drops every outbound event if the remote peer hasn't sent `{"type":"set_client_state","payload":{"state":true}}`. `junos-server/src/kstars_ws.rs` sends this on connect; don't remove it.

3. **`processDeviceCommands` silent drop.** `message.cpp:1664` — `if (!INDIListener::findDevice(device, …)) return;`. If the INDI driver for a device isn't registered yet when you send `device_property_get` / `device_property_set` / `device_property_subscribe`, the command is dropped with no reply and the subscription is not recorded. This is why `ws/retry.rs::spawn_retry_property` exists: it keeps firing subscribe+get for up to 60 s until the expected data actually lands in the store.

4. **Mount coordinates come from a timer, not a signal.** `kstars/indi/indimount.cpp:244` — `updateCoordinatesTimer.start()` is only called after the first `processNumber(EQUATORIAL_EOD_COORD)` arrives from INDI. For idle drivers that don't push until something changes (e.g. Telescope Simulator at rest), `new_mount_state` carries `{status, target, …}` but **no RA/Dec** until the user triggers movement. The retry fetches `EQUATORIAL_EOD_COORD` directly via `device_property_get` to short-circuit this.

5. **`new_mount_state` is sent from multiple places.** Coord updates (full payload `{ra, de, ra0, de0, az, at, ha, …}`, throttled to 1 s in `message.cpp:2552`) vs status-only updates (`{status}`, `{target}`, `{pierSide}`, …). Match arms must tolerate partial payloads.

### Where FOV inputs actually live

- **Focal length + aperture** → `get_scopes` (OAL scope DB, keyed by `name`). Cross-reference the active train's `scope` field against this list. *Not* in `train_settings_get`.
- **Pixel size + sensor WxH** → INDI `CCD_INFO` number property on the camera device. Fetch via `device_property_get {device, property:"CCD_INFO"}`. Element names: `CCD_MAX_X`, `CCD_MAX_Y`, `CCD_PIXEL_SIZE_X/_Y` (fallback `CCD_PIXEL_SIZE`).
- **`train_settings_get` is a red herring** — it returns `OpticalTrainSettings` (a map keyed by module-enum IDs like `"0"`, `"1"`, storing per-module configs), not hardware specs. Don't add arms that parse `focalLength`/`pixelSize` from it; those fields will never exist.
- **Active train** → `train_get_all`, take `trains[0]`. Fields: `{id, name, mount, camera, scope, guider}`.

### Adding a new device control

1. Find the message in `commands.h` and read its handler in `message.cpp` to learn the payload schema. If it's an INDI property, read `indistd.cpp::{numberToJson, switchToJson, textToJson}` for the exact wire shape (compact vs non-compact).
2. If KStars *sends* it, add a match arm in `ws/store.rs::apply_ekos_event` and add fields to the relevant `*StatusData` struct.
3. If the browser *sends* it, dispatch via `send(serde_json::json!({…}).to_string())`. For INDI properties that may arrive before the driver is registered, use the `spawn_retry_property` pattern.
4. To expose new state to the planetarium, plumb it through `compat.rs` (the `*Snapshot` types consumed by `SkyTab`).

## Styling

**Component styling is inline Tailwind utility classes in the `.rs` sources.** Per-component CSS files (`styles/components/*.css`, `shell.css`) have all been migrated away and no longer exist — do not add new ones.

`junos-web/index.html` links exactly four stylesheets via `<link data-trunk rel="css" …>`, in this order:

`styles/tokens.css` → `styles/base.css` → `styles/tailwind.css` → `styles/responsive.css`

- `tokens.css` — design tokens (`--bg`, `--text-blue`, `--accent-cyan`, `--sp-*`, `--r-*`, `--fs-*`, `--font-mono`). Reference via `var(--name)` rather than restating hex/px literals; Tailwind's config maps utilities onto these.
- `base.css` — element defaults and the handful of genuinely global rules.
- `tailwind.css` — **generated, do not edit**. A Trunk `pre_build` hook runs `junos-web/bin/tailwindcss` (v3.4.17 standalone, fetched by `just setup-tailwind`) over `styles/tailwind.input.css` with `tailwind.config.js`. `Trunk.toml` ignores it in watch mode.
- `responsive.css` — the single home for hand-written `@media` rules, for the cases Tailwind's breakpoint prefixes don't cover.

In Leptos `view! {}`, style with Tailwind utilities in `class="…"`, and use `class:foo=move || cond` for state toggles. Inline `style=` is reserved for values that genuinely change per render — and even then prefer setting a CSS custom property consumed by a utility or token (see how `tab_wheel.rs` passes `--tw-rot`, `--tw-bx`, `--tw-cr`) rather than restating full property strings. Canvas2D paint strings (`ctx.fillStyle = …` in `sky/render/`) are *not* DOM CSS — leave them inline.

## Offline DSO tiles (Framing Assistant)

The Framing Assistant previews a target **entirely from a local tile cache** —
pre-downloaded hips2fits cutouts, one per catalog object. It never hits the
network. Generate the cache with:

```bash
uv run scripts/prefetch_dso_tiles.py            # all 7960 objects, hours
uv run scripts/prefetch_dso_tiles.py --status   # coverage report, no downloads
uv run scripts/prefetch_dso_tiles.py --limit 50 # smoke test
uv run scripts/prefetch_dso_tiles.py --index-only  # rebuild index.json from disk
```

Output goes to `.cache/dso_tiles/` (gitignored — **not** `junos-web/public/`,
unlike the other catalogs; ~10 GB at the current `TILE_PX`). Override the dir with
the script's `--out` flag or `DSO_TILE_DIR` — note `--dso-tile-dir` is the
*server's* flag, the script has no such option. Resumable: existing tiles are skipped
regardless of size, so changing `TILE_PX` leaves a resolution mix — `--status`
shows it, `--force` refetches.

`junos-server/src/dso_tiles.rs` serves the directory at `/api/dso_tiles/*`.
The cache is **optional** — a missing directory serves an empty index, and an
uncovered zone just previews as the mosaic grid over black.

Preview compositing lives in `junos-web/src/dso_tiles.rs::find_overlapping` +
`components/sky/framing.rs::load_preview`: every cached tile overlapping the
selected zone is stamped (simple TAN offset, no reprojection) onto one black
offscreen canvas sized to the zone, so a zone wider than any single tile or
spanning several objects renders as one adapted image with uncovered sky black.
Tiles are J2000/ICRS; `framing.rs` works in JNow and converts at the boundary.
The server's `/api/skysurvey` proxy is no longer used by the framing path (route
left in place, unused).

## Static assets

`junos-web/public/` contains two binary catalogs: `junos.bin` (stars) and `dso.bin` (deep-sky). Trunk copies both into `dist/`. They are checked in — do not regenerate or re-encode them as part of code changes.

## Code style observed in this codebase

- French and English comments coexist; mirror the surrounding file.
- Commands are dispatched as raw JSON strings; do not introduce a typed command enum.
- Arc-clone `SendCmd` aggressively before moving it into closures.
- Both crates build warning-free; keep it that way. Where a struct mirrors a wire shape (`ws/types.rs`, `catalog.rs`, `files/types.rs`) or declares strings ahead of the UI (`i18n`), it carries one `#[allow(dead_code)]` and a comment saying why — annotate rather than prune those.
- Debug traces go through `debug_log!`, not `leptos::logging::log!` directly, so they stay out of release builds.
- Tab components should take only the specific signals they need plus `SendCmd`, never `DeviceStore` whole. Use Leptos context only for values that need to cross many components (see `*Ctx` newtypes in `main.rs`).
