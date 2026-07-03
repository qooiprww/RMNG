# Pre-provisioned template image + automatic daemon hot-swap

## Context

Today the setup wizard builds the base image **in-product**: a ~30-min live apt run
(`bootstrap_base_image` → privileged sleep-infinity build container → `provision-clone.sh`
over exec → `docker commit`). And keeping clone binaries current is a **manual** per-host
"redeploy" button that hot-swaps `clone-daemon`/`agent-wrapper`.

Decisions made with the user:

- The wizard **downloads** a pre-provisioned template from Docker Hub
  (`pegasis0/rmng-template:latest`, configurable) with a real byte-progress bar; the
  in-product build path is **removed entirely**.
- Template production becomes a proper **template Dockerfile**: `provision-clone.sh`
  moves into the template build context and is **run by the build** as a few phase
  scripts (one `RUN` each, ordered by change frequency, for layer caching) — NOT inlined
  instruction-by-instruction: the script is real bash (functions, best-effort fallbacks,
  MODE_SPECS derivation, unit-file heredocs with conditional interpolation) that
  Dockerfile syntax can't express without losing readability and shellcheck-ability. The
  patched-gnome-shell build stage **moves** from the control-server Dockerfile to the
  template Dockerfile. The clone-daemon + agent-wrapper build stages exist in **both**
  Dockerfiles — the control-server keeps staging them as payloads because they are
  replaceable in live clones on control-server upgrade (feature B).
- The manual redeploy button/endpoint/MCP tool are removed; the control-server detects
  stale clone binaries by **server-side sha256 check** (no wire-protocol change) and
  hot-swaps them automatically — bouncing only the `systemd --user` units, never the
  container.

Key verified facts the design rests on:
- Image labels + boot config (Entrypoint/StopSignal/`rmng.image=1`) live in the image
  config manifest and survive push/pull — a pulled template arrives ready; only a local
  retag to `rmng/template:<name>` is needed. All of these are native Dockerfile
  instructions (`LABEL`, `STOPSIGNAL`, `ENTRYPOINT`).
- `provision-clone.sh` needs no running/privileged container: sysctls are written as
  `/etc/sysctl.d/` files, `systemctl mask`/`set-default` are offline symlink ops, linger
  is a file touch, the gnome deb installs via apt — all valid `docker build` RUN steps.
- `state.rs::read_from_disk` (state.rs:113-121) falls back to an **empty state** on any
  parse error → removing the persisted `Bootstrap` op kind needs a serde alias or it
  wipes hosts.
- bollard 0.19.4's `CreateImageInfo.progress_detail{current,total}` exposes byte
  progress; current `pull_image` (docker.rs:575) discards it; `jobs::op_progress` pins
  `op.pct` to the coarse step table (no mid-step movement today).
- clone-daemon re-sends `Hello` on every reconnect (server restart ⇒ daemon exits ⇒
  systemd restarts it ⇒ reconnect), so a Hello-triggered check covers clone create,
  daemon restart, AND control-server boot. `serve_clone` already receives `App`
  (mediaplane.rs:214).
- Workspace version is pinned 0.1.0 everywhere → hashes, not version strings. `sha2` is
  NOT yet a dependency; `sha256sum` exists in clones (coreutils).

---

## Feature A — template pull replaces in-product build

### A1. Wire types (`crates/wire/src/control.rs`, `crates/wire/src/config.rs`)

- `OperationKind`: drop `Bootstrap`, add `Pull` with `#[serde(alias = "bootstrap")]`
  (mandatory — see Context). Serde test: `"bootstrap"` deserializes to `Pull`.
- `DockerConfig` gains `template_reference: String`, default
  `"pegasis0/rmng-template:latest"` (short form — matches what RepoTags will contain).
  Immediate-apply; no redaction/one-time changes needed (`AppConfigRedacted` passes
  `docker` through wholesale).
- `cargo test -p wire` regenerates `OperationKind.ts` / `DockerConfig.ts`.

