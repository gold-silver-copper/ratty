#!/usr/bin/env bash
set -euo pipefail

ARTIFACTS="$PWD/smoke-artifacts"
mkdir -p "$ARTIFACTS"

Xvfb :99 -screen 0 1280x900x24 -nolisten tcp >"$ARTIFACTS/xvfb.log" 2>&1 &
XVFB_PID=$!
export DISPLAY=:99
export WINIT_UNIX_BACKEND=x11
export WGPU_BACKEND=vulkan
export LIBGL_ALWAYS_SOFTWARE=1
export RUST_BACKTRACE=1
LVP_ICD=$(find /usr/share/vulkan/icd.d -name 'lvp_icd*.json' -print -quit)
export VK_ICD_FILENAMES="$LVP_ICD"

openbox >"$ARTIFACTS/openbox.log" 2>&1 &
OPENBOX_PID=$!

RATTY_PID=""
cleanup() {
  if [[ -n "$RATTY_PID" ]]; then
    kill "$RATTY_PID" 2>/dev/null || true
  fi
  kill "$OPENBOX_PID" "$XVFB_PID" 2>/dev/null || true
}
trap cleanup EXIT

vulkaninfo --summary >"$ARTIFACTS/vulkan-info.txt" 2>&1

RUST_LOG=info ./target/debug/ratty \
  --config-file config/ratty.toml \
  --title "Ratty GPU Smoke" \
  --command ./widget/target/debug/examples/big_rat \
  >"$ARTIFACTS/ratty.log" 2>&1 &
RATTY_PID=$!

WINDOW_ID=""
for _ in $(seq 1 120); do
  WINDOW_ID=$(xdotool search --onlyvisible --name '^Ratty GPU Smoke$' 2>/dev/null | head -n 1 || true)
  if [[ -n "$WINDOW_ID" ]]; then
    break
  fi
  if ! kill -0 "$RATTY_PID" 2>/dev/null; then
    cat "$ARTIFACTS/ratty.log"
    exit 1
  fi
  sleep 0.5
done
if [[ -z "$WINDOW_ID" ]]; then
  echo "Ratty window did not become visible" >&2
  cat "$ARTIFACTS/ratty.log"
  exit 1
fi

xdotool windowfocus --sync "$WINDOW_ID"
sleep 8

capture() {
  local name=$1
  sleep 2
  import -silent -window "$WINDOW_ID" "$ARTIFACTS/$name.png"
  test -s "$ARTIFACTS/$name.png"
}

resize_and_font_zoom() {
  local name=$1
  xdotool windowsize --sync "$WINDOW_ID" 1100 700
  xdotool key --window "$WINDOW_ID" ctrl+equal
  capture "$name"
  xdotool key --window "$WINDOW_ID" ctrl+alt+0
  xdotool windowsize --sync "$WINDOW_ID" 960 620
  sleep 2
}

# big_rat starts in orthographic mode and renders an animated inline RGP model.
capture 01-rgp-ortho-initial
resize_and_font_zoom 02-rgp-ortho-resize-font

# Application-issued RGP camera update to perspective, followed by orbit, pan, and zoom.
xdotool key --window "$WINDOW_ID" v
capture 03-rgp-perspective
xdotool mousemove --window "$WINDOW_ID" 470 310
xdotool mousedown 1
xdotool mousemove --window "$WINDOW_ID" 590 370
xdotool mouseup 1
xdotool mousemove --window "$WINDOW_ID" 500 340
xdotool mousedown 3
xdotool mousemove --window "$WINDOW_ID" 560 300
xdotool mouseup 3
xdotool click 4
capture 04-rgp-perspective-mouse
resize_and_font_zoom 05-rgp-perspective-resize-font

xdotool key --window "$WINDOW_ID" v
sleep 3
capture 06-rgp-mobius
resize_and_font_zoom 07-rgp-mobius-resize-font

xdotool key --window "$WINDOW_ID" v
capture 08-rgp-flat
resize_and_font_zoom 09-rgp-flat-resize-font

xdotool key --window "$WINDOW_ID" v
capture 10-rgp-ortho-return

# Exercise Ratty's own keyboard mode bindings independently of the application protocol.
xdotool key --window "$WINDOW_ID" ctrl+alt+p
capture 11-keyboard-perspective
xdotool key --window "$WINDOW_ID" ctrl+alt+m
sleep 3
capture 12-keyboard-mobius
xdotool key --window "$WINDOW_ID" ctrl+alt+m
sleep 3
capture 13-keyboard-mobius-exit
xdotool key --window "$WINDOW_ID" ctrl+alt+Return
capture 14-keyboard-ortho
xdotool key --window "$WINDOW_ID" ctrl+alt+Return
capture 15-keyboard-flat

# A typed application update forces PTY output, texture redraw, and RGP material refresh.
xdotool key --window "$WINDOW_ID" bracketright
capture 16-pty-redraw-brightness

identify -format '%f %wx%h colors=%k mean=%[fx:mean] deviation=%[fx:standard_deviation]\n' \
  "$ARTIFACTS"/*.png >"$ARTIFACTS/image-stats.txt"

xdotool key --window "$WINDOW_ID" q
for _ in $(seq 1 40); do
  if ! kill -0 "$RATTY_PID" 2>/dev/null; then
    wait "$RATTY_PID"
    RATTY_PID=""
    break
  fi
  sleep 0.5
done
if [[ -n "$RATTY_PID" ]]; then
  echo "Ratty did not exit after the demo quit" >&2
  exit 1
fi

if grep -E 'panicked|Unable to find a GPU|Encountered an error' "$ARTIFACTS/ratty.log"; then
  exit 1
fi
