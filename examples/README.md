# Examples

This directory contains reference artifacts for `paperforge`.

## Config

- [`config.toml`](config.toml) — annotated example. Copy to
  `~/.config/paperforge/config.toml` and edit.

## Playlists

Sample playlists to demonstrate the schema. Copy to
`~/.config/paperforge/playlists/<name>.json` and adjust paths to
your installed Workshop items.

- [`playlists/focus.json`](playlists/focus.json) — minimal scenes for
  deep work, applied across all 3 monitors.
- [`playlists/cyberpunk.json`](playlists/cyberpunk.json) — neon loops
  for late-night hacking, applied to 2 monitors.

## Playlist schema

```jsonc
{
  "name": "string (required, filesystem-safe)",   // no '/', no '..', no empty
  "description": "string (optional)",
  "outputs": ["DP-1", "HDMI-A-1"],                  // Wayland output names
  "wallpapers": ["/abs/path/scene-dir", "..."],   // iterated in order
  "fill": "fill"                                   // See below
}
```

`fill` options:

| Value | Behaviour |
|---|---|
| `stretch` | Stretch to fill (may distort) |
| `cover` | Crop to fit (no distortion) |
| `contain` | Letterbox (no distortion) |
| `center` | Native size, centered |
| `tile` | Repeat |
| `fill` | Resize to fill, may crop (default) |

## Usage

```bash
# Save a playlist (manual edit then copy to ~/.config/paperforge/playlists/)
cp examples/playlists/focus.json ~/.config/paperforge/playlists/

# Apply it
paperforge playlist apply focus

# Inspect what just got applied
paperforge playlist show focus
paperforge list
```
