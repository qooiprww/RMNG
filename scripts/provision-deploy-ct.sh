#!/usr/bin/env bash
# Provision a lean DEPLOYMENT CT for the control-server. Runs LOCALLY. Creates an
# Ubuntu CT with only the RUNTIME (GStreamer/VA, no compilers), copies the
# already-built binary out of the BUILD CT into it, installs the systemd unit +
# minimal config, generates the orchestration SSH key and authorizes it on the
# Proxmox node, and starts the service.
#
#   ./provision-deploy-ct.sh [flags] <proxmox-ssh-target> [hostname] [build-ct-hostname]
#   e.g. ./provision-deploy-ct.sh root@10.0.0.100 rmng-control rmng-build
#
# Flags (all optional; put before the positionals):
#   --storage <name>          rootfs storage             (default local-lvm)
#   --bridge <name>           network bridge             (default vmbr0)
#   --template <file>         CT template                (default ubuntu-26.04-…)
#   --cores <n>               CPU cores                  (default 4)
#   --memory <mb>             RAM in MiB                 (default 4096)
#   --rootfs-gb <gb>          rootfs size in GiB         (default 12)
#   --sock-dir <path>         host clone-socket dir      (default /srv/rmng-sock)
#   --proxmox-from-ct <tgt>   ssh target the CT uses     (default root@<node>)
#
# Prereq: run provision-build-ct.sh first (so the build CT has the binary). The
# deploy CT gets the GPU render node + a host dir bind-mounted at /run/ng (the
# clone media socket, shared with clone CTs). storage/bridge/sock-dir are also
# prefilled into the CT's config so the first-run wizard matches the real infra.
#
# NOTE: real provisioning; operator-supervised on first run.
set -euo pipefail

usage(){ sed -n '2,26p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2; exit 2; }

STORAGE="local-lvm"
BRIDGE="vmbr0"
TEMPLATE="ubuntu-26.04-standard_26.04-1_amd64.tar.zst"
CORES="4"
MEMORY="4096"
ROOTFS_GB="12"
SOCK_HOST_DIR="/srv/rmng-sock"   # host dir bind-mounted at /run/ng (+ into clones)
PROXMOX_FROM_CT=""               # SSH target the control-server uses from inside the CT (default root@<node>)

POS=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --storage)         STORAGE="$2"; shift 2;;
    --bridge)          BRIDGE="$2"; shift 2;;
    --template)        TEMPLATE="$2"; shift 2;;
    --cores)           CORES="$2"; shift 2;;
    --memory)          MEMORY="$2"; shift 2;;
    --rootfs-gb)       ROOTFS_GB="$2"; shift 2;;
    --sock-dir)        SOCK_HOST_DIR="$2"; shift 2;;
    --proxmox-from-ct) PROXMOX_FROM_CT="$2"; shift 2;;
    -h|--help)         usage;;
    --) shift; while [[ $# -gt 0 ]]; do POS+=("$1"); shift; done;;
    -*) echo "unknown flag: $1" >&2; usage;;
    *)  POS+=("$1"); shift;;
  esac
done
set -- "${POS[@]:-}"

[[ -n "${1:-}" ]] || usage
PROXMOX="$1"
HOSTNAME="${2:-rmng-control}"
BUILD_HOST="${3:-rmng-build}"
: "${PROXMOX_FROM_CT:=root@${PROXMOX#*@}}"

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"   # rmng/
say(){ printf '\n\033[1;36m==>\033[0m %s\n' "$*"; }

say "copying deploy setup script to $PROXMOX"
scp -q "$here/scripts/cs-deploy-ct.sh" "$PROXMOX:/tmp/cs-deploy-ct.sh"

say "creating runtime CT + copying binary from build CT '$BUILD_HOST'…"
ssh "$PROXMOX" \
  HOSTNAME="$HOSTNAME" BUILD_HOST="$BUILD_HOST" STORAGE="$STORAGE" BRIDGE="$BRIDGE" \
  TEMPLATE="$TEMPLATE" CORES="$CORES" MEMORY="$MEMORY" ROOTFS_GB="$ROOTFS_GB" \
  SOCK_HOST_DIR="$SOCK_HOST_DIR" PROXMOX_FROM_CT="$PROXMOX_FROM_CT" \
  'bash -s' <<'NODE'
set -euo pipefail
prog(){ printf '\033[1;32mP\033[0m %s\n' "$*" >&2; }

# Locate the build CT by hostname (Name is the last column of `pct list`).
BUILD_ID="$(pct list | awk -v h="$BUILD_HOST" 'NR>1 && $NF==h {print $1}' | head -1)"
[ -n "$BUILD_ID" ] || { echo "build CT '$BUILD_HOST' not found — run provision-build-ct.sh first" >&2; exit 1; }
pct exec "$BUILD_ID" -- test -x /usr/local/bin/rmng-control-server \
  || { echo "build CT has no /usr/local/bin/rmng-control-server — build it first" >&2; exit 1; }

