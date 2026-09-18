# Junos

A local, browser-based client for [KStars/Ekos](https://kstars.kde.org/).

Junos runs **alongside KStars on your own network** and acts as a transparent
relay between KStars and your browser. KStars thinks it is talking to
`ekoslive.com`; in reality the traffic never leaves your LAN. Nothing is sent
to the cloud, and no account is required.

Two crates make it up:

- **`junos-server`** — the relay. KStars connects *into* it; browsers connect
  to it; it also serves the compiled frontend.
- **`junos-web`** — the browser app. A Leptos + WebGPU UI that speaks the
  Ekos Live wire format directly.

## Features

The UI is a 12-tab shell (a wheel on phones, a side strip on desktop), in this
order:

- **Profiles** — create, edit, start and stop Ekos equipment profiles,
  optical trains and scopes. Works before Ekos is online, so this is where a
  session begins.
- **Sky** — a WebGPU planetarium: stars, deep-sky objects, constellations,
  grids, solar-system bodies, and a mount-anchored FOV reticle. Drag to pan,
  scroll or pinch to zoom. Right-click anywhere for **Goto**,
  **Goto & Align**, **Add to Scheduler** or the **Framing Assistant**.
- **Mount** — coordinates in JNow and J2000, goto/sync, park, tracking,
  slew rate, meridian flip, and plate solving.
- **Focus** — autofocus with a live HFR v-curve, manual stepping, the full
  settings pane, and an **Aberration Inspector** for tilt and field curvature.
- **Imaging** — camera and cooling status, all exposure settings, a capture
  **sequence queue**, and a live preview with pan and pinch-zoom.
- **Files** — browse the captures folder: thumbnails, FITS headers, rename
  and delete, solve-and-slew-to-this-framing, and a **LiveStacker**.
- **Polar Align** — the Ekos polar alignment assistant, with azimuth and
  altitude error readout and correction guidance.
- **Guide** — start/stop guiding on any backend (Internal, PHD2, LinGuider),
  with a drift timeline, a target scatter plot, and the full settings pane.
- **Scheduler** — the job queue, plus a visual job builder that writes the
  `.esq` sequence file for you.
- **Mosaic** — plan a mosaic (grid, overlap, position angle) and **Send to
  Scheduler** to import every tile as a job. Its center can be picked on the
  sky map.
- **Flat Cal** — dust cap, flat panel, and the ADU optimizer.
- **Devices** — a full **INDI control panel in the browser**: every property
  of every connected device, plus the INDI message log.

The interface is available in **English and French** (toggle in the tab
wheel/strip).

## Install

Junos is Linux-only and builds natively for **x86_64** and **aarch64**
(Raspberry Pi). Prebuilt packages are attached to each
[GitHub Release](https://github.com/alexandre-carmone/Junos/releases).

### Arch Linux (including Arch Linux ARM)

The updater script fetches the right package for your architecture from the
Releases page and installs it — no local build:

```bash
./packaging/arch/update.sh          # install / update
sudo systemctl enable --now junos-web
```

Install it as a system command if you want to re-run it later:

```bash
sudo install -Dm755 packaging/arch/update.sh /usr/local/bin/junos-web-update
```

See [`packaging/arch/README.md`](packaging/arch/README.md) for building the
package locally with `makepkg`, and for a nightly auto-update timer.

### Portable tarball

Download `junos-web-<version>-<arch>-linux.tar.gz` from the Releases page,
unpack it, and run:

```bash
./junos-server --http-addr 0.0.0.0:8090 \
               --https-addr 0.0.0.0:8443 \
               --dist-dir ./dist
```

The tarball also ships a sample systemd unit. See
[`packaging/portable/README.txt`](packaging/portable/README.txt).

### Docker

```bash
docker build -t junos .
docker run -p 8080:8080 -p 8443:8443 junos
```

Note this image is meant for a machine on your own LAN, not for publishing:
it is single-stage (so it carries the whole Rust toolchain) and it bakes a
self-signed certificate in at build time, which means every container built
from it shares the same key.

### NixOS

The flake exposes a module:

```nix
{
  inputs.junos.url = "github:alexandre-carmone/Junos";
  # ...
  imports = [ junos.nixosModules.default ];
  services.junos-web.enable = true;
}
```

The module's `httpAddr` and `httpsAddr` default to **`127.0.0.1`** (unlike the
bare binary, which binds `0.0.0.0`), so for LAN access set both explicitly in
addition to `openFirewall = true`. `nix run github:alexandre-carmone/Junos`
starts the server without installing anything.

### From source

Requires `rustup` and `cargo` on your PATH.

```bash
just install     # adds the wasm32-unknown-unknown target, installs trunk,
                 # and downloads the Tailwind CLI into junos-web/bin/
just             # release build (frontend + server), then run
```

Or with Nix: `nix develop` gives you a shell with the whole toolchain pinned.

Other recipes:

```bash
just build        # release build only, no run
just check        # fast typecheck of both crates
just dev-wasm     # `trunk watch` for the frontend
just dev-server   # `cargo run -p junos-server`
just gen-cert     # regenerate the self-signed TLS cert in .certs/
just arch-pkg     # build the Arch package locally (amd64 | arm64)
just clean        # cargo clean + rm junos-web/dist
```

## Using it with KStars

1. Start KStars and open **Ekos**.
2. In the Ekos Live settings, point the **offline server** at your Junos host:
   - built from source (or `nix run`): `http://<host>:8080`
   - installed from the Arch package or run with the tarball command above:
     **`http://<host>:8090`**

   The packaged service uses 8090 because port 8080 is commonly already taken.
   If the browser UI loads but never reports `Ekos online`, this is almost
   always the wrong port.
3. Start your equipment profile — the built-in simulators are fine for
   testing.
4. Open `https://<host>:8443` in a browser and accept the self-signed
   certificate.
5. Click **Start** in Ekos. The top status strip should flip to
   **Ekos online**, and the mount-anchored FOV reticle should appear on the
   sky view.

## Browser access & TLS

The server listens on two ports: plain HTTP for KStars, and HTTPS for
browsers. The browser side needs TLS because **WebGPU is only exposed in
secure contexts** — over plain HTTP the UI loads but the sky view stays
blank.

A self-signed certificate is generated into `.certs/cert.pem` and
`.certs/key.pem` (relative to the working directory) on first run, covering
`localhost`, `127.0.0.1`, and every non-loopback IPv4 address of the host.
Later runs reuse it, so trust established on a phone survives restarts.
Supply your own with `--tls-cert` and `--tls-key` (both together), or disable
TLS entirely with `--no-https` for a headless run.

> ### iPhone / iPad — read this first
>
> WebGPU on iOS Safari is gated behind three things, and skipping any of them
> silently breaks the planetarium:
>
> 1. **Connect over HTTPS** — `https://<lan-ip>:8443`. Safari exposes
>    `navigator.gpu` only in secure contexts.
> 2. **Enable the WebGPU feature flag** (iOS 18+): Settings → Apps → Safari
>    → Advanced → **Feature Flags → WebGPU**.
> 3. **Trust the certificate** — without trust, Safari refuses the WebSocket
>    upgrade and the UI never connects to the relay.
>
> To trust it:
>
> 1. Visit `https://<lan-ip>:8443` in Safari and accept the warning.
> 2. Get `.certs/cert.pem` onto the device (AirDrop is easiest) and let iOS
>    prompt you — Settings → **Profile Downloaded → Install**.
> 3. Settings → General → About → **Certificate Trust Settings** → enable the
>    Junos certificate. It appears as `junos-dev` when the server generated
>    it, or `junos-web` if it came from `just gen-cert`, Docker, or the NixOS
>    module.
>
> Desktop Chrome, Firefox and Safari on the LAN can use either port; only iOS
> strictly requires the HTTPS one.

## Configuration

Every option is a long flag with a matching environment variable.

| Flag | Env | Default | Purpose |
| --- | --- | --- | --- |
| `--http-addr` | `HTTP_ADDR` | `0.0.0.0:8080` | KStars-facing listener |
| `--https-addr` | `HTTPS_ADDR` | `0.0.0.0:8443` | Browser-facing TLS listener |
| `--no-https` | `NO_HTTPS` | off | Disable TLS entirely |
| `--tls-cert` | `TLS_CERT` | auto-generated | Your own certificate |
| `--tls-key` | `TLS_KEY` | auto-generated | Your own private key |
| `--dist-dir` | `DIST_DIR` | `junos-web/dist` | Where the compiled frontend lives |
| `--captures-dir` | `CAPTURES_DIR` | see below | Root of the Files tab |
| `--dso-tile-dir` | `DSO_TILE_DIR` | `.cache/dso_tiles` | Offline DSO tile cache |

Notes:

- `--tls-cert` and `--tls-key` must be given **together**; supplying only one
  is a fatal error.
- `--captures-dir` sandboxes the `/api/files/*` browser. If unset, Junos uses
  `$HOME/Pictures` when that directory already exists, and otherwise the
  current working directory. The resolved directory is created on startup if
  it is missing.
- Both listeners serve the same routes. The HTTP/HTTPS split is a convention
  for who connects where, not a restriction.

## Framing Assistant & offline DSO tiles

The **Framing Assistant** plans a mosaic against a real image of the sky. It
is not a tab: open it from the Sky tab by right-clicking (or long-pressing)
a point and choosing **Framing assistant**. It shows your camera's tile grid
over a survey preview of that region, lets you adjust the center, grid size,
overlap and position angle, and then hands the plan to the Mosaic planner
with **Send to Mosaic planner**.

The preview is served **entirely from a local tile cache** — it never touches
the network at runtime. The cache is **optional**: without it the Framing
Assistant still works, and an uncovered region simply previews as the tile
grid over black.

To build it:

```bash
uv run scripts/prefetch_dso_tiles.py --limit 50   # smoke test first
uv run scripts/prefetch_dso_tiles.py              # ~7960 objects, takes hours
uv run scripts/prefetch_dso_tiles.py --status     # coverage report, no downloads
```

Tiles are downloaded from the CDS `hips2fits` service, one cutout per catalog
object, into `.cache/dso_tiles/` (gitignored — this one is **not** checked in,
unlike the star and DSO catalogs). Expect around **10 GB** at the current tile
resolution. The download is resumable, so you can interrupt and re-run it;
use `--out` (or the `DSO_TILE_DIR` environment variable) to put the cache
somewhere else, and `--force` to refetch existing tiles.

Point the server at it with `--dso-tile-dir` if you moved it.

## How it works

```
   KStars (Ekos Live "offline server")
         │  ws://<host>:8080/message/ekos
         │  ws://<host>:8080/media/ekos
         ▼
   ┌─────────────────────┐
   │     junos-server    │   Axum + Tokio relay
   │  (Rust, native)     │   also serves the frontend at /
   └─────────────────────┘
         ▲
         │  wss://<host>:8443/ws
         │
   Browser ── junos-web (Leptos + WebGPU)
```

KStars connects *inbound* to the relay as if it were `ekoslive.com`. A
broadcast channel fans every KStars event out to all connected browsers, and
browser commands are forwarded back to the attached KStars session.

The server does **no protocol translation** — messages flow through opaque.
All Ekos Live semantics live in the WASM client. Beyond the relay, the server
adds a handful of local HTTP APIs the browser cannot do on its own: the
captures-folder browser (`/api/files/*`), FITS thumbnailing and star/tilt
analysis, the offline DSO tile cache (`/api/dso_tiles/*`), and launching
KStars or PHD2 on the host (`/api/apps/*`).

## Repository layout

- **`junos-server/`** — the Axum/Tokio relay and local HTTP APIs.
- **`junos-web/`** — the Leptos + WebGPU browser app. Binary star and
  deep-sky catalogs live in `junos-web/public/`.
- **`scripts/`** — `gen_dso_catalog.py` builds the deep-sky catalog;
  `prefetch_dso_tiles.py` downloads the offline DSO tile cache.
- **`packaging/`** — Arch Linux package, portable tarball, and CI notes.
- **`nix/`**, **`flake.nix`** — dev shell, packages, and the NixOS module.
- **`justfile`**, **`Dockerfile`** — build entry points.
- **`kstars/`** — a read-only checkout of the upstream KStars C++ source,
  kept as the authoritative reference for the Ekos Live wire format. Not
  part of the build.

## Packaging & CI

- [`.github/workflows/ci.yml`](.github/workflows/ci.yml) typechecks both
  crates and runs the server tests on every push and pull request
  (`just check` plus `just test`).
- [`.github/workflows/release.yml`](.github/workflows/release.yml) builds the
  portable tarball and the Arch package for x86_64 and aarch64 — natively on
  both, no emulation — and attaches them to a GitHub Release. Trigger it by
  pushing a version tag:

  ```bash
  git tag 0.1.2 && git push origin 0.1.2
  ```

See [`packaging/README.cicd.md`](packaging/README.cicd.md) for details.

## License

Junos is licensed under the **GNU General Public License v3.0 or later**. See
[`LICENSE`](LICENSE).

The project also redistributes third-party data under **GPL-2.0-or-later**:
the Stellarium-derived nebula textures and constellation figures described in
Credits below. Upstream KStars/Ekos, whose protocol Junos speaks, is likewise
GPL-2.0-or-later.

## Credits

This project would not exist without the work of several upstream
projects. In particular:

- **[KStars / Ekos](https://kstars.kde.org/)** (KDE, GPL-2.0-or-later) —
  Junos speaks the Ekos Live wire format directly. The `kstars/`
  directory in this repo is a read-only checkout of the upstream KStars
  source kept as the authoritative protocol reference. All Ekos session
  logic, INDI device management, and plate-solving is performed by
  KStars itself; Junos is only a relay and a UI.

- **[Stellarium](https://stellarium.org/)** (Stellarium team,
  GPL-2.0-or-later) — the **constellation stick figures** baked into
  `junos-web/public/junos.bin` are built from Stellarium's
  `skycultures/modern_st` sky culture.

  Stellarium is licensed under the GNU General Public License v2 or
  later. The redistributed assets remain under that license; see
  <https://github.com/Stellarium/stellarium> for the upstream source
  and full license text.

- **Hipparcos / Tycho / AT-HYG** star catalogs and the **OpenNGC** deep-sky
  catalog feed the binary catalogs in `junos-web/public/`. Deep-sky preview
  tiles are cutouts from the **CDS hips2fits** service.
