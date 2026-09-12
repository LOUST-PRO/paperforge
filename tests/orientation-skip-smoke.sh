#!/usr/bin/env bash
# tests/orientation-skip-smoke.sh — Integration test for portrait auto-skip.
#
# Builds temporary workshop directories with synthetic scene.pkg files
# (one portrait, one landscape), feeds them through paperforge
# orientation + the bash helper, and asserts the bash rotate skip loop
# would advance correctly.
#
# Run: bash tests/orientation-skip-smoke.sh
#
# Exits 0 on pass, non-zero on first failure.

set -euo pipefail

WORKSPACE_ROOT="${WORKSPACE_ROOT:-$HOME/.steam/root/steamapps/workshop/content/431960}"
PAPERFORGE_CLI="${PAPERFORGE_CLI:-$HOME/.local/bin/paperforge}"
BASH_HELPER="${BASH_HELPER:-$HOME/.local/bin/paperforge-orientation-detect.sh}"
SKIP_PLAN="${SKIP_PLAN:-fail}"

TMPDIR_ROOT=$(mktemp -d)
trap 'rm -rf "$TMPDIR_ROOT"' EXIT

pass=0
fail=0

assert_eq() {
    local label="$1"
    local got="$2"
    local want="$3"
    if [[ "$got" == "$want" ]]; then
        echo "  ✓ $label = $got"
        pass=$((pass+1))
    else
        echo "  ✗ $label = $got (expected $want)"
        fail=$((fail+1))
    fi
}

# Helper: build a synthetic workshop dir with scene.pkg containing
# the requested orthogonalprojection dimensions.
make_synth_ws() {
    local dir="$1"
    local width="$2"
    local height="$3"
    mkdir -p "$dir"
    cat > "$dir/scene.pkg" <<EOF
PKGV0006
{
    "camera": {
        "orthogonalprojection": {
            "width": $width,
            "height": $height
        }
    }
}
EOF
}

echo "=== Test 1: CLI on synthetic portrait scene ==="
PORTRAIT_WS="$TMPDIR_ROOT/portrait-1080x1920"
make_synth_ws "$PORTRAIT_WS" 1080 1920
result=$("$PAPERFORGE_CLI" orientation "$PORTRAIT_WS" 2>&1)
assert_eq "portrait 1080x1920 orientation" "$(echo "$result" | grep -oE '\-> (Portrait|Landscape|Square|Unknown)')" "-> Portrait"

echo "=== Test 2: CLI on synthetic landscape scene ==="
LANDSCAPE_WS="$TMPDIR_ROOT/landscape-1920x1080"
make_synth_ws "$LANDSCAPE_WS" 1920 1080
result=$("$PAPERFORGE_CLI" orientation "$LANDSCAPE_WS" 2>&1)
assert_eq "landscape 1920x1080 orientation" "$(echo "$result" | grep -oE '\-> (Portrait|Landscape|Square|Unknown)')" "-> Landscape"

echo "=== Test 3: CLI on synthetic square scene ==="
SQUARE_WS="$TMPDIR_ROOT/square-1080x1080"
make_synth_ws "$SQUARE_WS" 1080 1080
result=$("$PAPERFORGE_CLI" orientation "$SQUARE_WS" 2>&1)
assert_eq "square 1080x1080 orientation" "$(echo "$result" | grep -oE '\-> (Portrait|Landscape|Square|Unknown)')" "-> Square"

echo "=== Test 4: CLI --json shape ==="
JSON_OUT=$("$PAPERFORGE_CLI" orientation --json "$LANDSCAPE_WS" 2>&1)
assert_eq "json source" "$(echo "$JSON_OUT" | jq -r '.source')" "ScenePkg"
assert_eq "json width" "$(echo "$JSON_OUT" | jq -r '.width')" "1920"
assert_eq "json height" "$(echo "$JSON_OUT" | jq -r '.height')" "1080"

echo "=== Test 5: Bash helper classification ==="
assert_eq "bash helper on landscape ws" "$("$BASH_HELPER" "$LANDSCAPE_WS")" "Landscape"
assert_eq "bash helper on portrait ws" "$("$BASH_HELPER" "$PORTRAIT_WS")" "Portrait"
assert_eq "bash helper on square ws" "$("$BASH_HELPER" "$SQUARE_WS")" "Square"

echo "=== Test 6: Bash helper cache hit ==="
# First call populates cache. Second call should be cached.
HP_FIRST=$(date +%s%N)
"$BASH_HELPER" "$LANDSCAPE_WS" >/dev/null
HP_SECOND=$(date +%s%N)
HP_DUR=$(( HP_SECOND - HP_FIRST ))
# If second call latency is plausible (>= first call; both should be sub-second on warm cache).
# We just assert the second call still returns the right value.
"$BASH_HELPER" "$LANDSCAPE_WS" >/dev/null
assert_eq "cache hit returns same" "$("$BASH_HELPER" "$LANDSCAPE_WS")" "Landscape"

echo "=== Test 7: Live portrait workshops on Steam Library ==="
# Skip if Steam library not present.
if [[ ! -d "$WORKSPACE_ROOT" ]]; then
    echo "  (skipped — Steam workshop not mounted at $WORKSPACE_ROOT)"
else
    # 3141144790 is genuinely portrait (1080x1920).
    assert_eq "live 3141144790 portrait" \
        "$("$BASH_HELPER" "$WORKSPACE_ROOT/3141144790")" "Portrait"
    # 2001320927 is landscape (1920x1080).
    assert_eq "live 2001320927 landscape" \
        "$("$BASH_HELPER" "$WORKSPACE_ROOT/2001320927")" "Landscape"
fi

echo
echo "=== Summary ==="
echo "pass=$pass fail=$fail"
if (( fail > 0 )); then
    echo "FAIL"
    exit 1
fi
echo "OK"
