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
- `scripts/` — `precompress_dist.sh` writes `.gz` (and `.br` when brotli is on PATH) siblings next to the dist's wasm / js / css / html / catalogs (not `tycho.bin`, incompressible) so the server serves them pre-compressed; every build path calls it (`just build-wasm`, `flake.nix`, the tarball script, the PKGBUILD). Then three generators. `fetch_skymap.py` is PEP-723 self-contained (`uv run scripts/fetch_skymap.py`) and mirrors the CDS `P/DSS2/color` HiPS tree (orders 0–7 by default) plus the Mellinger panorama into `.cache/skymap/` (see "Offline sky map"); `gen_dso_catalog.py` is plain `python3` and writes `junos-web/public/dso.bin`: OpenNGC (`openngc.csv` + `openngc_addendum.csv`) merged with Sharpless, vdB, Barnard, Lynds, Abell, Hickson, Arp and non-NGC cluster catalogs from VizieR/SIMBAD (cached in `scripts/dso_sources/`, offline unless `--refresh`), one entry per object via SIMBAD cross-ids then position, plus `dso_extra.csv` — the curated EN/FR common names, the exotic objects no catalog has, and emission-signature overrides (`lines` column, `Ha3 OIII2 SII1 RGB2`). Add a nickname, an object or a signature there, not in code. It also writes each object's IAU constellation (Roman's 1875 boundaries, VizieR VI/42, cross-checked against OpenNGC's `Const`) and its emission-line signature, derived from the kind and the catalogs' types (OpenNGC `HII`/`EmN`/`RfN`/`Neb`/`Cl+N` and Hubble type, SIMBAD otype, Sharpless vs vdB). `gen_star_tiles.py` (plain `python3`, Tycho-2 cached in `.cache/tycho2/`) writes `junos-web/public/tycho.bin`: every Tycho-2 star `junos.bin` lacks, in ~5° sky tiles. There is **no generator for `junos.bin`** — the base star catalog (names, constellation figures) is checked in and authoritative; regenerate `tycho.bin` if it ever changes. Don't regenerate outputs as part of unrelated code changes.
- `packaging/` — Arch Linux package (`packaging/arch/`), portable tarball (`packaging/portable/`), and CI notes. `.github/workflows/` has `ci.yml` (mirrors `just check`) and `release.yml` (tag-triggered two-arch build).
- `flake.nix` / `nix/` — Nix dev shell, packages, and the `services.junos-web` NixOS module. Already provided; don't propose adding one.

## Frontend tabs

`junos-web` is no longer planetarium-only. The `Tab` enum lives in `main.rs` (**not** `components/tabs.rs`, which only holds the `TabContent` router). Render order is `TABS` in `components/tab_wheel.rs`, used by both the mobile wheel and the desktop strip (`components/tab_bar.rs`):

`Profiles`, `Sky`, `Targets`, `Mount`, `Focus`, `Imaging`, `Files`, `PolarAlign`, `Guide`, `Scheduler`, `Mosaic`, `Devices` — 12 tabs. Displayed labels come from `i18n` (`tab_*` keys); `PolarAlign` renders as "Polar Align". There is no dust cap / flat panel tab (removed): the cap and panel are reached through the Devices tab, the Scheduler's queue steps and `sequence_editor/cover.rs`.

Each tab module lives directly under `components/` (some as directories: `guide/`, `imaging/`, `files/`, `scheduler/`, `sky/`, `targets/`). `SkyTab` is kept mounted (`display:none` when inactive) so its WebGPU context and catalog state survive tab switches, and `MosaicTab` so its form survives the Pick on Sky round trip; the other tabs are lazy via `<Show>`. All 12 are substantial — none is a stub any more. When extending one, grow `DeviceStore` (`ws/store.rs`) and the `apply_ekos_event` match arm only as needed.

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

Workspace root `Cargo.toml` sets `default-members = ["junos-server"]`, so `cargo build`/`cargo run` from the root operate on the server only. `junos-web` is only buildable through Trunk (or `cargo check --target wasm32-unknown-unknown -p junos-web`). The wasm is built with the `wasm-release` profile (`opt-level = "z"`, fat LTO, `panic = "abort"`; `data-cargo-profile` in `index.html`, `--profile wasm-release` in `flake.nix`), kept apart from `release` so the server keeps unwinding and a fast build.

### Server ports

`junos-server` binds **two** ports by default:

- **HTTP on `:8080`** — KStars-facing. KStars' Ekos Live client connects here.
- **HTTPS on `:8443`** — browser-facing. iOS Safari requires TLS to expose WebGPU, so the browser must hit `https://<host>:8443`. A self-signed cert is auto-generated into `.certs/` on first run.

Pass `--no-https` to skip TLS for headless/CI runs. `config.rs` (clap, env-aware) parses `--http-addr`, `--https-addr`, `--dist-dir`, and the TLS flags.

`just test` runs the server-side unit tests (`cargo test -p junos-server`). `junos-web` is wasm-only — wgpu's webgpu backend does not build for the host — so its tests need a wasm runner and are not in that recipe. Everything else is verified manually: run KStars, enable Ekos Live, point it at `http://localhost:8080`, start an equipment profile (simulators are fine), open the browser to `https://localhost:8443` (accept the self-signed cert), click Start in Ekos, check the HUD shows the mount RA/Dec and the mount-anchored FOV reticle appears on the sky.

## Architecture

### Server (`junos-server`)

`hub.rs` is the central state. A `tokio::sync::broadcast` channel of `Arc<Outbound>` (`Text` or `Preview {kind, full, lite}`) fans KStars events out to every connected browser, plus an `Option<mpsc::Sender>` that points at the currently-attached KStars session (only one KStars can be attached at a time). Its replay cache keeps the last message per sticky type, the guide history and the latest preview per `FrameKind`, and counts the browsers wanting lite previews.

