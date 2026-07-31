# paperforge systemd unit

This directory contains the user-level systemd unit for running
`paperforge daemon` in the background of your Wayland session.

## Files

| File | Purpose |
|---|---|
| `paperforge.service` | The unit itself. Uses `%h`/`%U` specifiers so it does not embed any host-specific path or UID. |
| `paperforge.service.d/10-hardening.conf` | Hardening drop-in: `MemoryHigh`/`MemoryMax`, `KillSignal`, `TimeoutStopSec`, `StartLimitBurst`. |
| `install.sh` | Idempotent installer / uninstaller. |

## Quick install

```sh
# Build + install the binary first.
cargo build --release --workspace --exclude paperforge-gui
install -m 0755 target/release/paperforge ~/.local/bin/paperforge

# Install + enable the unit.
contrib/systemd/install.sh
```

The unit resolves `ExecStart=%h/.local/bin/paperforge daemon` at
activation time, so it points to whatever your home directory is.
If your binary lives elsewhere (e.g. `~/.cargo/bin/paperforge` or
`/usr/local/bin/paperforge`), edit the unit's `ExecStart=` line
before linking.

## Verifying the unit

```sh
systemd-analyze verify ~/.config/systemd/user/paperforge.service
```

A clean run prints nothing. Any complaint (unknown directive,
specifier mismatch, missing binary) lands in the output.

## What gets hardened

The drop-in sets:

- `MemoryHigh=96M` — soft cap; the kernel reclaims when the daemon
  crosses it (slow path).
- `MemoryMax=128M` — hard cap; OOM-killer fires above this.
  Sized for the daemon itself, not its LWE children. To cap the
  entire tree, set `Delegate=yes` on the unit and bump `MemoryMax`
  to ~800M (3 monitors × 250 MiB LWE + 16 MiB daemon).
- `StartLimitBurst=5` / `StartLimitIntervalSec=120` — five restarts
  in two minutes before systemd gives up. Prevents tight crash
  loops from spamming journal.
- `TimeoutStopSec=15` / `KillSignal=SIGTERM` — graceful shutdown
  window for the hotplug watcher and LWE pool.

## Compositor compatibility

`PartOf=niri.service` ties the daemon to the niri lifecycle. If
you use Hyprland or sway, drop that line and replace `After=`/
`Wants=` with the equivalent for your compositor.

## Logs

```sh
journalctl --user -u paperforge.service -f
```

`StandardOutput=journal` + `StandardError=journal` are set in the
unit; logs land in the user journal under the `paperforge`
identifier (set by `SyslogIdentifier` in the drop-in).
