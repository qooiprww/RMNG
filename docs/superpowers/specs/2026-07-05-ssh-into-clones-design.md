# SSH into any clone through the control-server

**Status:** design approved, pending spec review
**Date:** 2026-07-05

## Goal

Give the operator a **real SSH session into any clone**, routed through the
control-server (the single exposed box), with **zero laptop-side setup**: click a
"Copy SSH command" button in the web UI, paste the one-liner into any terminal, press
enter. The same one-liner also drops into VSCode Remote-SSH's "Add New SSH Host…"
prompt, and works with `scp` / `rsync -e` / `git`.

"Real SSH" (not a `docker exec` shell) is a hard requirement: it must support the SFTP
subsystem and remote command execution so `scp`, `sftp`, `git`-over-ssh, `rsync`, and the
VSCode remote agent all work. That rules out terminating SSH at the control-server and
bridging with `docker exec` — the session must terminate at a **real `sshd` inside the
clone**.

## Chosen shape (from brainstorming)

| Decision | Choice |
|---|---|
| Client experience | Real `ssh` client (not `rmng shell`, not web terminal) |
| Topology | Single-hop *feel*: one `ssh <clone>` command, no `~/.ssh/config` |
| Bridge / auth to control-server | **Bastion `sshd`** on the control-server, **public-key** auth |
| Key registration | Paste public keys in the web UI / API |
| Laptop-side artifact | **None** — a copyable one-line `ssh` command with inline `-J` |
| Bastion account name | `rmng` (reuse the existing uid-1000 SMB account) |

## Architecture

```
laptop                     control-server container            clone container
─────────                  ────────────────────────            ───────────────
ssh -J rmng@HOST:2222  ──►  bastion sshd :2222        ──TCP──►  real sshd :22
     rmng@myclone           (pubkey, jump-only,      (rmng net    (pubkey, user
  (uses default key/        PermitOpen = live         DNS = %h)    rmng, uid 1000)
   ssh-agent)               clones:22 only)
                            │
                            └─ config/keys/host-key/PermitOpen written &
                               reloaded by the control-server process from state
```

- The laptop's SSH session is **encrypted end-to-end to the clone's own `sshd`**. The
  bastion only splices a TCP stream (a `-W` / ProxyJump direct-tcpip forward) and never
  sees the session — this is why SFTP/scp/git/VSCode all work.
- `myclone` is the clone's **hostname**, which equals its **host id** and its **DNS name
  on the `rmng` network** (`docker.rs`: clone hostname == host id; `dial_host` /
  `inspect_ip` resolve it). The bastion, itself attached to the `rmng` network under the
  control alias, resolves `myclone` via Docker's embedded DNS. The laptop never resolves
  the clone name — with `-J`, the final `HostName` is resolved by the jump host.

### The copied command

Per-clone, the UI "Copy SSH command" button (and `rmng ssh <clone>`) yields:

```
ssh -J rmng@<PUBLIC_HOST>:<BASTION_PORT> -o StrictHostKeyChecking=accept-new rmng@<CLONE_HOSTNAME>
```

- `-J rmng@<PUBLIC_HOST>:<BASTION_PORT>` — the inline jump through the bastion. No
  `~/.ssh/config` needed.