- `kstars_ws.rs` handles `GET /message/ekos` and `GET /media/ekos` (KStars connects to these as an Ekos Live "offline server"). On connect it sends KStars the `set_client_state` handshake (required — KStars drops every outbound event until it receives that) and publishes a synthetic `new_connection_state {connected:true}` to the hub. Inbound text is broadcast to browsers verbatim; binary media frames are decoded (in `spawn_blocking`) from the 512-byte metadata header plus JPEG/FITS payload and re-emitted as `new_preview_image` — an `Outbound::Preview` carrying the frame's kind, the full JSON and, while any browser asked for lite previews, a 960 px / q70 re-encode (`lite_jpeg`; KStars itself always sends 1920 px / q90, `media.h:124`). The focus star overlay is measured on the full frame and scaled onto the lite one.
- `proxy.rs` handles `GET /ws` (browser side). On connect it tells the new browser the current KStars-attached state and replays the hub's snapshot, then loops: KStars events → browser, browser commands → KStars via the hub. Previews are filtered per browser: the client sends `{"type":"junos_media_filter","payload":{"kinds":["+G"],"max_width":960,"quality":70}}` (kinds ⊆ `+F` focus, `+A` align, `+G` guide, `capture`; `main.rs` derives it from the active tab and re-sends it on each `connected` rising edge) and gets only those kinds — nothing until it has sent one — in the lite variant when it asked for `max_width`. A newly wanted kind is served the hub's latest frame of that kind at once. `junos_*` messages are for the relay and never reach KStars.
- `auth.rs` is a stub for `POST /api/authenticate` (no real auth — local relay only).
- `config.rs` parses the clap `Config` (all long-only flags, each with an env var): `--http-addr`/`HTTP_ADDR`, `--https-addr`/`HTTPS_ADDR`, `--dist-dir`/`DIST_DIR`, `--captures-dir`/`CAPTURES_DIR`, `--skymap-dir`/`SKYMAP_DIR`, `--skymap-online`/`SKYMAP_ONLINE`, `--taskqueue-dir`/`TASKQUEUE_DIR`, `--tls-cert`, `--tls-key`, `--no-https`. Also `resolved_captures_dir()` / `resolved_skymap_dir()` / `resolved_taskqueue_dir()`.
- `tls.rs` — cert/key resolution and self-signed generation into `.certs/` (`CN=junos-dev`, SANs = localhost + 127.0.0.1 + every non-loopback IPv4). Supplying only one of `--tls-cert`/`--tls-key` is a fatal error.
- `files.rs` — the Files tab's backend: `/api/files/{list,meta,thumb,raw,download,rename,delete,resolve}`, FITS header parsing and thumbnailing. Sandboxed to the resolved captures dir by canonicalize checks. `starfind.rs` is the star detector `kstars_ws.rs` runs on focus frames for the Focus tab's star overlay.
- `apps.rs` — `/api/apps/{launch,stop,state,power,guider}`: spawn and monitor KStars or PHD2 on the server host; `power {action: poweroff|reboot}` runs `systemctl <action>` as the server's user (logind + polkit decide — the NixOS module's `hostPower.enable` and the Arch polkit rule grant it to service installs); `guider {action: connect|disconnect}` runs `dbus-send` against KStars' `/KStars/Ekos/Guide` `connectGuider`/`disconnectGuider` — the Guide tab's Connect PHD2 button, since Ekos Live has no such command and `option_set GuiderType` changes nothing live (`Guide::processGuideOptions` is never called).
- `skymap.rs` — serves the offline sky map, a local HiPS mirror, at `/api/skymap/*` (see "Offline sky map" below): `info` (JSON, `no-cache` + weak ETag → 304; derived from a startup directory scan into `AppState.skymap` when `info.json` is missing), `properties`, `Norder{k}/Dir{d}/Npix{n}.jpg` tiles (`parse_tile` validates the three segments and rebuilds the path from the numbers; `immutable` for a year + strong ETag), `allsky.jpg` / `allsky_small.jpg` (a day). With `--skymap-online`, a missing tile is fetched from the CDS mirrors (reqwest, `Semaphore(4)`), stored in the tree and served; failure → 502 `no-store`.
- `http_cache.rs` — the `Cache-Control` middleware for the static frontend: `index.html` → `no-cache`; the Trunk-hashed wasm / js / css and the three `*.bin` → `immutable` for a year (the catalogs are fetched with a `?v=` the client bumps on regeneration). `/api/*` is left to its handlers. `main.rs` puts every HTTP route under it and under tower-http's `CompressionLayer` (br/gzip, level 4), with `ServeDir::precompressed_{br,gzip}` serving the siblings `scripts/precompress_dist.sh` wrote; the three WebSocket routes sit outside both.
- `taskqueue.rs` — `/api/taskqueue/{list,queue/:name,script/:name}`: reads/writes the KStars task-queue collections (`collections/*.json`, a `tasks` collection or an `items` queue) and their shell scripts (`scripts/*.sh`, chmod 0755) that the Scheduler's startup/shutdown queue editor builds. Slug names only; reserved `observatory_startup`/`observatory_shutdown`.
- `planning.rs` — `/api/planning/{list,raw,rename,delete}` for Files › Planning: four flat folders keyed by `kind` — `schedules` (`~/.junos-schedules/*.esl`), `sequences` (`~/.junos-sequences/*.esq`), `queues` / `scripts` (the task-queue dir). Bare file names with the kind's extension, canonicalize-checked; only schedules can be renamed (the others are referenced by absolute path), reserved queues can't be deleted. `main.rs` creates both `~/.junos-*` folders at startup, since KStars' `QFile` won't — KStars is assumed to run on the same host as the same user.
- `main.rs` — also serves `GET /api/config`, which reports the resolved captures dir. The frontend reads it once into `CaptureDirCtx` so the sequencer forms default to a folder the Files tab can browse.

There is **no protocol translation** in the server. Messages flow through opaque. All Ekos Live semantics live in the WASM client. The one exception is the `junos_*` control family (`junos_media_filter`, above), consumed by `proxy.rs`.

