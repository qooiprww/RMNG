# Multiple Simultaneous Viewers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let N native viewers connect to one control-server at once, each receiving the video/cursor/clipboard/forwards stream and each able to send input.

**Architecture:** Mirror model — all viewers watch the same globally-`selected` clone through the existing shared encoder set. Multi-viewer is a fan-out problem: the encode thread pre-frames each message once and `try_send`s it into a per-viewer bounded channel drained by a per-viewer writer thread, so a slow viewer cannot stall the encoder or the others. Overflow disconnects the slow viewer (it reconnects and re-primes). Clipboard sources become per-viewer; forward-status clear becomes ref-counted.

**Tech Stack:** Rust, `std::thread` + `std::sync::mpsc::sync_channel` (no tokio in this path), `media` crate (GStreamer encoders + unix-socket `Conn`/`Listener`), `wire` crate (protocol types), `axum` (unaffected).

## Global Constraints

- **Design spec:** `docs/superpowers/specs/2026-07-03-control-server-multi-viewer-design.md` — all decisions there are binding.
- **Scope is `control-server` only.** Do NOT modify `crates/viewer/`, `crates/wire/`, `crates/media/`, or the frontend. Extra viewers are plain independent TCP connections to port 1; the wire protocol is unchanged.
- **Mirror model:** the encoder set stays a single shared `HashMap<monitor_id, Encoder>`. Do not add per-viewer encoders.
- **Free-for-all input:** every viewer's input relays to the globally-`selected` clone; no control lock.
- **No blocking on viewer sockets from the encode thread.** All fan-out is non-blocking `try_send`. This is the core isolation guarantee — never `write_all` a viewer socket from the encode/metadata threads.
- **Blocking std threads, no tokio** in `mediaplane.rs`'s media path (matching existing code).
- **Build/test where the `media` crate links GStreamer** (the VA build box — see the CT 106 build-box note). Run `cargo test -p control-server` and `cargo build -p control-server` there.
- Existing tests must stay green: `teardown_only_removes_own_connection`, `build_forwards_msg_unions_enabled_rules_only`, `forward_header_frames_round_trip`, `splice_forward_pipes_both_ways` (mediaplane.rs); `report_then_snapshot_reflects_state`, `conn_count_and_clear` (forward.rs).

---

## File Structure

- **`crates/control-server/src/forward.rs`** (modify) — add a viewer ref-count so runtime forward status is cleared only when the last viewer disconnects.
- **`crates/control-server/src/mediaplane.rs`** (modify) — the whole change. Tasks 2–4 *add* new, unit-tested, initially-unused helpers (viewer registry, framing, per-viewer clipboard). Task 5 rewires the live paths onto those helpers and deletes the single-viewer code.

Tasks 1–4 each compile and pass their own tests with the old single-viewer path still intact (new helpers carry `#[allow(dead_code)]` until Task 5 wires them in and removes the attribute). Task 5 is the irreducible integration.

---

## Task 1: ForwardBus ref-counted viewer clear

**Files:**
- Modify: `crates/control-server/src/forward.rs`
- Test: `crates/control-server/src/forward.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing new.
- Produces: `ForwardBus::viewer_joined(&self)`, `ForwardBus::viewer_left(&self)`. `viewer_left` clears all runtime status (and broadcasts) only when the connected-viewer count returns to zero. Task 5's `read_viewer_input` teardown calls `viewer_left`; the accept loop calls `viewer_joined`.

- [ ] **Step 1: Write the failing test**

Add to `crates/control-server/src/forward.rs` inside `mod tests`:

```rust
#[test]
fn viewer_refcount_clears_only_when_last_leaves() {
    let bus = ForwardBus::new();
    let (_seed, mut rx) = bus.subscribe();
    // Two viewers connect; one reports a listening rule.
    bus.viewer_joined();
    bus.viewer_joined();
    bus.report(ForwardStatusMsg {
        host_id: "h".into(),
        id: "f8080".into(),
        state: ForwardState::Listening,
        error: None,
    });
    let _ = rx.try_recv(); // drain the report broadcast

    // First viewer leaves — status must persist (another viewer remains).
    bus.viewer_left();
    assert!(bus.snapshot_json().contains("\"f8080\""), "status cleared too early");

    // Last viewer leaves — status is now cleared.
    bus.viewer_left();
    assert_eq!(bus.snapshot_json(), "{}", "status not cleared after last viewer left");

    // Underflow guard: an extra leave with no viewers must not panic or clear-loop.
    bus.viewer_left();
    assert_eq!(bus.snapshot_json(), "{}");
}
```

Note: `snapshot_json` is currently private. Add `#[cfg(test)]` visibility by leaving it private and calling it from the same-module test (it already is same-module — no change needed).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p control-server viewer_refcount_clears_only_when_last_leaves`
Expected: FAIL — `no method named viewer_joined` / `viewer_left` found.

- [ ] **Step 3: Write minimal implementation**

In `crates/control-server/src/forward.rs`, add the import and a counter field, and the two methods.

Change the top `use` block to add `AtomicUsize`:

```rust
use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::broadcast;
use wire::forward::{ForwardRuntime, ForwardState, ForwardStatusMsg};
```

Add the field to the struct:

```rust
pub struct ForwardBus {
    tx: broadcast::Sender<String>,
    inner: RwLock<HashMap<String, HashMap<String, ForwardRuntime>>>,
    /// Number of currently-connected viewers. Runtime status is a union of what the
    /// viewers report; it is cleared only when this returns to zero so one viewer
    /// disconnecting does not blank a rule another viewer is still serving.
    viewers: AtomicUsize,
}
```

Initialize it in `new`:

```rust
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(16);
        Self { tx, inner: RwLock::new(HashMap::new()), viewers: AtomicUsize::new(0) }
    }
```

