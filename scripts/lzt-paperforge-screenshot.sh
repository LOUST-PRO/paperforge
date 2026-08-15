#!/usr/bin/env bash
# lzt-paperforge-screenshot.sh — capture a screenshot of the
# paperforge-gui window for the agent to inspect.
#
# Operator flow:
#   1. Run `make build-gui` (or `cargo run -p paperforge-gui`).
#   2. Run `bash scripts/lzt-paperforge-screenshot.sh` from any
#      workspace / terminal. The script will find the paperforge-gui
#      window via Niri IPC and screenshot it without you needing to
#      alt-tab to it first.
#   3. Output is the absolute path to the PNG, ready for
#      `Read tool image_path=...`.
#
# Why this matters (PR 9.6 follow-up):
#   Niri 25.05+ exposes `niri msg action screenshot-window --id <ID>`
#   which screenshots a window by its DBus id without focusing it.
#   Before that, capturing a window required focusing it first (via
#   `niri msg action focus-window --id <ID>` + grim of the focused
#   output) which clobbered the operator's current focus. This script
#   prefers the per-window screenshot action when available so the
#   operator can stay focused on whatever they're working on.
#
# Multi-backend:
#   - Niri (primary): use `niri msg action screenshot-window --id <id>`
#     to capture directly without changing focus.
#   - swaymsg fallback: capture focused output (operator must focus
#     paperforge-gui first). sway doesn't expose per-window screenshot
#     via IPC.
#   - hyprctl fallback: same limitation as sway. Use `hyprctl
#     activewindow` for geometry + grim -g.
#   - X11: `import -window root` as last resort.
#
# This script is gitignored (`scripts/lzt-paperforge-*.sh` pattern)
# — it's an operator tool for the dev loop, not something that
# ships in the repo. Adding a similar helper for another GUI
# crate? Mirror this shape (find window by app_id → capture → print
# path → exit).

set -euo pipefail

OUT_DIR="${TMPDIR:-/tmp}"
OUT_FILE="$(mktemp -p "$OUT_DIR" -t paperforge-gui-screenshot-XXXXXX.png)"
APP_ID="${PAPERFORGE_APP_ID:-paperforge-gui}"

die() {
    echo "lzt-paperforge-screenshot: $*" >&2
    exit "${2:-1}"
}

# ---- Niri: per-window screenshot via IPC (primary) -----------------
#
# This is the cleanest path on Niri 25.05+: the IPC action screenshots
# the window by its DBus id WITHOUT requiring focus, so the operator
# can keep working in whatever app they're currently focused on. No
# focus restoration, no grim geometry math, no edge cases with the
# window being on a non-focused workspace.

if command -v niri >/dev/null 2>&1 && command -v jq >/dev/null 2>&1; then
    # Enumerate all windows and find the one matching our app_id.
    # niri returns `app_id` as a string (e.g. "paperforge-gui" or
    # "com.louzt.PaperforgeGUI" depending on the GTK app_id override
    # we set). We do an exact match first, then fall back to a
    # substring match for the rare case where GTK appends a
    # version/arch suffix.
    WIN_JSON="$(niri msg --json windows 2>/dev/null || true)"
    if [[ -z "$WIN_JSON" || "$WIN_JSON" == "null" ]]; then
        die "niri msg --json windows returned no data (compositor IPC down?)" 2
    fi

    WIN_ID="$(printf '%s' "$WIN_JSON" | jq -r --arg id "$APP_ID" \
        '[.[] | select(.app_id == $id)][0].id // empty' 2>/dev/null)"

    if [[ -z "$WIN_ID" || "$WIN_ID" == "null" ]]; then
        # Fallback: substring match on app_id (e.g. if the binary
        # reports "com.louzt.PaperforgeGUI.debug" instead of
        # "paperforge-gui").
        WIN_ID="$(printf '%s' "$WIN_JSON" | jq -r --arg id "$APP_ID" \
            '[.[] | select(.app_id | contains($id))][0].id // empty' 2>/dev/null)"
    fi

    if [[ -z "$WIN_ID" || "$WIN_ID" == "null" ]]; then
        die "no window found for app_id=$APP_ID (launch paperforge-gui first)" 2
    fi

    # Use the per-window screenshot action. --path makes niri write
    # the PNG directly to our temp file (no clipboard pollution).
    # --id targets the specific window by DBus id; niri doesn't
    # require focus for this.
    if ! niri msg action screenshot-window --id "$WIN_ID" --path "$OUT_FILE" >/dev/null 2>&1; then
        # Some Niri builds (pre-25.05) don't expose --id. Try the
        # fallback: focus + capture focused window.
        if ! PREV_FOCUS_JSON="$(niri msg --json focused-window 2>/dev/null)"; then
            die "niri msg --json focused-window failed" 2
        fi

        if ! niri msg action focus-window --id "$WIN_ID" >/dev/null 2>&1; then
            die "focus-window failed (Niri build too old for screenshot-window --id?)" 2
        fi
        # Capture whatever output is now focused (which is the
        # paperforge-gui window we just focused).
        if command -v grim >/dev/null 2>&1; then
            grim "$OUT_FILE"
        else
            die "niri focus succeeded but grim not installed (fallback)" 3
        fi
        # Best-effort restore of previous focus — only works if the
        # previous window id is available. If the operator wasn't
        # focused on any window, skip.
        PREV_ID="$(printf '%s' "$PREV_FOCUS_JSON" | jq -r '.id // empty' 2>/dev/null || true)"
        if [[ -n "$PREV_ID" && "$PREV_ID" != "null" && "$PREV_ID" != "$WIN_ID" ]]; then
            niri msg action focus-window --id "$PREV_ID" >/dev/null 2>&1 || true
        fi
    fi

    echo "$OUT_FILE"
    exit 0
