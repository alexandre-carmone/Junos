# Arch Linux package (`junos-web`)

Builds an installable package for **x86_64** and **aarch64** (Arch Linux ARM on
Raspberry Pi). Ships the `junos-server` binary, the compiled WASM frontend, and a
systemd service.

## Build & install

```bash
cd packaging/arch
makepkg -si          # build + install (pulls makedepends automatically)
```

`makepkg` fetches the source tarball for tag `v$pkgver` from GitHub. For a local
test build against an unreleased tree, create the tarball yourself and point the
`source=()` at it (or drop a `junos-web-<ver>.tar.gz` next to the PKGBUILD and set
`sha256sums`).

Build dependencies are `rustup` + `trunk` (both packaged on `x86_64` and
`aarch64`). `makepkg -s` installs them automatically. The toolchain itself is
provisioned by `rustup` inside a hermetic `$srcdir` (stable + the
`wasm32-unknown-unknown` target) — Arch Linux ARM has no `rust-wasm` package, so
relying on `rustup` is what makes the aarch64 build work. The Tailwind CLI is not
packaged on ALARM either, so the PKGBUILD downloads the arch-correct standalone
binary (checksummed) into `junos-web/bin/`. The build reaches the network (rustup
toolchain, cargo crates, and `trunk` fetching `wasm-bindgen-cli`/`wasm-opt`).

## Run

```bash
sudo systemctl enable --now junos-web
```

- Browser UI: `https://<host>:8443` (self-signed cert — accept it in the browser;
  iOS Safari needs TLS for WebGPU).
- Point KStars' Ekos Live "offline server" at `http://<host>:8090`
  (**8090**, not the upstream default 8080, which is already taken on this host).

The service runs under `DynamicUser` with `WorkingDirectory=/var/lib/junos-web`;
the self-signed TLS cert is generated into `/var/lib/junos-web/.certs/` on first
start.

### LAN access / firewall

Both ports bind `0.0.0.0`. If a firewall is active, open them:

```bash
# firewalld example
sudo firewall-cmd --add-port=8090/tcp --add-port=8443/tcp --permanent
sudo firewall-cmd --reload
```

### Files tab root

By default the Files tab is sandboxed to the working dir (`/var/lib/junos-web`).
To expose a captures folder, add a drop-in:

```bash
sudo systemctl edit junos-web
```

```ini
[Service]
ExecStart=
ExecStart=/usr/bin/junos-server --http-addr 0.0.0.0:8090 --https-addr 0.0.0.0:8443 --dist-dir /usr/share/junos-web/dist --captures-dir /srv/astro/captures
ReadWritePaths=/srv/astro/captures
```