Add the two methods inside `impl ForwardBus` (place them next to `clear`):

```rust
    /// A viewer connected: it will start reporting its forward listeners.
    pub fn viewer_joined(&self) {
        self.viewers.fetch_add(1, Ordering::SeqCst);
    }

    /// A viewer disconnected. Clear all runtime status only when the last one leaves
    /// (its listeners are gone, and no other viewer remains to keep the union alive).
    pub fn viewer_left(&self) {
        // Saturating decrement: never underflow if called spuriously.
        let prev = self
            .viewers
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| Some(v.saturating_sub(1)))
            .unwrap();
        if prev <= 1 {
            self.clear();
        }
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p control-server --lib forward`
Expected: PASS — `viewer_refcount_clears_only_when_last_leaves`, `report_then_snapshot_reflects_state`, `conn_count_and_clear` all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/control-server/src/forward.rs
git commit -m "feat(control-server): ref-count viewers in ForwardBus so status clears only when the last leaves"
```

---

## Task 2: Viewer registry + fan-out isolation primitive

**Files:**
- Modify: `crates/control-server/src/mediaplane.rs` (add near the top, after imports)
- Test: `crates/control-server/src/mediaplane.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing new.
- Produces (all initially `#[allow(dead_code)]`, wired in Task 5):
  - `struct ViewerConn { id: u64, tx: std::sync::mpsc::SyncSender<Arc<[u8]>> }`
  - `type Viewers = Arc<Mutex<HashMap<u64, ViewerConn>>>`
  - `const VIEWER_CHAN_CAP: usize`
  - `fn next_viewer_id() -> u64`
  - `fn broadcast_bytes(viewers: &Viewers, buf: &Arc<[u8]>)` — try_send `buf.clone()` to every viewer; remove any whose channel is full or disconnected.
  - `fn send_bytes_to(viewers: &Viewers, id: u64, buf: Arc<[u8]>)` — targeted send; remove the viewer on failure.
  - `fn remove_viewer(viewers: &Viewers, id: u64)` — idempotent removal (dropping the stored `tx`).

- [ ] **Step 1: Write the failing test**

Add to `crates/control-server/src/mediaplane.rs` inside `mod tests`:

```rust
#[test]
fn broadcast_drops_only_the_full_viewer() {
    use std::sync::mpsc::sync_channel;

    // Viewer A: capacity 4, actively has room. Viewer B: capacity 1, pre-filled → full.
    let (tx_a, rx_a) = sync_channel::<Arc<[u8]>>(4);
    let (tx_b, _rx_b) = sync_channel::<Arc<[u8]>>(1);
    let filler: Arc<[u8]> = Arc::from(vec![0u8; 1]);
    tx_b.try_send(filler).unwrap(); // B is now full (nobody drains _rx_b)

    let viewers: Viewers = Arc::new(Mutex::new(HashMap::new()));
    viewers.lock().unwrap().insert(1, ViewerConn { id: 1, tx: tx_a });
    viewers.lock().unwrap().insert(2, ViewerConn { id: 2, tx: tx_b });

    let msg: Arc<[u8]> = Arc::from(vec![7u8, 8, 9]);
    broadcast_bytes(&viewers, &msg);

    // A received the message and stayed; B was full and got removed.
    assert_eq!(&*rx_a.recv().unwrap(), &[7u8, 8, 9]);
    assert!(viewers.lock().unwrap().contains_key(&1), "fast viewer was wrongly dropped");
    assert!(!viewers.lock().unwrap().contains_key(&2), "full viewer was not dropped");
}

#[test]
fn remove_viewer_is_idempotent() {
    use std::sync::mpsc::sync_channel;
    let (tx, _rx) = sync_channel::<Arc<[u8]>>(1);
    let viewers: Viewers = Arc::new(Mutex::new(HashMap::new()));
    viewers.lock().unwrap().insert(5, ViewerConn { id: 5, tx });
    remove_viewer(&viewers, 5);
    remove_viewer(&viewers, 5); // second call must be a no-op, not a panic
    assert!(viewers.lock().unwrap().is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p control-server broadcast_drops_only_the_full_viewer`
Expected: FAIL — `cannot find type Viewers` / `ViewerConn` / `function broadcast_bytes`.

- [ ] **Step 3: Write minimal implementation**

At the top of `crates/control-server/src/mediaplane.rs`, extend the imports. The existing imports already include `std::sync::{Arc, Mutex}` and `std::collections::HashMap`. Add the channel + atomic imports:

```rust
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
```

Then add this block right after the imports (before `struct ClipState`):

```rust
/// One connected viewer: its id + the sender into its per-viewer writer thread.
/// The writer thread owns the socket; producers only ever `try_send` here, so a slow
/// viewer's socket never blocks the encode or metadata threads.
#[allow(dead_code)] // wired in by the multi-viewer integration task
struct ViewerConn {
    id: u64,
    tx: SyncSender<Arc<[u8]>>,
}

/// The live set of viewers, keyed by id. Held under a short-lived lock only to
/// snapshot/insert/remove; all sends are non-blocking `try_send`.
#[allow(dead_code)]
type Viewers = Arc<Mutex<HashMap<u64, ViewerConn>>>;

/// Bounded per-viewer queue depth. `try_send` overflow disconnects the viewer (it
/// reconnects + re-primes). Bounds worst-case queued memory at CAP × max-AU per slow
/// viewer; tune here if joins on very bursty desktops need more slack.
#[allow(dead_code)]
const VIEWER_CHAN_CAP: usize = 128;

/// Monotonic per-process viewer id.
#[allow(dead_code)]
fn next_viewer_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Fan one pre-framed message out to every viewer. A viewer whose channel is full
/// (too slow) or disconnected (writer already gone) is removed from the registry;
/// dropping its stored `tx` lets its writer thread's channel disconnect, which tears
/// the viewer down. Never blocks: `try_send` is non-blocking.
#[allow(dead_code)]
fn broadcast_bytes(viewers: &Viewers, buf: &Arc<[u8]>) {
    let mut guard = viewers.lock().unwrap();
    let mut dead: Vec<u64> = Vec::new();
    for (id, v) in guard.iter() {
        if v.tx.try_send(buf.clone()).is_err() {
            dead.push(*id);
        }
    }
    for id in dead {
        guard.remove(&id);
    }
}

/// Send one pre-framed message to a single viewer; remove it on failure.
#[allow(dead_code)]
fn send_bytes_to(viewers: &Viewers, id: u64, buf: Arc<[u8]>) {
    let mut guard = viewers.lock().unwrap();
    if let Some(v) = guard.get(&id) {
        if v.tx.try_send(buf).is_err() {
            guard.remove(&id);
        }
    }
}

/// Idempotently remove a viewer (dropping its `tx`).
#[allow(dead_code)]
fn remove_viewer(viewers: &Viewers, id: u64) {
    viewers.lock().unwrap().remove(&id);
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p control-server broadcast_drops_only_the_full_viewer remove_viewer_is_idempotent`
Expected: PASS (both).

