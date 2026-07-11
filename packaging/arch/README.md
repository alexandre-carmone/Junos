# Arch Linux package (`junos-web`)

Builds an installable package for **x86_64** and **aarch64** (Arch Linux ARM on
Raspberry Pi). Ships the `junos-server` binary, the compiled WASM frontend, and a
systemd service.

## Build & install

```bash
cd packaging/arch
makepkg -si          # build + install (pulls makedepends automatically)
```

## Update from GitHub Releases

No local build needed on the target host — `update.sh` fetches the latest
release package for the running architecture (`x86_64` or `aarch64`) and
installs it with `pacman -U`:

```bash
./update.sh              # update if a newer release exists
./update.sh --force      # reinstall even if already current
```

Install it as a short system command:

```bash
sudo install -Dm755 update.sh /usr/local/bin/junos-web-update
junos-web-update
```

### Nightly auto-update (optional)

Run the updater on a timer. Create the unit + timer, then enable:

```bash
sudo tee /etc/systemd/system/junos-web-update.service >/dev/null <<'EOF'
[Unit]
Description=Update junos-web from GitHub Releases
Wants=network-online.target
After=network-online.target

[Service]
Type=oneshot
ExecStart=/usr/local/bin/junos-web-update
# pacman -U here restarts nothing; bounce the service if it updated:
ExecStartPost=-/usr/bin/systemctl try-restart junos-web.service
EOF

sudo tee /etc/systemd/system/junos-web-update.timer >/dev/null <<'EOF'
[Unit]
Description=Nightly junos-web update check

[Timer]
OnCalendar=daily
Persistent=true
RandomizedDelaySec=30m

[Install]
WantedBy=timers.target
EOF

sudo systemctl daemon-reload
sudo systemctl enable --now junos-web-update.timer
```

`update.sh` runs `sudo pacman -U`; for an unattended timer either run the
service as root (as above — no `sudo` prompt) or add a NOPASSWD sudoers rule.

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
