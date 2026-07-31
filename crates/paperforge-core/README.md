# `paperforge-core`

> Core library for `paperforge` — Workshop wallpaper inventory, LWE
> backend IPC via POSIX signals, per-monitor playlists, audio toggle,
> and a trait-based backend abstraction.
>
> 📖 **Workspace overview & install**: see the [main README](https://github.com/louzt/paperforge).
> 🇪🇸 [Documentación en español](https://github.com/louzt/paperforge/blob/main/docs/README.es.md).

## What's in this crate?

`paperforge-core` is the MIT-licensed library that every frontend
(CLI, TUI, GUI) talks to. It is **GPL-free** — `linux-wallpaperengine`
is reached via process isolation only.

| Module | Purpose |
|---|---|
| `inventory` | `walkdir` scanner with mtime cache, detects Workshop scenes + loose media |
| `paths` | Auto-detect Steam + Flatpak + `~/Wallpapers` source roots |
| `backend` | `WallpaperBackend` trait + `LweBackend` (POSIX signals) |
| `audio` | `LweAudioController` (SIGUSR1/SIGUSR2 toggle/mute/unmute) |
| `playlist` | `Playlist` + `PlaylistStore` (JSON files in `$XDG_CONFIG_HOME/paperforge/playlists/`) |
| `config` | `Config` + `ConfigPaths` (TOML loader) |
| `daemon` | `PaperforgeDaemon` (tokio + `tokio::sync::RwLock` for state) |
| `hotplug` | `CompositorHotplugSource` (Wayland output events) |
| `dbus` | zbus IPC service (exposed as `org.paperforge.Daemon`) |
| `lwe_probe` | Runtime detection of LWE audio signal support |
| `error` | Crate-wide `Error` type |

## Quickstart (library usage)

```rust
use paperforge_core::{
    backend::LweBackend,
    inventory::Inventory,
    paths::default_paths,
    WallpaperBackend, // trait
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Discover wallpapers from all default roots.
    let mut inv = Inventory::new();
    for root in default_paths().all() {
        inv.scan(root, 4)?;
    }
    println!("found {} entries ({} LWE-compatible)",
             inv.entries().count(),
             inv.entries().filter(|e| e.kind.lwe_compatible()).count());

    // Pause any running LWE instances to free GPU/CPU.
    let backend = LweBackend::new();
    for pid in backend.list_pids().await? {
        backend.pause(pid).await?;
    }
    Ok(())
}
```

## Backend trait — adding a new backend

Implement `WallpaperBackend` for any Wayland wallpaper daemon:

```rust
use paperforge_core::backend::{BackendState, WallpaperBackend};
use paperforge_core::error::Result;
use async_trait::async_trait;

pub struct MpvpaperBackend { /* ... */ }

#[async_trait]
impl WallpaperBackend for MpvpaperBackend {
    async fn list_pids(&self) -> Result<Vec<i32>> { /* mpv IPC */ }
    async fn state(&self, pid: i32) -> Result<BackendState> { /* ... */ }
    async fn pause(&self, pid: i32) -> Result<()> { /* mpv IPC pause */ }
    async fn resume(&self, pid: i32) -> Result<()> { /* mpv IPC unpause */ }
}
```

The CLI dispatches to whichever backend is configured in
`~/.config/paperforge/config.toml`:

```toml
[backend]
kind = "lwe"   # or "swww" / "hyprpaper" / "mpvpaper"
```

## License boundary

`paperforge-core` is **MIT**. `linux-wallpaperengine` (the
[`louzt/`](https://github.com/louzt/linux-wallpaperengine) fork) is
**GPL-3.0**. We never link or import LWE source — `paperforge-core`
talks to LWE exclusively via:

- `fork+exec` of the LWE binary
- POSIX signals (`SIGSTOP`, `SIGCONT`, `SIGUSR1`, `SIGUSR2`)
- reads of `/proc/<pid>/cmdline` and `/proc/<pid>/status`

Per the FSF GPL FAQ, two programs communicating via IPC remain
separate programs. No GPL derivative work is generated.

## Build & test

```bash
cargo build -p paperforge-core
cargo test  -p paperforge-core --lib        # 120 tests
cargo clippy -p paperforge-core --all-targets -- -D warnings
```

License: MIT. See [LICENSE](https://github.com/louzt/paperforge/blob/main/LICENSE).
