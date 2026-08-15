#!/usr/bin/env bash
# install-desktop-entry.sh — install paperforge-gui as a discoverable
# XDG application on the current user's desktop.
#
# After this runs, the GUI shows up in:
#   - GNOME Shell / KDE Plasma / elementary OS app launchers
#   - fuzzel / rofi / walker / anyrunner (search "wallpaper" or
#     "paperforge")
#   - The dock / taskbar (with a proper icon, not just a generic
#     gear icon)
#
# Without this, the only way to start paperforge-gui is to remember
# the binary path and run it from a terminal. This script turns it
# into a first-class desktop app.
#
# Where it installs:
#   - Desktop entry:  ~/.local/share/applications/paperforge-gui.desktop
#   - Icon:           ~/.local/share/icons/hicolor/scalable/apps/paperforge-gui.svg
#
# Both are user-scoped (no root needed), so:
#   - Multiple users on the same system each get their own entry.
#   - System-wide install (root, /usr/share/applications) is not
#     covered — that's a packaging concern (Debian package, Flatpak).
#     If you're packaging paperforge, add a `.deb` recipe in
#     `contrib/systemd/` or similar.
#
# What it does to the .desktop file:
#   - Substitutes PAPERFORGE_GUI_EXEC_PLACEHOLDER with the resolved
#     absolute path to the binary. We resolve in this order:
#       1. `command -v paperforge-gui` (if installed on $PATH)
#       2. ./target/release/paperforge-gui (local cargo build)
#       3. ./target/debug/paperforge-gui (local cargo dev build)
#       4. /usr/local/bin/paperforge-gui (manual install)
#     The launcher needs an absolute path; relative paths break
#     when the launcher's cwd differs from the install cwd.
#
# What it runs after copying:
#   - `update-desktop-database` if available (GNOME / KDE): refreshes
#     the .desktop cache so the launcher picks up the new entry.
#   - `gtk-update-icon-cache` if available: refreshes the icon
#     theme cache so the new icon appears immediately.
#   - Both are no-ops if the binaries aren't installed; the script
#     still succeeds in that case (the launchers will pick up the
#     files on next login).
#
# Usage:
#   bash scripts/install-desktop-entry.sh
#   PAPERFORGE_BIN=/custom/path/to/binary bash scripts/install-desktop-entry.sh
#
# Uninstall:
#   rm ~/.local/share/applications/paperforge-gui.desktop
#   rm ~/.local/share/icons/hicolor/scalable/apps/paperforge-gui.svg
#   update-desktop-database ~/.local/share/applications  # if installed

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

DESKTOP_SRC="$REPO_ROOT/assets/paperforge-gui.desktop"
ICON_SRC="$REPO_ROOT/assets/icons/paperforge-gui.svg"

if [[ ! -f "$DESKTOP_SRC" ]]; then
    echo "install-desktop-entry: missing $DESKTOP_SRC" >&2
    exit 1
fi
if [[ ! -f "$ICON_SRC" ]]; then
    echo "install-desktop-entry: missing $ICON_SRC" >&2
    exit 1
fi

# ---- Resolve the binary path -----------------------------------------

resolve_binary() {
    # Honor explicit override first
    if [[ -n "${PAPERFORGE_BIN:-}" ]]; then
        if [[ -x "$PAPERFORGE_BIN" ]]; then
            echo "$PAPERFORGE_BIN"
            return 0
        fi
        echo "PAPERFORGE_BIN is set but not executable: $PAPERFORGE_BIN" >&2
        return 1
    fi

    # Then $PATH lookup (most common case after a `cargo install`)
    if command -v paperforge-gui >/dev/null 2>&1; then
        command -v paperforge-gui
        return 0
    fi

    # Then local cargo build outputs (dev or release)
    for candidate in \
        "$REPO_ROOT/target/release/paperforge-gui" \
        "$REPO_ROOT/target/debug/paperforge-gui"
    do
        if [[ -x "$candidate" ]]; then
            echo "$candidate"
            return 0
        fi
    done

    # Last resort: common system install path
    if [[ -x "/usr/local/bin/paperforge-gui" ]]; then
        echo "/usr/local/bin/paperforge-gui"
        return 0
    fi

    echo "could not find paperforge-gui binary. Set PAPERFORGE_BIN or build it first." >&2
    echo "  tried: \$PATH, target/release, target/debug, /usr/local/bin/paperforge-gui" >&2
    return 1
}

PAPERFORGE_BIN_PATH="$(resolve_binary)"

# ---- Compute XDG destinations ----------------------------------------

XDG_DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
APPS_DIR="$XDG_DATA_HOME/applications"
ICON_DIR="$XDG_DATA_HOME/icons/hicolor/scalable/apps"

mkdir -p "$APPS_DIR" "$ICON_DIR"

# ---- Install .desktop (with Exec= substitution) ----------------------

DESKTOP_DEST="$APPS_DIR/paperforge-gui.desktop"
# sed in-place; the placeholder is unique enough that we don't need
# to anchor. Using | as the sed delimiter because the Exec path may
# contain / (it always does — absolute path).
sed "s|PAPERFORGE_GUI_EXEC_PLACEHOLDER|$PAPERFORGE_BIN_PATH|g" \
    "$DESKTOP_SRC" \
    > "$DESKTOP_DEST"
chmod 0644 "$DESKTOP_DEST"

# ---- Install icon ---------------------------------------------------

ICON_DEST="$ICON_DIR/paperforge-gui.svg"
cp "$ICON_SRC" "$ICON_DEST"
chmod 0644 "$ICON_DEST"

# ---- Refresh caches (best-effort) ------------------------------------

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$APPS_DIR" 2>/dev/null || true
fi

if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t "$XDG_DATA_HOME/icons/hicolor" 2>/dev/null || true
fi

# ---- Report ----------------------------------------------------------

cat <<EOF
✓ Installed paperforge-gui desktop entry

  Desktop entry: $DESKTOP_DEST
  Icon:          $ICON_DEST
  Binary:        $PAPERFORGE_BIN_PATH

Next steps:
  - Open your app launcher (GNOME Shell: Activities / Super key,
    KDE Plasma: Application Launcher, fuzzel/rofi: Mod+Space).
  - Type "paperforge" or "wallpaper".
  - The icon should appear; click to launch.

If the icon doesn't show:
  - Some compositors (GNOME Shell) require a logout/login to pick
    up new .desktop files the first time.
  - For fuzzel/rofi: the cache is auto-refreshed on next invocation.
  - Verify with: cat $DESKTOP_DEST | grep Exec=

Uninstall:
  rm $DESKTOP_DEST
  rm $ICON_DEST
EOF
