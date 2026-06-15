#!/bin/bash
# Hardened live visual-validation capture for fp-app.
# Each scenario: launch -> ACTIVATE the window (so the SDL app stays responsive and
# does not beachball while backgrounded) -> wait -> screencapture -> HARD kill -9
# (so no window ever lingers on screen, even if a scenario hangs). Serial (one
# window at a time). Output PNGs in /tmp/fpval/.
set +e
REPO=/Users/sdoumbouya/code/claude-env/FightersParadise
OUT=/tmp/fpval; BIN="$REPO/target/debug/fp-app"; APP=""
mkdir -p "$OUT"

capwin(){ # $1=png : crop to fp-app window if Accessibility geometry available, else full screen
  local GEOM X Y W H
  GEOM=$(osascript -e 'tell application "System Events" to tell process "fp-app" to get {position, size} of window 1' 2>/dev/null)
  if echo "$GEOM" | grep -qE '^-?[0-9]+, *-?[0-9]+, *[0-9]+, *[0-9]+$'; then
    X=$(echo "$GEOM"|awk -F', *' '{print $1}'); Y=$(echo "$GEOM"|awk -F', *' '{print $2}')
    W=$(echo "$GEOM"|awk -F', *' '{print $3}'); H=$(echo "$GEOM"|awk -F', *' '{print $4}')
    screencapture -x -R"$X,$Y,$W,$H" "$1"; echo "  cropped $1 $X,$Y,$W,$H exit=$?"
  else screencapture -x "$1"; echo "  fullscreen $1 (geom='$GEOM') exit=$?"; fi
}
capfull(){ screencapture -x "$1"; echo "  fullscreen $1 exit=$?"; }
launch(){ RUST_LOG=error "$BIN" $1 >/tmp/fpval_run.log 2>&1 & APP=$!; }
front(){ osascript -e 'tell application "System Events" to set frontmost of process "fp-app" to true' 2>/dev/null; }
hardkill(){ kill -9 "$APP" 2>/dev/null; wait "$APP" 2>/dev/null; APP=""; }

echo "=== build ==="; (cd "$REPO" && cargo build -p fp-app 2>&1 | tail -1)
echo "=== 1 KFM (SFF v2 — must be COLOR) ==="; launch "$REPO/test-assets/kfm/kfm.def"; sleep 4; front; sleep 1; capwin "$OUT/01_kfm.png"; hardkill; sleep 1
echo "=== 2 evilken (SFF v1) ==="; launch "$REPO/test-assets/evilken/evilken.def"; sleep 4; front; sleep 1; capwin "$OUT/02_evilken.png"; hardkill; sleep 1
echo "=== 3 title menu (no args) ==="; launch ""; sleep 1; front; sleep 3; capfull "$OUT/03_menu.png"; hardkill; sleep 1
echo "=== 4 keyboard (KFM): before / 15x right-arrow / after ==="; launch "$REPO/test-assets/kfm/kfm.def"; sleep 4; front; sleep 1; capwin "$OUT/04_kbd_before.png"
for i in $(seq 1 15); do osascript -e 'tell application "System Events" to key code 124' 2>/dev/null; done
sleep 0.5; capwin "$OUT/05_kbd_after.png"; hardkill
echo "=== FILES ==="; ls -la "$OUT"/*.png 2>&1; echo "CAPTURE_DONE"
