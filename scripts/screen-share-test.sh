#!/bin/bash
# Runs langis-calls' screen share test (the portal's picker, then frames
# over PipeWire) in a throwaway headless desktop: sway, PipeWire,
# WirePlumber and xdg-desktop-portal-wlr, on a private D-Bus session.
# Nothing touches the running desktop, if there is one.
#
# Needs (Arch): sway swaybg pipewire wireplumber xdg-desktop-portal
# xdg-desktop-portal-wlr. Run from the repository root:
#   scripts/screen-share-test.sh
set -euo pipefail

if [ -z "${LANGIS_SHARE_SESSION:-}" ]; then
    test=$(cargo test -p langis-calls --lib --no-run --message-format=json 2>/dev/null \
        | grep -o '"executable":"[^"]*langis_calls-[^"]*"' | head -1 | cut -d'"' -f4)
    [ -n "$test" ] || { echo "couldn't build the test" >&2; exit 1; }
    # A short path: sway's socket path must fit in sockaddr_un.
    dir=$(mktemp -d /tmp/lss.XXXX)
    trap 'rm -rf "$dir"' EXIT
    LANGIS_SHARE_SESSION=$dir LANGIS_SHARE_TEST=$test dbus-run-session -- "$0"
    exit
fi

dir=$LANGIS_SHARE_SESSION
mkdir -p "$dir/run" "$dir/config/xdg-desktop-portal-wlr" "$dir/config/xdg-desktop-portal"
chmod 700 "$dir/run"
# Share the one output without asking.
printf '[screencast]\nchooser_type=none\noutput_name=HEADLESS-1\n' > "$dir/config/xdg-desktop-portal-wlr/config"
printf '[preferred]\ndefault=wlr\n' > "$dir/config/xdg-desktop-portal/sway-portals.conf"
echo 'output HEADLESS-1 resolution 1920x1080 bg #2c6bed solid_color' > "$dir/sway.conf"

export XDG_RUNTIME_DIR=$dir/run XDG_CONFIG_HOME=$dir/config XDG_CURRENT_DESKTOP=sway
export WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1
unset DISPLAY WAYLAND_DISPLAY SWAYSOCK

cleanup() { kill $(jobs -p) 2>/dev/null || true; wait 2>/dev/null || true; }
trap cleanup EXIT

pipewire > "$dir/pipewire.log" 2>&1 &
sleep 1
wireplumber > "$dir/wireplumber.log" 2>&1 &
sway -c "$dir/sway.conf" > "$dir/sway.log" 2>&1 &
for _ in $(seq 50); do ls "$XDG_RUNTIME_DIR"/wayland-? > /dev/null 2>&1 && break; sleep 0.2; done
export WAYLAND_DISPLAY=$(basename "$(ls "$XDG_RUNTIME_DIR"/wayland-? | head -1)")
export SWAYSOCK=$(ls "$XDG_RUNTIME_DIR"/sway-ipc.* | head -1)
dbus-update-activation-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP XDG_RUNTIME_DIR XDG_CONFIG_HOME
/usr/lib/xdg-desktop-portal-wlr > "$dir/portal-wlr.log" 2>&1 &
sleep 1.5
/usr/lib/xdg-desktop-portal > "$dir/portal.log" 2>&1 &
sleep 2

# The portal only sends a frame when the screen changes: keep it changing.
(while true; do
    for color in '#2c6bed' '#1b1b1d'; do
        swaymsg output HEADLESS-1 bg "$color" solid_color > /dev/null 2>&1 || true
        sleep 0.3
    done
done) &

"$LANGIS_SHARE_TEST" --ignored --nocapture a_share_delivers_frames_from_the_portal
