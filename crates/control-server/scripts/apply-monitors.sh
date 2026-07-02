#!/usr/bin/env bash
# Runs INSIDE the clone container as root (the control-server streams this over
# `docker exec bash -s`). Apply a new monitor layout to a RUNNING clone WITHOUT
# reprovisioning: rewrite the clone-daemon `RMNG_MONITORS` + the gnome-headless dummy
# mode specs from the config, then restart the headless GNOME session + the daemon
# (which re-creates the virtual monitors and applies the layout at startup). Talks to
# the user's `systemd --user` manager via runuser + its XDG_RUNTIME_DIR/DBus bus.
#   apply-monitors.sh <username> <monitors-csv "WxH+X+Y[*],...">
set -euo pipefail
say(){ echo "    [ct] $*"; }
USER="${1:-rmng}"; MONS="$2"
# dummy mode specs want just WxH (unique, colon-joined) — strip +X+Y and the * primary mark.
MODE_SPECS="$(printf '%s' "$MONS" | tr ',' '\n' | sed -E 's/\+.*$//; s/\*$//' | awk 'NF && !seen[$0]++' | paste -sd: -)"
[ -n "$MODE_SPECS" ] || MODE_SPECS="1920x1080"
U="$(id -u "$USER")"
GU="/home/$USER/.config/systemd/user/gnome-headless.service"
CU="/home/$USER/.config/systemd/user/rmng-clone-daemon.service"
# systemctl --user for the target user: run as them with a session bus address + runtime dir.
uctl(){ runuser -u "$USER" -- env \
  XDG_RUNTIME_DIR="/run/user/$U" \
  DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$U/bus" \
  systemctl --user "$@"; }

say "config monitors=$MONS"
sed -i "s#MUTTER_DEBUG_DUMMY_MODE_SPECS=.*#MUTTER_DEBUG_DUMMY_MODE_SPECS=$MODE_SPECS#" "$GU"
grep -q RMNG_MONITORS "$CU" || sed -i '/RMNG_SOCKET/a Environment=RMNG_MONITORS=x' "$CU"
sed -i "s#RMNG_MONITORS=.*#RMNG_MONITORS=$MONS#" "$CU"
uctl daemon-reload >&2

# Stop the daemon BEFORE bouncing GNOME: otherwise the running daemon loses its Mutter
# session when gnome-shell restarts, crashes, and systemd auto-restarts it — racing our
# explicit restart into the StartLimit and leaving the service failed.
say "restarting headless GNOME"
uctl stop rmng-clone-daemon.service >&2 2>/dev/null || true
uctl restart gnome-headless.service >&2
sleep 6
say "starting clone-daemon"
uctl reset-failed rmng-clone-daemon.service >&2 2>/dev/null || true
uctl start rmng-clone-daemon.service >&2

say "applied monitor layout for $USER"
