#!/usr/bin/env bash
# Shared env + helpers for the template provisioning phase scripts (10 / 15 / 20 / 30).
# SOURCED (not executed) as the first line of every phase script — so these exports land
# at the top of each script, per the "env inside the scripts, never image ENV" rule:
#
#   * DEBIAN_FRONTEND=noninteractive — apt must never block on a prompt during the build.
#   * SYSTEMD_OFFLINE=1 — the `systemctl mask` / `set-default` calls in phase 10 are pure
#     symlink ops: systemd is NOT PID 1 during `docker build`, so systemctl must run in
#     offline mode (no bus to reach). This is deliberately NOT baked as image ENV — it
#     would otherwise leak into the booted system and confuse the real PID-1 systemd.
export DEBIAN_FRONTEND=noninteractive
export SYSTEMD_OFFLINE=1

# Plain build-log helpers. The exec-era `[ct]` progress protocol (the control-server parsed
# `    [ct] <msg>` lines out of `docker exec`) is gone — this is a straight `docker build`,
# so a step line is just a build-log line.
log()  { echo "  >> $*"; }
warn() { echo "  !! WARN: $*" >&2; }

# apt install that WARNs instead of aborting — reserved for the genuinely optional
# third-party toolbox apps (phase 20). A transient miss there degrades the toolbox; it must
# never sink the whole template build. (Load-bearing steps deliberately do NOT use this.)
apti() { apt-get install -y -qq "$@" || warn "install failed: $*"; }
