# LXC→Docker port audit: vestigial machinery

Audited 2026-07-02 on branch `docker-port`. Goal: find logic designed for the Proxmox
LXC backend that was ported as-is to Docker but is unnecessary (or newly built to
replicate an LXC-era shape Docker doesn't need). Decisions below were made by the
operator during the audit; nothing has been implemented yet.

Baseline for judging carryover: clones are now privileged systemd-PID-1 containers on a
user-defined `rmng` bridge (Docker-IPAM static IPs, Docker-managed
resolv.conf/hosts/hostname), created from `rmng/template:*` images via commit. The old
stack drove `pct` over SSH, used DHCP on a LAN bridge, and found the control-server via
a UDP `advertise_ip` trick.

---

## Decisions

### 1. Replace the manual static-IP scheme with Docker DNS + clone self-identification — APPROVED

The LXC stack used DHCP; the Docker port built a *new* manual allocator to get stable
addresses. Docker's embedded DNS resolves container names (== host ids) on the `rmng`
bridge, which removes the need for all of it:

- The allocator + race-retry loop: `crates/control-server/src/provision.rs:281-331`,
  `crates/control-server/src/docker.rs:557-577`. Two concurrent clones can pick the same
  lowest-free IP and Docker rejects the loser at container start ("Address already in
  use"); the bounded retry exists to patch a race the design created.
- The `.2` self-pin + `connect_self_to_network`: `docker.rs:475-492`, plus the
  `networkWarning` plumbing around it. Replace with a network alias on the rmng bridge.
- `SubnetPlan`'s reserved `.1`/`.2`/`.10+` addresses: `docker.rs:1166-1233`.
- Control→clone dials switch to `http://<host.id>:port` (container name == host id):
  `monitor.rs:27`, `chat.rs:44-45`, `mcp.rs:241`.
- The two inbound source-IP reverse lookups need a small protocol change — the clone
  self-identifies with its `clone_id` (it already knows it; the media plane already does
  exactly this with `Hello{clone_id}`):
  - `mcp.rs:220` (per-clone MCP `set_state`: `hosts.find(|h| h.host == peer_ip)`)
  - `web.rs:570-583` (`POST /api/detector-feedback`, same match)
  - sender side: `crates/clone-daemon/src/detector.rs:408` and
    `agent-wrapper/src/server.ts` / `config.ts`.
- `Host.host` becomes a display-only string (kept for legacy/unmanaged rows that address
  a real endpoint by IP). Dev mode (server on the host, not on the bridge) keeps its
  gateway-IP fallback since clones can't resolve a host process by name.

### 2. Boot reconciliation + name addressing for state.json — APPROVED

- `rmng.managed=1` is stamped on every managed container/volume
  (`docker.rs:543,782,822,840`) but **never queried** — there is no `list_containers`
  call anywhere in the crate. The host list, IPs, and container ids are all
  reconstructed from `state.json`, exactly as the Proxmox server tracked CTs by ctid.
- No startup reconciliation: `main.rs:39-79` trusts `state.json` verbatim. A container
  removed behind the server's back stays a "live" host row forever; the only nod to
  drift is the `unmanaged` count log in `state.rs:46-57`.
- Plan: at boot, diff state against `docker ps -a --filter label=rmng.managed=1` — flag
  orphan rows, log unknown containers.
- Drop the stored 64-hex `Host.container` id (`crates/wire/src/control.rs:94-100`):
  container name == host id (`docker.rs:203`, `provision.rs:297-301`), and every bollard
  call accepts name-or-id. Keep a `managed` flag (or derive from the label) for what the
  `Some`/`None` id currently encodes. Touch points when dropping: the delete path
  (`jobs.rs:434,462`) and the create-failure cleanup (`provision.rs:319-321`).
- Also state-derived where Docker could answer: image `in_use_by`
  (`web.rs:358-365`), IP reservations for the allocator (`provision.rs:286-289`) —
  both fall out naturally with decisions 1+2.
- What genuinely belongs in state.json (not reconstructible from Docker): Claude
  account bindings/rotation, Linear metadata, poller-derived UI state (`agent_report`,
  `unread`, …), `selected`, operations log, presets/groups/monitors config.

### 3. Provision-script cleanups — ALL APPROVED

In `crates/control-server/scripts/provision-clone.sh`:

- **Remove the snapd purge/disable block** (`:37-41`). The Ubuntu LXC template shipped
  snapd; the `ubuntu:26.04` Docker rootfs doesn't — the disable/purge/rm are no-ops.
  Keep only the `nosnap.pref` apt pin.
- **Remove `aa-teardown` + apparmor disable** (`:42`). Inert today (userspace not
  installed at that point), and a host-affecting footgun: in a privileged container
  `aa-teardown` unloads profiles from the **shared host kernel** (AppArmor isn't
  namespaced here the way LXC apparmor stacking was).
- **Remove the `systemd-networkd-wait-online` mask** (`:97-103`). Wrong backend — the
  image uses NetworkManager, so the unit never enters boot. The `systemd-udevd` +
  `systemd-udev-trigger` masks on the same line are still correct (privileged container
  sees host uevents) and stay.
- **Remove the `systemctl --user` start tail + `sleep 2`** (`:576-583`). The build
  container is `sleep infinity` — systemd is not PID 1, no user manager exists, all five
  calls no-op into `|| true`. The `sleep 2` is pure dead wait on every base-image build.
  Units start from `default.target.wants` symlinks on first real clone boot.
- **Drop `network-manager` entirely** (`:65`). Docker owns eth0, resolv.conf, and
  /etc/hosts; NM serves only GNOME Settings cosmetics — and it's what drags in
  ModemManager (hence its mask, `:79-84`) and forces the unmanaged-eth0 conf
  (`:86-95`). Removing it deletes that whole chain.
  - Ordering note: this makes the also-approved "mask `NetworkManager-wait-online`"
    item moot. Treat that mask as the fallback if the E2E shows headless GNOME
    misbehaves without NM. **Needs a live desktop check on the staging box before
    landing.**
- **Retag stale LXC comments** on things that stay: the ModemManager mask rationale
  ("in an LXC", `:79-84` — the mask is still correct under Docker, the wording isn't),
  the config-layer "baked into CTs" wording (`config.rs:451`), and the socket
  `chmod 0777` comment about "uid-mapped CTs" (`crates/media/src/sock.rs:26-29`) —
  there is no idmapping under Docker; 0777 may still be wanted (root server vs uid-1000
  daemon) but the comment should say that.

### 4. pid:host clone-home browser — KEEP AS-IS

`homes.rs` + `compose.yaml:27` (`pid: "host"`) recreate the LXC-era "browse every
clone's home on the host" affordance (the sshfs `mounts.rs` successor) via
`data/hosts/<id>` → `/proc/<pid>/root/home/rmng` symlinks. Verified: nothing in
web.rs/files.rs consumes them — it's purely an operator convenience, self-contained
(~200 lines), and opt-in (omit `pid: host` and it's simply off). Keeping.

---

## Smaller findings (recommendations, not yet decided)

- **`cloneSocket` one-time config machinery is mostly ceremony.** The Settings field is
  rendered `readOnly disabled` ("fixed by the container's shared sock volume",
  `SettingsPanel.tsx:737-753`), yet the one-time enforcement
  (`control-server/src/config.rs:458-460`) and restart-required wiring remain. The path
  is dictated by the `rmng-sock` volume mount. *Recommend: hardcode
  `/srv/rmng-sock/clones.sock`, delete the knob.*
- **`sock_source_dir` parses a human-readable string** — recovers the bind source by
  stripping `"mounted from "` off the env-report detail line
  (`provision.rs:943-959`). *Recommend: store the discovered source path as its own
  `EnvReport` field when nearby.*
- **`RSHARED` propagation on the sock bind** (`docker.rs:750-753`). Mount propagation
  affects sub-mounts, not files; a socket file appearing in the dir doesn't need it.
  Likely cargo-culted. *Recommend: drop after an E2E clone create.*
- **Retired-topology fallback IPs.** `http://10.60.0.1:9000/9002` compiled into
  `detector.rs:28-40` and `agent-wrapper/src/config.ts:39`; `10.0.0.42:8080` inference
  default in `wire/src/config.rs:288-290`. All point at the retired Proxmox subnet,
  unreachable from Docker clones — dead-man defaults that mask misconfiguration.
  *Recommend: replace with empty/disabled defaults.*
- **`migrate_legacy` proxmox config folding** (`control-server/src/config.rs:51-100`) —
  exists solely to read a Proxmox-era config.json (envPresets→presets, legacy `linear`
  keys, `cloneAccounts`, `proxmox.hostnamePrefix` fold). *Recommend: keep until
  staging/prod configs have been rewritten once, then delete.*
- **LXC-parity resource defaults.** Clone CPU/mem defaults and the `+8 GiB` swap
  constant are documented as "matching LXC parity" (`docker.rs:82`,
  `wire/src/config.rs:151-172`). Inherited numbers, not wrong. *Recommend: retune when
  convenient.*
- **`b64_encode` hand-rolled, "ported verbatim from orchestrate.rs"**
  (`provision.rs:82-94`) — carried from the bash-protocol era; the `base64` crate is the
  idiomatic replacement. *Cosmetic.*

---

## Considered and cleared (not vestigial — do not remove)

- **The `/srv/rmng-sock` unix media socket.** Carries dmabuf GPU fds via `SCM_RIGHTS`
  alongside each frame (`wire/src/socket.rs:1-7`, `media/src/sock.rs:46-67`,
  `clone-daemon/src/transport.rs:30-38`). TCP has no fd-passing, so replacing it would
  forfeit the zero-copy dmabuf→VA-API encode path. All the sock-volume machinery
  (named volume, per-clone bind, `sockMount` env check, `RMNG_SOCKET`) serves this
  transport and stays. The `Hello{clone_id}` routing exists because one shared socket
  multiplexes the fleet — also stays.
- **Machine-id handling.** Fresh random id injected per clone (`provision.rs:362-367`)
  and truncated at commit (`provision.rs:686-694`) are Docker-era fixes —
  systemd-in-docker won't persist a generated id into an empty `/etc/machine-id`. The
  commit-time truncate is technically redundant with injection-at-clone but is cheap
  defense; keep. The `/var/lib/dbus/machine-id` symlink (`provision-clone.sh:76`) is a
  Docker fix (0ee5c0a), not carryover.
- **Base-image bootstrap via exec+commit** rather than `docker build`. Shaped like the
  old "provision a CT, template it" flow, but the product's whole image model is
  commit-based (commit-from-clone), so the shared plumbing earns its keep; layer caching
  buys little for a one-shot wizard build. Keep.
- Also fine (Docker-era, genuinely needed): dind + containerd-store per-clone volumes,
  `StopSignal=SIGRTMIN+3` boot config baked at commit, udevd/udev-trigger masks,
  locale/timezone setup (minimal base image has none), stock-ubuntu uid-1000 eviction +
  `rmng` pinned to uid 1000, linger via `/var/lib/systemd/linger`, `container=docker`
  marker env.

---

## Suggested implementation order

1. **Scripts cleanup** (decision 3) — isolated, no control-plane changes; verify with a
   base-image rebuild + one clone E2E on staging (the NM removal is the only item with
   desktop-visible risk).
2. **Boot reconciliation + name addressing** (decision 2) — control-server only.
3. **DNS + self-ID addressing** (decision 1) — touches the clone protocol
   (detector-feedback + `set_state` senders), so it wants a full E2E pass and a clone
   re-provision on staging; existing clones keep working during rollout only if the
   handlers accept both self-ID and source-IP matching during a transition window.
