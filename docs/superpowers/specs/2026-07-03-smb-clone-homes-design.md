# SMB share for clone homes — design

**Date:** 2026-07-03
**Status:** Approved (brainstorm), pending implementation plan
**Replaces:** the sshfs/sftp "browse clone homes" access method in `docs/DEPLOY.md`.

## Summary

Run a Samba (`smbd`) server **inside the control-server container** exposing a single
read-write SMB share whose root is the list of clone ids. `smb://<host>/clones` from a Mac
(Finder ⌘K) lists every running managed clone as a directory (`<id>/…`), each rooted at that
clone's `/home/rmng`. This replaces the sshfs-to-the-Docker-host recipe with a method that
mounts natively in Finder (no macFUSE / kernel extension) and needs no client-side setup.

It works from *inside* the container — not just on the Docker host — because the control-server
already runs with `pid: "host"` (compose.yaml), so `/proc/<clone-pid>/root` resolves in this
namespace. That is the same property the clone-home reconciler (`homes.rs`) already relies on.

## Requirements (locked during brainstorm)

| Decision | Choice |
|---|---|
| Transport | SMB (Samba `smbd`), inside the control-server container |
| Share root | one share (`clones`) whose root lists `data/hosts/<id>` dirs |
| Auth | **fixed built-in credential** — SMB user `rmng`, baked-in password; no config surface |
| Access mode | **read-write**, created files owned by the clone's `rmng` user (uid 1000) |
| On/off | **always on**, gated only by `pid: host` (empty share without it) — no toggle/config |
| Port | **445** (standard); Finder `smb://<host>` with no port suffix; `-p 445:445` |
| Supervision | the control-server binary spawns & supervises `smbd` (single-binary ENTRYPOINT preserved) |

**Explicitly out of scope (YAGNI):** per-clone shares, config.json toggle, wizard/Settings UI,
configurable credentials or port, read-only mode.

## Architecture

```
Mac Finder ──smb://<host>:445/clones──▶ smbd (in control-server container)
                                          │  share path = /data/hosts
                                          ▼
                              data/hosts/<id>  (symlink, maintained by homes.rs)
                                          │  → /proc/<clone-pid>/root/home/rmng
                                          ▼
                              clone container rootfs (via shared pid namespace)
```

- The control-server binary remains the container's sole `ENTRYPOINT`. `smbd` is a child
  process it spawns and supervises — no shell entrypoint, no supervisord/s6.
- The share serves the existing `data/hosts` directory of symlinks. No new reconciler; the SMB
  layer only reads what `homes.rs` maintains (with one possible change to pid selection — see
  the ownership spike).

## Components

### New: `crates/control-server/src/smb.rs`

One module, three responsibilities, spawned from `main.rs` next to `homes::run`:

1. **Config generation** — `fn render_smb_conf() -> String`, pure and unit-testable, written to
   `/etc/samba/smb.conf` (or `/data`) at startup.
2. **Account provisioning** — ensure a local `rmng` user (uid 1000) exists, then set its SMB
   password once via `smbpasswd -a` (idempotent).
3. **Supervisor** — spawn `smbd --foreground --no-process-group`; pipe stdout/stderr to the
   `smb` tracing target; on exit, restart with capped backoff (`30s · 2^failures`, capped ~5 min).

`main.rs` gains `tokio::spawn(smb::run(app.clone()))`. Unconditional (always-on); harmless
without `pid: host` (empty `data/hosts`).

### `smb.conf` (shape)

```ini
[global]
   server min protocol = SMB2          # macOS needs SMB2/3
   unix extensions = no                # required, else `wide links` is ignored
   allow insecure wide links = yes
   security = user
   smb ports = 445
   load printers = no
   printing = bsd
   disable spoolss = yes
   vfs objects = catia fruit           # tidy macOS xattr / AppleDouble handling
   log level = 1

[clones]
   path = /data/hosts
   read only = no
   wide links = yes                    # follow the absolute /proc/* symlinks
   follow symlinks = yes
   force user = rmng                    # created files owned by uid 1000
   force group = rmng
   valid users = rmng
```

Load-bearing lines (asserted by a unit test): `wide links = yes`, `unix extensions = no`,
`force user = rmng`, `path = /data/hosts`, `server min protocol = SMB2`.

### `Dockerfile` (runtime stage)

- Add `samba` (`smbd` + `vfs_fruit`/`catia`) to the apt install list. This **partly reverses**
  the current comment that the image deliberately omits SSH/sshfs; update that comment to
  explain SMB now serves clone homes (still no SSH).
- Create user `rmng` at **uid 1000** in the runtime stage (so `force user = rmng` maps to the
  clone's uid; clones aren't userns-remapped, so uid 1000 is consistent everywhere).
- `EXPOSE 9000-9003 9005 445`.

