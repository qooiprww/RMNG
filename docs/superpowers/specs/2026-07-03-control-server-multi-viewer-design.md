# Multiple simultaneous viewers — design

**Status:** approved design, ready for implementation planning
**Date:** 2026-07-03

## Goal

Let more than one native [viewer](../../../crates/viewer/README.md) connect to a
single control-server at the same time. Today the port-1 media plane holds exactly
one viewer slot — a second connection silently overwrites the first
([mediaplane.rs:143](../../../crates/control-server/src/mediaplane.rs#L143)). After
this change, N viewers can be connected concurrently, each receiving the video /
cursor / clipboard / forwards stream and each able to send input.

## Decisions (locked)

- **Mirror model (shared selection).** Every viewer sees and drives the same
  globally-`selected` clone. Activating a different clone (from any viewer or the web
  UI) switches everyone. The encoder set stays **shared** — one encode, N deliveries.
  Per-viewer independent clone selection is explicitly **not** in scope.
- **Free-for-all input.** Every viewer's input is relayed to the selected clone;
  users coordinate socially. No control lock, no driver role, no new protocol. This
  is the existing input path, just from N reader threads instead of one.
- **Per-viewer isolation via channels (Approach B).** Each viewer owns one bounded
  channel + one writer thread. The shared encode thread `try_send`s each frame
  (non-blocking) and never touches a viewer socket directly, so a slow viewer cannot
  stall the encoder or the other viewers.
- **Overflow disconnects the slow viewer.** A viewer whose channel fills is dropped;
  it reconnects and re-primes with a fresh keyframe. No shared-encoder IDR storms.
- **Per-viewer clipboard sources.** The single `VIEWER_SRC` constant becomes a
  per-viewer id so the lazy clipboard broker can route requests/replies to the exact
  viewer, and so copies propagate viewer↔viewer.
- **Ref-counted forward status.** Forward runtime status is cleared only when the
  **last** viewer disconnects, fixing today's bug where any one viewer leaving blanks
  everyone's status.
- **No `viewer` / `wire` / frontend changes.** The entire change lives in
  `control-server` (`mediaplane.rs` + a small `forward.rs` addition). Extra viewers
  are just independent TCP connections to port 1; the wire protocol is unchanged.

## Why this shape

The mirror + free-for-all decisions are what keep this tractable:

- Because all viewers watch the **same** selected clone, the existing single shared
  encoder set (`HashMap<monitor_id, Encoder>`,
  [mediaplane.rs:100](../../../crates/control-server/src/mediaplane.rs#L100)) stays
  correct as-is. Multi-viewer becomes a **fan-out** problem — encode once, deliver to
  N sockets — not a re-architecture of the media pipeline. Per-viewer independent
  selection would instead require a distinct encoder set per distinct clone (more
  GPU/CPU) and would decouple video-plane selection from the global `state.selected`
  that the web UI also reads.
- Because input is free-for-all, the input path is unchanged: each viewer already
  spawns its own `read_viewer_input` thread that relays to the selected clone. N
  threads relaying to one clone needs no arbitration protocol.
- The media plane is **blocking std threads, no tokio** (matching the port-forward
  design's constraints). So per-viewer isolation is naturally expressed as a
  `std::sync::mpsc::sync_channel` (bounded) + a std writer thread per viewer, not an
  async task.

Rejected alternatives:

- **Direct fan-out under one lock (Approach A / C).** Writing N sockets from the
  encode thread means a wedged socket adds per-frame latency for everyone (A, with a
  `SO_SNDTIMEO` mitigation) or stalls the encode thread while holding a lock that
  input/clipboard also take (C). Chosen against in favor of true per-viewer isolation.
- **Take-control lock (one driver).** A control-ownership protocol prevents cursor
  fighting but needs new port-1 messages and viewer-side UI. Out of scope; social
  coordination is acceptable for the target (a handful of operators).

## Architecture

### Viewer registry

Replace `viewer: Arc<Mutex<Option<TcpStream>>>`
([mediaplane.rs:120](../../../crates/control-server/src/mediaplane.rs#L120)) with:

```rust
struct ViewerConn {
    id: u64,
    tx: std::sync::mpsc::SyncSender<Arc<[u8]>>, // bounded; carries pre-framed bytes
}
type Viewers = Arc<Mutex<HashMap<u64, ViewerConn>>>;
```

- `id` is a process-lifetime counter (`AtomicU64`).
- `tx` carries **fully pre-framed** message bytes (`[tag][…]`) as `Arc<[u8]>` — built
  **once** per message and cloned (cheap refcount bump) into each viewer's channel.
- The registry mutex is held only briefly: to insert/remove a viewer, or to snapshot
  `tx` handles for a fan-out. `try_send` is non-blocking, so it is safe to send while
  holding the lock.

### Writer thread + channel (the isolation boundary)

On viewer connect:

1. Assign `id`. `try_clone` the accepted `TcpStream` → **read half** (input thread)
   and **write half** (writer thread).
2. Create `sync_channel::<Arc<[u8]>>(CAP)` where `CAP` is a bounded queue (target ≈ a
   couple seconds of frames; a concrete default is chosen during implementation and
   made a named constant). Spawn the **writer thread**, which owns the write half and
   the `Receiver`: `while let Ok(buf) = rx.recv() { if write_all(&buf).is_err() { break } }`,
   then `shutdown(Both)` on exit (see teardown — this wakes the reader whether the loop
   ended on a write error *or* on channel disconnect).
3. Push the **mode handshake first** (so it precedes any video), then insert
   `ViewerConn` into the registry, then prime this viewer (below).

The encode thread and all metadata producers only ever `try_send` into these channels
— they never block on a socket. That is the whole point of Approach B.

### Fan-out helpers

Each `write_*` helper splits into two forms over pre-framed bytes:

- **Broadcast** — build bytes once, `for each viewer in registry: try_send`. Used for
  live video AUs (the encoder callback), cursor, layout, clipboard offers, forward-set
  pushes.
- **Targeted** — send to one `ViewerConn.tx`. Used for connect-time priming and for
  clipboard request/reply routing to a specific viewer.

`try_send` outcomes:
- `Ok` — queued.
- `Err(Full)` — viewer too slow → **remove it from the registry** (see teardown). Do
  not block, do not drop individual AUs (dropping mid-stream AUs corrupts the decoder
  until the next keyframe; a shared IDR to recover would penalize all viewers).
- `Err(Disconnected)` — writer already gone → remove from registry (idempotent).

### Connect / prime

Priming a late joiner without disturbing others:

- **Metadata** (layout, last cursor shape, current clipboard offer, forward set) is
  sent **targeted** to the new viewer's channel only.
- **Video**: `force_idr_all` + re-encode the selected clone's last captured frame so
  the new viewer paints immediately even on a static, damage-driven desktop. This runs
  through the **shared** encoder, so existing viewers receive one redundant keyframe
  per join — negligible at operator-scale connect rates.

Ordering guarantee: because `mode` is pushed to the new viewer's channel before the
viewer is inserted into the registry, and the channel is FIFO, `mode` always precedes
the first video AU that viewer receives.

### Teardown (single-owner rule)

A viewer can die three ways: it closes the socket (reader sees EOF), its socket write
fails (writer sees an error), or its channel overflows (fan-out sees `Full`). All
converge on **exactly one** teardown, owned by the **reader thread**:

- **Reader thread** (sole teardown owner) — on EOF/error: remove the viewer from the
  registry, `clip_forget_source(viewer_src)`, and `forwards.viewer_left()`.
- **Writer thread** — on **any** exit (write error *or* channel disconnect from the
  registry entry being dropped): `shutdown(Both)` the socket to wake the reader, then
  exit. Does *not* run teardown.
- **Overflow (fan-out)** — remove the `ViewerConn` from the registry. Dropping its
  `tx` disconnects the writer's channel → the writer exits → `shutdown(Both)` → the
  reader wakes and runs teardown.

Removing a viewer from the registry drops the only long-lived `tx`; once transient
fan-out clones drop, the channel disconnects and the writer stops. All teardown steps
are idempotent (keyed by `id`).

### Clipboard (per-viewer sources)

- `VIEWER_SRC` (a single constant,
  [mediaplane.rs:51](../../../crates/control-server/src/mediaplane.rs#L51)) becomes a
  per-viewer source id, e.g. `format!("\0viewer:{id}")`.
- `send_clip_to`: a dest with the viewer prefix resolves to that viewer's `tx`;
  otherwise it is a clone id (unchanged).
- `clip_dests(source)`: all clone ids **plus all viewer source ids**, minus `source`.
  So a viewer's offer fans to every clone *and* the other viewers (copy-on-A /
  paste-on-B works), and a clone's offer fans to every viewer.
- `broker_request` routes the `Request` to the single owner (a specific clone or a
  specific viewer); `broker_data` routes each `Data` reply back to its recorded
  requesters. This per-source routing is **required for correctness** — with one
  shared viewer id, a request to "the viewer owner" would fan to all viewers and
  collect conflicting replies.
- `clip_forget_source(id)` on viewer disconnect: if the leaving viewer owns the
  current offer, drop the offer; scrub its id from every `pending` requester list.

No `wire` or viewer changes: the viewer still sends untagged `ClipboardMsg`; the
server assigns the source id per connection.

### Port forwards

The data plane (`serve_forward`,
[mediaplane.rs:691](../../../crates/control-server/src/mediaplane.rs#L691)) is already
per-connection and needs no change. Two adjustments:

- The desired forward set is **broadcast** to every viewer on connect and on change;
  each viewer opens its own local listeners (each operator machine gets its own
  `127.0.0.1:<local_port>`).
- [`ForwardBus`](../../../crates/control-server/src/forward.rs) gains a viewer
  ref-count: `viewer_joined()` on connect, `viewer_left()` on disconnect. The
  end-of-`read_viewer_input` `clear()`
  ([mediaplane.rs:656](../../../crates/control-server/src/mediaplane.rs#L656)) is
  replaced by `viewer_left()`, which clears runtime status **only when the count
  reaches zero**. Conn counts already aggregate correctly across viewers.

**Limitation (accepted):** the per-rule `listening`/`error` state remains
last-writer-wins across viewers. The frontend renders one status per rule and has no
per-viewer identity; making status truly per-viewer is a cross-cutting web-UI change,
out of scope. Forwards still *function* per-viewer regardless of which state the UI
shows.

**Limitation (accepted):** a *chronically* slow viewer will thrash-reconnect
(connect → overflow → disconnect → reconnect). This is acceptable for now; a
viewer-side reconnect backoff is out of scope (viewer code is untouched).

## Data flow (after)

```
clone-daemon ──frames──► shared encoder(s) ──AU──► [fan-out]
                                                     │ try_send Arc<[u8]>
                                    ┌────────────────┼────────────────┐
                                    ▼                ▼                ▼
                             viewer 1 chan    viewer 2 chan    viewer 3 chan
                                    │                │                │
                             writer thread    writer thread    writer thread
                                    ▼                ▼                ▼
                                socket 1         socket 2         socket 3

each viewer's input thread ──InputMsg──► selected clone   (free-for-all merge)
                            ──Clipboard─► broker (per-viewer source routing)
                            ──FwdStatus─► ForwardBus (ref-counted)
```

## Scope

- **In scope:** `crates/control-server/src/mediaplane.rs` (registry, writer threads,
  fan-out helpers, per-viewer priming/teardown, per-viewer clipboard sources) and
  `crates/control-server/src/forward.rs` (ref-counted `viewer_joined`/`viewer_left`).
- **Out of scope / unchanged:** the `viewer` binary, the `wire` crate, the web
  frontend, global `state.selected`, the input relay path, the forward data plane.

## Testing

- **Fan-out isolation:** a viewer whose channel is full is removed without blocking
  other viewers' sends (a fast viewer keeps receiving while a full one is dropped).
- **Clipboard routing:** with two distinct viewer sources, an offer from viewer A
  reaches viewer B and the clones; a `Request` for a viewer-owned offer routes only to
  that viewer; `Data` routes back to the correct requester; `clip_forget_source`
  drops a leaving owner's offer and scrubs pending lists.
- **Forward ref-count:** status persists while ≥1 viewer is connected and clears only
  when the last leaves.
- **Regression:** existing `teardown_if_current`, `build_forwards_msg`,
  `forward_header_frames_round_trip`, `splice_forward`, and `ForwardBus` tests stay
  green.
