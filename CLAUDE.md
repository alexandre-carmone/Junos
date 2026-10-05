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

Each tab module lives directly under `components/` (some as directories: `guide/`, `imaging/`, `files/`, `scheduler/`, `sky/`). `SkyTab` is kept mounted (`display:none` when inactive) so its WebGPU context and catalog state survive tab switches, and `MosaicTab` so its form survives the Pick on Sky round trip; the other tabs are lazy via `<Show>`. All 12 are substantial — none is a stub any more. When extending one, grow `DeviceStore` (`ws/store.rs`) and the `apply_ekos_event` match arm only as needed.

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

`just test` runs the server-side unit tests (`cargo test -p junos-server`). `junos-web` is wasm-only — wgpu's webgpu backend does not build for the host — so its tests need a wasm runner and are not in that recipe. Everything else is verified manually: run KStars, enable Ekos Live, point it at `http://localhost:8080`, start an equipment profile (simulators are fine), open the browser to `https://localhost:8443` (accept the self-signed cert), click Start in Ekos, check the HUD shows the mount RA/Dec and the mount-anchored FOV reticle appears on the sky.

## Architecture

### Server (`junos-server`)

`hub.rs` is the central state. A `tokio::sync::broadcast` channel fans KStars events out to every connected browser, plus an `Option<mpsc::Sender>` that points at the currently-attached KStars session (only one KStars can be attached at a time).

- `kstars_ws.rs` handles `GET /message/ekos` and `GET /media/ekos` (KStars connects to these as an Ekos Live "offline server"). On connect it sends KStars the `set_client_state` handshake (required — KStars drops every outbound event until it receives that) and publishes a synthetic `new_connection_state {connected:true}` to the hub. Inbound text is broadcast to browsers verbatim; binary media frames are decoded from the 512-byte metadata header plus JPEG/FITS payload and re-emitted as `new_preview_image`.
- `proxy.rs` handles `GET /ws` (browser side). On connect it tells the new browser the current KStars-attached state, then loops: KStars events → browser, browser commands → KStars via the hub.
- `auth.rs` is a stub for `POST /api/authenticate` (no real auth — local relay only).
- `config.rs` parses the clap `Config` (all long-only flags, each with an env var): `--http-addr`/`HTTP_ADDR`, `--https-addr`/`HTTPS_ADDR`, `--dist-dir`/`DIST_DIR`, `--captures-dir`/`CAPTURES_DIR`, `--dso-tile-dir`/`DSO_TILE_DIR`, `--taskqueue-dir`/`TASKQUEUE_DIR`, `--tls-cert`, `--tls-key`, `--no-https`. Also `resolved_captures_dir()` / `resolved_dso_tile_dir()` / `resolved_taskqueue_dir()`.
- `tls.rs` — cert/key resolution and self-signed generation into `.certs/` (`CN=junos-dev`, SANs = localhost + 127.0.0.1 + every non-loopback IPv4). Supplying only one of `--tls-cert`/`--tls-key` is a fatal error.
- `files.rs` (+ `starfind.rs`) — the Files tab's backend: `/api/files/{list,meta,thumb,raw,download,rename,delete,resolve,tilt}`, FITS header parsing, thumbnailing, and a star detector used by the tilt/aberration analyzer. Sandboxed to the resolved captures dir by canonicalize checks.
- `apps.rs` — `/api/apps/{launch,stop,state}`: spawn and monitor KStars or PHD2 on the server host.
- `dso_tiles.rs` — serves the offline DSO tile cache at `/api/dso_tiles/*` (see "Offline DSO tiles" below).
- `taskqueue.rs` — `/api/taskqueue/{list,queue/:name,script/:name}`: reads/writes the KStars task-queue collections (`collections/*.json`, a `tasks` collection or an `items` queue) and their shell scripts (`scripts/*.sh`, chmod 0755) that the Scheduler's startup/shutdown queue editor builds. Slug names only; reserved `observatory_startup`/`observatory_shutdown`.
- `planning.rs` — `/api/planning/{list,raw,rename,delete}` for Files › Planning: four flat folders keyed by `kind` — `schedules` (`~/.junos-schedules/*.esl`), `sequences` (`~/.junos-sequences/*.esq`), `queues` / `scripts` (the task-queue dir). Bare file names with the kind's extension, canonicalize-checked; only schedules can be renamed (the others are referenced by absolute path), reserved queues can't be deleted. `main.rs` creates both `~/.junos-*` folders at startup, since KStars' `QFile` won't — KStars is assumed to run on the same host as the same user.
- `main.rs` — also serves `GET /api/config`, which reports the resolved captures dir. The frontend reads it once into `CaptureDirCtx` so the sequencer forms default to a folder the Files tab can browse.
- `skysurvey.rs` — `/api/skysurvey`, a same-origin hips2fits proxy. Route still registered but no longer used by the framing path.