### `compose.yaml` + the `docker run` one-liner comment

- Add `- "445:445"` to `ports:` and `-p 445:445` to the one-liner in the header comment.

## Auth

Fixed built-in credential: SMB user `rmng` with a baked-in password (proposed: `rmng`). Same on
every deployment — consistent with RMNG's existing "trust the private network" posture (the web
UI/API on 9000 has no front-door auth either). No config.json field, no wizard change. macOS
prompts once and offers to save to Keychain.

## Ownership / traversal — the validation spike

**The problem.** `force user = rmng` makes `smbd` act as uid 1000 for file operations, including
symlink resolution. But `homes.rs` currently links `data/hosts/<id>` → the clone's **main PID**
(`/proc/<pid>/root`), and that PID is the clone's root-owned init. Following a root-owned
process's `/proc/<pid>/root` requires ptrace-level access (`PTRACE_MODE_READ_FSCREDS`) — root or
CAP_SYS_PTRACE — which a uid-1000 session lacks. So "traverse as root" and "write as uid 1000"
pull in opposite directions.

**Primary resolution (R2).** Point the browse target at a **uid-1000** process in each clone
(the `systemd --user` / agent-wrapper / desktop session — same rootfs via the shared mount
namespace, but ptrace-followable by uid 1000). Then `force user = rmng` gives both traversal
*and* correct ownership. The sub-question the spike answers: how the reconciler reliably finds a
uid-1000 pid per clone (scan the clone's host-visible pids by cgroup / container id and pick one
whose `/proc/<p>/status` `Uid` is 1000). This changes `homes.rs`'s pid selection. Changing the
symlink target is transparent to existing consumers (host-side browsing, `docker exec`) because
every pid in the clone shares one rootfs.

**Fallback (R3).** `smbd` runs as **root** (traversal works for all clones); created files land
root-owned, with a `force group` / periodic chgrp to patch group ownership. Sacrifices strict
uid mapping but always works. Chosen only if R2 proves flaky (e.g. no reliably-present uid-1000
process during a clone's boot window).

**Gate.** The spike passes when, on a real clone: (1) the id appears in the share, (2) a file
reads, (3) a file created over SMB shows as uid 1000 inside the clone (`docker exec … ls -n`).

## Error handling / edges

- **No `pid: host`** — `data/hosts` stays empty; the reconciler already warns once per clone.
  The share mounts but is empty; `smbd` still runs. No new warning.
- **Host port 445 already bound** — `docker compose up` fails at publish with a clear Docker
  error. Documented in DEPLOY.md as the single prerequisite ("host 445 must be free").
- **smbd binary/config error** — surfaced via the `smb` tracing target (stdout/stderr piped),
  matching the `clip`/`homes` target convention. First-start failure logs ERROR once, keeps
  retrying rather than taking the server down.
- **Clone PID change mid-session** — the reconciler repoints within 15 s; an open Finder handle
  to a since-restarted clone gets an I/O error and the user re-navigates. Acceptable, documented.

## Docs changes

- **`docs/DEPLOY.md` "Browsing clone homes"** — remove the sshfs bullet and the
  `sshfs -o follow_symlinks …` command; replace with the SMB method (`smb://<host>/clones`, the
  fixed `rmng` credential, the host-445 prerequisite). Keep the other two access paths (host-side
  volume path, `docker exec`).
- **`crates/control-server/src/homes.rs`** module doc — reframe the "successor to the sshfs
  reconciler" framing if pid selection changes; note the SMB share now consumes `data/hosts`.
- **`Dockerfile`** comments — the two notes that say the image deliberately omits `openssh`/`sshfs`
  become "SMB share (samba) serves clone homes; still no SSH."

## Testing

- **Unit (Rust) — `render_smb_conf()`**: assert the rendered config contains the load-bearing
  lines. Regression guard against dropping a critical option.
- **Unit (Rust) — `homes.rs` pid selection** (if the spike changes it): pure test feeding a
  fake `/proc`-style listing, asserting a uid-1000 pid is chosen — same style as the existing
  `entries_to_remove` test.
- **Integration / manual (the real proof, on CT 106)**: `docker compose up --build`, create a
  clone, then from the Mac `smb://<host>/clones` → confirm the id appears, a file reads, and a
  file created over SMB shows as uid 1000 inside the clone. This is the spike's pass/fail gate;
  it cannot be faked in a unit test.

## Open items carried into the plan

1. **The ownership/traversal spike (R2 vs R3)** — prove R2 on a real clone before committing to
   the `homes.rs` pid-selection change; fall back to R3 if it's flaky.
2. **smb.conf location** — `/etc/samba/smb.conf` (Samba default; smbd finds it with no flag) vs
   a generated path under `/data` passed via `smbd -s`. Decide in the plan.
