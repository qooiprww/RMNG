# HTTP API reference — web port (default `:9000`)

The control-server's **port 2** serves the React management UI (a static SPA served from
disk), the JSON control API, and two SSE streams. It binds `0.0.0.0:{listen.web}` with
`ConnectInfo::<SocketAddr>` (the source IP is used by `/api/detector-feedback`).

- **No auth.** All endpoints are open; the server is meant to sit behind a tailnet /
  firewall. Path params that hit the filesystem (`notes`, `uploads`) are validated as
  DNS labels / `<hex>.<ext>` to prevent traversal.
- **Source files:** routes + handlers in [crates/control-server/src/web.rs](../crates/control-server/src/web.rs);
  chat handlers in [chat.rs](../crates/control-server/src/chat.rs); persisted shapes in
  [files.rs](../crates/control-server/src/files.rs); wire types in
  [crates/wire/src/control.rs](../crates/wire/src/control.rs) and [config.rs](../crates/wire/src/config.rs).
- All request/response bodies are JSON unless noted (uploads/feedback are `multipart/form-data`).
- The frontend talks to this port using ts-rs-generated types in `frontend/app/lib/wire/`,
  kept byte-compatible with the Rust `wire` types.

## Endpoint summary

| Method | Path | Purpose | Success |
|---|---|---|---|
| GET | `/events` | Global state SSE (snapshot + diffs) | 200 SSE `ControlState` |
| POST | `/api/activate` | Select the host shown in the viewer | 200 `ControlState` |
| POST | `/api/reorder` | Reorder the host list | 200 `ControlState` |
| POST | `/api/clone` | Start a clone from an image (Linear ticket / new ticket / plain) | 200 `{ok, op}` |
| POST | `/api/clone/redeploy` | Hot-swap a clone's daemon (+agent) binaries | 200 `{ok}` |
| POST | `/api/monitors/apply` | Push the saved monitor layout to all running clones | 200 `{ok,applied,errors}` |
| POST | `/api/delete` | Destroy a clone / unregister a plain host | 200 `Operation` |
| GET | `/api/setup/env` | Setup wizard environment preflight rows | 200 `SetupEnv` |
| GET | `/api/images` | List clone-source images (`rmng.image=1`) | 200 `ImageInfo[]` |
| POST | `/api/images/bootstrap` | Build the base image `rmng/template:<name>` from the base OS | 200 `Operation` |
| POST | `/api/images/commit` | Commit a running clone to a new image | 200 `Operation` |
| POST | `/api/images/delete` | Remove a clone-source image | 200 `{ok}` |
| GET | `/api/notes/:id` | Fetch a host's rich-text notes | 200 `[block]` |
| POST | `/api/notes/:id` | Save a host's notes | 204 |
| POST | `/api/upload` | Upload an image (multipart) | 200 `{url}` |
| GET | `/uploads/:file` | Serve an uploaded image | 200 binary |
| POST | `/api/detector-feedback` | Clone reports a wrong needs-human verdict (multipart) | 200 `{ok,id,host}` |
| GET | `/api/config` | Current config, secrets redacted | 200 `AppConfigRedacted` |
| PUT | `/api/config` | Merge a partial config update (persists 0600) | 200 `{ config, restartRequired, networkWarning? }` |
| POST | `/api/config/test` | Test a setting (currently `"docker"`) | 200 `{ok,message}` |
| POST | `/api/claude/import/check` | Check a clone is signed in via claude.ai | 200 `{ok,email,orgName,subscriptionType}` |
| POST | `/api/claude/import` | Import a Claude account from a signed-in clone | 200 `{ok,email,cleared}` |
| POST | `/api/claude/refresh` | Force one usage poll now | 200 `{ok,rateLimited}` |
| GET | `/api/claude/recommended` | Recommended account for a new clone | 200 `{email}` |
| POST | `/api/claude/swap` | Change a clone's Claude account/group (email/`auto`/`group:<name>`/`none`) | 200 `{ok,account,group,selection}` |
| POST | `/api/claude/rotate` | Run one group-rotation pass now | 200 `{ok}` |
| GET | `/api/chat/:id` | Chat snapshot for a host | 200 `ChatSnapshot` |
| POST | `/api/chat/:id` | Send a message to the host's agent | 202 |
| GET | `/api/chat/:id/events` | Per-host chat SSE | 200 SSE `ChatSnapshot` |
| POST | `/api/chat/:id/abort` | Abort the in-flight agent turn | 204 |
| GET | `/*` | SPA fallback (embedded frontend) | 200 asset / `index.html` |

