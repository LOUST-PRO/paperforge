# lzt-wallcraft
Frontends Rust para `linux-wallpaperengine` Workshop en Linux.

> **Status**: Pre-0.1.0 (Fase 6A in progress). API surface todavía puede cambiar.

## Que es?

`lzt-wallcraft` gestiona los wallpapers animados de **Wallpaper Engine Workshop**
sobre Linux. Hoy se enfoca en el backend `linux-wallpaperengine` (fork
de Almamu mantenido por `louzt/`) que renderiza escenas Workshop.

Stack:
- **Workspace Rust** (3 crates: `lzt-wallcraft-core`, `lzt-wallcraft-cli`,
  `lzt-wallcraft-tui`)
- **MIT licensed** — `lzt-wallcraft-core` habla con LWE via subprocess
  + POSIX signals (separación de procesos = licencia clean respecto al
  GPL-3.0 de LWE)
- **Backend-friendly**: `WallpaperBackend` trait permite añadir swww,
  hyprpaper, mpvpaper, awww como backends alternativos en el futuro

## Por que no waypaper?

[`waypaper`](https://github.com/anufrievroman/waypaper) es excelente y
OSS. `lzt-wallcraft` lo complementa (no lo reemplaza) con:

| Feature | waypaper | lzt-wallcraft |
|---|---|---|
| GUI GTK | ✅ | 🚧 Fase 6C (Dioxus) |
| CLI | basico | ✅ (set/pause/resume/list/scan/audio/playlist) |
| **Playlists por monitor** | ❌ | ✅ (`playlist apply focus`) |
| Audio toggle via IPC | ❌ | ✅ (SIGUSR1/SIGUSR2) |
| Auto-detect Steam + Flatpak | parcial | ✅ |
| Inventory con mtime | ❌ | ✅ |
| Inventario raw JSON via `scan` | ❌ | ✅ |

La killer feature es **playlists reutilizables por monitor** — con
waypaper cada wallpaper es one-off, con nosotros guardas listas
`focus` / `cyberpunk` / `gaming` y las aplicas en un comando.

## Quickstart

```bash
# Build (release)
cargo build --release

# Smoke test
./target/release/lzt-wallcraft paths
./target/release/lzt-wallcraft list
./target/release/lzt-wallcraft scan --max-depth 2

# Lanzar wallpaper (Workshop scene)
./target/release/lzt-wallcraft set \
  ~/.steam/root/steamapps/workshop/content/431960/1234567 \
  --output DP-1

# Pausar todos los LWE (libera GPU/CPU)
./target/release/lzt-wallcraft pause

# Reanudar
./target/release/lzt-wallcraft resume

# Audio
./target/release/lzt-wallcraft audio toggle
./target/release/lzt-wallcraft audio mute
./target/release/lzt-wallcraft audio unmute

# Playlists
./target/release/lzt-wallcraft playlist list
./target/release/lzt-wallcraft playlist show focus
./target/release/lzt-wallcraft playlist apply focus
./target/release/lzt-wallcraft playlist delete focus
```

## Subcomandos

```
lzt-wallcraft <CMD>

Commands:
  set       Lanzar LWE con un scene directory o archivo
  pause     SIGSTOP todas las instancias LWE
  resume    SIGCONT todas las instancias LWE
  list      Listar PIDs LWE corriendo + state
  scan      Escanear paths default, print discovered entries
  audio     Audio control via POSIX signals (toggle/mute/unmute)
  playlist  Playlist management (list/show/save/apply/delete)
  paths     Print auto-detected source paths
```

## Arquitectura

```
crates/lzt-wallcraft-core/   # lib MIT, sin dependencias GPL
├── inventory.rs             # walkdir scanner con mtime cache
├── paths.rs                 # auto-detect Steam/Flatpak/local dirs
├── backend.rs               # WallpaperBackend trait + LweBackend
├── audio.rs                 # LweAudioController (SIGUSR1/SIGUSR2)
├── playlist.rs              # Playlist + PlaylistStore (JSON files)
├── config.rs                # Config + ConfigPaths (TOML)
└── error.rs                 # Crate-wide Error type

crates/lzt-wallcraft-cli/    # binario `lzt-wallcraft`
└── src/main.rs              # clap derive, 8 subcommands

crates/lzt-wallcraft-tui/    # placeholder Fase 6B
└── src/lib.rs
```

### Compatibilidad de licencias

`lzt-wallcraft-core` es MIT. `linux-wallpaperengine` (el fork de
`louzt/`) es GPL-3.0. **No mezclamos código fuente** — `lzt-wallcraft`
solo habla con LWE via:
- `fork+exec` de la binary LWE
- POSIX signals (`SIGSTOP`, `SIGCONT`, `SIGUSR1`, `SIGUSR2`)
- Lectura de `/proc/<pid>/cmdline` y `/proc/<pid>/status`

FSF GPL FAQ confirma que dos programas que se comunican por IPC
siguen siendo programas separados. No se genera trabajo derivado
con GPL.

## Build

```bash
cargo build                    # debug
cargo build --release          # release (lto + strip)
cargo test --all               # 46 tests
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

Build dependencies:
- `rustc >= 1.75` (usa `let ... else { ... };`)
- `pkg-config` (para algunas deps)
- Linux (testea `/proc/<pid>/cmdline` directamente — no portable a macOS)

## Roadmap

- **Fase 6A** ✅ — Core lib + CLI + audio SIGUSR (este PR)
- **Fase 6B** 🚧 — `ratatui` TUI con grid + vim-nav + thumbnails
- **Fase 6C** 🚧 — `Dioxus` GUI desktop con lazy-load video preview
- **Fase 6D** ⏳ — Steam Workshop API catalog (read-only)

## Contribuir

Ver [`CONTRIBUTING.md`](CONTRIBUTING.md). PRs bienvenidos — sobre todo
backends adicionales (`swww`, `hyprpaper`, `mpvpaper`).

## License

MIT — ver [`LICENSE`](LICENSE).

## Provenance

- Fork / companion a [`louzt/linux-wallpaperengine`](https://github.com/louzt/linux-wallpaperengine)
- Inspired by [`waypaper`](https://github.com/anufrievroman/waypaper) (GPL-3.0)
- Powered by `tokio`, `clap`, `walkdir`, `notify`, `nix`, `serde`, `dirs`
