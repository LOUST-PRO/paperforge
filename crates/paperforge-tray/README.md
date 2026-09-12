# paperforge-tray

StatusNotifierItem (SNI) tray icon for [paperforge](../README.md). Sits in
the Wayland status notifier (dankbar, KDE plasma, GNOME Shell via the
AppIndicator extension) and exposes a right-click menu with per-monitor
controls plus global actions.

## What is it

Every menu action shells out to the canonical
`paperforge-rotate.sh` orchestrator. The tray is intentionally a thin
presentational layer — it does NOT talk to LWE, the daemon, or D-Bus
directly. All wallpaper side-effects go through the same script that the
`paperforge-rotate.timer` fires hourly, which means:

- The bypass-the-rotten-pool invariant stays in **one place**.
- The self-healing guard in rotate.sh (kills paperforge.service if it
  reactivates) protects tray-triggered rotations too.
- Logs from tray actions appear alongside hourly-rotation logs in
  `journalctl --user -u paperforge-rotate.service`.

## Menu shape

```
┌─ paperforge · 3 monitor(s) ─────────────────┐
│  DP-1: 3240721055 → 2992660460                │
│  eDP-1: 2643615970 → 2549203773               │
│  HDMI-A-1: 2913305556 → 2749671515            │
│ ─────────────────────────────────────────────  │
│ DP-1 (10)                                       │
│   Next wallpaper  (→ 2992660460)               │
│ eDP-1 (10)                                      │
│   Next wallpaper  (→ 2549203773)               │
│ HDMI-A-1 (10)                                   │
│   Next wallpaper  (→ 2749671515)               │
│ ─────────────────────────────────────────────  │
│ Rotate all now                                  │
│ Open TUI                                        │
│ ─────────────────────────────────────────────  │
│ Quit                                            │
└────────────────────────────────────────────────┘
```

Future versions will add "Random from playlist" and "Renew playlist" actions
under each per-monitor submenu — those require extending
`paperforge-rotate.sh` with `--random <monitor>` and
`--renew <monitor>` flags first.

## Install

```bash
# From crates.io
cargo install paperforge-tray --locked

# Or from the workspace root (development)
cargo build --release -p paperforge-tray
install -m 0755 target/release/paperforge-tray ~/.local/bin/
```

Then either launch it directly (`paperforge-tray &`) or enable the
provided systemd user service:

```bash
systemctl --user daemon-reload
systemctl --user enable --now paperforge-tray.service
```

## Configuration

| Env var | Default | Purpose |
|---|---|---|
| `PAPERFORGE_PLAYLISTS` | `~/.config/paperforge/playlists` | Where `<monitor>.json` playlists live |
| `RUST_LOG` | `info` | tracing-subscriber filter (e.g. `RUST_LOG=paperforge-tray=debug`) |

CLI flags override env vars:

```bash
paperforge-tray --playlist-dir /srv/paperforge/playlists \
                --rotate-bin /usr/local/bin/paperforge-rotate.sh
```

## Compatibility

- **Wayland only.** The StatusNotifierItem protocol lives on the session
  D-Bus; X11 desktops use the legacy `tray-icon` protocol which this
  crate does not implement. (For X11, look at `paperforge-gui`.)
- **Rust 1.80+** (per `ksni` requirement).
- Tested on dankbar (Quickshell-based), KDE plasma 6, and GNOME Shell
  with the AppIndicator extension.

## Architecture / Design decisions

- **`Tray` impl is `Send + 'static`** because ksni spawns the D-Bus loop
  on a tokio task. Closures passed to `StandardItem::activate` are
  `Box<dyn Fn(&mut Self) + Send>` so we can mutate the tray state from
  inside them if we ever need to (e.g. force-refresh monitors after a
  rotate).
- **Menu is rebuilt on every open** — `Tray::menu()` is called fresh by
  ksni whenever the user right-clicks the icon. That means the "current"
  / "next" labels stay accurate without us having to push updates via
  D-Bus property change notifications.
- **No caching of `paperforge-rotate.sh` path** — passed at construction
  time and cloned into each closure. Cheap; no surprises.

## License

Licensed under MIT. See `LICENSE` at the project root.

Contact: `opensource@loust.pro`.