### A2. Docker primitives (`crates/control-server/src/docker.rs`)

- Delete `BASE_DOCKER_IMAGE`, `create_build_container`.
- Rework `pull_image` (single caller) to emit a `PullEvent` enum:
  `Status{layer,status}` (per-(layer,status) dedupe, as today → op log) and
  `Bytes{frac}` (aggregate 0..1). Aggregation in a pure, unit-tested `PullAggregator`:
  per-layer download/extract byte counters from `progress_detail`;
  `frac = 0.7·(Σdl_cur/Σdl_tot) + 0.3·(Σex_cur/Σex_tot)`; `Already exists` layers weigh
  zero; monotonic via `max(frac, peak)`; emit only on integer-percent change (caps
  state.json writes/SSE broadcasts at ≤100 per pull). Surface `info.error` as hard error.
- Add `tag_image(source, repo, tag)` (bollard `tag_image`) and
  `image_labels(reference) -> HashMap` (via `inspect_image`).

### A3. Pull flow (`crates/control-server/src/provision.rs`)

- Delete `PROVISION_SCRIPT` include_str, `bootstrap_pct`, `bootstrap_base_image`,
  `bootstrap_after_create`.
- Add `pull_template(app, remote_ref, name, on_progress) -> Result<String>`:
  1. Gate: `is_dns_label(name)`; reference trimmed/non-empty/no whitespace; reject `@`
     digest refs (mis-split by `split_reference`).
  2. Collision: bail if `rmng/template:<name>` exists.
  3. `pull` step: byte events map to pct `2 + frac·88` (2–90).
  4. `verify` (91): require label `rmng.image == "1"` else bail
     ("not an RMNG template … build one with template/Dockerfile"). Verify BEFORE
     tagging. WARN (not fail) if StopSignal ≠ SIGRTMIN+3.
  5. `tag` (94): retag to `rmng/template:<name>`; **keep** the remote tag (delete-flow
     consequence documented: deleting the local tag only untags; the row re-lists under
     the remote ref; second delete frees layers).
  6. `done` (100). Return canonical local ref.
- Progress contract: a `PullProgress` enum (`Step{step,msg}` / `Pct{pct,msg}`) — jobs
  consumes it directly instead of the shared `(step,msg)` callback. New `pull_pct`
  table: queued 0, pull 2, verify 91, tag 94, done 100; wire into `step_pct`.

### A4. Jobs + web (`jobs.rs`, `web.rs`, `main.rs`)

- Delete `start_bootstrap`/`run_bootstrap`. Add `start_pull(app, name, reference)`
  (same guards as bootstrap: DNS label, in-flight same-target) + `run_pull` with a
  pull-specific closure: `Step` → step/message/log + `pct.max(pull_pct)`;
  `Pct` → `pct.max(p)` + message, no log push.
- `make_op` Pull arm; `start_commit`'s duplicate-tag guard `Bootstrap` → `Pull`.
- `web.rs`: `/api/images/bootstrap` → `POST /api/images/pull`
  `{name, reference?}` (reference defaults to `cfg.docker.template_reference`).
- Hardening (small, included): `jobs::fail_stale_ops(app)` at boot in `main.rs` — mark
  persisted `Running` ops as `Error` ("interrupted by server restart") + prune;
  today a crashed-mid-op server blocks same-named ops forever.

### A5. Template Dockerfile (replaces the in-product bootstrap; provision script moves into the build)

New `template/` directory, built with repo-root context
(`docker build -f template/Dockerfile -t pegasis0/rmng-template:$(date +%Y%m%d) -t
pegasis0/rmng-template:latest .` then `docker push` both tags — documented in
DEPLOY.md; optionally a ~20-line `scripts/publish-template.sh` wrapper for
build+tag+push convenience):

