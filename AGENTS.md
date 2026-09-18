# AGENTS.md

`CLAUDE.md` in this directory is the single source of truth for how this repo
works: layout, build commands, architecture, the Ekos Live wire format and its
pitfalls, styling rules, and code conventions. Read it first.

Nothing repo-specific lives here. This file exists only so agents that look for
`AGENTS.md` are pointed at `CLAUDE.md` instead of guessing.

## The short version

- `junos-web` is a LAN-only KStars/Ekos Live relay plus browser UI, not a cloud
  client. Two crates: `junos-server` (Axum) and `junos-web` (Leptos + WebGPU).
- `just check` typechecks both crates; `just test` runs the server tests;
  `just build` then `just run` builds and starts everything.
- `kstars/` is read-only upstream C++ reference. Grep it, never edit it.
- Licensed GPL-3.0-or-later. The constellation figures baked into `junos.bin`
  are derived from Stellarium and stay GPL-2.0-or-later.

Everything else: see `CLAUDE.md`.