- `rmng@<CLONE_HOSTNAME>` — the real end-to-end target (the clone's `sshd`).
- `-o StrictHostKeyChecking=accept-new` — first connection is trusted silently (no
  interactive prompt); a *changed* key still errors. Safe because clone host keys are
  **stable** (see below), so `accept-new` never degrades into a scary mismatch.
- No `-i`: `ssh` uses the operator's default key / ssh-agent, which is the private half of
  a public key they registered in the UI.

`scp` / `rsync` / `git` reuse the same jump:

```
scp -J rmng@<PUBLIC_HOST>:<BASTION_PORT> file rmng@<CLONE_HOSTNAME>:~
rsync -e 'ssh -J rmng@<PUBLIC_HOST>:<BASTION_PORT>' -a ./ rmng@<CLONE_HOSTNAME>:~/dst
GIT_SSH_COMMAND='ssh -J rmng@<PUBLIC_HOST>:<BASTION_PORT>' git clone rmng@<CLONE_HOSTNAME>:repo.git
```

VSCode Remote-SSH: run **"Remote-SSH: Add New SSH Host…"**, paste the same `ssh …`
one-liner, and VSCode writes its own config entry. No hand-written config.

## Components

| Piece | Location | Responsibility |
|---|---|---|
| SSH settings | `crates/wire/src/config.rs` (`AppConfig`) | new `ssh` section (see below). Public keys are **not secret** → pass through `AppConfigRedacted` to the browser. |
| Bastion port | `crates/wire/src/config.rs` (`ListenConfig`) | `bastion` port, default `2222`. |
| Config/key/known-hosts generator | new `crates/control-server/src/ssh.rs` | **pure** functions: render `sshd_config`, the bastion `authorized_keys`, the dynamic `PermitOpen` allowlist, and per-clone host-key material, from a fleet + settings snapshot. Unit-testable. |
| Bastion supervisor | `ssh.rs`, wired in `main.rs` | ensure host key exists (persisted), write config + keys, spawn `sshd -D -f <cfg>`, and on settings/fleet change rewrite `authorized_keys` + `PermitOpen` and `SIGHUP` to reload. Restart the subprocess if it dies. |
| Clone `sshd` | `template/Dockerfile`, `template/setup/30-user.sh` | bake `openssh-server`, enable `ssh.service`, pubkey-only, root disabled, login user `rmng`. |
| Clone key material injection | `crates/control-server/src/provision.rs` | add `TarEntry`s to the existing pre-boot `upload_tar`: the clone's `~rmng/.ssh/authorized_keys` (mode 600, dir 700, uid 1000) **and** the clone's stable host key. Push updated `authorized_keys` to *running* clones via `upload_tar` when keys change. |
| Copy-command UI + CLI | `frontend/…`, `crates/cli/src/` | per-clone "Copy SSH command" button + Settings → SSH Access panel (paste keys, show bastion port); `rmng ssh <clone>` prints the one-liner. |

### `AppConfig.ssh`

```rust
pub struct SshConfig {
    /// Whether the bastion sshd runs and keys are provisioned into clones.
    pub enabled: bool,
    /// Authorized SSH public keys (one full line each: `ssh-ed25519 AAAA… comment`).
    /// Installed on the bastion AND every clone. Not secret.
    pub authorized_keys: Vec<String>,
    /// Public host/IP the copied command's `-J` jump points at. Empty ⇒ the control-server
    /// infers it from the web request's Host header (the address already used to reach the
    /// UI). Override only when the ssh entry point differs from the web entry point.
    pub public_host: String,
}
```

`ListenConfig` gains `bastion: u16` (default 2222).

## Data flow — registering a key

1. Operator pastes a public key in **Settings → SSH Access** (or `PUT /api/config`). It
   lands in `AppConfig.ssh.authorized_keys` and persists to `state.json`.
2. Control-server rewrites the **bastion** `authorized_keys` (`/etc/rmng/ssh/authorized_keys`)
   and `SIGHUP`s the bastion `sshd`.
3. Control-server propagates the key to **all existing clones**, covering every state:
   - **Running clones** — pushed live via `upload_tar` into `~rmng/.ssh/authorized_keys`.
     `sshd` reads `authorized_keys` fresh on each new connection, so the key is usable
     **immediately, no clone restart** and no `sshd` reload needed.
   - **Stopped clones** — the pre-boot inject path writes the current key set on next boot.
   - **Future clones** — receive it at provision time (pre-boot inject).

   Propagation is **best-effort per clone**: a clone that is unreachable/mid-transition is
   skipped (it will converge on its next boot from persisted settings), and one clone's
   failure never blocks the others. `authorized_keys` is always written as the **full
   current set** (idempotent overwrite), so adds and removals both converge.
4. Operator clicks **Copy SSH command** on a clone (or runs `rmng ssh <clone>`), pastes,
   and connects.

## Provisioning details

- **Clone `sshd`**: listens on `0.0.0.0:22` *inside the container* — reachable only on the
  internal `rmng` network, **never host-published**. `PasswordAuthentication no`,
  `PermitRootLogin no`, login user `rmng` (uid 1000, already has passwordless sudo).
- **Stable clone host key**: the control-server generates and **persists a per-clone SSH
  host key** in `data_dir` (e.g. `data_dir/ssh/hostkeys/<clone>/`) and injects it via the
  same `upload_tar` on every (re)provision. A clone's SSH identity is therefore constant
  across rebuilds, so `accept-new` trusts it once and never trips a "host key changed"
  error. (This replaces per-boot host-key regeneration.)
- **Bastion `sshd`** (inside the control-server container):
  - Runs as a **subprocess supervised by the control-server** (not a baked system
    service), so config is regenerated from live state and reloaded on change.
  - Reuses the existing **`rmng` account** (uid 1000, `-M` no-home, `/usr/sbin/nologin` —
    the SMB account). SSH access is scoped by a `Match User rmng` block; the account stays
    `nologin` and SMB is unaffected (Samba does not use the login shell). `-W` /
    ProxyJump forwarding does not spawn a shell, so `nologin` is no obstacle.
  - **`AuthorizedKeysFile /etc/rmng/ssh/authorized_keys`** (explicit path — the account
    has no home dir; the control-server owns this file).
  - `PasswordAuthentication no`, `PermitRootLogin no`, `AllowUsers rmng`,
    `AllowTcpForwarding yes`, `PermitTTY no`, `X11Forwarding no`, `AllowAgentForwarding no`.
  - **`PermitOpen`** regenerated to exactly the **live clones' `:22`** and reloaded on
    every fleet change, so the bastion cannot be used as an open jump to arbitrary internal
    hosts. A `ForceCommand` refuses any interactive/exec attempt (jump-only).
  - **Bastion host key** persisted in `data_dir` (e.g. `data_dir/ssh/bastion/`) so the
    bastion's identity is stable across control-server restarts (no client host-key
    warnings on the jump).

