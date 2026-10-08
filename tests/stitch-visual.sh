#!/usr/bin/env bash
# Make a native-size, whole-client GTK-left / GPUI-right comparison image.
# Run in the headless image with ImageMagick; never capture the live desktop.
set -euo pipefail
: "${GTK_IMAGE:?GTK full-window PNG required}"
: "${GPUI_IMAGE:?GPUI 780x791 client PNG required}"
: "${OUTPUT:?output PNG required}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

gtk_source="$GTK_IMAGE"
if [[ -n "${GTK_ROOT_IMAGE:-}" ]]; then
  # GTK popovers are separate X11 surfaces and are absent from import -window.
  # The root capture has black outside its surfaces on headless Xvfb; fill
  # those holes from the application capture before extracting its client.
  convert "$GTK_IMAGE" \
    \( "$GTK_ROOT_IMAGE" -crop 790x800+0+0 +repage -transparent black \) \
    -compose over -composite "$work/gtk-with-popover.png"
  gtk_source="$work/gtk-with-popover.png"
fi

convert "$gtk_source" -crop 780x791+5+4 +repage "$work/gtk-client.png"
[[ "$(identify -format '%wx%h' "$work/gtk-client.png")" == 780x791 ]]
[[ "$(identify -format '%wx%h' "$GPUI_IMAGE")" == 780x791 ]]
convert "$work/gtk-client.png" -size 8x791 xc:'#c8c3ba' "$GPUI_IMAGE" \
  +append "$OUTPUT"
[[ "$(identify -format '%wx%h' "$OUTPUT")" == 1568x791 ]]