- [ ] **Step 5: Commit**

```bash
git add crates/control-server/src/mediaplane.rs
git commit -m "feat(control-server): add viewer registry + non-blocking fan-out primitive"
```

---

## Task 3: Framing helpers (Arc<[u8]>) + broadcast wrappers

**Files:**
- Modify: `crates/control-server/src/mediaplane.rs`
- Test: `crates/control-server/src/mediaplane.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `broadcast_bytes`, `Viewers` (Task 2); the existing tag consts `T_VIDEO`, `T_MODE`, `T_CLIPBOARD`, `T_CURSOR`, `T_LAYOUT`, `T_FORWARDS`.
- Produces (initially `#[allow(dead_code)]`):
  - `fn frame_video(monitor_id: u32, au: &[u8]) -> Arc<[u8]>` → `[T_VIDEO][u32be monitor_id][u32be len][au]`.
  - `fn frame_json<T: serde::Serialize>(tag: u8, msg: &T) -> Option<Arc<[u8]>>` → `[tag][u32be len][json]`.
  - `fn broadcast_video(viewers: &Viewers, monitor_id: u32, au: &[u8])`.
  - `fn broadcast_json<T: serde::Serialize>(viewers: &Viewers, tag: u8, msg: &T)`.

These are byte-identical to the current `write_frame` / `write_json_frame` wire formats, so the viewer needs no change.

- [ ] **Step 1: Write the failing test**

Add to `mod tests`:

```rust
#[test]
fn frame_video_layout_matches_wire() {
    let au = [0xAAu8, 0xBB, 0xCC];
    let f = frame_video(0x01020304, &au);
    // [tag=0][monitor_id be][len be][au]
    assert_eq!(f[0], T_VIDEO);
    assert_eq!(&f[1..5], &0x01020304u32.to_be_bytes());
    assert_eq!(&f[5..9], &(au.len() as u32).to_be_bytes());
    assert_eq!(&f[9..], &au);
}

#[test]
fn frame_json_layout_matches_wire() {
    let f = frame_json(T_CURSOR, &serde_json::json!({"x": 1})).unwrap();
    let json = serde_json::to_vec(&serde_json::json!({"x": 1})).unwrap();
    assert_eq!(f[0], T_CURSOR);
    assert_eq!(&f[1..5], &(json.len() as u32).to_be_bytes());
    assert_eq!(&f[5..], &json[..]);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p control-server frame_video_layout_matches_wire frame_json_layout_matches_wire`
Expected: FAIL — `cannot find function frame_video` / `frame_json`.

- [ ] **Step 3: Write minimal implementation**

Add these functions in `mediaplane.rs` near the other `write_*`/framing helpers (e.g. just after `write_json_frame`):

```rust
/// Pre-frame one H.264 AU: `[0u8][u32be monitor_id][u32be len][AnnexB]`. Built once
/// and cloned (refcount) into each viewer's channel.
#[allow(dead_code)]
fn frame_video(monitor_id: u32, au: &[u8]) -> Arc<[u8]> {
    let mut framed = Vec::with_capacity(9 + au.len());
    framed.push(T_VIDEO);
    framed.extend_from_slice(&monitor_id.to_be_bytes());
    framed.extend_from_slice(&(au.len() as u32).to_be_bytes());
    framed.extend_from_slice(au);
    Arc::from(framed)
}

/// Pre-frame a tagged JSON message: `[tag][u32be len][json]`. `None` if serialization
/// fails (never for our types).
#[allow(dead_code)]
fn frame_json<T: serde::Serialize>(tag: u8, msg: &T) -> Option<Arc<[u8]>> {
    let json = serde_json::to_vec(msg).ok()?;
    let mut framed = Vec::with_capacity(5 + json.len());
    framed.push(tag);
    framed.extend_from_slice(&(json.len() as u32).to_be_bytes());
    framed.extend_from_slice(&json);
    Some(Arc::from(framed))
}

/// Fan one AU out to every viewer.
#[allow(dead_code)]
fn broadcast_video(viewers: &Viewers, monitor_id: u32, au: &[u8]) {
    broadcast_bytes(viewers, &frame_video(monitor_id, au));
}

/// Fan one tagged JSON message out to every viewer.
#[allow(dead_code)]
fn broadcast_json<T: serde::Serialize>(viewers: &Viewers, tag: u8, msg: &T) {
    if let Some(buf) = frame_json(tag, msg) {
        broadcast_bytes(viewers, &buf);
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p control-server frame_video_layout_matches_wire frame_json_layout_matches_wire`
Expected: PASS (both).