There is **no protocol translation** in the server. Messages flow through opaque. All Ekos Live semantics live in the WASM client.

### Frontend (`junos-web`)

Leptos 0.7 CSR. Entry point `main.rs` → `App()` → tab wheel + active tab. Module layout:

- `ws/` — the WebSocket spine (`mod.rs` owns `use_junos_ws()` and the cross-referencing Effects that derive `telescope_settings` from `scopes ∩ trains`; `store.rs` owns `DeviceStore` and `apply_ekos_event()`; `retry.rs` owns `spawn_retry_property()`, fired per device for `CCD_INFO` and `EQUATORIAL_EOD_COORD`; `types.rs` the payload structs). `ws_helpers.rs` sits alongside it.
- `compat.rs` — flat snapshot types (`MountSnapshot`, `CameraSnapshot`, `SiteSnapshot`, `SolveSnapshot`) derived from `DeviceStore`. The sky module imports these, not `DeviceStore`.
- `main.rs` — wires catalogs, site location, language, the Leptos contexts `sky/actions.rs` reads (`ServiceBusyCtx`, `SchedulerPrefillCtx`, `FramingCtx`, `MosaicPlannerCtx`), and the tab shell. Also defines `debug_log!`, a `leptos::logging::log!` that compiles out of release builds.
- `components/tabs.rs` — the `TabContent` router (mount/dismount policy). `components/tab_wheel.rs` (mobile wheel, owns `TABS`) and `components/tab_bar.rs` (desktop strip) are the two switchers; `components/tab_wheel_icons.rs` has the per-tab icons.
- `components/sky/` — planetarium. Dual-canvas renderer (WebGPU bottom + Canvas2D overlay, fallback to all-Canvas2D). See below.
- `components/{mount,focus,polar_align,mosaic_tab,flat_cal,devices,profiles}.rs` and `components/{guide,imaging,files,scheduler}/` — the other tabs. Each takes only the signals it needs plus `SendCmd`; never `DeviceStore` whole.
- `components/mount.rs` — same header as Guide (device · state badge · settings), then cards — Position, Motion (hold-to-move pad + slew-rate pills), GoTo (RA/Dec or by name), Plate solve — one column on phones, two from `md`, and a pinned footer: Park/Unpark, Tracking and Stop (`mount_abort`, always one tap away). The pad only ever stops the direction it started, on release, leave, `pointercancel` or unmount. Meridian flip and solver parameters live in one `sheet`, each change sent at once; the solve timeline and log open in a second one.
- `components/imaging/` — same header as Focus (camera · state badge · reveal in Files), then the frame — pinned on phones with the cards scrolling beneath, frame | cards from `md`; a tap opens it full screen with pan/zoom (`components/zoom.rs`) — and cards: progress tiles, One shot (`fields.rs`: exposure + presets, frame type, filter, gain or ISO; an edit stays pinned until KStars echoes it), Cooling, the queue (`jobs.rs`). Pinned footer: Preview, Loop, Start/Stop. Busy is `status_is_active` *or* a queue job "In Progress": KStars rests on "Image Received" between frames and after a lone preview. The sequence editor and a job's details open as `sheet`s.
- `components/files.rs` (+ `files/`) — the captures browser. Header (Planning · Live Stack button with a state dot · refresh), a toolbar (breadcrumb, search, sort, type pills), then folder rows and a thumbnail grid (two columns on phones). A tap opens the viewer (`viewer.rs`), a page over the list laid out like Imaging — frame pinned on phones with the FITS info cards beneath, frame | cards from `md` — with ‹ ›, arrow keys or a swipe stepping through the listed files, and a pinned footer: Delete (then the next file), Download, Resolve & Slew (`align_load_and_slew {filename}`, host path from `CaptureDirCtx`). Live Stack is a `sheet` (`livestack.rs`): stats, the newest image in `outputDirectory`, and settings cards (`settings.rs`) that show KStars' values and send each key on change — `livestacker_set_all_settings` merges and echoes the full map. Planning is a second `sheet` (`files/planning/`, over `/api/planning/*`): the Scheduler's schedules, job sequences, startup/shutdown queues and scripts, grouped, each opening formatted (`parse.rs` reads `.esl`/`.esq` with `roxmltree`, `view.rs`) above its source, with Delete, Rename (schedules), Download and Load in Scheduler (`scheduler_load_file`) / Imaging (`capture_load_sequence_file`). The viewer shows an `.esl`/`.esq` in the captures (a mosaic import writes them there; the Planning type pill) the same way. Shared signals travel in one `Shared` struct; one `frame_zoom` serves the viewer and the stacked frame.
- `components/focus.rs` (+ `focus/` for the aberration inspector) — phone-first like the sky: the frame stays pinned on phones with the controls and HFR V-curve scrolling beneath it, two columns from `md` (one DOM, the scroll wrapper is `md:contents`). Settings open as a bottom sheet / floating panel. The Autofocus button becomes Stop for any focus state other than Idle/Complete/Failed/Aborted.
- `components/polar_align.rs` — same layout as Focus (pinned align frame, controls beneath / right column, settings sheet). A 4-step strip (3 captures → Adjust) and one primary button driven by the stage (`primary_for`: Start → Stop / Rotation done → Start refresh → Stop); while adjusting, Total/Az/Alt tiles with move arrows and an error bullseye.
- `components/mosaic_tab.rs` — same header as Focus, then cards (Target · Grid · Capture sequence · Scheduler) in one column, two from `md`, and a pinned footer (tile count × sequence time, Send to Scheduler). The Grid card draws the tiles as an SVG (`tile_diagram`, north up / east left, rotated by PA). Pick on Sky hands off to the Sky tab, whose banner has a Cancel; leaving the Sky abandons the pick.
- `components/scheduler.rs` (+ `scheduler/`) — same header (plus Save schedule, `view_save.rs`: `scheduler_save_file` to `~/.junos-schedules/<name>.esl`, listed in Files › Planning), then two sub-tabs. **Jobs**: the job cards and the live log (one column on phones, jobs | log from `md`) and a pinned footer: Add job and Start/Stop. `scheduler_start_job` is a toggle that KStars only turns into a stop while RUNNING (2), so the button shows Stop only then and is disabled during startup/shutdown/loading. **Startup & shutdown** (`view_procedures.rs`): the four procedure slots as a night timeline with the startup/shutdown switches, each change sent at once; a slot opens the queue editor (`view_queue_editor.rs`) over the timeline on phones, beside it from `md`. The editor's steps are built-in templates, custom INDI steps (`view_indi_step.rs`: Set / Wait until on any device·property·element, picked from the device's `device_get` listing with a form per property type — number with range, text, one-of-many switch as a list, lone switch as a button, any-of-many On/Off, light, property state — or typed by hand when Ekos is offline) and shell scripts with snippets (`queue_snippets.rs`). Add job, Save schedule and Settings open as `sheet`s (bottom sheet on phones, centered panel on md+); Settings only links to the sub-tab for procedures. The Sky's "Add to Scheduler" fills the add-job form and opens it. `scheduler/altitude.rs` draws tonight's altitude chart — every job above the list, and the target with its estimated session in the add-job form — recomputed locally from RA/Dec and the site, since Ekos Live sends only a job's current altitude.
- `components/guide/` — same header (state badge · export log · settings), then RMS tiles, the drift plot (`timeline.rs`) and target plot (`target.rs`), and the guide frame — one column on phones, drift | target from `md` — with a pinned footer: Capture/Loop (internal guider only) and Guide/Stop. `GuideTab` memoizes the derived `GuideSnapshot` once and splits it, so a log line doesn't redraw the plots. Every parameter lives in the settings sheet (`settings.rs`: collapsible cards, each change sent at once).
- `components/devices.rs` — the INDI control panel. Same header (selected device's connection badge), then the device list (a chip strip on phones, a sidebar from `md`), pills for the device's INDI groups (one group shown at a time), one card per property, and a pinned footer with the latest device message that opens them all in a `sheet`. Each card is a `<form>`: numbers/texts are buffered and Set or Enter sends the whole vector; switches apply at once (pills, a select past 6, checkboxes for NOFMANY). Numbers render INDI `%m` as sexagesimal, and an edited sexagesimal string is sent as-is (KStars runs `f_scansexa`).
- `components/profiles.rs` — the gear tab. Same header (selected profile · Ekos state · refresh), then cards — Applications (KStars/PHD2 via `/api/apps`) and Profiles, Rigs (Ekos online only) and Telescopes — one column on phones, two from `md`, and a pinned footer: New profile. A profile row has Launch/Stop (`profile_start`/`profile_stop`); its pencil, or a rig's or telescope's, opens an editor `sheet` with Delete and Save. An editor keeps its item in one `RwSignal` draft that small rows bind to through `get`/`set` fn pointers. KStars keys profiles by name, so a rename is delete + add and a taken name disables Save; `train_*` gets no reply, so the tab re-fetches.
- `components/{coord_input,dialog_modal,form,frame_type,zoom}.rs` and `components/sequence_editor/` — shared pieces, not tabs. `form.rs` holds the touch-sized form vocabulary (`CARD`, `ROW`, `FOOTER`, `setting_row`, `check_row`, `toggle_chip`, …), `sheet` (the bottom sheet / centered panel the tabs open over themselves) and `JobOptions` — steps, start/completion and constraints — that Mosaic and Scheduler both edit and send (`settings_json`). `dialog_modal.rs` surfaces KStars' blocking `dialog_get_info` prompts. `sequence_editor/` is the card-based capture-sequence builder shared by Imaging, Mosaic and Scheduler (`model.rs` row + validation, `esq.rs` XML, `card.rs` one job card); `frame_type.rs` holds the frame-type pills it shares with the Imaging one-shot panel. `zoom.rs` is the full-screen pan/zoom frame (wheel/pinch, drag, double-click reset) used by Imaging and Files.
- `dso_tiles.rs` — offline tile index fetcher (`/api/dso_tiles/index.json`) and `DsoTileIndex::find_overlapping`, consumed by the Framing Assistant.
- `astro.rs` / `coords.rs` / `ephemeris.rs` — equatorial↔horizontal math (Julian date, GMST/LST, precession to/from J2000, `fov_deg(focal, sensor_px, pixel_um)`), and ephemerides for solar-system bodies. Correct — reuse, do not reimplement.
- `catalog.rs`, `dso_catalog.rs` — async fetchers for `public/junos.bin` and `public/dso.bin`.
- `dom.rs` — `event_target_value` / `event_target_checked`, the only two DOM-event readers; every tab uses these rather than rolling its own.
- `i18n/` — module dir (`mod.rs` + `en.json` + `fr.json`, embedded via `include_str!`). EN default, FR selected via the `EN`/`FR` pill in the tab bar/wheel and persisted to localStorage `junos_lang`; there is no browser auto-detect. The `translations!` macro declares the schema — a key missing from one language **panics** on first use. Many unused strings; don't gratuitously prune.

`SendCmd = Arc<dyn Fn(String) + Send + Sync>` — type-erased command sink. Components dispatch raw JSON strings via `send(serde_json::json!({"type":"…","payload":{…}}).to_string())`. Do not introduce a typed command enum.

### Planetarium (`components/sky/`)

The most fully-featured surface. Treat as stable — make targeted edits when adding overlays or interactions; don't rewrite. Layout (phone-first): a top bar (search · follow-mount · layers), a bottom stack (HUD above the time bar), and two sheets that are bottom sheets on phones and floating panels on md+ (layers panel, target card), all kept `md:right-[72px]` clear of the desktop tab strip. Structure:

- `mod.rs` — `SkyTab` component, canvas/GPU setup, event loop, gestures, localStorage persistence (`sky_center_alt`, `sky_center_az`, `sky_fov_radius`, `sky_follow_mount`, plus one `persisted(key, default)` signal per render toggle and `sky_dso_mag_limit`). Also owns the `DsoImageCache` (`dso_images.rs`) handed to every `Frame`. Gestures: click/tap → Mosaic pick-on-sky if armed, else the target card for the object under the pointer (`hit_test`); right-click / 500 ms long-press → the target card at the pressed point (snapped to an object); drag pans (and only then stops following the mount); pinch/wheel zooms. Touch handlers `preventDefault`, so taps are detected in `on_touchend`, not via mouse events.
- `clock.rs` — `SkyClock`, the sky's single time source (`sim = base + elapsed × rate`): everything that projects the sky reads `clock.jd()`, never `astro::now_jd()`. Also the local-time helpers (`night_hour`: HH:MM within the noon→noon night; `on_date`) and `night_twilights` (astronomical dusk/dawn, sun −18°).
- `time_bar.rs` — the time bar: ‹ › step (1m/10m/1h/1d), ▶▶ time-lapse at one step per second, Now, date and hour pickers, dusk/dawn jumps. Amber border whenever the sky isn't live. Time is never persisted.
- `render/` — Canvas2D overlay (`mod.rs`, `layer.rs`, `params.rs`, `pipeline.rs` + one module per layer in `render/layers/`: allsky, dso_image, stars, dso, grids, ground, zenith, constellation_names, center_crosshair, mount_crosshair, fov_reticle, solve_marker, slew_trail, solar_system, mosaic, scheduler_jobs). GPU draw order is fixed in `gpu/mod.rs::render_inner`: clear → allsky → dso_image → lines → dso symbols → constellations → stars → text. Draws grid, horizon, constellations (falls back from GPU), DSO labels, `render_center_fov()` and `render_mount_fov()` — the two FOV rectangles. Both call `astro::fov_deg` with `RenderParams.{fl, cam_pixel_size_um, cam_sensor_width, cam_sensor_height, rotation_deg, mount_ra_h, mount_dec_deg}`.
- `controls.rs` — layers panel: one `LayerChip` per toggle in `SkyToggles` (grouped Sky / Grids / Deep sky / Equipment), the DSO mag slider, and the observer location.
- `search.rs` — catalog object search (in the top bar).
- `actions.rs` — the target card (`SkyTarget`): object or sky-position info (JNow, J2000, Alt/Az at the displayed time) and five actions: Center, `mount_goto_rade`, goto-then-`align_solve`, Framing assistant (`FramingCtx`), Add to Scheduler (`SchedulerPrefillCtx`), the last two pre-filled with the object name. Reads `ServiceBusyCtx` (to disable Goto / Goto & Align while a device is busy), `SchedulerPrefillCtx` and `FramingCtx` from the crate root — these newtypes live in `main.rs` and must be provided. `MosaicPlannerCtx` (also in `main.rs`) drives the Pick-on-Sky flow that hands a center off to the Mosaic tab.
- `framing.rs` — the Framing Assistant modal (opened only from `actions.rs`, not a tab). See "Offline DSO tiles" below.
- `hud.rs` (the only HUD, DOM, in GPU and Canvas2D-fallback mode alike), `picking.rs`, `object_search.rs`, `dso_index.rs`, `dso_render.rs`, `dso_shape.rs`, `solar_render.rs`, `utils.rs`, `gpu/`, `shaders/*.wgsl` — the remaining pieces.

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
6. **Scheduler "script" slots take task-queue JSON, not scripts.** In KStars 3.8+ `schedulerPreStartupScript`/`schedulerPostStartupScript`/`schedulerPreShutdownScript`/`schedulerPostShutdownScript` are absolute paths to task *collections* (`QueueManager::loadQueue`); a shell script only runs as a `script_execute` task inside one, and must be executable with a shebang. KStars silently drops a task with an unknown `template_id` or a missing/out-of-range parameter, and a device task in the pre-startup or post-shutdown slot (INDI down) stalls the scheduler — `components/scheduler/queue_model.rs` mirrors `kstars/kstars/data/taskqueue/templates/system/*.json` and enforces all three. A collection can only name templates, so a queue with a custom INDI step is written in KStars' own queue format (`{"items":[{"task":{…,"actions":[…]}}]}`, `queue_native.rs`, built-in steps spelled out from a copy of the templates' actions); `loadQueue` reads both. That format ignores a task's `failure_action` for a missing device — it always aborts the queue.
7. **Module logs arrive newest first.** `new_guide_state {log}`, `new_scheduler_state {log}` and `new_capture_state {log}` carry `getLogText()`, and KStars prepends each entry (`Guide::appendLogText`, `SchedulerProcess::appendLogText`, `Capture::appendLogText`). Show them as-is; the latest line is the first one.
8. **`scheduler_save_file` can silently write nothing.** `Scheduler::save` (`scheduler.cpp:2304`) skips the write when the job list hasn't changed since it was last saved or loaded, then the handler replies with whatever file already sits at that path — or not at all. Its reply, like those of `scheduler_load_file` and `capture_load_sequence_file`, lands in `DeviceStore::file_reply` (`ws_helpers::file_command` sends and waits); Save schedule trusts only the file's mtime moving.
9. **`scheduler_import_mosaic` empties the job list.** `FramingAssistantUI::createJobs` (`framingassistantui.cpp:653`) calls `removeAllJobs()` before adding the tiles, so the Mosaic tab never uses it: Send to Scheduler lays the tiles out itself (`sky::derive_planner_mosaic_plan`, the sky's preview) and adds one job per tile like Add job does (`scheduler_set_all_settings` + `scheduler_add_jobs`), all sharing one `.esq`. KStars inserts an added job below the selected row, not at the end.

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

## Offline DSO tiles (Framing Assistant, sky imagery)

The Framing Assistant previews a target **entirely from a local tile cache** —
pre-downloaded hips2fits cutouts, one per catalog object. It never hits the
network. The same cache feeds the planetarium's imagery (below). Generate it with:

```bash
uv run scripts/prefetch_dso_tiles.py            # all 7960 objects, hours
uv run scripts/prefetch_dso_tiles.py --status   # coverage report, no downloads
uv run scripts/prefetch_dso_tiles.py --limit 50 # smoke test
uv run scripts/prefetch_dso_tiles.py --index-only  # rebuild index.json from disk
uv run scripts/prefetch_dso_tiles.py --thumbs   # sprites (thumbs/) from the tiles on disk
uv run scripts/prefetch_dso_tiles.py --allsky   # Milky Way panorama (allsky.jpg + allsky_small.jpg)
```

Output goes to `.cache/dso_tiles/` (gitignored — **not** `junos-web/public/`,
unlike the other catalogs; ~10 GB at the current `TILE_PX`). Override the dir with
the script's `--out` flag or `DSO_TILE_DIR` — note `--dso-tile-dir` is the
*server's* flag, the script has no such option. Resumable: existing tiles are skipped
regardless of size, so changing `TILE_PX` leaves a resolution mix — `--status`
shows it, `--force` refetches.

`junos-server/src/dso_tiles.rs` serves the directory at `/api/dso_tiles/*`
(`index.json`, `<slug>.jpg`, `thumbs/<slug>.jpg`, `allsky.jpg`, `allsky_small.jpg`;
names are `[a-z0-9_]+.jpg` only). The cache is **optional** — a missing
directory serves an empty index, an uncovered zone just previews as the mosaic
grid over black, and the sky draws symbols only.

**Sky imagery.** `--thumbs` (Pillow, also run at the end of a fetch) writes a
512 px copy of each tile with the sky background subtracted and a circular fade
to black, and `--allsky` fetches one plate-carrée J2000 panorama
(`CDS/P/Mellinger/color`, centred on RA 0h, north up, **east left**:
`u = 0.5 − RA/360`). Both are drawn inside the WebGPU render pass, before the
lines and stars: `gpu/layers/allsky.rs` (full-screen triangle, per-pixel inverse
projection + inverse precession in `shaders/allsky.wgsl`) then
`gpu/layers/dso_image.rs` (additive sprites from a mipmapped `texture_2d_array`,
LRU slots, ≤ 2 uploads per frame). `components/sky/dso_images.rs` owns the
decoded `<img>`s (bounded, loads bump an epoch signal); `render/layers/dso_image.rs`
culls like `dso_render::build`, keeps objects with a tile whose sprite is ≥ 10 px,
rotates them to sky north (`dso_shape::probe`) and lists them in `Frame.imaged`
so the symbol layers skip their outline. `gpu/texture_upload.rs` builds the mip
chains by canvas halving + `copyExternalImageToTexture` — that call **panics the
wasm module on any validation error**, so keep its preconditions (decoded image,
exact canvas size, `MipUploader::USAGE`). Canvas2D fallback: sprites via
`lighter`, no Milky Way. Toggles: `sky_show_dso_images`, `sky_dso_images_brightness`,
`sky_show_milky_way`, `sky_milky_way_opacity`.

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