NAME="${TEMPLATE##*/}"
pveam list local 2>/dev/null | grep -q "$NAME" || { pveam update >/dev/null 2>&1 || true; pveam download local "$NAME" >/dev/null 2>&1 || true; }
TMPL="local:vztmpl/$NAME"

ID="$(pvesh get /cluster/nextid 2>/dev/null || true)"
[ -n "$ID" ] || { for i in $(seq 200 999); do pct status "$i" >/dev/null 2>&1 || { ID=$i; break; }; done; }
[ -n "$ID" ] || { echo "no free CT id" >&2; exit 1; }

# World-writable so uid-mapped clone CTs can create/connect to the socket here.
mkdir -p "$SOCK_HOST_DIR"; chmod 0777 "$SOCK_HOST_DIR"
prog "pct create $ID ($HOSTNAME) — runtime only"
pct create "$ID" "$TMPL" \
  --hostname "$HOSTNAME" --unprivileged 1 --features nesting=1,keyctl=1,fuse=1 \
  --cores "$CORES" --memory "$MEMORY" --swap 2048 --rootfs "$STORAGE:$ROOTFS_GB" \
  --net0 "name=eth0,bridge=$BRIDGE,ip=dhcp" --onboot 1 >&2
# GPU render node (VA-API encode) + apparmor opt-out + shared clone-socket dir.
# Mount at the SAME path (not under /run — the CT's tmpfs would shadow it).
{
  echo 'dev0: /dev/dri/renderD128,gid=993,mode=0666'
  echo 'lxc.apparmor.profile: unconfined'
  echo "mp0: $SOCK_HOST_DIR,mp=$SOCK_HOST_DIR"
} >> "/etc/pve/lxc/$ID.conf"

prog "starting CT $ID"
pct start "$ID" >&2
prog "waiting for DHCP + DNS"
IP=""
for _ in $(seq 1 60); do
  IP="$(pct exec "$ID" -- hostname -I 2>/dev/null | tr ' ' '\n' | grep -E '^[0-9]' | head -1 || true)"
  [ -n "$IP" ] && pct exec "$ID" -- getent hosts archive.ubuntu.com >/dev/null 2>&1 && break
  sleep 2
done
[ -n "$IP" ] || { echo "no DHCP lease" >&2; exit 1; }
RG="$(pct exec "$ID" -- getent group render 2>/dev/null | cut -d: -f3 || true)"
if [ -n "$RG" ] && [ "$RG" != 993 ]; then
  sed -i "s#renderD128,gid=[0-9]*#renderD128,gid=$RG#" "/etc/pve/lxc/$ID.conf"
  pct stop "$ID" >&2; pct start "$ID" >&2; sleep 5
fi

prog "copying the self-contained control-server from build CT $BUILD_ID → deploy CT $ID"
# One binary: it embeds the frontend + the gzipped clone-daemon + agent-wrapper,
# which it distributes to clones during provisioning. Nothing else to copy.
pct pull "$BUILD_ID" /usr/local/bin/rmng-control-server /tmp/rmng-control-server.bin >&2
pct exec "$ID" -- mkdir -p /usr/local/bin >&2
pct push "$ID" /tmp/rmng-control-server.bin /usr/local/bin/rmng-control-server >&2
pct exec "$ID" -- chmod 755 /usr/local/bin/rmng-control-server >&2
rm -f /tmp/rmng-control-server.bin

prog "configuring (cs-deploy-ct.sh)"
pct push "$ID" /tmp/cs-deploy-ct.sh /root/cs-deploy-ct.sh >&2
pct exec "$ID" -- bash /root/cs-deploy-ct.sh "$PROXMOX_FROM_CT" "$SOCK_HOST_DIR" "$STORAGE" "$BRIDGE" >&2

prog "authorizing the control-server's key on the Proxmox node"
PUB="$(pct exec "$ID" -- cat /root/.ssh/id_ed25519.pub)"
install -d -m700 /root/.ssh; touch /root/.ssh/authorized_keys; chmod 600 /root/.ssh/authorized_keys
grep -qF "$PUB" /root/.ssh/authorized_keys || echo "$PUB" >> /root/.ssh/authorized_keys
rm -f /tmp/cs-deploy-ct.sh
echo "RESULT $ID $IP"
NODE

say "deploy CT ready (RESULT <id> <ip> above)."
echo "Dashboard: http://<ip>:9000  → open Settings to enter Linear / Claude (import accounts from a signed-in clone)."
echo
echo "Clone-side reminder: each clone CT must bind-mount the same host dir so the"
echo "clone-daemon finds the media socket —  mp0: $SOCK_HOST_DIR,mp=$SOCK_HOST_DIR"
