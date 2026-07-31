# `paperforge-gui`

> Dioxus 0.8-alpha desktop GUI for `paperforge` — Phase 6C, in
> progress. Same `paperforge-core` library, lazy-load video previews
> and visual playlist editor.
>
> 📖 **Workspace overview**: see the [main README](https://github.com/louzt/paperforge).

## Status

🚧 **In progress.** `paperforge-gui` currently builds against
`dioxus = 0.8.0-alpha.0` and is **not feature-complete**. The CLI
(`paperforge-cli`) and TUI (`paperforge-tui`) are the recommended
frontends until 6C ships.

Why alpha? Dioxus 0.8 is itself an alpha release with breaking
changes every minor. We are tracking the upstream API and will
stabilize this GUI when 0.8 hits stable. See
[Dioxus roadmap](https://github.com/DioxusLabs/dioxus/milestones).

## Planned features

- Visual playlist editor (drag wallpapers between playlists, reorder by drag)
- Lazy-load video previews on scroll
- Live preview before apply (renders to an in-app panel, not the desktop)
- Per-output wallpaper picker with thumbnail grid
- D-Bus integration via `paperforge-core::dbus`

## Why Dioxus?

| Framework | Why we picked / rejected |
|---|---|
| **Dioxus** ✅ | Rust-native, native widgets on desktop, async-first, easy IPC |
| Slint | Smaller community, less async support |
| Egui | Immediate-mode is wrong for a playlist editor (state churn) |
| GTK4-rs | C-FFI adds complexity; native look mismatches the cross-DE goal |

## Build (when ready)

```bash
# Currently only buildable from source
git clone https://github.com/louzt/paperforge
cd paperforge
cargo build --release -p paperforge-gui

# With dev tools (only useful during development)
cargo build --release -p paperforge-gui --features devtools
```

## Contributing

This crate is the highest-risk for breaking changes (Dioxus alpha).
If you want to contribute, start with `paperforge-core` or
`paperforge-cli` — both are stable and have a clear API surface.

When Dioxus 0.8 ships stable, this crate will follow.

License: MIT.
