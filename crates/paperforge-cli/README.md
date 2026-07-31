# `paperforge` (CLI)

> Command-line interface for `paperforge-core` — set / pause / resume
> / list / scan / audio / playlist over the LWE Workshop backend and
> alternative backends (swww / hyprpaper / mpvpaper).
>
> 📖 **Workspace overview**: see the [main README](https://github.com/LOUST-PRO/paperforge).
> 🇪🇸 [Documentación en español](https://github.com/LOUST-PRO/paperforge/blob/main/docs/README.es.md).

## Install

```bash
cargo install paperforge-cli

# Or from source
git clone https://github.com/LOUST-PRO/paperforge
cd paperforge
cargo build --release
sudo install -m 0755 target/release/paperforge /usr/local/bin/
```

Requires `rustc >= 1.75`, `pkg-config`, Linux (uses `/proc/<pid>/status`).

## Usage

```text
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

## Recipes

### Set a wallpaper on a specific output

```bash
paperforge set ~/.steam/root/steamapps/workshop/content/431960/1234567 \
  --output DP-1
```

### Pause everything (free GPU/CPU)

```bash
paperforge pause
# ... do something intensive
paperforge resume
```

### Audio toggle

```bash
paperforge audio toggle
paperforge audio mute
paperforge audio unmute
```

### Playlist workflow

```bash
# Create a playlist from scenes you want on DP-1 + DP-2.
paperforge playlist save focus \
  scene1 scene2 scene3 \
  --output DP-1 --output DP-2

# List, show, apply, delete
paperforge playlist list
paperforge playlist show focus
paperforge playlist apply focus
paperforge playlist delete focus
```

### Audit your setup

```bash
paperforge paths                  # what paths will paperforge scan?
paperforge scan --max-depth 2     # what's actually there?
paperforge list                   # what's running?
```

## Configuration

Config lives at `$XDG_CONFIG_HOME/paperforge/config.toml`
(`~/.config/paperforge/config.toml` by default).

```toml
[backend]
kind = "lwe"                       # "lwe" / "swww" / "hyprpaper" / "mpvpaper"
binary = "linux-wallpaperengine"   # override backend binary path

[paths]
extra_sources = ["/data/wallpapers"]

[audio]
require_explicit = true            # refuse SIGUSR1/2 unless explicitly toggled
```

See [`examples/config.toml`](https://github.com/LOUST-PRO/paperforge/blob/main/examples/config.toml)
for the annotated reference.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | success |
| `1` | runtime / IPC error |
| `2` | invalid CLI args |
| `3` | backend not reachable |

License: MIT.
