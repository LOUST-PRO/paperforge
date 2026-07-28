# Changelog

All notable changes to `lzt-wallcraft` are documented here. The format
is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `crates/lzt-wallcraft-cli/tests/cli.rs` — 6 integration tests using
  `CARGO_BIN_EXE_lzt-wallcraft` (no extra deps).
- Real SIGSTOP/SIGCONT round-trip test using a `sleep` child + `/proc/<pid>/status`
  state inspection.
- `Inventory` edge-case tests: video-typed projects, unknown type,
  corrupt `project.json` mid-scan, empty inventory.
- `Config` tests: extra_sources roundtrip, backend construction,
  source_roots inclusion.
- `AudioCommand` serialization test (kebab-case lowercase).

### Changed
- `LweBackend::list_pids` now walks `/proc/<pid>/cmdline` directly
  instead of `pgrep -f linux-wallpaperengine`. Robust to
  `/proc/<pid>/comm` being truncated to 15 chars (TASK_COMM_LEN),
  eliminates false positives when cwd contains the pattern substring,
  faster (no subprocess fork), and testable via a sync helper.
- `BackendKind::process_basename` renamed to `process_pattern` to
  better reflect that it's a substring match. Deprecated alias kept
  for backward compatibility until 0.2.0.
- Removed dead `unused_to_silence_warnings` fn from `cli/src/main.rs`.

### Performance
- `list_pids` no longer spawns `pgrep` subprocess — direct `/proc`
  read is ~30x faster on a ~1000 PID system.

## [0.1.0] — 2026-07-28

### Added
- Initial release of `lzt-wallcraft` (Fase 6A).
- Workspace with 3 crates:
  - `lzt-wallcraft-core` — lib (inventory, paths, backend, audio,
    playlist, config, error)
  - `lzt-wallcraft-cli` — `lzt-wallcraft` binary with 8 subcommands
  - `lzt-wallcraft-tui` — placeholder for Fase 6B
- `WallpaperBackend` trait + `LweBackend` impl (POSIX signals:
  SIGSTOP/SIGCONT for pause/resume, SIGUSR1/SIGUSR2 for audio).
- `LweAudioController` (toggle/mute/unmute via SIGUSR1/SIGUSR2/SIGCONT).
- `Playlist` + `PlaylistStore` (JSON files in
  `$XDG_CONFIG_HOME/lzt-wallcraft/playlists/`).
- `Inventory` scanner (walkdir + mtime, detects Workshop scenes +
  loose images + loose videos).
- `default_paths` auto-detect (native Steam + Flatpak + `~/Wallpapers`).
- `Config` + `ConfigPaths` (TOML at `config.toml`).
- 24 unit tests covering all public APIs.

### Provenance
- Designed to complement (not replace) [`waypaper`](https://github.com/anufrievroman/waypaper).
- License: MIT.
- Backend: [`louzt/linux-wallpaperengine`](https://github.com/louzt/linux-wallpaperengine) (GPL-3.0) via IPC.

[Unreleased]: https://github.com/louzt/lzt-wallcraft/compare/0.1.0...HEAD
[0.1.0]: https://github.com/louzt/lzt-wallcraft/releases/tag/0.1.0
