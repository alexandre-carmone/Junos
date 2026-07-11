# CI/CD

Two GitHub Actions workflows live in `.github/workflows/`.

## `ci.yml` — on every push to `main` and every PR

Fast typecheck only (`cargo check` for `junos-server` + `junos-web` on wasm),
mirroring `just check`. No artifacts. Runs on x86_64.

## `release.yml` — on version tags and manual dispatch

Triggered by pushing a version tag (`git tag 0.1.2 && git push origin 0.1.2`)
or the **Run workflow** button (`workflow_dispatch`). Both x86_64 and **aarch64
(Raspberry Pi)** build **natively** — x86_64 on `ubuntu-24.04`, aarch64 on
`ubuntu-24.04-arm` — so there is no qemu emulation. Two jobs, each a 2-arch
matrix:

| Job         | Produces                                   | How |
|-------------|--------------------------------------------|-----|
| `tarball`   | `junos-web-<ver>-<arch>-linux.tar.gz` (+ `.sha256`) — binary, `dist/`, sample unit, run notes. Runs on any glibc Linux. | `packaging/portable/build-tarball.sh` |
| `archpkg`   | `junos-web-<ver>-<arch>.pkg.tar.zst` for Arch / Arch Linux ARM | `packaging/arch/build-local.sh` (native container) |

On a **tag** the `tarball` and `archpkg` artifacts are attached to the matching
GitHub Release (created automatically). On **manual dispatch** they are uploaded
as workflow artifacts only (no Release). Pass a `version` input to override the
label; otherwise the tag name / `git describe` is used.

### Versioning note

`packaging/arch/PKGBUILD` has a static `pkgver`. On a tag build the workflow
`sed`s it to the tag name so the package is labelled correctly. Keep tags in the
`MAJOR.MINOR.PATCH` shape the trigger expects (`0.1.2`, not `v0.1.2`).

## (Optional, not enabled) Nix binary cache — self-hosted Attic

The release workflow does **not** currently build or push a Nix cache. If you
later want NixOS consumers to substitute prebuilt binaries instead of compiling
(a full build is slow, especially on a Pi), add a job that builds the flake for
`x86_64-linux` and `aarch64-linux` and pushes to a self-hosted
[Attic](https://github.com/zhaofengli/attic) server. Sketch:

```yaml
  nix-cache:
    if: vars.ATTIC_ENDPOINT != ''
    strategy: { matrix: { include: [
      { system: x86_64-linux,  runner: ubuntu-24.04 },
      { system: aarch64-linux, runner: ubuntu-24.04-arm } ] } }
    runs-on: ${{ matrix.runner }}
    steps:
      - uses: actions/checkout@v4
      - uses: cachix/install-nix-action@v30
        with: { extra_nix_config: "experimental-features = nix-command flakes" }
      - uses: ryanccn/attic-action@v0
        with:
          endpoint: ${{ vars.ATTIC_ENDPOINT }}
          cache: ${{ vars.ATTIC_CACHE || 'rekos-web' }}
          token: ${{ secrets.ATTIC_TOKEN }}
      - run: nix build .#default .#junosServer .#junosWebDist -L
```

**Server + secrets setup:**

1. Run an Attic server somewhere reachable (container/binary) and create a cache:
   ```bash
   atticd                                  # the server (behind HTTPS)
   atticadm make-token --sub ci --push rekos-web   # -> push token for CI
   attic cache create rekos-web            # from an authed client
   ```
2. In the repo, **Settings → Secrets and variables → Actions**:
   - **Variables:** `ATTIC_ENDPOINT` = `https://cache.yourdomain.org`
     (and optionally `ATTIC_CACHE` if not `rekos-web`).
   - **Secrets:** `ATTIC_TOKEN` = the push token from step 1.

**Consume it** — on the installing host / in your NixOS config:

```nix
nix.settings = {
  substituters = [ "https://cache.yourdomain.org/rekos-web" ];
  trusted-public-keys = [ "rekos-web:<PUBLIC-KEY>" ];
};
```

Get the public key with `attic cache info rekos-web` (or from the server). Then
install via the flake (module + `services.junos-web.enable = true;`, see
`flake.nix` / `nix/module.nix`) or
`nix profile install github:alexandre-carmone/ekos-web-rust`.

### Alternative backends

Attic isn't the only option — the same job can instead sign paths and
`nix copy` to an **S3/MinIO** bucket (`s3://…?endpoint=…`) or push over **SSH**
to a box running **Harmonia**/`nix-serve`. All three need a signing keypair
(`nix key generate-secret …`); the consumer config is the same shape (a
`substituters` URL + `trusted-public-keys`).

## ARM runner availability

`ubuntu-24.04-arm` is free for public repositories. On a **private** repo, Linux
arm64 hosted runners are billed — if that's a concern, drop the aarch64 matrix
legs and build the Pi package on the Pi itself (`makepkg -si`, see
`packaging/arch/README.md`).
