#!/usr/bin/env bash
# Phase 15 — install the patched gnome-shell .deb (shell-01 hide the screen-sharing
# indicator + shell-03 enable org.gnome.Shell.Eval for the clone-daemon window-management
# MCP tools). The deb is COPYed from the gnome-build stage to /tmp/gnome-shell.deb, so it is
# ALWAYS present — this is a load-bearing RMNG feature (window-mgmt tools + a clean capture),
# so a failed install FAILS the build rather than publishing a degraded template.
#
# Installed with `dpkg -i`, NOT apt: phase 10's Recommends strip already purged gdm3 and
# gnome-remote-desktop, and this deb — a repack of stock gnome-shell — carries the same
# `Recommends: gdm3` that the stock package does, so an `apt install ./deb` would re-drag
# gdm3/g-r-d back in. dpkg installs the (strictly newer, +ngshell) deb straight over the
# stock shell — identical hard deps, already satisfied by phase 10 — without touching
# Recommends. This also means no apt lists are needed here (phase 10 dropped them).
set -euo pipefail
. /setup/lib.sh

DEB=/tmp/gnome-shell.deb
test -f "$DEB"

log "install patched gnome-shell (shell-01 + shell-03)"
dpkg -i "$DEB"
log "patched gnome-shell installed: $(dpkg-query -W -f='${Version}' gnome-shell)"
rm -f "$DEB"