- [ ] **Step 5: Commit**

```bash
git add crates/control-server/src/mediaplane.rs
git commit -m "feat(control-server): add Arc-framed video/json builders + broadcast wrappers"
```

---

## Task 4: Per-viewer clipboard source helpers

**Files:**
- Modify: `crates/control-server/src/mediaplane.rs`
- Test: `crates/control-server/src/mediaplane.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `ClipState` (existing), `Viewers` (Task 2).
- Produces (initially `#[allow(dead_code)]`):
  - `fn viewer_src(id: u64) -> String` → `"\0viewer:{id}"`.
  - `fn viewer_src_id(s: &str) -> Option<u64>` → parses the id back, `None` for clone ids.
  - `fn clip_forget_source(clip: &Mutex<ClipState>, src: &str)` → drops the offer if `src` owns it and scrubs `src` from every pending requester list.

The `send_clip_to` / `clip_dests` / `broker_*` rewrites that *use* these land in Task 5 (they touch the live `Viewers`, which only exists once wired). Here we add and unit-test the pure helpers.

- [ ] **Step 1: Write the failing test**

Add to `mod tests`:

```rust
#[test]
fn viewer_src_roundtrips_and_rejects_clone_ids() {
    let s = viewer_src(42);
    assert_eq!(viewer_src_id(&s), Some(42));
    assert_eq!(viewer_src_id("some-clone-host"), None);
    assert_eq!(viewer_src_id("\0viewer:notanum"), None);
}

#[test]
fn clip_forget_source_drops_owned_offer_and_scrubs_pending() {
    use wire::socket::ClipboardOffer;
    let clip: Mutex<ClipState> = Mutex::new(ClipState::default());
    let owner = viewer_src(1);
    let other = viewer_src(2);
    {
        let mut c = clip.lock().unwrap();
        c.offer = Some((ClipboardOffer { serial: 1, mime_types: vec!["text/plain".into()] }, owner.clone()));
        // A pending request that the owner (viewer 1) and viewer 2 both wanted.
        c.pending.insert((1, "text/plain".into()), vec![owner.clone(), other.clone()]);
    }

    clip_forget_source(&clip, &owner);

    let c = clip.lock().unwrap();
    assert!(c.offer.is_none(), "offer owned by the leaving viewer should be dropped");
    // owner scrubbed from the requester list; viewer 2 remains.
    assert_eq!(c.pending.get(&(1, "text/plain".into())), Some(&vec![other.clone()]));
}

#[test]
fn clip_forget_source_removes_empty_pending_entries() {
    let clip: Mutex<ClipState> = Mutex::new(ClipState::default());
    let only = viewer_src(9);
    clip.lock().unwrap().pending.insert((7, "image/png".into()), vec![only.clone()]);
    clip_forget_source(&clip, &only);
    assert!(clip.lock().unwrap().pending.is_empty(), "emptied requester list should be removed");
}
```

Check the real `ClipboardOffer` field names before running — if they differ from `{ serial, mime_types }`, match the actual definition in `crates/wire/src/socket.rs`. Adjust the test literal only (not the helper).

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p control-server viewer_src_roundtrips_and_rejects_clone_ids clip_forget_source_drops_owned_offer_and_scrubs_pending clip_forget_source_removes_empty_pending_entries`
Expected: FAIL — `cannot find function viewer_src` / `clip_forget_source`.

- [ ] **Step 3: Write minimal implementation**

Add near `ClipState` / the broker functions in `mediaplane.rs`:

```rust
/// Per-viewer clipboard source id. Distinct per connection so the lazy broker can
/// route a `Request` to the exact viewer that owns the current offer and a `Data`
/// reply back to the exact requester. (Replaces the old single `VIEWER_SRC`.)
#[allow(dead_code)]
fn viewer_src(id: u64) -> String {
    format!("\0viewer:{id}")
}

/// Parse a viewer source id back to its numeric id; `None` for clone ids.
#[allow(dead_code)]
fn viewer_src_id(s: &str) -> Option<u64> {
    s.strip_prefix("\0viewer:").and_then(|n| n.parse().ok())
}

