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
# Belt-and-suspenders on every exit path, success OR failure: another actor (the binswap
# sweep, a manual /api/monitors/apply racing this one) can land a concurrent bounce of the
# SAME rmng-clone-daemon.service mid-script and abort us between the `stop` and `start`
# below — that's exactly what the deliberately-unguarded restart/start calls further down
# are for (see the comment there); this trap is the safety net for whatever they don't
# catch. It does NOT change the script's exit code (no `exit` inside it, and every command
# in it is `|| true`-guarded) — a real failure still propagates so the caller
# (`provision::apply_monitors`) still surfaces it — it only makes sure the daemon itself is
# left reset-failed + started so a would-be swap has a live target instead of a stranded one.
trap 'uctl reset-failed rmng-clone-daemon.service >&2 2>/dev/null || true; uctl start rmng-clone-daemon.service >&2 2>/dev/null || true' EXIT

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
# One retry: a concurrent redeploy_clone (or another apply-monitors run) touching this same
# unit can land a transient systemd job-conflict on the first attempt. Retry once after a
# short beat for that transient case; a second failure is a real problem and must still
# abort the script (`set -e`) rather than be swallowed — the EXIT trap above still leaves
# the daemon reset-failed + started regardless of how this script ends.
uctl restart gnome-headless.service >&2 2>/dev/null || { sleep 2; uctl restart gnome-headless.service >&2; }
sleep 6
say "starting clone-daemon"
uctl reset-failed rmng-clone-daemon.service >&2 2>/dev/null || true
uctl start rmng-clone-daemon.service >&2

say "applied monitor layout for $USER"