- `template/Dockerfile` stages:
  1. `bun-build` — agent-wrapper `bun build --compile` only (no frontend). Duplicated
     from the main Dockerfile **by design** (user decision: both images carry these
     build commands).
  2. `rust-build` — cargo build `-p clone-daemon` only (cache-mounted like the main
     Dockerfile's stage).
  3. `gnome-build` — **moved verbatim** from the control-server Dockerfile (lines
     52-77: deb-src build-dep layer + `gnome-patch/build-shell-deb.sh`).
  4. Final stage `FROM ubuntu:26.04`: `provision-clone.sh` moves here (git mv) and is
     **run by the build**, split into phase scripts under `template/setup/` — one `RUN`
     each, ordered by change frequency so a tweak to user setup never re-runs the
     20-minute apt/toolbox layers. NOT inlined instruction-by-instruction: the script is
     real bash (functions like `apti`/`mc_install`/`mona_install`, the MODE_SPECS
     derivation from the monitors CSV, unit-file heredocs with
     `${MONITORS:+…}`-conditional interpolation) that Dockerfile syntax can't express
     without losing readability; unit files stay as script heredocs because they need
     that templating. Shape:
     - `ARG USERNAME=rmng`, `ARG MONITORS="2560x1440+2560+0*,2560x1440+0+0"`,
       `ARG CLONE_SOCKET=/srv/rmng-sock/clones.sock`; `ENV DEBIAN_FRONTEND=noninteractive
       SYSTEMD_OFFLINE=1` during setup.
     - `COPY template/setup/ /setup/`, then roughly:
       `RUN /setup/10-desktop.sh` (locale/tz, headless GNOME + Mutter + VA-API +
       PipeWire, Recommends strip + masks — biggest, rarest change) →
       `COPY --from=gnome-build` the deb + `RUN /setup/15-gnome-patch.sh` →
       `RUN /setup/20-toolbox.sh` (third-party apt repos, dev toolbox, HMCL/Mission
       Center/Monaspace, dconf defaults) → `RUN /setup/30-user.sh` (user + groups +
       linger, fish shell, PATH rc, keyring, CLAUDE.md, claude/uv/rustup/nvm installs,
       systemd user units + wants symlinks).
     - Script adjustments while splitting: drop the exec-era scaffolding (`[ct]`
       progress protocol, positional-arg parsing → env from `ARG`s); drop the
       `/root/rmng-clone-daemon` / `/root/agent-wrapper` install block — binaries land
       via `COPY --from` below; revisit best-effort `WARN`s per step (in `docker build`,
       load-bearing failures should fail the build, not publish a degraded template;
       keep WARN only for genuinely optional apps).
     - Then binaries LAST (a daemon rebuild only busts these cheap layers):
       `COPY --from=rust-build … /opt/rmng/bin/rmng-clone-daemon`,
       `COPY --from=bun-build … /opt/rmng/bin/agent-wrapper` (0755 root, same paths the
       hot-swap targets).
     - Tail: apt clean + rm lists, `: > /etc/machine-id`, uid-1000 assertion
       (`RUN [ "$(id -u rmng)" = 1000 ]`), then image config:
       `LABEL rmng.image=1 rmng.base=1`, `ENV container=docker`,
       `STOPSIGNAL SIGRTMIN+3`, `ENTRYPOINT ["/sbin/init"]`, `CMD []`.
     (The script on this branch is already post-audit-clean — snapd pin only, no
     aa-teardown, no networkd-wait-online mask, NetworkManager purged — so the split is
     a reorganization, not a rewrite.)
- `crates/control-server/scripts/provision-clone.sh` is **moved** into
  `template/setup/` (split), not kept: the control-server no longer embeds it.
  `apply-monitors.sh` + `claude-import.sh` stay `include_str!`'d in the control-server.
- **Control-server `Dockerfile`**: remove the `gnome-build` stage and the
  `gnome-shell.deb` COPY (line 146) — its only consumer was the deleted bootstrap; the
  hot-swap covers only clone-daemon + agent-wrapper, which stay staged in
  `/usr/local/share/rmng/`. Update the header comment (stages 1/3/4; payloads = 2
  binaries + static). Control-server image shrinks by the multi-GB gnome build-dep
  stage's output deb.
- `.dockerignore`: ensure `template/` is not excluded (only `/scripts/` is today);
  update stale comments.
- Versioning: immutable `YYYYMMDD` tags + moving `latest` (rollback = point
  `templateReference` at a dated tag in Settings).

### A6. Frontend

- `api.ts`: `bootstrapBaseImage` → `pullTemplate(name, reference?)`.
- `types.ts` OperationKind mirror; `OperationProgress.tsx` VERB: `pull: "Pulling"`.
- `SetupWizard.tsx`: step "Base image" → "Download template"; inputs = template reference
  (prefilled from config) + local name (default `base`); op lookup `kind === "pull"`;
  keep Skip link + Next-blocked-while-running; Finish summary row.
- `ImagesSection.tsx`: "+ Build base image" → "+ Pull template" (prompt reference
  prefilled from new `templateRef` prop, then local name); `_index.tsx` passes
  `templateRef` from `cfg`, kind filters `"bootstrap"` → `"pull"`.
- `SettingsPanel.tsx`: "Template reference" text input in the docker section.

---

## Feature B — automatic hash-based binary hot-swap

### B1. Swap engine refactor (`provision.rs`)

- `REDEPLOY_UNITS` becomes a `pub struct RedeployUnit { payload, unit, bin }` table
  (folds in the bin-name match). `redeploy_clone` drops `daemon_only`; takes
  `units: &[(&'static RedeployUnit, Vec<u8>)]` — caller resolves payload bytes. Keep
  uid resolution, stop-tolerated → upload_tar → reset-failed+start, `run_user_systemctl`
  verbatim.

### B2. New module `crates/control-server/src/binswap.rs`

- Add `sha2 = "0.10"` to control-server Cargo.toml.
- `SwapState` on `App` (`app.rs` field + `App::new` init; `main.rs`: `mod binswap;` +
  `binswap::spawn(app.clone())` beside the other loops, before `mediaplane::spawn`):
  - `tx: OnceLock<mpsc::UnboundedSender<String>>` — `request_check(clone_id)` is
    sync-callable from the mediaplane thread.
  - `expected: OnceLock<Vec<ExpectedUnit>>` — sha256 hex per unit, warmed eagerly at
    worker start (`None` = payload absent in dev → that unit's check skipped). Cache
    hashes, not bytes (agent-wrapper is ~90 MB).
  - `hosts: Mutex<HashMap<String, HostGuard>>` with
    `HostGuard { failures, next_swap_allowed, pending }`.
- Single worker task drains the channel → `check_host(app, id)`, the one guarded path:
  host managed? container running? → exec `sha256sum /opt/rmng/bin/<bins>` via
  `exec_capture` → stale set = missing-or-mismatched hashes ∪ `pending` → empty ⇒ reset
  guard; else if past `next_swap_allowed`: re-read payloads, **refuse to upload bytes
  whose hash differs from the cached expected hash** (dev payload replaced under a
  running server ⇒ WARN "restart the control-server", skip — makes swap-loops impossible)
  → `redeploy_clone`. Backoff gates **swaps, not checks**:
  `next = now + min(30s·2^failures, 30min)`; the post-swap Hello re-check is the success
  verification (match ⇒ reset). `Err` ⇒ `pending = stale units` (covers upload-ok/
  start-failed, where hashes match afterward). `parse_sha256sum` is pure + unit-tested
  (tolerates interleaved error lines from merged stderr).
- Sweep loop: first pass 60 s after boot, then every 5 min — enqueue every managed host
  whose container is in `list_managed_containers()` (docker.rs:706). Catches clones whose
  stale daemon can't even connect (the graceful-mismatch story). Docker down ⇒ WARN, skip
  pass.
- Failure surfacing: log-only (debug for no-ops, WARN with operator guidance for
  failures); no state_note/unread writes (would fight monitor.rs and the agent).
  agent-wrapper swaps immediately even mid-session (decided); note in module doc that a
  future "defer while `monitor_state == Working`" is a small change.

### B3. Triggers + mediaplane hardening (`mediaplane.rs`)

- Hello arm (`serve_clone`, :457-461): after the conns insert,
  `app.binswap.request_check(&h.clone_id)` (`app` is already a parameter).
- Fix the disconnect-teardown race (routine under auto-swap): remove from
  `conns`/`latest` only when `Arc::ptr_eq(map_entry, &this_conn)` — a late old-thread
  teardown must not clobber the new session.

### B4. Remove manual surfaces

- `web.rs`: `/api/clone/redeploy` route + `RedeployReq` + `clone_redeploy`.
- `mcp.rs`: `redeploy` tool declaration + handler arm.
- Frontend: `api.ts` `redeployClone`; `_index.tsx` import + `onRedeploy` handler/confirm;
  `SidebarHost.tsx` prop + `RedeployIcon` + button.

---

## Docs (both features)

- `docs/API.md`: `/api/images/bootstrap` → `/api/images/pull` (body, kinds list);
  delete `/api/clone/redeploy` row + section.
- `docs/DEPLOY.md`: wizard step = download template; new "Publishing the template"
  subsection (template/Dockerfile build + tag + push, date tags, `docker login`,
  delete-only-untags note); upgrade path = automatic hot-swap (hash check on daemon
  connect + 5-min sweep; agent-wrapper bounce drops the in-flight Claude session; dev
  caveat: hashes pinned at start — restart the dev server after restaging
  `embedded-bin/`).
- `docs/MCP.md`: drop `redeploy` row. `docs/SCRIPTS.md`: provision-clone.sh row →
  moved to `template/setup/` phase scripts run by template/Dockerfile; embedded-script
  count drops to two.
  `README.md` + `crates/control-server/README.md` + `CODEX_PARITY.md:52`: reword
  bootstrap/redeploy mentions; Dockerfile header comment update.

## Sequencing

1. Template Dockerfile (A5) — standalone, provable before anything else changes:
   build it, inspect labels/entrypoint, boot a container from it manually.
2. Feature A backend (wire → docker.rs → provision.rs → jobs/web/main), then frontend;
   `cargo test -p wire -p control-server` (regenerates ts-rs), `bun run build`.
3. Feature B engine (B1–B3) with manual path still present → smoke → B4 removals.
4. Docs; control-server Dockerfile slimming can land with step 2 (its gnome stage is
   only dead weight after A5 exists).

## Verification

- Unit: `PullAggregator` (monotonic under growing totals, cached layers weigh zero,
  integer-pct throttle), `parse_sha256sum`, OperationKind alias round-trip, legacy
  state.json fixture with a `"bootstrap"` op loads with hosts intact.
- Template: `docker build -f template/Dockerfile …` → `docker inspect` shows
  `rmng.image=1`/`rmng.base=1`, `/sbin/init` entrypoint, `SIGRTMIN+3`; `docker run
  --privileged` boots systemd, headless GNOME + linger units come up; push to Docker Hub.
- E2E wizard (staging CT 106): wiped `/data`, no local templates → wizard → Download
  template shows a moving byte-accurate bar → Finish → clone from the pulled template
  boots, registers ≤90 s, streams video, stops cleanly.
- Error paths: bogus reference (verbatim daemon error), `ubuntu:26.04` (not-a-template
  error), duplicate name (400), re-pull of cached image completes fast at 100%.
- Auto-swap: stage modified dev payloads → restart server → daemons reconnect → exactly
  one swap per clone, then "up to date" on subsequent Hellos/sweeps (no loop across
  ≥2 sweep cycles). Stop a clone's daemon unit + stale payload → sweep swaps AND starts
  it within ~6 min. Clone create from a stale template swaps once post-create;
  wait-ready tolerates the bounce. Manual surfaces gone (UI hover = commit/account/
  delete; POST /api/clone/redeploy 404s; MCP tools/list has no redeploy).