/// A source (a leaving viewer, or a departed clone) is gone: if it owns the current
/// offer, drop the offer; remove it from every pending requester list, discarding
/// lists that become empty.
#[allow(dead_code)]
fn clip_forget_source(clip: &Mutex<ClipState>, src: &str) {
    let mut clip = clip.lock().unwrap();
    if clip.offer.as_ref().is_some_and(|(_, owner)| owner == src) {
        clip.offer = None;
    }
    clip.pending.retain(|_, requesters| {
        requesters.retain(|r| r != src);
        !requesters.is_empty()
    });
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p control-server viewer_src_roundtrips clip_forget_source`
Expected: PASS (all three).

- [ ] **Step 5: Commit**

```bash
git add crates/control-server/src/mediaplane.rs
git commit -m "feat(control-server): add per-viewer clipboard source ids + source-forget cleanup"
```

---

## Task 5: Integration — writer threads, connect/prime/teardown, per-viewer clipboard routing

This is the irreducible rewire: it flips every live path from the single `viewer: Arc<Mutex<Option<TcpStream>>>` slot onto the `Viewers` registry, removes the old single-viewer functions, and removes every `#[allow(dead_code)]` added in Tasks 2–4. It ends with a compile gate, the full unit-test suite, and a documented two-viewer manual smoke test.

**Files:**
- Modify: `crates/control-server/src/mediaplane.rs` (the live paths)

**Interfaces:**
- Consumes: everything from Tasks 2–4.
- Produces: the running multi-viewer media plane. No new public API.

- [ ] **Step 1: Replace the `Viewer` type alias and encoder plumbing**

In `mediaplane.rs`, delete the old alias:

```rust
type Viewer = Arc<Mutex<Option<TcpStream>>>;
```

The `Encoders` alias stays. Change `encoder_for` to fan out via the registry instead of the single viewer. Replace the whole `encoder_for` function with:

```rust
/// Get-or-create the encoder for a monitor at `w`×`h`; its AUs are framed with
/// `monitor_id` and broadcast to every viewer. If an encoder exists at a different
/// resolution it is rebuilt fresh.
fn encoder_for(
    encoders: &Encoders,
    viewers: &Viewers,
    monitor_id: u32,
    w: u32,
    h: u32,
    chroma: ChromaMode,
) -> Option<Arc<Encoder>> {
    let mut map = encoders.lock().unwrap();
    if let Some((e, ew, eh)) = map.get(&monitor_id) {
        if *ew == w && *eh == h {
            return Some(e.clone());
        }
        tracing::info!("monitor {monitor_id} resolution {ew}x{eh} → {w}x{h}; rebuilding encoder");
    }
    let viewers = viewers.clone();
    match Encoder::new(chroma, move |au, _idr| broadcast_video(&viewers, monitor_id, &au)) {
        Ok(e) => {
            let e = Arc::new(e);
            map.insert(monitor_id, (e.clone(), w, h));
            Some(e)
        }
        Err(err) => {
            tracing::error!("encoder for monitor {monitor_id} init failed: {err}");
            None
        }
    }
}
```

- [ ] **Step 2: Replace the single-viewer write helpers**

Delete these now-obsolete functions (their wire formats now live in `frame_*` / `broadcast_*`):
`write_mode`, `write_forwards`, `write_frame`, `write_clip`, `write_layout`, `write_cursor`, `write_json_frame`.

Keep `build_forwards_msg` (still used, and unit-tested). Add a broadcast wrapper for forwards next to it:

```rust
/// Broadcast the current desired forward set to every viewer (port-1 tag 5).
fn broadcast_forwards(viewers: &Viewers, state: &wire::ControlState, forward_port: u16) {
    broadcast_json(viewers, T_FORWARDS, &build_forwards_msg(state, forward_port));
}
```

- [ ] **Step 3: Rewrite the clipboard broker + `send_clip_to` + `clip_dests` onto `Viewers`**

Replace `send_clip_to`, `clip_dests`, `broker_offer`, `broker_request`, `broker_data` with these `Viewers`-based versions (delete the old `const VIEWER_SRC` line too):

```rust
/// Send a clipboard message to one endpoint (a clone by id, or a viewer by src id).
fn send_clip_to(handle: &MediaHandle, viewers: &Viewers, dest: &str, msg: ClipboardMsg) {
    if let Some(id) = viewer_src_id(dest) {
        if let Some(buf) = frame_json(T_CLIPBOARD, &msg) {
            send_bytes_to(viewers, id, buf);
        }
        return;
    }
    let conn = handle.conns.lock().unwrap().get(dest).cloned();
    if let Some(c) = conn {
        let server_msg = match msg {
            ClipboardMsg::Offer(o) => ServerMsg::ClipboardOffer(o),
            ClipboardMsg::Request(r) => ServerMsg::ClipboardRequest(r),
            ClipboardMsg::Data(d) => ServerMsg::ClipboardData(d),
        };
        let _ = c.send(&server_msg);
    }
}

/// Every clipboard destination except `source`: all *other* clones + all *other*
/// viewers (remote↔local + remote↔remote + viewer↔viewer).
fn clip_dests(handle: &MediaHandle, viewers: &Viewers, source: &str) -> Vec<String> {
    let mut dests: Vec<String> =
        handle.conns.lock().unwrap().keys().cloned().filter(|id| id != source).collect();
    for id in viewers.lock().unwrap().keys() {
        let src = viewer_src(*id);
        if src != source {
            dests.push(src);
        }
    }
    dests
}

/// Broker: a new selection is offered by `source`. Becomes the current clipboard;
/// advertise it to every other clone + every other viewer.
fn broker_offer(handle: &MediaHandle, viewers: &Viewers, offer: ClipboardOffer, source: &str) {
    tracing::debug!(target: "clip", "offer serial={} from {source:?}: {:?}", offer.serial, offer.mime_types);
    handle.clip.lock().unwrap().offer = Some((offer.clone(), source.to_string()));
    let dests = clip_dests(handle, viewers, source);
    for d in dests {
        send_clip_to(handle, viewers, &d, ClipboardMsg::Offer(offer.clone()));
    }
}

/// Broker: `requester` wants the current owner's bytes for a MIME — record it and
/// forward the request to the owner (lazy fetch).
fn broker_request(handle: &MediaHandle, viewers: &Viewers, req: ClipboardRequest, requester: &str) {
    let owner = {
        let mut clip = handle.clip.lock().unwrap();
        let Some((_, owner)) = clip.offer.clone() else {
            tracing::debug!(target: "clip", "request serial={} mime={} from {requester:?} dropped: no offer", req.serial, req.mime_type);
            return;
        };
        if owner == requester {
            return; // don't ask the owner to fetch from itself
        }
        clip.pending.entry((req.serial, req.mime_type.clone())).or_default().push(requester.to_string());
        owner
    };
    tracing::debug!(target: "clip", "request serial={} mime={} from {requester:?} -> owner {owner:?}", req.serial, req.mime_type);
    send_clip_to(handle, viewers, &owner, ClipboardMsg::Request(req));
}

/// Broker: the owner returned bytes — deliver to everyone who requested them.
fn broker_data(handle: &MediaHandle, viewers: &Viewers, data: ClipboardData) {
    let requesters = handle
        .clip
        .lock()
        .unwrap()
        .pending
        .remove(&(data.serial, data.mime_type.clone()))
        .unwrap_or_default();
    tracing::debug!(target: "clip",
        "data serial={} mime={} ({} bytes) -> {requesters:?}",
        data.serial, data.mime_type, data.bytes.len()
    );
    for r in requesters {
        send_clip_to(handle, viewers, &r, ClipboardMsg::Data(data.clone()));
    }
}
```

- [ ] **Step 4: Rewrite `prime_viewer` to prime one new viewer**

Replace `prime_viewer` with a version that sends metadata to a single viewer's `tx` and triggers a shared keyframe. It takes the new viewer's `SyncSender` directly (called during connect, before/after registry insert per Step 6):

```rust
/// Prime a freshly-connected viewer: push its mode + the selected clone's last-known
/// metadata (layout, cursor shape, clipboard offer, forward set) straight into its
/// channel, then force a shared keyframe and re-encode the last frame so it paints at
/// once even on a static, damage-driven desktop. The re-encode broadcasts (existing
/// viewers get one redundant keyframe per join — negligible).
fn prime_viewer(
    handle: &MediaHandle,
    encoders: &Encoders,
    viewers: &Viewers,
    tx: &SyncSender<Arc<[u8]>>,
    selected: Option<String>,
    chroma: ChromaMode,
    state: &wire::ControlState,
    forward_port: u16,
) {
    // Mode FIRST so the viewer picks its decode path before any AU.
    if let Some(b) = frame_json(T_MODE, &ModeMsg { chroma }) {
        let _ = tx.try_send(b);
    }
    // Forward set so it opens its listeners.
    if let Some(b) = frame_json(T_FORWARDS, &build_forwards_msg(state, forward_port)) {
        let _ = tx.try_send(b);
    }
    let Some(sel) = selected else { return };
    // Layout + last cursor shape + current clipboard offer, targeted to this viewer.
    if let Some(l) = handle.layout.lock().unwrap().get(&sel).cloned() {
        if let Some(b) = frame_json(T_LAYOUT, &l) {
            let _ = tx.try_send(b);
        }
    }
    if let Some(c) = handle.cursor.lock().unwrap().get(&sel).cloned() {
        if let Some(b) = frame_json(T_CURSOR, &c) {
            let _ = tx.try_send(b);
        }
    }
    if let Some((offer, _)) = handle.clip.lock().unwrap().offer.clone() {
        if let Some(b) = frame_json(T_CLIPBOARD, &ClipboardMsg::Offer(offer)) {
            let _ = tx.try_send(b);
        }
    }
    // Video: re-encode each monitor's latest frame (broadcasts via the shared encoder).
    let frames: Vec<(u32, OwnedFd, u32, u64, u32, u32)> = {
        let latest = handle.latest.lock().unwrap();
        latest
            .get(&sel)
            .map(|m| {
                m.values()
                    .filter_map(|f| {
                        dup_owned(&f.fd).map(|fd| (f.monitor_id, fd, f.fourcc, f.modifier, f.width, f.height))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    for (mid, fd, fourcc, modifier, w, h) in frames {
        if let Some(enc) = encoder_for(encoders, viewers, mid, w, h, chroma) {
            enc.force_idr();
            if let Err(e) = enc.push(fd, fourcc, modifier, w, h) {
                tracing::warn!("prime re-encode failed: {e}");
            }
        }
    }
}

/// Re-prime ALL viewers after a selection change: force fresh keyframes + rebroadcast
/// the newly-selected clone's last frame / cursor / layout to everyone.
fn reprime_all(
    handle: &MediaHandle,
    encoders: &Encoders,
    viewers: &Viewers,
    selected: Option<String>,
    chroma: ChromaMode,
) {
    force_idr_all(encoders);
    let Some(sel) = selected else { return };
    if let Some(l) = handle.layout.lock().unwrap().get(&sel).cloned() {
        broadcast_json(viewers, T_LAYOUT, &l);
    }
    if let Some(c) = handle.cursor.lock().unwrap().get(&sel).cloned() {
        broadcast_json(viewers, T_CURSOR, &c);
    }
    let frames: Vec<(u32, OwnedFd, u32, u64, u32, u32)> = {
        let latest = handle.latest.lock().unwrap();
        latest
            .get(&sel)
            .map(|m| {
                m.values()
                    .filter_map(|f| {
                        dup_owned(&f.fd).map(|fd| (f.monitor_id, fd, f.fourcc, f.modifier, f.width, f.height))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    for (mid, fd, fourcc, modifier, w, h) in frames {
        if let Some(enc) = encoder_for(encoders, viewers, mid, w, h, chroma) {
            enc.push(fd, fourcc, modifier, w, h).ok();
        }
    }
}
```

- [ ] **Step 5: Rewrite `read_viewer_input` to carry a viewer id + registry, and own teardown**

Replace `read_viewer_input` with:

```rust
/// Viewer → server: `[u8 type][u32be len][JSON]`. type 0 = InputMsg (to the selected
/// clone), type 1 = ClipboardData (broker fans it out), type 2 = ForwardStatusMsg.
/// This thread is the SOLE owner of the viewer's teardown.
fn read_viewer_input(mut sock: TcpStream, handle: Arc<MediaHandle>, app: App, viewers: Viewers, id: u64) {
    let src = viewer_src(id);
    let mut tag = [0u8; 1];
    let mut hdr = [0u8; 4];
    while sock.read_exact(&mut tag).is_ok() {
        if sock.read_exact(&mut hdr).is_err() {
            break;
        }
        let len = u32::from_be_bytes(hdr) as usize;
        if len > 1 << 20 {
            break;
        }
        let mut body = vec![0u8; len];
        if sock.read_exact(&mut body).is_err() {
            break;
        }
        match tag[0] {
            T_VIDEO => {
                let Ok(input) = serde_json::from_slice::<InputMsg>(&body) else { continue };
                let Some(clone) = app.store.selected() else { continue };
                let _ = handle.send_input(&clone, input);
            }
            T_CLIPBOARD => {
                if let Ok(msg) = serde_json::from_slice::<ClipboardMsg>(&body) {
                    match msg {
                        ClipboardMsg::Offer(o) => broker_offer(&handle, &viewers, o, &src),
                        ClipboardMsg::Request(r) => broker_request(&handle, &viewers, r, &src),
                        ClipboardMsg::Data(d) => broker_data(&handle, &viewers, d),
                    }
                }
            }
            T_FORWARD_STATUS => {
                if let Ok(msg) = serde_json::from_slice::<wire::forward::ForwardStatusMsg>(&body) {
                    app.forwards.report(msg);
                }
            }
            _ => break,
        }
    }
    // Sole teardown: drop from the registry (stops fan-out, disconnects the writer),
    // forget this viewer's clipboard state, and release its forward ref-count.
    remove_viewer(&viewers, id);
    clip_forget_source(&handle.clip, &src);
    app.forwards.viewer_left();
}
```

- [ ] **Step 6: Rewrite the port-1 accept loop (writer thread + connect + prime + reader)**

In `spawn`, replace the `viewer` local and the entire port-1 TCP accept block. Change the shared locals near the top of `spawn`:

```rust
    let handle = app.media.clone();
    let viewers: Viewers = Arc::new(Mutex::new(HashMap::new()));
    let encoders: Encoders = Arc::new(Mutex::new(HashMap::new()));
    let last_sel: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
```

Replace the port-1 accept block with:

```rust
    // Port 1 TCP: viewers + their input. Each viewer gets a bounded channel + a writer
    // thread; producers only `try_send`, so a slow viewer never blocks the encoders.
    {
        let (viewers, handle, app, encoders) =
            (viewers.clone(), handle.clone(), app.clone(), encoders.clone());
        std::thread::spawn(move || match TcpListener::bind(("0.0.0.0", video_port)) {
            Ok(l) => {
                tracing::info!("port 1 (video) listening on 0.0.0.0:{video_port}");
                for stream in l.incoming().flatten() {
                    let _ = stream.set_nodelay(true);
                    if let Err(e) = wire::net::set_keepalive(&stream) {
                        tracing::warn!("viewer keepalive setup failed: {e}");
                    }
                    tracing::info!("viewer connected: {:?}", stream.peer_addr());
                    let read_half = match stream.try_clone() {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!("viewer clone failed: {e}");
                            continue;
                        }
                    };
                    let id = next_viewer_id();
                    app.forwards.viewer_joined();

                    // Writer thread: drain the channel to the socket; shut down on any
                    // exit (write error OR channel disconnect) to wake the reader.
                    let (tx, rx) = std::sync::mpsc::sync_channel::<Arc<[u8]>>(VIEWER_CHAN_CAP);
                    let mut write_half = stream;
                    std::thread::spawn(move || {
                        while let Ok(buf) = rx.recv() {
                            if write_half.write_all(&buf).is_err() {
                                break;
                            }
                        }
                        let _ = write_half.shutdown(std::net::Shutdown::Both);
                    });

                    // Prime this viewer (mode first, then metadata + a shared keyframe),
                    // then register it so live frames start fanning to it.
                    let state = app.store.get();
                    prime_viewer(
                        &handle, &encoders, &viewers, &tx,
                        app.store.selected(), chroma, &state, forward_port,
                    );
                    viewers.lock().unwrap().insert(id, ViewerConn { id, tx });

                    // Reader thread owns teardown.
                    let (handle, app, viewers) = (handle.clone(), app.clone(), viewers.clone());
                    std::thread::spawn(move || read_viewer_input(read_half, handle, app, viewers, id));
                }
            }
            Err(e) => tracing::error!("port 1 bind {video_port} failed: {e}"),
        });
    }
```

Note the ordering: `prime_viewer` pushes `mode` into the channel **before** the viewer is inserted into the registry, so `mode` is guaranteed first; the shared keyframe from prime's re-encode fans to the registry — the new viewer is inserted immediately after, and the next real frames (all keyframes after `force_idr`) reach it. A one-frame race where the very first shared keyframe predates the insert is covered by the `force_idr` making subsequent frames keyframes too.

- [ ] **Step 7: Rewire the selection-watcher thread and `serve_clone`**

In the selection-watcher thread block, replace the `viewer`-based checks/prime. It currently captures `viewer`; capture `viewers` instead:

```rust
    {
        let (app, encoders, viewers, last_sel, handle) =
            (app.clone(), encoders.clone(), viewers.clone(), last_sel.clone(), handle.clone());
        std::thread::spawn(move || {
            let (_seed, mut rx) = app.store.subscribe();
            loop {
                match rx.blocking_recv() {
                    Ok(_) | Err(RecvError::Lagged(_)) => {
                        let state = app.store.get();
                        if !viewers.lock().unwrap().is_empty() {
                            broadcast_forwards(&viewers, &state, forward_port);
                        }
                        let sel = app.store.selected();
                        let mut ls = last_sel.lock().unwrap();
                        if *ls == sel {
                            continue;
                        }
                        *ls = sel.clone();
                        if viewers.lock().unwrap().is_empty() {
                            continue;
                        }
                        reprime_all(&handle, &encoders, &viewers, sel, chroma);
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });
    }
```

Update the clone-socket accept spawn to pass `viewers` instead of `viewer`:

```rust
    std::thread::spawn(move || {
        loop {
            match listener.accept() {
                Ok(conn) => {
                    let (handle, app, encoders, viewers, last_sel) =
                        (handle.clone(), app.clone(), encoders.clone(), viewers.clone(), last_sel.clone());
                    std::thread::spawn(move || serve_clone(conn, handle, app, encoders, viewers, last_sel));
                }
                Err(e) => {
                    tracing::error!("clone accept failed: {e}");
                    break;
                }
            }
        }
    });
```

Change `serve_clone`'s signature and body to use `viewers`. Replace the signature line:

```rust
fn serve_clone(
    conn: Conn,
    handle: Arc<MediaHandle>,
    app: App,
    encoders: Encoders,
    viewers: Viewers,
    last_sel: Arc<Mutex<Option<String>>>,
) {
```

Inside `serve_clone`, apply these substitutions in the message-loop body:
- Frame path selection-change branch: replace
  ```rust
  force_idr_all(&encoders);
  prime_viewer(&handle, &encoders, &viewer, sel.clone(), chroma);
  ```
  with
  ```rust
  reprime_all(&handle, &encoders, &viewers, sel.clone(), chroma);
  ```
- Frame encode push: `encoder_for(&encoders, &viewer, ...)` → `encoder_for(&encoders, &viewers, ...)`.
- `broker_offer(&handle, &viewer, o, &id)` → `broker_offer(&handle, &viewers, o, &id)` (same for `broker_request`).
- `broker_data(&handle, &viewer, d)` → `broker_data(&handle, &viewers, d)`.
- Cursor branch: `write_cursor(&viewer, &c)` → `broadcast_json(&viewers, T_CURSOR, &c)`.
- Layout branch: `write_layout(&viewer, &l)` → `broadcast_json(&viewers, T_LAYOUT, &l)`.

- [ ] **Step 8: Compile-gate the rewire**

Run: `cargo build -p control-server`
Expected: builds clean. Fix any remaining reference to the deleted `Viewer` alias / `write_*` helpers / `VIEWER_SRC` the compiler flags. There must be **zero** `#[allow(dead_code)]` left from Tasks 2–4 (remove each attribute as the item is now used); a `cargo build` warning-free of `dead_code` confirms it.

Run: `cargo clippy -p control-server 2>&1 | grep -i "dead_code" || echo "no dead_code"`
Expected: `no dead_code`.

- [ ] **Step 9: Run the full unit-test suite**

Run: `cargo test -p control-server`
Expected: PASS — the new tests (`broadcast_drops_only_the_full_viewer`, `remove_viewer_is_idempotent`, `frame_*`, `viewer_src_*`, `clip_forget_source_*`, `viewer_refcount_*`) plus all pre-existing tests (`teardown_only_removes_own_connection`, `build_forwards_msg_unions_enabled_rules_only`, `forward_header_frames_round_trip`, `splice_forward_pipes_both_ways`, `report_then_snapshot_reflects_state`, `conn_count_and_clear`).

- [ ] **Step 10: Two-viewer manual smoke test**

Build the control-server image/binary and run it against a running clone (see `docs/DEPLOY.md`). Then, from two machines (or two viewer instances), connect both to port 1 of the same control-server and verify:

1. **Both paint.** Both viewers show the selected clone's desktop and update live.
2. **Shared selection.** Activating a different clone (from the web UI or either viewer) switches **both** viewers.
3. **Free-for-all input.** Moving the mouse / typing from either viewer reaches the clone.
4. **Clipboard both ways.** Copy in the clone → paste in each viewer. Copy in viewer A → paste in viewer B and in the clone.
5. **Independent forwards.** An enabled port-forward opens a listener on **each** viewer machine; hitting `127.0.0.1:<local_port>` on each reaches the clone service. Disconnecting viewer A leaves viewer B's forward status intact (does not blank in the UI).
6. **Slow-viewer isolation.** Disconnecting/killing one viewer (or throttling it) does not stall or corrupt the other viewer's video.

Record the results in the commit message. If any check fails, treat it as a bug against this task (do not proceed to commit until fixed or explicitly deferred).

- [ ] **Step 11: Commit**

```bash
git add crates/control-server/src/mediaplane.rs
git commit -m "feat(control-server): support multiple simultaneous viewers (mirror + free-for-all)

Fan out the shared encoder to a per-viewer bounded channel + writer thread
(Approach B); per-viewer clipboard sources; ref-counted forward status.
Smoke-tested with two viewers: shared selection, merged input, viewer<->viewer
clipboard, independent forwards, slow-viewer isolation."
```

---

## Self-Review (completed during authoring)

**Spec coverage:**
- Viewer registry + per-viewer channel/writer isolation → Tasks 2, 5. ✅
- Overflow disconnects slow viewer → Task 2 (`broadcast_bytes` removal) + Task 5 (writer shutdown-on-disconnect). ✅
- Connect/prime (mode first, targeted metadata, shared keyframe) → Task 5 Steps 4, 6. ✅
- Single-owner teardown (reader owns; writer shuts down to kick reader; overflow drops registry entry) → Task 5 Steps 5, 6. ✅
- Per-viewer clipboard sources + `clip_forget_source` → Tasks 4, 5. ✅
- Forwards: broadcast set to all viewers; ref-counted clear; last-writer-wins state limitation → Task 1 + Task 5 Steps 6, 7. ✅
- Data plane unchanged → not touched (correct). ✅
- No viewer/wire/frontend changes → Global Constraints; `frame_*` preserve wire format. ✅

**Placeholder scan:** none — every code step carries complete code; the one deferred value (`VIEWER_CHAN_CAP`) is set to a concrete `128` with a tuning note.

**Type consistency:** `Viewers`/`ViewerConn`/`SyncSender<Arc<[u8]>>`, `broadcast_bytes`/`send_bytes_to`/`remove_viewer`, `frame_video`/`frame_json`/`broadcast_video`/`broadcast_json`/`broadcast_forwards`, `viewer_src`/`viewer_src_id`/`clip_forget_source`, `prime_viewer`/`reprime_all`, `ForwardBus::viewer_joined`/`viewer_left` are used with identical names/signatures across tasks. ✅
