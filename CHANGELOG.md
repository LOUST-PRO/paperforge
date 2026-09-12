# Changelog

All notable changes to `paperforge` are documented here. The format
is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Portrait auto-skip heuristic**: orientation-aware skip in the
  per-playlist rotation loop. Wallpapers whose physical orientation
  (portrait vs landscape) contradicts the monitor's orientation are
  skipped automatically, so a 9:16 Workshop scene never lands on a
  16:9 monitor again. Detection works on `scene.pkg`
  (`orthogonalprojection` block for Workshop scenes), `ffprobe`
  shell-out with 5s timeout for video workshops, and inline PNG
  IHDR / JPEG SOF0 header parsing for image workshops. Web workshops
  (`index.html`) return `Unknown` and pass through by default.
- `paperforge orientation <PATH>` CLI subcommand with `--json`
  output (`paperforge-core::orientation::detect`).
- New `monitor_orientation` and `orientation_fallback` fields on
  the playlist JSON schema, both optional with safe defaults
  (`landscape` + `allow`). Existing playlists pick up portrait
  filtering automatically — no opt-in required.
- `paperforge-core::orientation::orientation_compatible(monitor,
  scene, fallback)` pure decision function with a truth table
  covering 12 monitor/scene/fallback combinations (rustdoc).
- `crates/paperforge-core/tests/orientation-skip-smoke.sh` — 12
  integration assertions covering synthetic + live Steam-library
  workshops (portrait 1080x1920, 1080x2400, 2395x3500; landscape
  1920x1080; square 1080x1080), CLI `--json` shape, and bash-helper
  cache hit.
- `crates/paperforge-cli/tests/cli.rs` — 6 integration tests using
  `CARGO_BIN_EXE_paperforge` (no extra deps).
- Real SIGSTOP/SIGCONT round-trip test using a `sleep` child + `/proc/<pid>/status`
  state inspection.
- `Inventory` edge-case tests: video-typed projects, unknown type,
  corrupt `project.json` mid-scan, empty inventory.
- `Config` tests: extra_sources roundtrip, backend construction,
  source_roots inclusion.
- `AudioCommand` serialization test (kebab-case lowercase).

### Performance
- Operator bash helper `paperforge-orientation-detect.sh` caches
  results in `$XDG_CONFIG_HOME/paperforge/orientation-cache.json`
  keyed by workshop ID + scene.pkg mtime. Cache hit cost is a
  single `jq` lookup (~ms); cache miss runs the CLI parser
  (~50–300ms fork+exec).

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
- Initial release of `paperforge` (Fase 6A).
- Workspace with 3 crates:
  - `paperforge-core` — lib (inventory, paths, backend, audio,
    playlist, config, error)
  - `paperforge-cli` — `paperforge` binary with 8 subcommands
  - `paperforge-tui` — placeholder for Fase 6B
- `WallpaperBackend` trait + `LweBackend` impl (POSIX signals:
  SIGSTOP/SIGCONT for pause/resume, SIGUSR1/SIGUSR2 for audio).
- `LweAudioController` (toggle/mute/unmute via SIGUSR1/SIGUSR2/SIGCONT).
- `Playlist` + `PlaylistStore` (JSON files in
  `$XDG_CONFIG_HOME/paperforge/playlists/`).
- `Inventory` scanner (walkdir + mtime, detects Workshop scenes +
  loose images + loose videos).
- `default_paths` auto-detect (native Steam + Flatpak + `~/Wallpapers`).
- `Config` + `ConfigPaths` (TOML at `config.toml`).
- 24 unit tests covering all public APIs.

### Provenance
- Designed to focus on what's missing in generic Wayland wallpaper
  switchers (per-monitor playlists, audio control via signals, etc).
- License: MIT.
- Backend: [`louzt/linux-wallpaperengine`](https://github.com/louzt/linux-wallpaperengine) (GPL-3.0) via IPC.

[Unreleased]: https://github.com/LOUST-PRO/paperforge/compare/0.1.0...HEAD
[0.1.0]: https://github.com/LOUST-PRO/paperforge/releases/tag/0.1.0