fi

# ---- Sway fallback (focus + grim of focused output) ----------------

if command -v swaymsg >/dev/null 2>&1 && command -v grim >/dev/null 2>&1; then
    # Sway doesn't expose per-window screenshots via IPC. We have to
    # focus the target window, capture the focused output, then
    # optionally restore the previous focus. Use the focused tree
    # node + recurse to find our app_id.
    PREV_FOCUS_ID=$(swaymsg -t get_tree 2>/dev/null \
        | jq -r '.. | objects | select(.focused == true) | .id // empty' \
        | head -1 || true)

    # Find any window matching our app_id and focus it.
    TARGET_ID=$(swaymsg -t get_tree 2>/dev/null \
        | jq -r --arg id "$APP_ID" '.. | objects | select(.app_id == $id) | .id' \
        | head -1 || true)

    if [[ -z "$TARGET_ID" ]]; then
        die "no window found for app_id=$APP_ID (focus the paperforge-gui window first)" 2
    fi

    swaymsg "[con_id=$TARGET_ID] focus" >/dev/null 2>&1 || true
    grim "$OUT_FILE"

    # Restore previous focus if possible
    if [[ -n "$PREV_FOCUS_ID" && "$PREV_FOCUS_ID" != "$TARGET_ID" ]]; then
        swaymsg "[con_id=$PREV_FOCUS_ID] focus" >/dev/null 2>&1 || true
    fi

    echo "$OUT_FILE"
    exit 0
fi

# ---- Hyprland fallback (focus + grim of geometry) -----------------

if command -v hyprctl >/dev/null 2>&1 && command -v grim >/dev/null 2>&1 && command -v jq >/dev/null 2>&1; then
    CLIENTS=$(hyprctl clients -j 2>/dev/null || true)
    TARGET=$(printf '%s' "$CLIENTS" | jq -r --arg id "$APP_ID" \
        '[.[] | select(.class == $id or .initialClass == $id)][0]' 2>/dev/null || true)

    if [[ -z "$TARGET" || "$TARGET" == "null" ]]; then
        die "no window found for class=$APP_ID (focus the paperforge-gui window first)" 2
    fi

    ADDR=$(printf '%s' "$TARGET" | jq -r '.address // empty')
    AT=$(printf '%s' "$TARGET" | jq -r '.at | "\(.[0]),\(.[1])"')
    SIZE=$(printf '%s' "$TARGET" | jq -r '.size | "\(.[0])x\(.[1])"')
    IFS=',' read -r X Y <<<"$AT"
    IFS='x' read -r W H <<<"$SIZE"
    GRIM_GEOMETRY="${W}x${H}+${X}+${Y}"

    grim -g "$GRIM_GEOMETRY" "$OUT_FILE"
    echo "$OUT_FILE"
    exit 0
fi

# ---- Generic grim (full focused output, no app targeting) ---------

if command -v grim >/dev/null 2>&1; then
    die "no compositor IPC found (niri, swaymsg, hyprctl) — capturing full output as fallback" 0 \
        || true
    grim "$OUT_FILE"
    echo "$OUT_FILE"
    exit 0
fi

# ---- X11 fallback -------------------------------------------------

if command -v import >/dev/null 2>&1; then
    import -window root "$OUT_FILE"
    echo "$OUT_FILE"
    exit 0
fi

die "no screenshot tool found (install grim + niri, or grim + swaymsg, or grim + hyprctl, or import)" 3