Error statuses: `400` validation, `404` unknown id/file, `409` chat busy / image still in
use, `500` server (I/O), `502` the Docker daemon or agent-wrapper is unreachable. Error bodies
are a plain string or `{error}`.

---

## State & SSE

### `GET /events`
Subscribe to all control-state changes. Emits a full `ControlState` JSON snapshot
immediately, then a fresh snapshot on every `store.mutate()`; a `ping` comment every 20 s
keeps the connection alive. This is what the dashboard subscribes to.

`ControlState` ([control.rs](../crates/wire/src/control.rs)):

| Field | Type | Notes |
|---|---|---|
| `selected` | `string?` | host id shown in the viewer |
| `monitors` | `MonitorSpec[]` | global desired layout |
| `hosts` | `Host[]` | all registered clones + plain hosts |
| `operations` | `Operation[]` | in-flight + recent clone/delete/bootstrap/commit jobs |
| `claude_accounts` | `ClaudeUsage[]` | per-account 5h/7d usage + spend |

`Host` carries connection info (`id`, `host`, `port`, `username`, …), the Docker `container`
id (full 64-hex; the container *name* equals the host id — `Some` marks a managed clone,
`None` a plain unmanaged row), the `source` image reference, the assigned
`claude_account_email`, Linear metadata (`linear_workspace`, `linear_ticket`, `linear_branch`,
…), `agent_report` (working/idle), `state_note`, and `monitor_state` (working/idle/offline).
`Operation` carries `id`, `kind` (clone/delete/bootstrap/commit), `target`, `source`,
`status`, `step`, `pct`, a rolling `log`, `container`, and timestamps.

---

## Host selection & ordering

### `POST /api/activate` — body `{ "id": string | null }`
Set `selected` (or clear with `null`). Returns the updated `ControlState`. The media plane
re-targets port 1 to the newly selected clone.

### `POST /api/reorder` — body `{ "order": string[] }`
Reorder `hosts` by the given list of ids. Returns the updated `ControlState`.

---

## Clone lifecycle

### `POST /api/clone`
Start a clone container from a clone-source image. Runs async — returns an `Operation` id
immediately; progress flows over `/events`. After the clone is up the server kicks off the
agent's first message ([chat::kickoff_agent](../crates/control-server/src/chat.rs)).

Body (one of three task modes + optional account/instructions):
```jsonc
{
  "image": "rmng/template:base",      // required: clone-source image reference (from GET /api/images)
  // -- pick ONE task mode --
  "ticket": "DEV-123",              // existing Linear ticket, OR
  "create": { "team": "dev", "title": "...", "description": "..." },      // new ticket, OR
  "plain":  { "title": "quick task", "message": "do X" },                 // no ticket
  // -- optional --
  "preset": "<name>" | "auto",      // clone preset (env + Linear key). Ticket mode:
                                    //   absent/"auto" auto-selects by the ticket's labels
                                    //   (400 listing them if nothing matches). Plain mode:
                                    //   REQUIRED while any presets exist. Create mode:
                                    //   REQUIRED (the preset's key creates the ticket).
  "claudeAccount": "user@anthropic.com" | "auto" | "group:<name>" | "none",
  "agentInstructions": "...",       // extra context for the agent-wrapper
  "claudeInstructions": "..."       // extra instructions for Claude Code
}
```
`image` accepts a `rmng/template:<name>` reference, a full `sha256:…` id, or a bare 64-hex id;
whatever form is passed is canonicalized to the reference and recorded on the host as
`source`. The image must carry the `rmng.image=1` label (a raw non-image id is rejected). The
selected preset's vars are written into the clone's session env, plus `LINEAR_API_KEY=<preset
key>` (auths the clone's `linear` MCP). Hostname is derived (`pega-{ticket}` or a slug of the
plain title, with a numeric suffix on collision). Returns `{ "ok": true, "op": Operation }` or
`400 {error}`.

