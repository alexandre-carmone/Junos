# junos-web

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
   │     junos-server    │   Axum + Tokio relay
   │  (Rust, native)     │   serves the WASM frontend at /
   └─────────────────────┘
         ▲
         │  ws://localhost:8080/ws
         │
   Browser ── junos-web (Leptos + WebGPU planetarium)
```

- **`junos-server`** — a tiny Axum relay. KStars connects *into* it as if it
  were `ekoslive.com`. Browsers connect to `/ws` and exchange raw Ekos Live
  JSON messages with KStars. The server also serves the compiled WASM
  frontend from `junos-web/dist/`.
- **`junos-web`** — the browser app. Leptos 0.7 CSR with a WebGPU sky view.
  Speaks the Ekos Live wire format directly (`{type, payload}` JSON).

The server does **no protocol translation** — messages flow through opaque.
All Ekos Live semantics live in the WASM client.

## Repository layout

- **`junos-server/`** — Axum/Tokio relay (Rust, native).
- **`junos-web/`** — Leptos 0.7 CSR + WebGPU browser app.
- **`deprecated-junos/`** — old prototype Leptos crate kept for
  reference only; not part of the Cargo workspace.
- **`kstars/`** — read-only checkout of the upstream KStars C++ source,
  kept as the authoritative reference for the Ekos Live wire format.

## Install

One-time setup (requires `rustup` and `cargo` already on PATH):

```bash
just install
```

This adds the `wasm32-unknown-unknown` Rust target and installs `trunk`.

Or, with Nix: `nix develop` (the repo ships a `flake.nix` dev shell with
the toolchain pre-pinned).

## Build & run

```bash
just              # release build (wasm + server) then run
just build        # release build only, no run
just check        # fast typecheck of both crates
just dev-wasm     # `trunk watch` for the frontend
just dev-server   # `cargo run -p junos-server`
just clean        # cargo clean + rm junos-web/dist
```

> ⚠️ **iPhone / iPad users — read this first.**
> WebGPU on iOS Safari is gated behind two requirements that will silently
> break the planetarium if you skip them:
>
> 1. **You must connect over HTTPS** (`https://<lan-ip>:8443`). Safari only
>    exposes `navigator.gpu` in secure contexts — plain `http://…:8080`
>    will load the UI but the sky view stays blank.
> 2. **The WebGPU feature flag must be enabled** on iOS 18+:
>    Settings → Apps → Safari → Advanced → **Feature Flags → WebGPU**.
> 3. **The self-signed dev cert must be trusted** (see the iPhone steps
>    below) — without trust, Safari refuses the WebSocket upgrade and the
>    UI never connects to the relay.
>
> Desktop Chrome/Firefox/Safari on the LAN can use either port; only
> iOS strictly needs the HTTPS one.

## Transports

The server binds two ports by default, configured via two separate flags:

- `--http-addr` (default `0.0.0.0:8080`) — for KStars's Ekos Live
  connection. Plain HTTP keeps KStars's Qt websocket simple; the link
  stays on your LAN.
- `--https-addr` (default `0.0.0.0:8443`) — for the browser UI. iOS
  Safari (and most modern browsers in the long run) only expose
  `navigator.gpu` in secure contexts, so the WebGPU planetarium needs
  HTTPS even on a LAN.

A self-signed cert is generated on first run into `.certs/cert.pem` +
`.certs/key.pem`, covering `localhost`, `127.0.0.1`, and the host's
non-loopback IPv4 addresses. Subsequent runs reuse the same cert so trust
on the iPhone survives restarts. Drop your own cert in via `--tls-cert` /
`--tls-key` (or env vars). Pass `--no-https` to disable TLS entirely.

The `Files` tab is backed by `--captures-dir` (env `CAPTURES_DIR`),
which sandboxes the `/api/files/*` browser. If unset, the server falls
back to `$HOME/Pictures`, then to the current working directory.

To trust the dev cert on iPhone/iPad:

1. Visit `https://<lan-ip>:8443` from Safari, accept the warning.
2. Open `.certs/cert.pem` on the device (AirDrop is easiest) and let
   iOS prompt — Settings → **Profile Downloaded → Install**.
3. Settings → General → About → **Certificate Trust Settings** → enable
   the `junos-dev` certificate.
4. Safari → Settings → Apps → Safari → Advanced → **Feature Flags → WebGPU**
   (iOS 18+).

## Using it with KStars

1. Start KStars and open **Ekos**.
2. In Ekos Live settings, point the **offline server** at
   `http://localhost:8080`.
3. Start your equipment profile (simulators are fine for testing).
4. Open `https://localhost:8443` in your browser.
5. The top status strip should flip to **Ekos online** and the
   mount-anchored FOV reticle should appear on the sky view.

## Frontend tabs

`junos-web` is more than a planetarium. The tab wheel exposes:

- **Sky** — WebGPU planetarium with stars, DSOs, nebulae thumbnails,
  constellation lines, mount-anchored FOV reticle, search, and a
  right-click *Goto / Plate-solve* menu.
- **Mount**, **Focus**, **Guide**, **Imaging**, **Mosaic**,
  **PolarAlign**, **Scheduler** — Ekos module surfaces, progressively
  reintroduced from the deprecated upstream web client.
- **Files** — browser for captured frames under `--captures-dir`
  (defaults to `$HOME/Pictures`).
- **Profiles** — Ekos equipment profile selector / launcher.

## Credits

This project would not exist without the work of several upstream
projects. In particular:

- **[KStars / Ekos](https://kstars.kde.org/)** (KDE, GPL-2.0-or-later) —
  junos-web speaks the Ekos Live wire format directly. The `kstars/`
  directory in this repo is a read-only checkout of the upstream KStars
  source kept as the authoritative protocol reference. All Ekos session
  logic, INDI device management, and plate-solving is performed by
  KStars itself; junos-web is only a relay and a UI.

- **[Stellarium](https://stellarium.org/)** (Stellarium team,
  GPL-2.0-or-later) — the planetarium ships imagery and data sourced
  from the Stellarium GitHub repository:
  - **Nebulae thumbnails** in `junos-web/public/nebulae/` are derived
    from Stellarium's `nebulae/default/` texture set
    (see `scripts/download_nebulae.py`).
  - **Constellation stick figures** are built from Stellarium's
    `skycultures/modern_st` sky culture
    (see `scripts/gen_catalog.py`).

  Stellarium is licensed under the GNU General Public License v2 or
  later. The redistributed assets remain under that license; see
  <https://github.com/Stellarium/stellarium> for the upstream source
  and full license text.

- **Hipparcos / Tycho** catalogs and the OpenNGC deep-sky catalog feed
  the binary catalogs in `junos-web/public/`. Regeneration scripts
  live in `scripts/` (run with `uv run`).
