# Port forwarding — design

**Status:** approved design, ready for implementation planning
**Date:** 2026-07-03

## Goal

Let an operator forward a TCP port inside a remote host (a clone) to a port on
their own machine — the equivalent of `ssh -L localhost:<local> → host:<remote>`.
Configured from the host's three-dot menu in the frontend; the native viewer runs
the local listeners and tunnels the traffic; errors surface back in the frontend.

## Decisions (locked)

- **Local forwarding only.** A service inside the remote host becomes reachable at
  `127.0.0.1:<local_port>` on the machine running the viewer. (No remote/reverse
  forwarding in v1.)
- **Any host, always on.** Forwards are per-host and independent of which host is
  currently displayed. A forward for host B keeps tunneling while you view host A.
- **Persisted server-side.** Forward *config* lives in `state.json` per host,
  survives control-server restarts, and is re-pushed to the viewer on every
  (re)connect. Runtime *status* is volatile and never persisted.
- **Dedicated data connections (Approach 1).** Bulk forwarded bytes travel on a new
  control-server listen port (`9005`), one fresh TCP connection per accepted local
  socket, raw byte-splice to the clone. This keeps forwarded traffic entirely off
  the latency-critical port-1 video socket.
- **Config/status split.** Persisted config rides the existing `ControlState`
  snapshot; volatile runtime status rides a separate `forwards` SSE event (mirroring
  the existing `stats` event).

## Why this shape

The topology forces most of it:

- The viewer holds a **single** global `TcpStream` to the control-server (port-1,
  `9001`); video/input target the one globally-*selected* clone. It is a GTK4 app on
  blocking std threads — **no tokio** — so a forward listener is just another std
  thread.
