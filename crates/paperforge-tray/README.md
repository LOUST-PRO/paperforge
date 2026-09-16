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

```text
┌─ paperforge · 3 monitor(s) ────────────────────────┐
│  DP-1:    ← 3422465318 | 3240721055 | 2992660460 →  │
│  eDP-1:   ← 2549203773 | 2643615970 | 2789970990 →  │
│  HDMI-A-1:← 3307894201 | 2913305556 | 2749671515 →  │
│ ──────────────────────────────────────────────────── │
│ DP-1 (10)                                              │
│   Next wallpaper     (→ 2992660460)                    │
│   Previous wallpaper (← 3422465318)                    │
│ eDP-1 (10)                                             │
│   Next wallpaper     (→ 2789970990)                    │
│   Previous wallpaper (← 2549203773)                    │
│ HDMI-A-1 (10)                                          │
│   Next wallpaper     (→ 2749671515)                    │
│   Previous wallpaper (← 3307894201)                    │
│ ──────────────────────────────────────────────────── │
│ Rotate all now →                                       │
│ ← Previous all                                         │
│ Open TUI                                               │
│ Open GUI                                               │
│ ──────────────────────────────────────────────────── │
│ Quit                                                   │
└───────────────────────────────────────────────────────┘
```

The header rows are read-only labels (`enabled = false`) showing
`previous | current | next` for each playlist. The current index is
read from `$XDG_RUNTIME_DIR/paperforge-rotate-state.json`; the
next and previous indices are computed with wrap-around modulo so
the labels stay correct at the playlist boundaries.

Every action shells out to `paperforge-rotate.sh` with `--previous`
appended for the retreat variants:

| Action | Command |
|---|---|
| Per-monitor Next | `paperforge-rotate.sh <playlist>` |
| Per-monitor Previous | `paperforge-rotate.sh --previous <playlist>` |
| Rotate all now | `paperforge-rotate.sh` |
| Previous all | `paperforge-rotate.sh --previous` |
| Open TUI | `paperforge-tui` |
| Open GUI | `paperforge-gui` |
| Quit | `process::exit(0)` |

The skip logic (blacklisted workshops, orientation mismatch) is
honored by `paperforge-rotate.sh` for both directions, so
"Previous wallpaper" on a monitor with N scenes may skip past
several and land on the closest still-applicable scene in the
backward direction.

The Open TUI / Open GUI entries fire-and-forget spawn their
respective binaries with detached stdin/stdout/stderr so a slow
window boot (Wayland surface allocation, font load for the GPUI
GUI) does not block the ksni D-Bus loop. If the binary is missing,
the spawn fails silently and the user sees "command not found" in
their terminal — there is no tray-side dialog because SNI does not
support modal notifications.

## Install

```bash
# From crates.io
cargo install paperforge-tray --locked

# From the source workspace (development). The `--path` and `--bin`
# flags are required because paperforge is a multi-crate workspace;
# without them cargo fails with "no package found" at the repo root.
cargo install --path crates/paperforge-tray --bin paperforge-tray --locked

# Or from a Git tag (when releasing from a tag rather than crates.io)
cargo install --git https://github.com/LOUST-PRO/paperforge \
  --tag v0.1.1 \
  --path crates/paperforge-tray \
  --bin paperforge-tray \
  --locked
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
