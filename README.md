# paperforge

Rust frontends for **Wallpaper Engine Workshop** scenes on Linux.

> **Status**: v0.1.0-rc1 — pre-release. API may change before 0.1.0.
> **License**: MIT. Backend IPC to `linux-wallpaperengine` (GPL-3.0)
> preserves license cleanliness via process isolation.

## What is it?

`paperforge` is a Rust workspace that orchestrates animated wallpapers
on Wayland. It launches Workshop scenes, manages playlists, toggles
audio, and exposes a debug TUI — all without re-implementing a
renderer.

The feature set focuses on what's missing in most generic Wayland
wallpaper switchers:

| Feature | Typical switchers | paperforge |
|---|---|---|
| Per-monitor playlists | ❌ | ✅ `paperforge playlist apply focus` |
| Audio control via POSIX signals | ❌ | ✅ `paperforge audio toggle` |
| Steam + Flatpak auto-detect | partial | ✅ |
| Inventory with mtime | ❌ | ✅ |
| Read-only TUI debugger | ❌ | ✅ `paperforge-tui` |
| Multiple backends (LWE, swww, hyprpaper, mpvpaper) | ❌ | ✅ |

The killer feature is **per-monitor reusable playlists**: define
`focus` / `cyberpunk` / `gaming` once, apply on demand.

## Install

```bash
# crates.io (once published)
cargo install paperforge-cli

# From source
git clone https://github.com/LOUST-PRO/paperforge
cd paperforge
cargo build --release
sudo install -m 0755 target/release/paperforge /usr/local/bin/
```

Requires `rustc >= 1.75`, `pkg-config`, Linux (uses `/proc/<pid>/status`
directly).

## Quickstart

```bash
# Discover what your system has
paperforge paths

# List running wallpapers
paperforge list

# Launch a Workshop scene on a specific output
paperforge set ~/.steam/root/steamapps/workshop/content/431960/1234567 \
  --output DP-1

# Free GPU/CPU without quitting
paperforge pause && paperforge resume

# Save and apply a playlist
paperforge playlist save focus scene1 scene2 scene3 \
  --output DP-1 --output DP-2
paperforge playlist apply focus

# Debug interactively
paperforge-tui
```

## Subcommands

```
paperforge <COMMAND>

Commands:
  set         Launch LWE with a scene dir or file
  pause       SIGSTOP all running LWE instances
  resume      SIGCONT all paused LWE instances
  list        List running LWE PIDs + state
  scan        Scan default paths, print discovered entries
  audio       Audio control (toggle/mute/unmute) via SIGUSR1/SIGUSR2
  playlist    Playlist management (list/show/save/apply/delete)
  paths       Print auto-detected source paths
```

`paperforge-tui` is a separate binary — a read-only TUI debugger over
inventory, running PIDs, playlists, and Wayland outputs.

## Architecture

```
paperforge/                         (MIT)
├── crates/paperforge-core/         MIT, no GPL deps
│   ├── inventory.rs    walkdir scanner + mtime cache
│   ├── paths.rs        Steam / Flatpak / local auto-detect
│   ├── backend.rs      WallpaperBackend trait + LweBackend
│   ├── audio.rs        LweAudioController (SIGUSR1/SIGUSR2)
│   ├── playlist.rs     Playlist + PlaylistStore (JSON)
│   ├── config.rs       Config + ConfigPaths (TOML)
│   ├── daemon.rs       PaperforgeDaemon (tokio + tokio::sync::RwLock)
│   ├── hotplug.rs      CompositorHotplugSource (Wayland output events)
│   ├── dbus.rs         zbus IPC service (differentiation vs swww)
│   ├── lwe_probe.rs    runtime detection of LWE audio signal support
│   └── error.rs        crate-wide Error type
├── crates/paperforge-cli/          bin `paperforge` (clap)
├── crates/paperforge-tui/          bin `paperforge-tui` (ratatui)
└── crates/paperforge-gui/          bin `paperforge-gui` (Dioxus, WIP)
```

### License boundary

`paperforge-core` is MIT. `linux-wallpaperengine` is GPL-3.0.
We never link or import LWE source — `paperforge` talks to LWE
exclusively via:

- `fork+exec` of the LWE binary
- POSIX signals (`SIGSTOP`, `SIGCONT`, `SIGUSR1`, `SIGUSR2`)
- reads of `/proc/<pid>/cmdline` and `/proc/<pid>/status`

Per the FSF GPL FAQ, two programs communicating via IPC remain
separate programs. No GPL derivative work is generated.

### Backends

| Backend | Pause | Audio | Workshop | Trait impl |
|---|---|---|---|---|
| `LweBackend` | SIGSTOP/SIGCONT | SIGUSR1/SIGUSR2 | ✅ | first-class |
| `SwwwBackend` | swww CLI | n/a | ❌ | proof-of-concept |
| `HyprpaperBackend` | n/a (stateless) | n/a | ❌ | stub |
| `MpvpaperBackend` | mpv IPC `pause` | mpv IPC | ❌ | stub |

Workshop scenes (the killer feature) are only renderable through LWE.

## Build & test

```bash
cargo build --release              # lto + strip
cargo test --workspace             # 158 tests (one lwe_probe needs upstream binary)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

CI: `.github/workflows/ci.yml` runs `cargo test --workspace`,
clippy with `-D warnings`, and `cargo fmt --check` on every push.

## Roadmap

- **6A** ✅ — Core lib + CLI + audio SIGUSR (v0.1.0)
- **6B** ✅ — `ratatui` TUI debugger
- **6C** 🚧 — Dioxus GUI desktop with lazy-load video preview
- **6D** ⏳ — Steam Workshop API catalog (read-only)
- **6E** ⏳ — Wireguard-aware network kill-switch for offline playlists

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md). New backends are the most
useful contribution — implement `WallpaperBackend` for `swww`,
`awww`, `hyprpaper`, `mpvpaper`, or `waypipe`.

## License

MIT — see [`LICENSE`](LICENSE).

## Provenance

- Companion to [`louzt/linux-wallpaperengine`](https://github.com/louzt/linux-wallpaperengine)
- Powered by `tokio`, `clap`, `ratatui`, `walkdir`, `notify`,
  `nix`, `serde`, `dirs`, `zbus`

## Translations

- [Español](docs/README.es.md)

## LZT hardening fork

This repository is a Lou-maintained fork with hardening applied:

- Defensive defaults (fail-closed, no silent fallbacks)
- Sanitization gate on public-facing files
- No telemetry by default

See [LICENSE-FORK.md](./LICENSE-FORK.md) for the full hardening addendum
and [ci-debug-lab](https://github.com/louzt/ci-debug-lab) for the broader
verification toolkit (cgroups v2, /dev/shm sizing, canary probes).
