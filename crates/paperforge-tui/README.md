# `paperforge-tui`

> Read-only TUI debugger for `paperforge` — live view of Wayland
> outputs, running LWE PIDs + state, playlists, and wallpaper
> inventory. Four panels on independent timers (2s / 5s / 10s / 30s),
> vim-friendly keys.
>
> 📖 **Workspace overview**: see the [main README](https://github.com/LOUST-PRO/paperforge).

## What is this?

`paperforge-tui` is **not** a frontend for end users — it's a
debugging tool. It exists so a human (or an LLM agent) can read the
state of `paperforge-core` at a glance without scraping D-Bus,
parsing `playlist.json` files, or grepping `/proc`.

Use it when you want to know:

- *Which Wayland outputs exist right now?*
- *Which LWE PIDs are running, and in what state (Running / Paused)?*
- *What playlists are saved, and how many wallpapers / outputs does each have?*
- *What's the wallpaper inventory look like (path / kind / size / mtime)?*
- *Did my last `paperforge set …` actually launch a process?*

## Install & run

```bash
cargo install paperforge-tui
paperforge-tui

# Add extra wallpaper source roots (besides the auto-detected ones)
paperforge-tui --source /data/wallpapers --source ~/projects/wp
```

## Layout

```
┌─ outputs (3) ─┬─ running (2) ─┬─ playlists (1) ─┐
│ DP-1          │ 1234 running  │ focus           │
│ eDP-1         │ 1235 paused   │                 │
│ HDMI-A-1      │               │                 │
└───────────────┴───────────────┴─────────────────┘
┌─ inventory (124) ─────────────────────────────────┐
│ /scenes/forest  │ Workshop │ 2.1 MiB │ 2026-07-10 │
│ /scenes/cyber   │ Workshop │ 5.4 MiB │ 2026-07-12 │
│ /videos/aurora  │ Video    │ 12  MiB │ 2026-07-15 │
└───────────────────────────────────────────────────┘
1-4 focus  ↑↓/jk nav  r refresh  q quit
```

## Refresh intervals

| Panel | Interval | Why |
|---|---|---|
| `outputs` | 2s | hotplug events happen fast; cheap fetch |
| `running` | 5s | `/proc` reads — fast but not free |
| `playlists` | 10s | JSON file reads, changes rarely |
| `inventory` | 30s | `walkdir` scan — most expensive |

Each panel's refresh is independent. If one stalls (e.g. a slow
disk), the others keep updating.

## Keybindings

| Key | Action |
|---|---|
| `1` `2` `3` `4` | jump focus to a panel |
| `Tab` / `Backtab` | cycle panels |
| `↑` `↓` / `k` `j` | navigate rows in focused panel |
| `r` / `R` | force refresh (status message only — actual refresh happens on next tick) |
| `q` / `Esc` / `Ctrl-C` | quit |

## What it does NOT do

- **No writes**. You can't launch / pause / apply from this TUI. Use the CLI for that.
- **No D-Bus**. Talks directly to `paperforge-core` via in-process API.
- **No state mutation**. Errors from a fetch are surfaced in the status bar, not retried aggressively.

## When to use it

- Before reporting a bug: confirm the state is what you think it is.
- While writing a new backend trait impl: verify `list_pids` /
  `state` actually return what you expect.
- After `playlist apply`: confirm the playlist appears and the
  outputs list updated.
- During CI smoke tests in a headless terminal: `--source` lets you
  point at fixture directories without touching real wallpapers.

## Build

```bash
cargo build --release -p paperforge-tui
```

License: MIT.