## Security posture

- **New exposed surface** = the bastion port only. **Pubkey-only, no passwords, no root,
  jump-only.** Optional rate-limiting / fail2ban noted as future hardening.
- **Scope**: any registered key can reach **every** clone (single-operator model).
  Per-key / per-clone ACLs are explicitly **out of scope for v1**.
- **Host-key protection retained**: stable clone + bastion host keys mean
  `StrictHostKeyChecking=accept-new` gives friction-free first connect *and* real
  protection — no `StrictHostKeyChecking=no` / `UserKnownHostsFile=/dev/null` MITM hole.
- **Reusing the SMB `rmng` account** does not widen it: the `Match User rmng` sshd block
  grants only `-W` forwarding to `clones:22`; the account keeps `nologin` and no shell.

## Deploy changes

- Publish the bastion port: add `-p 2222:2222` to the `docker run` one-liner and
  `compose.yaml`; document in `docs/DEPLOY.md` (and the port map in `docs/PROTOCOL.md`).
- `openssh-server` added to the clone template → **template rebuild** required
  (see the rebuild-template runbook).
- No other host-side changes; the bastion runs inside the existing control-server image.

## Testing

- **wire unit**: `AppConfig` round-trips with the new `ssh` section; `bastion` port
  default; public keys survive the `AppConfigRedacted` round-trip (not stripped as
  secrets).
- **control-server unit** (`ssh.rs`, pure generators):
  - `sshd_config` render (expected directives present: `AuthorizedKeysFile`, `Match User
    rmng`, `PermitOpen`, `PasswordAuthentication no`, …).
  - `authorized_keys` render from a key list (dedup, one key per line, trailing newline).
  - `PermitOpen` render from a fleet snapshot (exactly the live clones' `:22`; empty fleet
    ⇒ no forwards permitted).
  - supervisor reload path: config change ⇒ file rewritten + reload signalled.
- **provision unit**: the pre-boot `upload_tar` contains the `authorized_keys` and clone
  host-key `TarEntry`s with correct path / mode (600 / 700) / owner (uid 1000).
- **Manual E2E** (W6800 box, CT 106): register a key → **Copy SSH command** → paste →
  `ssh` into a clone → run a command → `scp` a file both ways → connect **VSCode
  Remote-SSH** via the pasted command. Verify a **second** clone from the same command
  template, and that a *running* clone picks up a newly-added key without a restart.
  Documented in `docs/DEPLOY.md`.

## Out of scope (v1)

- Per-key / per-clone access control (all keys reach all clones).
- Rate-limiting / fail2ban on the bastion.
- A control-server-served `known_hosts` bundle (stable host keys + `accept-new` cover the
  common case).
- SSH access for anyone other than the operator (no multi-user / RBAC).