### Frontend (`junos-web`)

Leptos 0.7 CSR. Entry point `main.rs` → `App()` → tab wheel + active tab. Module layout:

- `ws/` — the WebSocket spine (`mod.rs` owns `use_junos_ws()` and the cross-referencing Effects that derive `telescope_settings` from `scopes ∩ trains`; `store.rs` owns `DeviceStore` and `apply_ekos_event()`; `retry.rs` owns `spawn_retry_property()`, fired per device for `CCD_INFO` and `EQUATORIAL_EOD_COORD`, the 20 s settings sweep and the mount-coordinate poll — 1.5 s while Sky / Mount / Polar Align is active (`store.mount_fast_poll`), 6 s otherwise, both paused while `document.hidden`; `types.rs` the payload structs). `ws_helpers.rs` sits alongside it.
- `net.rs` — `cached_bytes(url)`: the three catalogs go through the Cache API (`junos-catalogs`, keyed by the versioned URL, stale versions evicted, plain fetch on any failure) so a reload over weak wifi doesn't re-download 7–13 MB.
- `compat.rs` — flat snapshot types (`MountSnapshot`, `CameraSnapshot`, `SiteSnapshot`, `SolveSnapshot`) derived from `DeviceStore`. The sky module imports these, not `DeviceStore`.
- `main.rs` — wires catalogs, site location, language, the Leptos contexts `sky/actions.rs` reads (`ServiceBusyCtx`, `SchedulerPrefillCtx`, `FramingCtx`, `MosaicPlannerCtx`), `SkyFocusCtx` (another tab's "Show on Sky": SkyTab moves its clock, centres the object and opens its target card), `LiteModeCtx` (the `Lite` pill next to EN/FR in both switchers, localStorage `junos_lite`: lite previews from the relay, no `tycho.bin`, the sky map two orders coarser, the small Milky Way panorama) and the Effect that sends `junos_media_filter` from the active tab, and the tab shell. Also defines `debug_log!`, a `leptos::logging::log!` that compiles out of release builds.
- `components/tabs.rs` — the `TabContent` router (mount/dismount policy). `components/tab_wheel.rs` (mobile wheel, owns `TABS`) and `components/tab_bar.rs` (desktop strip) are the two switchers; `components/tab_wheel_icons.rs` has the per-tab icons.
- `components/sky/` — planetarium. Dual-canvas renderer (WebGPU bottom + Canvas2D overlay, fallback to all-Canvas2D). See below.
- `components/{mount,focus,polar_align,mosaic_tab,devices,profiles}.rs` and `components/{guide,imaging,files,scheduler}/` — the other tabs. Each takes only the signals it needs plus `SendCmd`; never `DeviceStore` whole.
- `components/mount.rs` — same header as Guide (device · state badge · settings), then cards — Position, Motion (hold-to-move pad + slew-rate pills), GoTo (RA/Dec or by name), Plate solve — one column on phones, two from `md`, and a pinned footer: Park/Unpark, Tracking and Stop (`mount_abort`, always one tap away). The pad only ever stops the direction it started, on release, leave, `pointercancel` or unmount. Meridian flip and solver parameters live in one `sheet`, each change sent at once; the solve timeline and log open in a second one.
- `components/imaging/` — same header as Focus (camera · state badge · reveal in Files), then the frame — pinned on phones with the cards scrolling beneath, frame | cards from `md`; a tap opens it full screen with pan/zoom (`components/zoom.rs`); a histogram overlays it (`histogram.rs`, toggle top-right): the displayed JPEG's levels counted in the browser, on a Display axis (0–255) or a fixed ADU axis up to the frame's full scale (the levels mapped back through the inverse of KStars' stretch, whose shadows / midtones / highlights come in the media metadata), Lin/Log, over KStars' raw median / mean / σ / min / max (`FrameStats`) — and cards: progress tiles, One shot (`fields.rs`: exposure + presets, frame type, filter, gain or ISO; an edit stays pinned until KStars echoes it), Cooling, the queue (`jobs.rs`). Pinned footer: Preview, Loop, Start/Stop. Busy is `status_is_active` *or* a queue job "In Progress": KStars rests on "Image Received" between frames and after a lone preview. The sequence editor and a job's details open as `sheet`s.
- `components/files.rs` (+ `files/`) — the captures browser. Header (Planning · Live Stack button with a state dot · refresh), a toolbar (breadcrumb, search, sort, type pills), then folder rows and a thumbnail grid (two columns on phones). A tap opens the viewer (`viewer.rs`), a page over the list laid out like Imaging — frame pinned on phones with the FITS info cards beneath, frame | cards from `md` — with ‹ ›, arrow keys or a swipe stepping through the listed files, and a pinned footer: Delete (then the next file), Download, Resolve & Slew (`align_load_and_slew {filename}`, host path from `CaptureDirCtx`). Live Stack is a `sheet` (`livestack.rs`): stats, the newest image in `outputDirectory`, and settings cards (`settings.rs`) that show KStars' values and send each key on change — `livestacker_set_all_settings` merges and echoes the full map. Planning is a second `sheet` (`files/planning/`, over `/api/planning/*`): the Scheduler's schedules, job sequences, startup/shutdown queues and scripts, grouped, each opening formatted (`parse.rs` reads `.esl`/`.esq` with `roxmltree`, `view.rs`) above its source, with Delete, Rename (schedules), Download and Load in Scheduler (`scheduler_load_file`) / Imaging (`capture_load_sequence_file`). The viewer shows an `.esl`/`.esq` in the captures (a mosaic import writes them there; the Planning type pill) the same way. Shared signals travel in one `Shared` struct; one `frame_zoom` serves the viewer and the stacked frame.
- `components/focus.rs` (+ `focus/vcurve.rs`, the V-curve parabola fit and axis ticks) — phone-first like the sky: the frame stays pinned on phones with the controls and HFR V-curve scrolling beneath it, two columns from `md` (one DOM, the scroll wrapper is `md:contents`). Settings open as a bottom sheet / floating panel. The Autofocus button becomes Stop for any focus state other than Idle/Complete/Failed/Aborted.
- `components/polar_align.rs` — same layout as Focus (pinned align frame, controls beneath / right column, settings sheet). A 4-step strip (3 captures → Adjust) and one primary button driven by the stage (`primary_for`: Start → Stop / Rotation done → Start refresh → Stop); while adjusting, Total/Az/Alt tiles with move arrows and an error bullseye.
- `components/mosaic_tab.rs` — same header as Focus, then cards (Target · Grid · Capture sequence · Scheduler) in one column, two from `md`, and a pinned footer (tile count × sequence time, Send to Scheduler). The Grid card draws the tiles as an SVG (`tile_diagram`, north up / east left, rotated by PA). Pick on Sky hands off to the Sky tab, whose banner has a Cancel; leaving the Sky abandons the pick.
- `components/scheduler.rs` (+ `scheduler/`) — same header (plus Save schedule, `view_save.rs`: `scheduler_save_file` to `~/.junos-schedules/<name>.esl`, listed in Files › Planning), then two sub-tabs. **Jobs**: the job cards and the live log (one column on phones, jobs | log from `md`) and a pinned footer: Add job and Start/Stop. `scheduler_start_job` is a toggle that KStars only turns into a stop while RUNNING (2), so the button shows Stop only then and is disabled during startup/shutdown/loading. **Startup & shutdown** (`view_procedures.rs`): the four procedure slots as a night timeline with the startup/shutdown switches, each change sent at once; a slot opens the queue editor (`view_queue_editor.rs`) over the timeline on phones, beside it from `md`. The editor's steps are built-in templates, custom INDI steps (`view_indi_step.rs`: Set / Wait until on any device·property·element, picked from the device's `device_get` listing with a form per property type — number with range, text, one-of-many switch as a list, lone switch as a button, any-of-many On/Off, light, property state — or typed by hand when Ekos is offline) and shell scripts with snippets (`queue_snippets.rs`). Add job, Save schedule and Settings open as `sheet`s (bottom sheet on phones, centered panel on md+); Settings only links to the sub-tab for procedures. The Sky's "Add to Scheduler" fills the add-job form and opens it. A sequence mixing lights and calibration rows is split by default (`split_calib`) into the lights job and a `<name> calibration` job added right below it — same target, start condition and constraints, track at most as a step, sequence completion once, its own `<safe_name>-calibration.esq` carrying the post-job cover script — because KStars aborts a job on repeated guide/focus/align failures before Capture starts, flats included. `scheduler/altitude.rs` draws tonight's altitude chart — every job above the list, and the target with its estimated session in the add-job form — recomputed locally from RA/Dec and the site, since Ekos Live sends only a job's current altitude. The add-job chart's session starts by default at the end of the job that ends last (KStars' `stopTime`); a picker swaps it for another job's end, an hour, or the start condition's estimate — an indication only, the job is still sent with its start condition.
- `components/guide/` — same header (state badge · export log · settings), then RMS tiles, the drift plot (`timeline.rs`) and target plot (`target.rs`), and the guide frame — one column on phones, drift | target from `md` — with a pinned footer: Capture/Loop (internal guider only) and Guide/Stop. `GuideTab` memoizes the derived `GuideSnapshot` once and splits it, so a log line doesn't redraw the plots. Every parameter lives in the settings sheet (`settings.rs`: collapsible cards, each change sent at once).
- `components/devices.rs` — the INDI control panel. Same header (selected device's connection badge), then the device list (a chip strip on phones, a sidebar from `md`), pills for the device's INDI groups (one group shown at a time), one card per property, and a pinned footer with the latest device message that opens them all in a `sheet`. Each card is a `<form>`: numbers/texts are buffered and Set or Enter sends the whole vector; switches apply at once (pills, a select past 6, checkboxes for NOFMANY). Numbers render INDI `%m` as sexagesimal, and an edited sexagesimal string is sent as-is (KStars runs `f_scansexa`).
- `components/profiles.rs` — the gear tab. Same header (selected profile · Ekos state · refresh), then cards — Applications (KStars/PHD2 via `/api/apps`) and Profiles, Rigs (Ekos online only) and Telescopes — one column on phones, two from `md`, and a pinned footer: New profile. A profile row has Launch/Stop (`profile_start`/`profile_stop`); its pencil, or a rig's or telescope's, opens an editor `sheet` with Delete and Save. An editor keeps its item in one `RwSignal` draft that small rows bind to through `get`/`set` fn pointers. KStars keys profiles by name, so a rename is delete + add and a taken name disables Save; `train_*` gets no reply, so the tab re-fetches.
- `components/targets/` — tonight's best targets. The observing window is a night (date) and a start → end (dusk → dawn by default, now once dark), both editable. `compute.rs` samples it every 10 min (`NightSky`: LST and the Moon) and keeps every object above the minimum altitude (`evaluate` → `Candidate`: altitude at start, peak and when, time up, Moon distance at the peak), precessing each once so a sample is one cosine; `score` is the Best score, a weighted geometric mean of altitude, time up, Moon glare (scaled by illumination and by how broadband the object is), brightness and size, its weights user-set. `filters.rs` holds `TargetPrefs` (sort, categories, bands, constellation, limits, weights — all `persisted` `targets_*`) and the filters sheet; `bands.rs` the Hα/OIII/SII/broadband chips and advice from `Dso::lines`; `detail.rs` the target sheet (thumbnail, facts, the Scheduler's `altitude_chart`, wavelengths) with Show on Sky (`SkyFocusCtx`), Framing (`FramingCtx`, then the Sky tab, where the overlay lives), Goto and Add to Scheduler (J2000, which the add-job form expects). Same header / cards / pinned footer (count, Filters) as the other planning tabs; window | list from `md`.
- `components/filter_settings.rs` + `components/filter_offsets.rs` — the shared **Filter settings** sheet, Ekos' "Filter Settings" dialog for the Capture train's wheel, opened from the header chip (current filter name, or "Filters") of Imaging, Focus and Mount and from a button in Mount › Plate solve. One card per slot (name, exposure, focus offset, autofocus on change, lock filter) edited in a local draft and saved whole with `fm_set_data`, then `fm_get_data` re-asked 500 ms later (no reply, no push); lock rules mirrored client-side (`normalize`); Move-to chips (`FILTER_SLOT`, disabled while the camera is busy); the `FlatSyncFocus` option; and the Build offsets wizard, which takes the sheet over while `FilterCtx::build_offsets_open` is on — `filter_offset_get_all_settings` (polled every 3 s by `ws/retry.rs::spawn_filter_offsets_loop` while the phase is Running), `filter_offset_set_all_settings`, `invoke_method` on `BuildFilterOffsets:<wheel>` for Run / Stop, and Save applies the computed offsets through `fm_set_data` so a filter can be left out. Everything reads `FilterCtx` (`main.rs`): the wheel snapshot, the table, the option and the wizard's state; `FilterCtx::hint` gives the "+120 · AF" caption the sequence cards and the Imaging queue show next to a filter. Focus' settings get a `focusFilter` row and Mount › Plate solve an `alignUseCurrentFilter` toggle — both only set the module's choice (KStars moves the wheel, with the offset, when it next captures); only the Imaging one-shot picker moves the wheel itself.
- `components/{coord_input,dialog_modal,form,frame_type,zoom}.rs` and `components/sequence_editor/` — shared pieces, not tabs. `form.rs` holds the touch-sized form vocabulary (`CARD`, `ROW`, `FOOTER`, `setting_row`, `check_row`, `toggle_chip`, …), `sheet` (the bottom sheet / centered panel the tabs open over themselves) and `JobOptions` — steps, start/completion and constraints — that Mosaic and Scheduler both edit and send (`settings_json`). `dialog_modal.rs` surfaces KStars' blocking `dialog_get_info` prompts. `sequence_editor/` is the card-based capture-sequence builder shared by Imaging, Mosaic and Scheduler (`model.rs` row + validation, including the per-row dither, `limits.rs` the sequence-wide refocus / HFR / guide-drift limits, `esq.rs` XML, `card.rs` one job card, `limits_panel.rs` the "Focus & guiding" section, which follows Ekos' values from `capture_get_all_settings` until edited, `cover.rs` the panel / dust cap action after the last frame); `frame_type.rs` holds the frame-type pills it shares with the Imaging one-shot panel. `zoom.rs` is the full-screen pan/zoom frame (wheel/pinch, drag, double-click reset) used by Imaging and Files.
- `skymap.rs` — the server's sky map: `SkyMapInfo` (`/api/skymap/info`, fetched once into `SkyMapCtx` in `main.rs`; `None` without a mirror), `tile_url(order, npix)` (the HiPS path) and `tile_id` (the `u32` the image caches key tiles by). `healpix.rs` — HEALPix NESTED math, no deps: `hash_dxdy` (cell + in-cell offsets), `cell_center`, `cell_vertices` ([S, E, N, W]), `cone_cells` (recursive descent from the 12 base cells), `table_base`. Correct and ported to WGSL — reuse, do not reimplement.
- `horizon.rs` — the horizon mask: the part of the sky walls and trees hide, as KStars' **Artificial Horizon** (named regions of `az alt` points, `skycomponents/artificialhorizoncomponent.h`). KStars is the store, over Ekos Live: `artificial_horizon_get` (reply: an array of `{name, enabled, ceiling, points}`), `artificial_horizon_import {data}` (KStars' text format, `Horizon <name>` then `az alt` lines; replaces the region of that name, no reply) and `artificial_horizon_toggle {region, enabled}` — all dispatched before the Ekos-start gate (`message.cpp:416`), so they only need KStars attached; there is no delete. `HorizonCtx` (`main.rs`) holds the last mask with a localStorage copy (`junos_horizon`), fetched on each `connected` edge (the relay never replays it). Visibility copies `SchedulerJob::satisfiesAltitudeConstraint`: above `min_alt` *and* above the highest enabled non-ceiling line at that azimuth (short-arc segments, linear where KStars follows a great circle; ceilings drawn, never block). Used by the sky (`render/layers/horizon_mask.rs`, Canvas2D in both modes; the layers panel's Horizon group lists the regions, New region / the pencil start a draft the sky edits — tap adds, drag moves, hold removes — and Save imports it), the Targets tab (`evaluate` keeps only visible samples; the peak is the best *visible* one), the altitude charts (`altitude_chart`'s `mask`: a strip along the night; the add-job estimate applies it when the job's `horizon` pill is on, which is the default) and a two-tap "Goto anyway?" on the Sky card, the Targets detail and the Mount tab (by RA/Dec only: a by-name Goto is resolved inside KStars).
- `astro.rs` / `coords.rs` / `ephemeris.rs` — equatorial↔horizontal math (Julian date, GMST/LST, precession to/from J2000, `fov_deg(focal, sensor_px, pixel_um)`), and ephemerides for solar-system bodies. Correct — reuse, do not reimplement.
- `catalog.rs`, `dso_catalog.rs` — async fetchers for `public/junos.bin` and `public/dso.bin` (each `Dso` carries its constellation `con` and its emission signature `lines`: Hα, OIII, SII, broadband, 0–3 each). `star_tiles.rs` parses `public/tycho.bin` (fetched lazily, see the planetarium notes) and picks the tiles under a view.
- `dom.rs` — `event_target_value` / `event_target_checked`, the only two DOM-event readers; every tab uses these rather than rolling its own.
- `i18n/` — module dir (`mod.rs` + `en.json` + `fr.json`, embedded via `include_str!`). EN default, FR selected via the `EN`/`FR` pill in the tab bar/wheel and persisted to localStorage `junos_lang`; there is no browser auto-detect. The `translations!` macro declares the schema — a key missing from one language **panics** on first use. Many unused strings; don't gratuitously prune.

`SendCmd = Arc<dyn Fn(String) + Send + Sync>` — type-erased command sink. Components dispatch raw JSON strings via `send(serde_json::json!({"type":"…","payload":{…}}).to_string())`. Do not introduce a typed command enum.

### Planetarium (`components/sky/`)

The most fully-featured surface. Treat as stable — make targeted edits when adding overlays or interactions; don't rewrite. Layout (phone-first): a top bar (search · follow-mount · layers), a bottom stack (HUD above the time bar), and two sheets that are bottom sheets on phones and floating panels on md+ (layers panel, target card), all kept `md:right-[72px]` clear of the desktop tab strip. Structure:

- `mod.rs` — `SkyTab` component, canvas/GPU setup, event loop, gestures, localStorage persistence (`sky_center_alt`, `sky_center_az`, `sky_fov_radius`, `sky_follow_mount`, plus one `persisted(key, default)` signal per render toggle and `sky_dso_mag_limit`). Also owns the `TileImageCache` (`tile_images.rs`: the LRU of decoded `<img>`s — sky-map tiles and the panorama — whose loads bump an epoch signal) handed to every `Frame`. Gestures: click/tap → Mosaic pick-on-sky if armed, else the target card for the object under the pointer (`hit_test`); right-click / 500 ms long-press → the target card at the pressed point (snapped to an object); drag pans (and only then stops following the mount); pinch/wheel zooms. Touch handlers `preventDefault`, so taps are detected in `on_touchend`, not via mouse events.
- `clock.rs` — `SkyClock`, the sky's single time source (`sim = base + elapsed × rate`): everything that projects the sky reads `clock.jd()`, never `astro::now_jd()`. Also the local-time helpers (`night_hour`: HH:MM within the noon→noon night; `on_date`) and `night_twilights` (astronomical dusk/dawn, sun −18°).
- `time_bar.rs` — the time bar: ‹ › step (1m/10m/1h/1d), ▶▶ time-lapse at one step per second, Now, date and hour pickers, dusk/dawn jumps. Amber border whenever the sky isn't live. Time is never persisted.
- `render/` — Canvas2D overlay (`mod.rs`, `layer.rs`, `params.rs`, `pipeline.rs` + one module per layer in `render/layers/`: allsky, skymap, stars, dso, grids, ground, horizon_mask, zenith, constellation_names, center_crosshair, mount_crosshair, fov_reticle, solve_marker, slew_trail, solar_system, mosaic, scheduler_jobs). GPU draw order is fixed in `gpu/mod.rs::render_inner`: clear → allsky → skymap → lines → dso symbols → constellations → stars → text. Draws grid, horizon, constellations (falls back from GPU), DSO labels, `render_center_fov()` and `render_mount_fov()` — the two FOV rectangles. Both call `astro::fov_deg` with `RenderParams.{fl, cam_pixel_size_um, cam_sensor_width, cam_sensor_height, rotation_deg, mount_ra_h, mount_dec_deg}`.
- `controls.rs` — layers panel: one `LayerChip` per toggle in `SkyToggles` (grouped Sky / Grids / Deep sky / Equipment), the DSO mag slider, and the observer location.
- `search.rs` — catalog object search (in the top bar).
- `actions.rs` — the target card (`SkyTarget`): object or sky-position info (JNow, J2000, Alt/Az at the displayed time) and five actions: Center, `mount_goto_rade`, goto-then-`align_solve`, Framing assistant (`FramingCtx`), Add to Scheduler (`SchedulerPrefillCtx`), the last two pre-filled with the object name. Reads `ServiceBusyCtx` (to disable Goto / Goto & Align while a device is busy), `SchedulerPrefillCtx` and `FramingCtx` from the crate root — these newtypes live in `main.rs` and must be provided. `MosaicPlannerCtx` (also in `main.rs`) drives the Pick-on-Sky flow that hands a center off to the Mosaic tab.
- `framing.rs` — the Framing Assistant modal (opened only from `actions.rs`, not a tab). See "Offline sky map" below. `skymap_sampler.rs` — `SkyMapSampler`, the CPU cutout renderer it and `components/sky_thumb.rs` (the Targets thumbnails) share.
- `deep_stars.rs` — the Tycho-2 layer. The star limit follows the zoom (`star_mag_limit` in `mod.rs`: the naked-eye sky when wide, 4 mag per tenfold zoom below 20°, capped at 12.5). Past `DEEP_FROM_MAG` the sky fetches `tycho.bin` once, and `DeepStarField` keeps the tiles within 1.5 × the field radius — uploaded to the GPU only when that tile set changes (`SkyRenderer::upload_deep_stars`: the base star pipelines with their own buffers), and drawn by `render_fallback_stars` in Canvas2D mode. Star size and brightness follow the magnitude relative to the limit, in `render.wgsl` and the fallback alike.
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
10. **An `.esq` head overwrites Ekos' Capture limits.** Whenever Capture loads a sequence (each Scheduler job start, `capture_load_sequence_file`), `SequenceQueue::setOptions` copies its `<RefocusEveryN>`, `<RefocusOnTemperatureDelta>`, `<RefocusOnMeridianFlip>`, `<HFRCheck>`, `<GuideDeviation>` and `<GuideStartDeviation>` into KStars' global options, where they stay. Leaving an element out doesn't keep Ekos' value: the `m_*Set` flags are never reset, so the last loaded file's value is applied again. `build_esq_xml` therefore always writes all of them from `SeqLimits`. Dithering is per row (`<GuideDitherPerJob>`: 0 = the Guide module's frequency, N, −1 = never); the Guide module's "Enable dithering" stays the master switch.
11. **A sequence ending on calibration frames leaves the panel lit and the cap closed.** KStars lights the panel and parks the cap before flats, and undoes both only before the next light frame (`SequenceJobState::checkLightFrameScopeCoverOpen`); the end-of-capture "light off" in `CameraProcess::stopCapturing` checks a flag nothing sets. `sequence_editor/cover.rs` fills the gap with a `<PostJobScript>` on the last row: a shell script, written through `/api/taskqueue/script` (KStars runs it with no shell or arguments, JSON on stdin), that calls KStars' D-Bus API (`getDevicesPaths`, `LightBox.setLightEnabled`, `DustCap.unpark`) — so it needs no device names and no INDI host.
12. **The Filter Settings table is request/reply only, and a write is all-or-nothing.** `fm_get_data` answers under its own type with the Capture *main camera's* FilterManager table (`message.cpp:1806`, `filtermanager.cpp:952`); `FilterManager::updated()` reaches nothing, so the client asks at bootstrap, in the 20 s sweep and after each write. `fm_set_data` has no reply, is ignored unless `device` matches, overwrites every column of every row it gets (a missing `lastafdt` becomes the literal `"yyyy-MM-dd hh:mm:ss"`), crashes KStars on an out-of-range `index`, and rewrites INDI `FILTER_NAME` — always round-trip whole rows. Build Filter Offsets (`filter_offset_*`) addresses the *alphabetically first* wheel (`manager.cpp:4420`), accepts only `adaptFocus`, `refFilter` and positional `numFocusRuns`, runs through `invoke_method` on the headless `BuildFilterOffsets:<wheel>` (never `showDialog`, it is modal), re-initialises only from the desktop dialog (one clean run per session) and asks a failed autofocus's Stop/Retry on the desktop (`KMessageBox`, not relayed). `components/filter_offsets.rs` states all three.

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