- Clones **publish no host ports**. They are reachable **only** from inside the
  `rmng` Docker bridge, which is exactly where the control-server sits. The server
  already dials arbitrary clone ports via `App::dial_host`
  ([app.rs:72](../../../crates/control-server/src/app.rs#L72)) for chat/status/MCP.
  So forwarded bytes **must** flow `viewer → control-server → clone`; there is no
  direct viewer→clone route.
- The port-1 wire protocol is raw `[u8 tag][…]` framing. (`wire::viewer::ToViewer` /
  `FromViewer` are vestigial/unused — do not build on them.)
- Frontend state arrives as the `/events` SSE `ControlState` snapshot, so forward
  config on the `Host` struct reaches the UI with no new plumbing; the volatile
  `forwards` event carries live status the same way `stats` does today.

Rejected alternatives:

- **Multiplex bytes over port-1** — bulk transfers would head-of-line-block the
  video stream on the shared TCP connection (fatal for a low-latency desktop), and
  would need per-channel flow control woven into both blocking read loops.
- **SSH (`ssh -L`)** — clones run no sshd; needs key provisioning; and since clones
  publish no ports it would have to ProxyJump through the control-server anyway —
  the same tunnel plus an sshd dependency, out of character with RMNG's
  self-contained wire protocol.

## Architecture

### Control plane (low-volume config + status)

```
Frontend ──PUT /api/hosts/:id/forwards──▶ control-server
                                            │  persist Host.forwards → state.json
                                            ├──port-1 tag 5 "forwards"──▶ viewer  (desired rules)
viewer ──port-1 tag 2 "forward_status"────▶ control-server
                                            └──SSE "forwards" event───▶ Frontend (live status)
```

### Data plane (bulk bytes; new port 9005)

```
app on operator machine
   │  connects to 127.0.0.1:<local_port>
   ▼
viewer local listener ──new TCP to control-server:9005──▶ control-server
   header {token?, host_id, remote_port}                   │ dial_host(host_id):remote_port
                                                            ▼
                                                     clone :remote_port  (over rmng bridge)
```

### End-to-end flow (host `pega-abc` dev server `:3000` → `localhost:8080`)

1. Operator picks **Port forward…** on `pega-abc`'s three-dot menu, adds
   `3000 → 8080`, saves.
2. Server validates, stores it on the host, persists, pushes the full desired rule
   set to the viewer over port-1 tag 5.
3. Viewer binds `127.0.0.1:8080`, reports `listening` over tag 2 → UI shows green.
4. Operator hits `localhost:8080`. Viewer accepts, opens a data connection to
   `:9005` with header `{host_id: "pega-abc", remote_port: 3000}`.
5. Server dials `pega-abc:3000` over the `rmng` bridge, writes a `0x00` status byte,
   byte-splices both ways. A failed dial returns a non-zero byte + reason and closes.

Because rules carry `host_id` and the server dials per-`host_id` (not the global
`selected`), forwards keep working across host switches. The data plane never
touches the video socket.

## Data model

### Persisted config — `Host` struct

In [crates/wire/src/control.rs](../../../crates/wire/src/control.rs#L59-L143), add to
`Host`:

```rust
#[serde(default, skip_serializing_if = "Vec::is_empty")]
pub forwards: Vec<PortForward>,

pub struct PortForward {
    pub id: String,            // stable rule id, server-assigned (short uuid)
    pub remote_port: u16,      // port inside the clone
    pub local_port: u16,       // bound at 127.0.0.1:<local_port> on the viewer machine
    pub enabled: bool,         // toggle without deleting
    pub label: Option<String>, // optional human note
}
```

This is the only forward-related data written to `state.json`. It is part of the
existing `ControlState` snapshot, so it flows to the frontend over `/events` and
persists across restarts. ts-rs generates the matching TS type.

### Volatile runtime status — `forwards` SSE event

Never persisted. The control-server keeps an in-memory map and emits an SSE event
`forwards` on `/events`, payload `Record<hostId, ForwardRuntime[]>`:

```ts
type ForwardRuntime = {
  id: string;                              // matches PortForward.id
  state: "listening" | "error" | "offline";
  error?: string;                          // set when state === "error"
  activeConns: number;                     // live tunneled connections
};
```

`offline` = no viewer connected (bind state unknown). On viewer disconnect the
server marks that host's rules `offline`; on connect they transition through
`listening`/`error` as reports arrive. The frontend merges runtime into config
by `id`.

## Wire protocol

### Port-1 additions (viewer control link)

Existing tags: server→viewer `0..4` (video/clipboard/cursor/layout/mode),
viewer→server `0,1` (input/clipboard). Add:

- **Server→viewer, tag `5` `forwards`**: `[5][u32be len][JSON]`, JSON =
  ```ts
  { forwardPort: number,                   // control-server's :9005
    rules: Array<{ hostId, id, remotePort, localPort, enabled }> }  // union of all hosts
  ```
  Sent right after the mode handshake on connect, and again on any config change.
  The viewer **reconciles** listeners to match, diffing by `id`: bind new, drop
  removed, restart a rule whose `localPort` changed, leave unchanged rules alone.
  Idempotent — a reconnect never double-binds.

- **Viewer→server, tag `2` `forward_status`**: `[2][u32be len][JSON]`, JSON =
  ```ts
  { hostId, id, state: "listening" | "error", error?: string }
  ```
  Sent when a listener binds, fails to bind, or later dies. The `read_viewer_input`
  loop in [mediaplane.rs](../../../crates/control-server/src/mediaplane.rs) parses it
  → updates the status map → triggers the `forwards` SSE emit.

### Data-plane header (port 9005)

Each tunneled connection opens a fresh TCP to `:9005`, sends one length-prefixed
JSON header, then the stream goes raw:

```ts
{ token?: string, hostId: string, remotePort: number }
```

The server replies with **one status byte** before any raw bytes: `0x00` =
connected (splice begins); non-zero = dial failed (followed by an optional UTF-8
reason, then close). The viewer treats a non-zero byte as a **connection-level**
failure (log/transient), distinct from a **rule-level** bind error.

### HTTP endpoint

`PUT /api/hosts/:id/forwards` with `{ forwards: PortForward[] }` (ids omitted for
new rules; server assigns). Server-side validation → `400` on failure:

- `remote_port` / `local_port` in `1..=65535`;
- **no duplicate `local_port` across all hosts' rules** (they share the one viewer
  machine's port space);
- unknown `host_id` → `404`.

On success: update `Host.forwards` → persist → broadcast `ControlState` → push
tag 5 to the current viewer. New `putForwards(hostId, forwards)` in
[frontend/app/lib/api.ts](../../../frontend/app/lib/api.ts).

## Error handling — two layers

**Case 1 — conflict within RMNG's own config** (two rules on the same local port,
or an out-of-range port). Caught **synchronously at `PUT` time**; rejected with
`400` before it reaches the viewer. The modal shows the error inline immediately.

**Case 2 — the local port is held by a non-RMNG process** (e.g. an unrelated local
dev server). Only the viewer discovers this at `bind()`. Its only channel is the
port-1 link, so the error rides back:

```
viewer:  TcpListener::bind(("127.0.0.1", 8080)) → Err(EADDRINUSE)
   └─ port-1 tag 2 forward_status { hostId, id, state:"error",
                                    error:"127.0.0.1:8080: address already in use" }
control-server: read_viewer_input parses tag 2 → status map → SSE "forwards" event
frontend: es.addEventListener("forwards", …) merges by hostId+id → row red + tooltip
```

- On a bind failure the viewer's listener thread **reports and exits** — no
  spin-retry. The rule stays `error` until the config changes (new push → fresh
  bind) or the viewer reconnects and re-reconciles. Status self-heals; nothing stale
  is persisted.
- **Connection-level** failures (server's dial to the clone refused) are reported
  per-connection and keep the listener **green** — "listener fine, target refused" —
  rather than flipping the whole rule to `error`.

## Components

### Frontend ([frontend/app](../../../frontend/app/))

- Add `"Port forward…"` to the managed block of `OverflowMenu` in
  [SidebarHost.tsx:194-208](../../../frontend/app/components/SidebarHost.tsx#L194-L208);
  thread an `onPortForward(host)` prop through
  [Sidebar.tsx](../../../frontend/app/components/Sidebar.tsx) → `SidebarHost`;
  handler in [_index.tsx](../../../frontend/app/routes/_index.tsx) opens a modal via
  new `forwardHost` state (same pattern as `commitHost`/`changeHost`).
- New `PortForwardModal.tsx`: rows of `remote → localhost:local` with a status
  badge, merging config (`host.forwards`) with runtime (the `forwards` SSE event) by
  `id`; add-row (remote/local/label), enable toggle, delete; Save → `putForwards`.
  Inline `400` text for Case-1; red badge + tooltip for Case-2.
- `useLiveState`: add `es.addEventListener("forwards", …)` (alongside `stats`) →
  `Record<hostId, ForwardRuntime[]>`. Optional small dot on the sidebar host row
  (green = active, red = error).
- `api.ts`: `putForwards(hostId, forwards)`.

### Control-server ([crates/control-server/src](../../../crates/control-server/src/))

- `config.rs`: `ListenConfig.forward: u16` (default `9005`); publish `9005` in
  [compose.yaml](../../../compose.yaml) / [Dockerfile](../../../Dockerfile) alongside
  `9001`.
- `web.rs`: the `PUT` handler (validate → `state.mutate` → persist); the `/events`
  SSE loop also emits the volatile `forwards` event from the status map.
- `mediaplane.rs`:
  - New forward-data listener thread on `listen.forward` — blocking `TcpListener`
    mirroring the video listener: accept → read header → `app.dial_host(host_id)` →
    `TcpStream::connect` → write status byte → two-way splice.
  - On viewer connect (after `write_mode`): push tag 5 (union of all `Host.forwards`
    + `forward_port`).
  - `read_viewer_input`: handle tag 2 → update status map → trigger `forwards` SSE
    emit.
  - Config change → re-push tag 5 to the current viewer; viewer disconnect → mark
    that host's rules `offline`.

### Viewer ([crates/viewer/src](../../../crates/viewer/src/))

- New `forward.rs`: a manager holding `id → listener thread`;
  `reconcile(rules, forward_addr)` diffs by `id` (bind new / drop removed / restart
  changed).
- Read loop
  [main.rs:207-294](../../../crates/viewer/src/main.rs#L207-L294): handle tag 5 →
  reconcile. Report tag 2 via the `try_clone`'d write half (shared writer / channel).
- Per listener: bind `127.0.0.1:local_port`; on accept → thread → connect
  `forward_addr` → write header → read status byte → `std::io::copy` each direction
  with half-close on EOF. `forward_addr` = host from `server_addr` + `forwardPort`
  from tag 5.

## Defaults & scope (v1)

- **TCP only** (no UDP).
- **Bind `127.0.0.1` only** (no LAN exposure of the forward).
- **Managed hosts only** in the menu — `dial_host` also supports unmanaged rows, so
  extending later is trivial, but v1 scopes the UI to clones.
- **Security:** the `9005` header carries the optional viewer `token`, validated
  *if* tokens are enforced — same network-trust posture as the video port today. But
  forwards grant reach to arbitrary clone ports, a privilege beyond viewing: if these
  ports are ever exposed beyond a trusted network, the token must be made mandatory.

## Testing

- **wire:** serde round-trips for `PortForward`, the tag-5 payload, and
  `forward_status` (matching the existing
  [viewer.rs](../../../crates/wire/src/viewer.rs) test style).
- **control-server:** forward-data handshake against a local echo listener (header →
  dial → `0x00` → bytes echo; refused dial → non-zero byte); validation tests
  (duplicate local port → `400`, out-of-range → `400`).
- **viewer:** headless test of `forward.rs` (bind ephemeral → connect → splice
  against a stub), keeping the module decoupled from GTK.
- **Manual E2E:** `python -m http.server 8000` in a clone, forward `8000 → 18000`,
  `curl localhost:18000` from the viewer machine; test `EADDRINUSE` by pre-binding
  `18000`; test remote-down by forwarding a closed port.

## Edge cases

- N concurrent local connections → N independent tunnels.
- Rule disabled/deleted → close listener + force-close its active tunnels.
- `localPort` changed → restart listener.
- Viewer offline → rules show `offline`; no listeners.
- Host stopped → dial refused → connection-level error; listener stays green.
