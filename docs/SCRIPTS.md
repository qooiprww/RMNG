# Scripts reference

The Docker port collapsed RMNG's script surface to almost nothing. The old
**developer build/deploy** scripts (`provision-build-ct.sh` / `cs-build-ct.sh` /
`provision-deploy-ct.sh` / `cs-deploy-ct.sh`) are gone — the build is a **Dockerfile**
(`docker build`, see [DEPLOY.md](DEPLOY.md#the-image-build)), and deploy is `docker run` /
`docker compose`. The old **SSH+`pct` orchestration** scripts (`bootstrap.sh` / `clone.sh` /
`redeploy.sh` / `delete.sh`) are gone too — those flows are now pure Rust in
[`provision.rs`](../crates/control-server/src/provision.rs), driving the bollard primitives in
[`docker.rs`](../crates/control-server/src/docker.rs). The `P <step> <msg>` / `RESULT` bash
protocol died with them: Rust emits progress directly through a `FnMut(&str, &str)` callback,
and a guest script's own stdout lines are line-buffered into the Operation log.

What survives is **three in-container guest scripts** (`include_str!`'d into the binary and
streamed to a container over `docker exec bash -s` — `DockerCtl::exec_script`), plus the
**gnome-patch build** (now a Dockerfile stage).

| Script | Runs where | Invoked by | Purpose |
|---|---|---|---|
| `crates/control-server/scripts/provision-clone.sh` | in a build container (`docker exec`) | `provision::bootstrap_base_image` | Turn `ubuntu:26.04` into a clone-source image: headless GNOME + clone-daemon + agent-wrapper + patched shell |
| `crates/control-server/scripts/apply-monitors.sh` | in a clone container (`docker exec`) | `provision::apply_monitors` | Re-apply a monitor layout to a running clone without reprovisioning |
| `crates/control-server/scripts/claude-import.sh` | in a clone container (`docker exec`) | `provision::run_clone_op` (`claude.rs`) | Read `claude auth status` / the credentials file, clear it, or install a token |
| `gnome-patch/build-shell-deb.sh` | the `gnome-build` Dockerfile stage | `docker build` | Build the patched gnome-shell `.deb` |

The three guest scripts are baked in at compile time
([provision.rs:28-30](../crates/control-server/src/provision.rs)) and fed to
`bash -s -- <args…>` over the exec's stdin at runtime — they are **not** pre-installed in any
container. Each emits its step lines as `    [ct] <message>`, which `provision.rs` strips for
the operation message; other stdout/stderr becomes plain log context.

---

## In-container guest scripts

### `provision-clone.sh <username> <password> <monitors> <clone_socket>`
Runs as root **inside a privileged `sleep infinity` build container** (systemd is *not* PID 1
there, so its `systemctl enable/mask/set-default` are pure symlink ops — the Rust caller sets
`SYSTEMD_OFFLINE=1` + `DEBIAN_FRONTEND=noninteractive`). Codifies the validated recipe:
`apt full-upgrade`; remove snap + disable guest AppArmor; install vanilla **headless GNOME +
Mutter + VA-API + PipeWire** (no GDM, no gnome-remote-desktop, no flatpak); **install the
patched gnome-shell deb** if it was pushed in; create the `rmng` user (uid 1000, sudo,
render/video groups, linger); install `clone-daemon` + `agent-wrapper` (to `/opt/rmng/bin`) +
the standalone `claude` CLI; write + enable three `systemd --user` units (`gnome-headless`,
`rmng-clone-daemon`, `agent-wrapper`). `<monitors>` is the config CSV (`WxH+X+Y[*],…`) → the
daemon's `RMNG_MONITORS` + the headless dummy mode specs; `<clone_socket>` → the daemon's
`RMNG_SOCKET`. The control-server pushes the payload binaries in first (to
`/root/rmng-clone-daemon`, `/root/agent-wrapper`, `/root/gnome-shell-patched.deb`), then
`bootstrap_base_image` cleans up + commits the container to `rmng/template:<name>`.

### `apply-monitors.sh <username> <monitors-csv>`
Runs as root inside a **running clone** container. Rewrites the clone-daemon's `RMNG_MONITORS`
+ the `gnome-headless` dummy mode specs from the new layout, then restarts the headless GNOME
session + the daemon (which re-creates the virtual monitors at startup). Talks to the target
user's `systemd --user` manager via `runuser` + its `XDG_RUNTIME_DIR` / session-bus address.
Driven by `POST /api/monitors/apply`.

### `claude-import.sh <user> status|read|clear|apply [b64]`
Runs inside the target **clone** container as the clone user, printing the raw result to
stdout. `status` — `claude auth status` JSON (stderr merged so a logged-out clone still
parses; never fails). `read` — the clone's `~/.claude/.credentials.json`. `clear` — delete
it, print `CLEARED`. `apply <b64>` — write `~/.claude/.credentials.json` (0600) from the
base64 JSON in `$3` (the current short-lived access token, refresh emptied). Backs
`claude.rs`'s `{check_clone_auth, import_clone_account, apply_clone_token}`; hot-swaps a
running clone's account with no restart (Claude Code re-reads creds per request).

---

## gnome-patch build

### `gnome-patch/build-shell-deb.sh`
Runs in the **`gnome-build` Dockerfile stage** (`docker build`). Repack approach: applies
shell-01 + shell-03 to the gnome-shell source, rebuilds only `libshell-<N>.so` (meson/ninja),
swaps it into the stock `.deb`, and bumps the version `+ngshell1`. Prints `DEB=<path>` — the
Dockerfile copies that to `/usr/local/share/rmng/gnome-shell.deb` in the runtime image, from
where `provision.rs` pushes it into build containers. Cached (skips if the deb is newer than
the patches; `FORCE=1` rebuilds). See [gnome-patch/README.md](../gnome-patch/README.md).