## Offline sky map (planetarium imagery, Framing Assistant, Targets thumbnails)

The sky's imagery is a **local mirror of the CDS `P/DSS2/color` HiPS**: the
whole sky in HEALPix **nested** cells, one 512 × 512 JPEG per cell, orders 0
(12 tiles, ~58°) to 9 (3.1 M tiles, ~0.8″/px); `hips_frame=equatorial`, i.e.
ICRS/J2000. The tree is kept in CDS's exact layout — `properties`, then
`Norder{k}/Dir{(n / 10000) * 10000}/Npix{n}.jpg`, `12·4^k` tiles per order —
so `rsync`, `wget -r` and any HiPS client understand it. Sizes: order 6 ≈
3.3 GB, 7 ≈ 13–20 GB (the default), 8 ≈ 53 GB, 9 ≈ 210 GB; the fine orders
are meant for `--cone` runs around chosen targets, not the full sky. The
browser is remote (a phone on wifi): it computes the cells under its view at
the order matching its zoom and streams **only those tiles** (≤ ~25 per view,
parents first), which the browser keeps for good (`immutable`). Generate with:

```bash
uv run scripts/fetch_skymap.py --max-order 3      # smoke: 1,020 tiles, ~25 MB
uv run scripts/fetch_skymap.py                    # orders 0..7
uv run scripts/fetch_skymap.py --status           # per-order coverage, max_order, panorama
uv run scripts/fetch_skymap.py --allsky           # Milky Way panorama (allsky.jpg + allsky_small.jpg)
uv run scripts/fetch_skymap.py --min-order 8 --max-order 9 --cone 83.82 -5.39 3
uv run scripts/fetch_skymap.py --prune-above 7 | --info-only | --selftest
```