### `POST /api/clone/redeploy` — body `{ "id": string, "daemonOnly"?: bool }`
Hot-swap a running clone's `clone-daemon` (+ `agent-wrapper` unless `daemonOnly`) from the
image's on-disk payloads (`/usr/local/share/rmng/`), pushed into `/opt/rmng/bin/` via a tar
upload + `systemctl --user` bounce — no reprovisioning (~10 s). The daemon reconnects to the
media socket; with `daemonOnly` the Claude session stays alive. Returns `{ok}`. (This is how
an image upgrade reaches already-running clones — see [DEPLOY.md](DEPLOY.md#upgrades).)

### `POST /api/monitors/apply`
Push `config.monitors` to every running clone (those with a `container`): rewrites each
clone's `RMNG_MONITORS` + dummy mode specs and restarts its GNOME + daemon. Returns
`{ "ok": bool, "applied": string[], "errors": string[] }` (partial success allowed).

### `POST /api/delete` — body `{ "id": string }`
Destroy a managed clone (stops it with `SIGRTMIN+3`, removes the container and its
`rmng-dind-<id>` inner-Docker volume) or unregister a plain host. Returns the `Operation`;
progress over `/events`.

---

## Images (clone-source templates) & setup

Clone sources are images labeled `rmng.image=1`, repo `rmng/template:<name>` — there is no
golden-CT / CoW model. `POST` bodies (references contain `/` and `:`, so nothing uses path
params).

### `GET /api/images` → `ImageInfo[]`
List clone-source images, newest first. Each `ImageInfo` carries `id` (`sha256:…`),
`reference` (`rmng/template:<name>`), `size_bytes`, `created_at`, `base` (true for the
wizard-built base, `rmng.base=1`), `created_from` (lineage, `rmng.created-from`), and
`in_use_by` (host ids of live clones whose `source` is this image). `502` if the daemon is
unreachable.

### `POST /api/images/bootstrap` — body `{ "name": string }`
Build the base image `rmng/template:<name>` from the fixed base OS `ubuntu:26.04` (from-zero:
headless GNOME + clone-daemon + agent-wrapper + patched gnome-shell). `name` is a bare DNS
label; the server prepends the repo. Rejects a name that already exists. Returns the driving
`Operation` (kind `bootstrap`, which the setup wizard watches for). The base OS is not
configurable — the patched gnome-shell is compiled against 26.04's GNOME only.

### `POST /api/images/commit` — body `{ "host": string, "name": string }`
Commit a running managed clone (`host`) to a new clone-source image `rmng/template:<name>`
(kind `commit`). `docker commit` **excludes volume mounts**, so the clone's inner-Docker state
(`/var/lib/docker`) never enters the image — clones always start with an empty inner Docker.
On-disk credentials in the clone's home **are** baked in (logged as a warning). Rejects a name
that already exists.

### `POST /api/images/delete` — body `{ "reference": string }` → `{ok}`
Remove a clone-source image. `409` if any host still runs on it (`in_use_by` non-empty) or a
running clone/commit references it; the daemon's own "in use by a container" `409` is surfaced
too.

### `GET /api/setup/env` → `SetupEnv`
The setup wizard's environment preflight: `{ rows: EnvCheckRow[] }`, each row `{ id, label,
ok, detail, required }`. Rows, in order: **Docker daemon** reachable (`dockerDaemon`,
required), **control-server container** detected (`selfContainer`, info — absence = dev mode),
**clone media socket mount** at `/srv/rmng-sock` (`sockMount`, required), **GPU render node**
`/dev/dri/renderD128` (`renderNode`, required). Cached from the Docker self-setup probe
(refreshed at startup and by `POST /api/config/test {docker}`).

---

## Notes & uploads

### `GET /api/notes/:id` → `[block]` &nbsp;·&nbsp; `POST /api/notes/:id` (204)
Per-host rich-text notes (BlockNote block array), stored at `data/notes/{id}.json`. `:id`
must be a DNS label. GET returns `[]` if none.

### `POST /api/upload` (multipart `file`) → `{ "url": "/uploads/<hex>.<ext>" }`
Image upload (png/jpeg/gif/webp/svg/avif/bmp, ≤15 MB) → `data/uploads/`.

### `GET /uploads/:file`
Serve an uploaded image by its generated `<16-hex>.<ext>` name, with the right Content-Type.

---

## Detector feedback

### `POST /api/detector-feedback` (multipart)
A clone's `clone-daemon report-detection` posts here when the needs-human detector's verdict
was wrong. The **caller's source IP** is matched against `hosts[].host` to resolve the clone
(so no auth/clone arg is needed). Fields: `kind` (`false-positive`|`false-negative`,
required), `detectorVerdict`, `detectorReason`, `actualState`, repeated `ignoreReason`,
`note`, and an optional `screenshot` file. Persists a JSON record + screenshot under
`data/detector-feedback/`. Returns `{ "ok": true, "id": "...", "host": "..." }`.

---

## Configuration

### `GET /api/config` → `AppConfigRedacted`
The full config with the only secret (preset Linear keys) replaced by `linearKeySet: bool`.
Everything else is returned verbatim — ports, monitors, the `docker` block
(`socket`/`subnet`/`hostnamePrefix`/`cloneCpus`/`cloneMemoryMb`; no secret — the local daemon
socket needs none), `staticDir`/`cloneSocket`/`chroma`, `setupComplete`, `detectorInferenceUrl`,
and claude poll config. See [PROTOCOL.md](PROTOCOL.md#config-schema) for the schema.

### `PUT /api/config` (partial merge) → `{ config, restartRequired, networkWarning? }`
Deep-merge a partial config over the stored one, persist to disk at `0600`, apply live.
Returns the redacted config plus `restartRequired: boolean` — set when a restart-required
field changed (the four listen ports, `cloneSocket`, `docker.socket`, `staticDir`, `chroma`)
so the UI can prompt for a restart. `cloneSocket` still triggers this pre-latch (the server
bound the old path at startup) even though it is a one-time field (see below). A wizard-finish
flip (`setupComplete` false → true) materializes the lazy `rmng` network here; a failure is
non-fatal and echoed as `networkWarning`. Merge rules: an **empty string keeps** the stored
value; a non-empty string replaces it; `presets` rows merge by name (blank `linearKey` keeps
the stored one). `docker.subnet` is validated as an IPv4 `/16`–`/24` CIDR. One-time fields
(`dataDir`, `cloneSocket`, `docker.subnet`) are locked once `setupComplete` latches (which
itself is a one-way latch).

### `POST /api/config/test` — body `{ "what": "docker" }` → `{ ok, message }`
Synchronously test a setting. Currently only `"docker"`: re-runs the Docker self-setup probe
and collapses the environment report (daemon reachable, sock mount, render node) into a single
`(ok, message)` verdict. The row-by-row breakdown is `GET /api/setup/env`.

---

## Claude accounts

| Endpoint | Body | Returns | Does |
|---|---|---|---|
| `POST /api/claude/import/check` | `{host}` | `{ok, email, orgName, subscriptionType}` | Run `claude auth status` in the clone; require a claude.ai login and return its identity |
| `POST /api/claude/import` | `{host}` | `{ok, email, cleared}` | Harvest the clone's OAuth pair (read off its disk) into the server's secret store, then delete the clone's credentials file |
| `POST /api/claude/refresh` | — | `{ok, rateLimited}` | Force one usage poll; `rateLimited` if any account hit 429 |
| `GET /api/claude/recommended` | — | `{email}` | Pinned account, else lowest-usage; `null` if none |
| `POST /api/claude/swap` | `{host, account}` | `{ok, account, group, selection}` | Resolve `account` (email / `auto` / `group:<name>` / `none`) and write the clone's `~/.claude/.credentials.json` via `docker exec`. A `group:` selection binds the clone to that group for rotation; `none` removes the credentials file (`account` null); the verbatim choice is echoed as `selection` and stored on the host (`502` if unreachable) |
| `POST /api/claude/rotate` | — | `{ok}` | Run one group-rotation pass immediately (the rotator otherwise runs every 10 min). Sticky: a clone keeps its account while it stays eligible (member, imported, 5h usage ≤ 90%); only clones whose account fell out of eligibility move, to the least-loaded / least-used member |

The single-token model (the server owns each account's OAuth pair and pushes the current
short-lived access token to assigned clones on every refresh) is described in
[PROTOCOL.md](PROTOCOL.md#claude-accounts).

---

## Per-host agent chat

The control-server proxies chat to each clone's agent-wrapper (`http://{host}:{agent_port}`,
default `:4096`), persisting history at `data/chats/{id}.json`.

| Endpoint | Body | Returns | Does |
|---|---|---|---|
| `GET /api/chat/:id` | — | `ChatSnapshot` | `{busy, activity, messages[]}` snapshot |
| `POST /api/chat/:id` | `{text}` | `202` / `409` if busy | Persist the user message, set busy, spawn the turn (opens the wrapper's `/events`, POSTs `/prompt`, relays activity, records the reply). Watchdog: 30 min hard / 3 min idle |
| `GET /api/chat/:id/events` | — | SSE `ChatSnapshot` | Snapshot + a fresh one on each message/activity/busy change; 20 s ping |
| `POST /api/chat/:id/abort` | — | `204` | Best-effort POST to the wrapper's `/abort`; clears busy |

`ChatMessage` = `{ id, role (user|assistant), text, ts }`.

---

## SPA fallback

### `GET /*`
Serves the installed React build from disk; unknown paths fall back to `index.html` for
client-side routing. The bundle is resolved at startup: `/usr/local/share/rmng/static` in the
image, else the repo dev build (`frontend/build/client`). A non-empty `staticDir` config field
(Settings → Advanced; restart-required) overrides that with any disk path (frontend
hot-reload during dev). If no frontend resolves anywhere, the route returns a 404 hint and the
API stays up.
