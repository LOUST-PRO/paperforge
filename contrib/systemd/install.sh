#!/usr/bin/env bash
# Install the paperforge systemd user unit + hardening drop-in.
#
# Usage:
#   contrib/systemd/install.sh              # install + enable
#   contrib/systemd/install.sh --no-enable  # install only
#   contrib/systemd/install.sh --uninstall  # remove the unit
#
# Idempotent: re-running with --no-enable overwrites the existing
# files (systemd picks up changes after `daemon-reload`).

set -euo pipefail

UNIT_DST="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
UNIT_FILE="paperforge.service"
DROPIN_DIR="paperforge.service.d"
DROPIN_FILE="10-hardening.conf"
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &> /dev/null && pwd)"

install_unit() {
  mkdir -p "$UNIT_DST/$DROPIN_DIR"
  cp -v "$SCRIPT_DIR/$UNIT_FILE" "$UNIT_DST/$UNIT_FILE"
  cp -v "$SCRIPT_DIR/$DROPIN_DIR/$DROPIN_FILE" "$UNIT_DST/$DROPIN_DIR/$DROPIN_FILE"
  systemctl --user daemon-reload
  echo "installed to $UNIT_DST/"
}

enable_service() {
  systemctl --user enable --now paperforge.service
  systemctl --user status paperforge.service --no-pager || true
}

uninstall() {
  systemctl --user disable --now paperforge.service 2>/dev/null || true
  rm -v -f "$UNIT_DST/$UNIT_FILE" "$UNIT_DST/$DROPIN_DIR/$DROPIN_FILE"
  rmdir "$UNIT_DST/$DROPIN_DIR" 2>/dev/null || true
  systemctl --user daemon-reload
  echo "uninstalled"
}

case "${1:-install}" in
  install|"")         install_unit; enable_service ;;
  --no-enable)        install_unit ;;
  --uninstall|remove) uninstall ;;
  -h|--help)
    sed -n '2,12p' "$0"
    ;;
  *)
    echo "unknown argument: $1" >&2
    exit 2
    ;;
esac