Output goes to `.cache/skymap/` (gitignored — **not** `junos-web/public/`).
`--out` or `SKYMAP_DIR` (the server reads the same variable) moves it; note
`--skymap-dir` is the *server's* flag. Resumable (existing tiles > 1 KB are
skipped, `.part` + `os.replace`, Ctrl-C still writes `info.json`); tiles are
static files on CDS's Apache (no per-request pause needed; `--pace 0.2
--workers 4` is the gentle setting; a 404 is final, 429/5xx back off and
rotate mirrors). The script writes **`info.json`** (also every 60 s during a
run), which the server serves verbatim at `/api/skymap/info`:

```json
{"survey":"CDS/P/DSS2/color","title":"DSS colored","frame":"equatorial",
 "tile_width":512,"tile_format":"jpg","survey_max_order":9,"max_order":7,
 "orders":{"0":12,"1":48,"…":0,"8":1204},"bytes":13968123456,"allsky":true,
 "attribution":"Digitized Sky Survey - STScI/NASA, Colored & Healpixed by CDS",
 "attribution_url":"http://archive.stsci.edu/dss/copyright.html","updated":"2026-10-25T10:12:33Z"}
```

`max_order` is the highest order such that every order up to it holds ≥ 99 %
of its tiles — the zoom the client may rely on everywhere (`null` when even
order 0 is incomplete); partial finer orders stay listed in `orders` so a
client may try them and tolerate a 404. Without `info.json` the server
derives the same shape from a startup scan. The mirror is **optional**:
`max_order: null` → the sky draws symbols over the Milky Way panorama alone
and the Framing Assistant previews the grid over black. `--skymap-online`
fills missing tiles from CDS on demand.

**HEALPix helpers live in three places** and must agree bit for bit: Python
(`scripts/fetch_skymap.py`: `ang2pix_nest`, `pix2ang_nest`, `cone_pixels`,
only for `--cone`), Rust (`junos-web/src/healpix.rs`) and WGSL
(`components/sky/shaders/skymap.wgsl`, per pixel). All three are the
healpix_cxx formulas (nested scheme, `phi = RA`, `z = sin Dec`, Morton bit
spread of `(ix, iy)` under `face << 2k`). `--selftest` round-trips every cell
of orders 0–4 and prints the shared vectors the Rust tests assert:
`pix2ang_nest(3, 0) = (45.000000, 4.780192)`, `(3, 767) = (315.000000,
−4.780192)`, `(7, 196607) = (315.000000, −0.298417)`, `(0, 4) = (0, 0)`,
`(1, 47) = (315.000000, −19.471221)`; `ang2pix_nest(3, 83.82, −5.39) = 334`,
`(7, 10.68, 41.27) = 10836`, `(5, 0.0, −89.5) = 8192`.

**Milky Way panorama.** `--allsky` fetches one plate-carrée J2000 panorama
(`CDS/P/Mellinger/color`, via hips2fits, centred on RA 0h, north up, **east
left**: `u = 0.5 − RA/360`), 4096 × 2048 plus a 2048 px `allsky_small.jpg`
for phones / Lite, served at `/api/skymap/allsky*.jpg`. It stays the
wide-field backdrop under the DSS2 layer.

**Client side.** `main.rs` fetches `/api/skymap/info` once into `SkyMapCtx`.
The sky draws the map in a **full-screen pass** (`gpu/layers/skymap.rs` +
`shaders/skymap.wgsl`, after the panorama): per pixel, the shared inverse
projection (`shaders/sky_inverse.wgsl`, concatenated in Rust in front of
both `allsky.wgsl` and `skymap.wgsl` — WGSL has no includes) gives J2000,
`hpx_hash_dxdy` the cell at the view's order, a storage-buffer table
(`slot + 1` per cell, orders 0..=8 flattened at `table_base(o) + hash`,
4 MiB) the resident layer of a 512² `texture_2d_array` atlas (64 layers on
desktop, 40 on phones), and a missing tile falls back to its parent cell
(shift the hash, fold the quadrant bits into dx/dy) down to three orders up —
so a zoom never shows a hole. `render/layers/skymap.rs` is the CPU side:
order = round(log2(58.63° · px/° / 512)) with hysteresis (−0.5 on phones),
cells = `healpix::cone_cells` over the canvas diagonal, kept when the
bounding box of their projected vertices meets the canvas — partially
visible cells included; `project_far_with`, clipped at 90° rather than
1.5 × the radius, since a 16:9 corner lies 2 × out — (≤ 48 / 32) plus
their grandparents (≤ 8 / 6) requested **first**, images through the
`TileImageCache` (6 concurrent loads), ≤ 4 atlas uploads per frame (the
sky stays at full frame rate while any decoded tile waits)
(`MipUploader`, whose `copyExternalImageToTexture` **panics the wasm on a
validation error** — keep its preconditions). The layer crossfades in over
the panorama between 25° and 10° of view radius (`crossfade`), so wide
fields request no tiles at all; brightness is a slider; Lite caps the order
at `max_order − 2`. While it shows, `Frame::image_min_half_px` makes the DSO
layers skip the outline of objects ≥ 10 px (the picture shows them).
Canvas2D fallback: each decoded tile is drawn as affine quads over its
cell (S, W, E vertices; `source-over`, ≤ 40 tiles), a cell wider than 8°
split into its 4ⁿ sub-cells with the matching image quadrants, so coarse
parents land on the right stars too; a sub-quad off the canvas is skipped. **HiPS orientation**: dx
runs from the cell's south vertex along S→E, dy along S→W, and the JPEG
stores column ← dy, row ← dx (top-left = S, top-right = W, bottom-left = E,
bottom-right = N) — one `uv = vec2(dy, dx)` line in the shader and one
matching line in the sampler; verify against the Tycho-2 stars at ~1°
fields (only one of the eight orientations aligns) after touching either.

The Framing Assistant and the Targets thumbnails reproject on the CPU
(`components/sky/skymap_sampler.rs`): a `CutoutSpec` (J2000 centre, field
across the shorter side, size, rotation; north up / east left at 0, i.e.
`astro::frame_point_eq`) is filled pixel by pixel — inverse TAN →
`hash_dxdy` → the tile's texel from pixels extracted once per tile
(`getImageData`, LRU) — with the same parent fallback, at a bounded
internal size (0.5 Mpx desktop, 0.25 phones) scaled onto the canvas.
`framing.rs`'s `Viewport` is the same TAN plane, so the mosaic grid sits on
the same stars as the image; its label reads "DSS2 · order k · N tiles".
`components/sky_thumb.rs` is a `<canvas>` rendered through the tab's shared
sampler until every tile under the field is in; the Targets rows (128 px at
48 CSS px) and the detail sheet (512 × 288) use it, the kind tag standing in
without a map.

## Static assets

`junos-web/public/` contains three binary catalogs: `junos.bin` (stars to V ~10), `tycho.bin` (the rest of Tycho-2, 2.2 M stars, 13 MB) and `dso.bin` (deep-sky). Trunk copies them into `dist/`. They are checked in — do not regenerate or re-encode them as part of code changes. Each is fetched with a `?v=N` (`fetch_catalog`, `fetch_deep_stars`, `fetch_dso_catalog`) that the server marks immutable and the browser keeps in the Cache API, so **any change to a catalog's bytes must bump its `?v=`** or browsers keep the old file. A `dso.bin` layout change touches two readers (`dso_catalog.rs`, the generator); it is at `v=3` (kind, then `lines` u8 and `con` 3 × ASCII). Most nebulae have no magnitude (99): the sky culls by `Dso::vis_mag`, a size-based stand-in, never by `mag`.

## Code style observed in this codebase

- French and English comments coexist; mirror the surrounding file.
- Commands are dispatched as raw JSON strings; do not introduce a typed command enum.
- Arc-clone `SendCmd` aggressively before moving it into closures.
- Both crates build warning-free; keep it that way. Where a struct mirrors a wire shape (`ws/types.rs`, `catalog.rs`, `files/types.rs`) or declares strings ahead of the UI (`i18n`), it carries one `#[allow(dead_code)]` and a comment saying why — annotate rather than prune those.
- Debug traces go through `debug_log!`, not `leptos::logging::log!` directly, so they stay out of release builds.
- Tab components should take only the specific signals they need plus `SendCmd`, never `DeviceStore` whole. Use Leptos context only for values that need to cross many components (see `*Ctx` newtypes in `main.rs`).
