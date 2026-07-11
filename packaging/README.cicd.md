# CI/CD

Two GitHub Actions workflows live in `.github/workflows/`.

## `ci.yml` — on every push to `main` and every PR

Fast typecheck only (`cargo check` for `junos-server` + `junos-web` on wasm),
mirroring `just check`. No artifacts. Runs on x86_64.

## `release.yml` — on version tags and manual dispatch

Triggered by pushing a version tag (`git tag 0.1.2 && git push origin 0.1.2`)
or the **Run workflow** button (`workflow_dispatch`). Both x86_64 and **aarch64
(Raspberry Pi)** build **natively** — x86_64 on `ubuntu-24.04`, aarch64 on
`ubuntu-24.04-arm` — so there is no qemu emulation. Three jobs, each a 2-arch
matrix:

| Job         | Produces                                   | How |
|-------------|--------------------------------------------|-----|
| `tarball`   | `junos-web-<ver>-<arch>-linux.tar.gz` (+ `.sha256`) — binary, `dist/`, sample unit, run notes. Runs on any glibc Linux. | `packaging/portable/build-tarball.sh` |
| `archpkg`   | `junos-web-<ver>-<arch>.pkg.tar.zst` for Arch / Arch Linux ARM | `packaging/arch/build-local.sh` (native container) |
| `nix-cache` | Pushes prebuilt store paths to Cachix for `nix`/NixOS installs | flake `.#default`, `.#junosServer`, `.#junosWebDist` |

On a **tag** the `tarball` and `archpkg` artifacts are attached to the matching
GitHub Release (created automatically). On **manual dispatch** they are uploaded
as workflow artifacts only (no Release). Pass a `version` input to override the
label; otherwise the tag name / `git describe` is used.

### Versioning note

`packaging/arch/PKGBUILD` has a static `pkgver`. On a tag build the workflow
`sed`s it to the tag name so the package is labelled correctly. Keep tags in the
`MAJOR.MINOR.PATCH` shape the trigger expects (`0.1.2`, not `v0.1.2`).

## Nix binary cache (NixOS installs)

`nix-cache` builds the flake for `x86_64-linux` and `aarch64-linux` and pushes
the results to **Cachix**, so consumers substitute prebuilt binaries instead of
compiling (a full build is slow, especially on a Pi).

**One-time setup:**

1. Create a cache at <https://app.cachix.org> (e.g. `rekos-web`).
2. If your chosen name differs, update the `name:` under **Set up Cachix** in
   `.github/workflows/release.yml`.
3. Generate a write auth token and add it as the repo secret
   `CACHIX_AUTH_TOKEN` (**Settings → Secrets and variables → Actions**).

Without the secret the job still builds but pushes nothing.

**Consume it** — on the installing host / in your NixOS config add the
substituter (replace `rekos-web` with your cache name):

```nix
nix.settings = {
  substituters = [ "https://rekos-web.cachix.org" ];
  trusted-public-keys = [ "rekos-web.cachix.org-1:<PUBLIC-KEY-FROM-CACHIX>" ];
};
```

Then install via the flake (module + `services.junos-web.enable = true;`, see
`flake.nix` / `nix/module.nix`) or `nix profile install github:alexandre-carmone/ekos-web-rust`.
The public key is shown on the cache's Cachix page.

## ARM runner availability

`ubuntu-24.04-arm` is free for public repositories. On a **private** repo, Linux
arm64 hosted runners are billed — if that's a concern, drop the aarch64 matrix
legs and build the Pi package on the Pi itself (`makepkg -si`, see
`packaging/arch/README.md`).
