# AGENTS.md

`CLAUDE.md` in this directory is the single source of truth for how this repo
works: layout, build commands, architecture, the Ekos Live wire format and its
pitfalls, styling rules, and code conventions. Read it first.

Nothing repo-specific lives here. This file exists only so agents that look for
`AGENTS.md` are pointed at `CLAUDE.md` instead of guessing.

## The short version

- `junos-web` is a LAN-only KStars/Ekos Live relay plus browser UI, not a cloud
  client. Three crates: `junos-server` (Axum), `junos-web` (Leptos + WebGPU)
  and `junos-guider` (a PHD2 2.6.14 port, the autoguider daemon).
- `just check` typechecks the three crates; `just test` runs the server and
  guider tests; `just build` then `just run` builds and starts everything.
- `kstars/` and `phd2/` are read-only upstream C++ references. Grep them,
  never edit them.
- Licensed GPL-3.0-or-later. The constellation figures baked into `junos.bin`
  are derived from Stellarium and stay GPL-2.0-or-later; `junos-guider` keeps
  PHD2's BSD-3-Clause notices (`junos-guider/THIRD_PARTY.md`).

Everything else: see `CLAUDE.md`.
