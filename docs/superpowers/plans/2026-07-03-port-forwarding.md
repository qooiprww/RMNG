# Port Forwarding Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let an operator forward a TCP port inside a remote host (clone) to `127.0.0.1:<local>` on their own machine, configured from the frontend's per-host three-dot menu.

**Architecture:** Two planes. **Control plane** (low volume): frontend `PUT`s per-host rules → control-server persists them on `Host.forwards` and pushes the union to the viewer over port-1 (new tag 5); the viewer reports listener status back (new tag 2) → control-server surfaces it to the frontend as a volatile `forwards` SSE event. **Data plane** (bulk bytes): the viewer binds a `127.0.0.1` listener per rule; each accepted connection opens a fresh TCP to a new control-server port (`9005`), sends a JSON header, and the server byte-splices to `dial_host(host_id):remote_port` over the `rmng` Docker bridge. This keeps forwarded traffic entirely off the latency-critical video socket.

**Tech Stack:** Rust (wire types with `serde` + `ts-rs`; control-server on `axum` + `tokio` with blocking-thread media plane; viewer on GTK4 + blocking std threads/sockets), TypeScript/React Router 7 frontend on Bun.

**Spec:** [docs/superpowers/specs/2026-07-03-port-forwarding-design.md](../specs/2026-07-03-port-forwarding-design.md)

## Global Constraints

- **Local forwarding only, TCP only** (no UDP, no reverse forwarding) in v1.
- **Bind `127.0.0.1` only** on the viewer — never `0.0.0.0`.
- **Forwards target any host** (rules carry `host_id`; the server dials per-`host_id`, independent of the global `selected`).
- **Config persists** on `Host.forwards` in `state.json`; **runtime status is volatile** (never persisted — separate `forwards` SSE event, mirroring `stats`).
- **Data port default `9005`** = `ListenConfig.forward` (restart-required, like the other listen ports).
- **Local ports are globally unique** across all hosts' rules (they share the one viewer machine's port space); the rule `id` is derived as `f{local_port}`.
- **Wire framing:** port-1 server→viewer tag `5` = forwards config; viewer→server tag `2` = forward status; both `[tag][u32be len][json]`. Data port framing: `[u32be len][json ForwardHeader]` then raw bytes, server replies one status byte (`0x00` ok / non-zero fail) before the raw stream.
- Do **not** build on `wire::viewer::ToViewer`/`FromViewer` — they are vestigial/unused.
- ts-rs types regenerate into `frontend/app/lib/wire/` when the `wire` crate's tests run (`#[ts(export)]`).
- Run commands from the repo root `/home/pegasis/Projects/RMNG`. Shell is fish; frontend commands use `cd frontend; and <cmd>`.

---

### Task 1: Wire data types — `PortForward`, `Host.forwards`, `ListenConfig.forward`

**Files:**
- Modify: `crates/wire/src/control.rs` (add `PortForward`; add `forwards` field to `Host` after `unread`, ~line 143)
- Modify: `crates/wire/src/config.rs` (add `forward` to `ListenConfig`, ~line 20; `default_forward` fn; `Default` impl ~line 50)
- Modify: `crates/wire/src/lib.rs` (re-export `PortForward` alongside `Host`)

**Interfaces:**
- Produces: `wire::PortForward { id: String, remote_port: u16, local_port: u16, enabled: bool, label: Option<String> }`; `Host.forwards: Vec<PortForward>`; `ListenConfig.forward: u16` (default 9005).

- [ ] **Step 1: Write the failing test**

Add to the bottom of `crates/wire/src/control.rs` (inside or after any existing `#[cfg(test)] mod tests` — if none exists in the file, add this module):

```rust
#[cfg(test)]
mod forward_tests {
    use super::*;

    #[test]
    fn port_forward_round_trips_camel_case() {
        let f = PortForward {
            id: "f8080".into(),
            remote_port: 3000,
            local_port: 8080,
            enabled: true,
            label: Some("dev".into()),
        };
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains("\"remotePort\":3000"), "got {json}");
        assert!(json.contains("\"localPort\":8080"), "got {json}");
        assert_eq!(serde_json::from_str::<PortForward>(&json).unwrap(), f);
    }

    #[test]
    fn host_forwards_defaults_empty_and_is_omitted() {
        let json = r#"{"id":"h","host":"h"}"#;
        let h: Host = serde_json::from_str(json).unwrap();
        assert!(h.forwards.is_empty());
        // empty forwards must not serialize (skip_serializing_if)
        assert!(!serde_json::to_string(&h).unwrap().contains("forwards"));
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p wire forward_tests`
Expected: FAIL — `PortForward` not found / `Host` has no field `forwards`.

- [ ] **Step 3: Add `PortForward` and the `Host.forwards` field**

In `crates/wire/src/control.rs`, immediately before `pub struct Host {` (~line 62), add:

```rust
/// One local-forward rule: a TCP port inside this clone (`remote_port`) exposed at
/// `127.0.0.1:<local_port>` on the machine running the native viewer. Persisted in
/// `state.json`; the viewer runs the listener. `id` is derived server-side as
/// `f{local_port}` (local ports are globally unique across all hosts' rules).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, Default)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "../../../frontend/app/lib/wire/")]
pub struct PortForward {
    pub id: String,
    pub remote_port: u16,
    pub local_port: u16,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}
```

Then inside `struct Host`, immediately after the `pub unread: bool,` field (~line 142), add:

```rust
    /// Local port-forward rules for this host (see [`PortForward`]). Persisted; the
    /// viewer runs the listeners and reports status out-of-band (volatile `forwards`
    /// SSE event, never stored here).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forwards: Vec<PortForward>,
```

In `crates/wire/src/lib.rs`, find the `pub use crate::control::{...}` re-export line that includes `Host` and add `PortForward` to it (e.g. `pub use crate::control::{... Host, PortForward, ...};`).

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p wire forward_tests`
Expected: PASS (2 tests).

- [ ] **Step 5: Add the `ListenConfig.forward` test**

Add to the bottom of `crates/wire/src/config.rs`:

```rust
#[cfg(test)]
mod listen_tests {
    use super::*;

    #[test]
    fn listen_config_forward_defaults_9005() {
        assert_eq!(ListenConfig::default().forward, 9005);
        // absent in JSON → default
        let lc: ListenConfig =
            serde_json::from_str(r#"{"web":9000,"video":9001,"cloneMcp":9002,"globalMcp":9003}"#)
                .unwrap();
        assert_eq!(lc.forward, 9005);
    }
}
```

- [ ] **Step 6: Run it to verify it fails**

Run: `cargo test -p wire listen_tests`
Expected: FAIL — `ListenConfig` has no field `forward`.

- [ ] **Step 7: Add the `forward` field + default**

In `crates/wire/src/config.rs`, inside `struct ListenConfig`, after the `pub daemon_mcp: u16,` field, add:

```rust
    /// The control-server's port-forward data plane. The viewer opens one TCP
    /// connection here per accepted local socket; the server splices to the clone.
    /// Restart-required (bound at startup).
    #[serde(default = "default_forward")]
    pub forward: u16,
```

After the `fn default_daemon_mcp()` function, add:

```rust
fn default_forward() -> u16 {
    9005
}
```

In `impl Default for ListenConfig`, add `forward: default_forward()` to the struct literal:

```rust
        Self { web: 9000, video: 9001, clone_mcp: 9002, global_mcp: 9003, daemon_mcp: default_daemon_mcp(), forward: default_forward() }
```

- [ ] **Step 8: Run all wire tests + confirm ts-rs regenerated the frontend types**

Run: `cargo test -p wire`
Expected: PASS. Then confirm the generated TS exists:
Run: `ls frontend/app/lib/wire/PortForward.ts frontend/app/lib/wire/ListenConfig.ts`
Expected: both files exist; `PortForward.ts` contains `remotePort` and `localPort`, `ListenConfig.ts` contains `forward`.

- [ ] **Step 9: Commit**

```bash
git add crates/wire/src/control.rs crates/wire/src/config.rs crates/wire/src/lib.rs frontend/app/lib/wire/
git commit -m "feat(wire): PortForward on Host + ListenConfig.forward (9005)"
```

---

### Task 2: Wire message types — `crates/wire/src/forward.rs`

**Files:**
- Create: `crates/wire/src/forward.rs`
- Modify: `crates/wire/src/lib.rs` (add `pub mod forward;`)

**Interfaces:**
- Consumes: nothing from prior tasks.
- Produces (all under `wire::forward`):
  - `ForwardRule { host_id: String, id: String, remote_port: u16, local_port: u16 }`
  - `ForwardsMsg { forward_port: u16, rules: Vec<ForwardRule> }` — server→viewer tag 5 body.
  - `ForwardState` enum `{ Listening, Error, Offline }` (serde `lowercase`, TS-exported).
  - `ForwardStatusMsg { host_id: String, id: String, state: ForwardState, error: Option<String> }` — viewer→server tag 2 body.
  - `ForwardRuntime { id: String, state: ForwardState, error: Option<String>, active_conns: u32 }` (TS-exported) — one entry in the `forwards` SSE event.
  - `ForwardHeader { token: Option<String>, host_id: String, remote_port: u16 }` — data-port header.

- [ ] **Step 1: Write the failing test**

Create `crates/wire/src/forward.rs` with ONLY the test module first (so the test fails to compile against missing types):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forwards_msg_round_trips() {
        let m = ForwardsMsg {
            forward_port: 9005,
            rules: vec![ForwardRule {
                host_id: "pega-abc".into(),
                id: "f8080".into(),
                remote_port: 3000,
                local_port: 8080,
            }],
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"forwardPort\":9005"), "got {json}");
        assert!(json.contains("\"hostId\":\"pega-abc\""), "got {json}");
        assert_eq!(serde_json::from_str::<ForwardsMsg>(&json).unwrap(), m);
    }

    #[test]
    fn status_msg_state_is_lowercase() {
        let m = ForwardStatusMsg {
            host_id: "h".into(),
            id: "f8080".into(),
            state: ForwardState::Error,
            error: Some("addr in use".into()),
        };
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"state\":\"error\""), "got {json}");
        assert_eq!(serde_json::from_str::<ForwardStatusMsg>(&json).unwrap(), m);
    }

    #[test]
    fn header_round_trips_without_token() {
        let h = ForwardHeader { token: None, host_id: "h".into(), id: "f22".into(), remote_port: 22 };
        let json = serde_json::to_string(&h).unwrap();
        assert!(!json.contains("token"), "None token must be omitted: {json}");
        assert_eq!(serde_json::from_str::<ForwardHeader>(&json).unwrap(), h);
    }
}
```

Add `pub mod forward;` to `crates/wire/src/lib.rs` (near the other `pub mod` lines).

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p wire --lib forward::tests`
Expected: FAIL — types not found (compile error).

- [ ] **Step 3: Add the type definitions**

Insert this ABOVE the `#[cfg(test)]` module in `crates/wire/src/forward.rs`:

```rust
//! Port-forwarding wire types shared by the control-server and the native viewer.
//! Control plane rides port-1 ([`ForwardsMsg`] server→viewer tag 5,
//! [`ForwardStatusMsg`] viewer→server tag 2); the data plane's per-connection header
//! is [`ForwardHeader`]. [`ForwardRuntime`] is the volatile `forwards` SSE payload entry.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// One rule pushed to the viewer: the persisted [`crate::PortForward`] subset it needs
/// to run a listener, tagged with its owning host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForwardRule {
    pub host_id: String,
    pub id: String,
    pub remote_port: u16,
    pub local_port: u16,
}

/// Server→viewer (port-1 tag 5): the full desired rule set (union across all hosts)
/// plus the control-server's data port, sent on connect and on every config change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForwardsMsg {
    pub forward_port: u16,
    pub rules: Vec<ForwardRule>,
}

/// Live state of a forward rule. `Offline` is server-derived (no viewer connected);
/// the viewer only ever reports `Listening`/`Error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
#[ts(export, export_to = "../../../frontend/app/lib/wire/")]
pub enum ForwardState {
    Listening,
    Error,
    Offline,
}

/// Viewer→server (port-1 tag 2): a rule's listener bound, failed to bind, or died.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForwardStatusMsg {
    pub host_id: String,
    pub id: String,
    pub state: ForwardState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One rule's runtime status inside the `forwards` SSE event (payload:
/// `Record<hostId, ForwardRuntime[]>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "../../../frontend/app/lib/wire/")]
pub struct ForwardRuntime {
    pub id: String,
    pub state: ForwardState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub active_conns: u32,
}

/// Data-plane header: the first length-prefixed JSON on a `:forward` connection,
/// followed by the raw byte stream. `id` is the rule id (so the server attributes
/// active-conn counts); `token` is reserved for auth (unenforced in v1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForwardHeader {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    pub host_id: String,
    pub id: String,
    pub remote_port: u16,
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p wire --lib forward::tests`
Expected: PASS (3 tests). Confirm generated TS:
Run: `ls frontend/app/lib/wire/ForwardState.ts frontend/app/lib/wire/ForwardRuntime.ts`
Expected: both exist.

- [ ] **Step 5: Commit**

```bash
git add crates/wire/src/forward.rs crates/wire/src/lib.rs frontend/app/lib/wire/
git commit -m "feat(wire): port-forward message types (ForwardsMsg/Status/Runtime/Header)"
```

---

### Task 3: `ForwardBus` — volatile status store + `forwards` SSE event

**Files:**
- Create: `crates/control-server/src/forward.rs`
- Modify: `crates/control-server/src/main.rs` (add `mod forward;`)
- Modify: `crates/control-server/src/app.rs` (add `forwards` to `App`, construct in `App::new`)
- Modify: `crates/control-server/src/web.rs` (add the `forwards` stream to `events`, ~line 122-145)

**Interfaces:**
- Consumes: `wire::forward::{ForwardRuntime, ForwardState, ForwardStatusMsg}` (Task 2).
- Produces: `crate::forward::ForwardBus` with `new()`, `subscribe() -> (String, broadcast::Receiver<String>)`, `report(&self, ForwardStatusMsg)`, `conn_opened(&self, host_id: &str, id: &str)`, `conn_closed(&self, host_id: &str, id: &str)`, `clear(&self)`. `App.forwards: Arc<ForwardBus>`.

- [ ] **Step 1: Write the failing test**

Create `crates/control-server/src/forward.rs`:

```rust
//! Volatile port-forward runtime status. Mirrors [`crate::monitor::StatsBus`]: an
//! in-memory `host_id → (rule_id → ForwardRuntime)` map broadcast to `/events` as a
//! named `forwards` SSE event. Never persisted — config lives on `Host.forwards`.

use std::collections::HashMap;
use std::sync::RwLock;

use tokio::sync::broadcast;
use wire::forward::{ForwardRuntime, ForwardState, ForwardStatusMsg};

pub struct ForwardBus {
    tx: broadcast::Sender<String>,
    inner: RwLock<HashMap<String, HashMap<String, ForwardRuntime>>>,
}

impl ForwardBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(16);
        Self { tx, inner: RwLock::new(HashMap::new()) }
    }

    /// Current snapshot (JSON `Record<hostId, ForwardRuntime[]>`) + a live receiver.
    pub fn subscribe(&self) -> (String, broadcast::Receiver<String>) {
        (self.snapshot_json(), self.tx.subscribe())
    }

    fn snapshot_json(&self) -> String {
        let inner = self.inner.read().unwrap();
        let by_host: HashMap<&String, Vec<&ForwardRuntime>> =
            inner.iter().map(|(h, m)| (h, m.values().collect())).collect();
        serde_json::to_string(&by_host).unwrap_or_else(|_| "{}".to_string())
    }

    fn broadcast(&self) {
        let _ = self.tx.send(self.snapshot_json());
    }

    /// Apply a viewer-reported status change (keeps the rule's `active_conns`).
    pub fn report(&self, msg: ForwardStatusMsg) {
        {
            let mut inner = self.inner.write().unwrap();
            let host = inner.entry(msg.host_id).or_default();
            let e = host.entry(msg.id.clone()).or_insert_with(|| ForwardRuntime {
                id: msg.id.clone(),
                state: ForwardState::Offline,
                error: None,
                active_conns: 0,
            });
            e.state = msg.state;
            e.error = msg.error;
        }
        self.broadcast();
    }

    pub fn conn_opened(&self, host_id: &str, id: &str) {
        self.bump(host_id, id, 1);
    }

    pub fn conn_closed(&self, host_id: &str, id: &str) {
        self.bump(host_id, id, -1);
    }

    fn bump(&self, host_id: &str, id: &str, delta: i64) {
        {
            let mut inner = self.inner.write().unwrap();
            let host = inner.entry(host_id.to_string()).or_default();
            let e = host.entry(id.to_string()).or_insert_with(|| ForwardRuntime {
                id: id.to_string(),
                state: ForwardState::Listening,
                error: None,
                active_conns: 0,
            });
            e.active_conns = (e.active_conns as i64 + delta).max(0) as u32;
        }
        self.broadcast();
    }

    /// Drop all status (viewer disconnected → every rule reverts to offline in the UI).
    pub fn clear(&self) {
        self.inner.write().unwrap().clear();
        self.broadcast();
    }
}

impl Default for ForwardBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_then_snapshot_reflects_state() {
        let bus = ForwardBus::new();
        let (seed, mut rx) = bus.subscribe();
        assert_eq!(seed, "{}");
        bus.report(ForwardStatusMsg {
            host_id: "h".into(),
            id: "f8080".into(),
            state: ForwardState::Listening,
            error: None,
        });
        let got = rx.try_recv().expect("broadcast on report");
        assert!(got.contains("\"h\""));
        assert!(got.contains("\"state\":\"listening\""));
        assert!(got.contains("\"activeConns\":0"));
    }

    #[test]
    fn conn_count_and_clear() {
        let bus = ForwardBus::new();
        let (_seed, mut rx) = bus.subscribe();
        bus.conn_opened("h", "f8080");
        assert!(rx.try_recv().unwrap().contains("\"activeConns\":1"));
        bus.conn_closed("h", "f8080");
        assert!(rx.try_recv().unwrap().contains("\"activeConns\":0"));
        bus.clear();
        assert_eq!(rx.try_recv().unwrap(), "{}");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p control-server forward::tests`
Expected: FAIL — `mod forward` not declared yet (compile error: unresolved module or `ForwardBus` unused elsewhere is fine; the failure is that `mod forward;` isn't in `main.rs`).

- [ ] **Step 3: Declare the module**

In `crates/control-server/src/main.rs`, add `mod forward;` alongside the other `mod` declarations (near `mod mediaplane;`).

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p control-server forward::tests`
Expected: PASS (2 tests).

- [ ] **Step 5: Wire `ForwardBus` into `App`**

In `crates/control-server/src/app.rs`, add a field to `struct App` after `pub stats: Arc<crate::monitor::StatsBus>,`:

```rust
    /// Volatile port-forward runtime status. Published by the media plane (viewer
    /// reports + data-conn counts); `/events` fans it out as a named `forwards` SSE
    /// event. SSE-only — never persisted (see [`crate::forward::ForwardBus`]).
    pub forwards: Arc<crate::forward::ForwardBus>,
```

In `App::new`, add to the returned struct literal after `stats: Arc::new(crate::monitor::StatsBus::new()),`:

```rust
            forwards: Arc::new(crate::forward::ForwardBus::new()),
```

- [ ] **Step 6: Add the `forwards` SSE event to `/events`**

In `crates/control-server/src/web.rs`, in `async fn events` (~line 122), after the `stats_stream` block and before the final `Sse::new(...)`, add:

```rust
    let (fwd_snapshot, fwd_rx) = app.forwards.subscribe();
    let fwd_initial = futures::stream::once(
        async move { Ok(Event::default().event("forwards").data(fwd_snapshot)) },
    );
    let fwd_updates = BroadcastStream::new(fwd_rx).filter_map(|r| async move {
        match r {
            Ok(json) => Some(Ok(Event::default().event("forwards").data(json))),
            Err(_) => None,
        }
    });
    let fwd_stream = fwd_initial.chain(fwd_updates);
```

Change the final combinator from two streams to three — replace:

```rust
    Sse::new(futures::stream::select(state_stream, stats_stream))
```

with:

```rust
    Sse::new(futures::stream::select(
        state_stream,
        futures::stream::select(stats_stream, fwd_stream),
    ))
```

- [ ] **Step 7: Build to verify wiring compiles**

Run: `cargo build -p control-server`
Expected: builds clean.

- [ ] **Step 8: Commit**

```bash
git add crates/control-server/src/forward.rs crates/control-server/src/main.rs crates/control-server/src/app.rs crates/control-server/src/web.rs
git commit -m "feat(control-server): ForwardBus + forwards SSE event"
```

---

### Task 4: `PUT /api/hosts/:id/forwards` — validate + persist

**Files:**
- Modify: `crates/control-server/src/web.rs` (add `validate_forwards`, `forwards_put`, request structs; register the route ~line 72; add `put` to the `routing` import ~line 15)

**Interfaces:**
- Consumes: `wire::{ControlState, PortForward}`, `app.store.get()`, `app.store.mutate(..)`.
- Produces: `fn validate_forwards(state: &ControlState, host_id: &str, inputs: Vec<ForwardInput>) -> Result<Vec<PortForward>, (StatusCode, String)>`; `PUT /api/hosts/:id/forwards`.

- [ ] **Step 1: Write the failing test**

Add to the bottom of `crates/control-server/src/web.rs`:

```rust
#[cfg(test)]
mod forwards_validation_tests {
    use super::*;
    use wire::{ControlState, Host};

    fn state_with(hosts: Vec<Host>) -> ControlState {
        ControlState { hosts, ..Default::default() }
    }

    fn host(id: &str) -> Host {
        Host { id: id.into(), host: id.into(), ..Default::default() }
    }

    fn input(remote: u16, local: u16) -> ForwardInput {
        ForwardInput { id: None, remote_port: remote, local_port: local, enabled: true, label: None }
    }

    #[test]
    fn assigns_ids_from_local_port() {
        let st = state_with(vec![host("a")]);
        let out = validate_forwards(&st, "a", vec![input(3000, 8080)]).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "f8080");
        assert_eq!(out[0].remote_port, 3000);
    }

    #[test]
    fn rejects_zero_port() {
        let st = state_with(vec![host("a")]);
        let err = validate_forwards(&st, "a", vec![input(0, 8080)]).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn rejects_duplicate_local_within_request() {
        let st = state_with(vec![host("a")]);
        let err = validate_forwards(&st, "a", vec![input(1, 8080), input(2, 8080)]).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn rejects_local_port_used_by_another_host() {
        let mut other = host("b");
        other.forwards = vec![wire::PortForward {
            id: "f8080".into(), remote_port: 9, local_port: 8080, enabled: true, label: None,
        }];
        let st = state_with(vec![host("a"), other]);
        let err = validate_forwards(&st, "a", vec![input(3000, 8080)]).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p control-server forwards_validation_tests`
Expected: FAIL — `validate_forwards` / `ForwardInput` not found.

- [ ] **Step 3: Add the request structs, validator, and handler**

In `crates/control-server/src/web.rs`, add near the other `#[derive(Deserialize)]` request structs (e.g. after `ReorderReq`):

```rust
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ForwardsPutReq {
    forwards: Vec<ForwardInput>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ForwardInput {
    #[serde(default)]
    id: Option<String>,
    remote_port: u16,
    local_port: u16,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    label: Option<String>,
}

/// Validate a host's proposed forward set against the whole state and normalize it into
/// `PortForward`s (ids derived `f{local_port}`). Errors: port 0, duplicate local port
/// within the request, or a local port already claimed by a *different* host (the viewer
/// binds them all on one machine → the local-port space is global).
fn validate_forwards(
    state: &wire::ControlState,
    host_id: &str,
    inputs: Vec<ForwardInput>,
) -> Result<Vec<wire::PortForward>, (StatusCode, String)> {
    let bad = |m: String| (StatusCode::BAD_REQUEST, m);
    // Local ports claimed by OTHER hosts.
    let mut taken: std::collections::HashSet<u16> = state
        .hosts
        .iter()
        .filter(|h| h.id != host_id)
        .flat_map(|h| h.forwards.iter().map(|f| f.local_port))
        .collect();
    let mut out = Vec::with_capacity(inputs.len());
    for inp in inputs {
        if inp.remote_port == 0 || inp.local_port == 0 {
            return Err(bad("ports must be 1–65535".into()));
        }
        if !taken.insert(inp.local_port) {
            return Err(bad(format!("local port {} is already in use", inp.local_port)));
        }
        out.push(wire::PortForward {
            id: inp.id.unwrap_or_else(|| format!("f{}", inp.local_port)),
            remote_port: inp.remote_port,
            local_port: inp.local_port,
            enabled: inp.enabled,
            label: inp.label,
        });
    }
    Ok(out)
}

/// `PUT /api/hosts/:id/forwards` — replace a host's forward rules. Validated
/// synchronously (returns 400 on conflict); persisted to `state.json`; the media plane
/// re-pushes the new set to the viewer off the store broadcast.
async fn forwards_put(
    State(app): State<App>,
    AxPath(id): AxPath<String>,
    Json(req): Json<ForwardsPutReq>,
) -> Result<Json<ControlState>, (StatusCode, String)> {
    let state = app.store.get();
    if !state.hosts.iter().any(|h| h.id == id) {
        return Err((StatusCode::NOT_FOUND, format!("no host '{id}'")));
    }
    let validated = validate_forwards(&state, &id, req.forwards)?;
    let next = app.store.mutate(|s| {
        if let Some(h) = s.hosts.iter_mut().find(|h| h.id == id) {
            h.forwards = validated;
        }
    });
    Ok(Json(next))
}
```

Register the route: in `pub fn router`, add after the `chat_abort` route (~line 72):

```rust
        .route("/api/hosts/:id/forwards", put(forwards_put))
```

Add `put` to the routing import at the top (~line 15): change `routing::{get, post}` to `routing::{get, post, put}`.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p control-server forwards_validation_tests`
Expected: PASS (4 tests).

- [ ] **Step 5: Build the whole server**

Run: `cargo build -p control-server`
Expected: builds clean.

- [ ] **Step 6: Commit**

```bash
git add crates/control-server/src/web.rs
git commit -m "feat(control-server): PUT /api/hosts/:id/forwards with validation"
```

---

### Task 5: Media plane control plane — push tag 5, handle tag 2, clear on disconnect

**Files:**
- Modify: `crates/control-server/src/mediaplane.rs` (tag consts; `write_forwards`; `build_forwards_msg`; capture `forward_port`; push on connect; re-push on store change; handle tag 2 in `read_viewer_input`; clear on disconnect)

**Interfaces:**
- Consumes: `wire::forward::{ForwardsMsg, ForwardRule, ForwardStatusMsg}`, `app.forwards` (Task 3), `app.store.get()`, `ListenConfig.forward` (Task 1).
- Produces: `fn build_forwards_msg(state: &wire::ControlState, forward_port: u16) -> wire::forward::ForwardsMsg`; server→viewer tag 5 push; viewer→server tag 2 handling.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` at the bottom of `crates/control-server/src/mediaplane.rs`:

```rust
    #[test]
    fn build_forwards_msg_unions_enabled_rules_only() {
        use wire::{ControlState, Host, PortForward};
        let mut a = Host { id: "a".into(), host: "a".into(), ..Default::default() };
        a.forwards = vec![
            PortForward { id: "f8080".into(), remote_port: 3000, local_port: 8080, enabled: true, label: None },
            PortForward { id: "f9".into(), remote_port: 9, local_port: 9, enabled: false, label: None },
        ];
        let mut b = Host { id: "b".into(), host: "b".into(), ..Default::default() };
        b.forwards = vec![
            PortForward { id: "f7000".into(), remote_port: 5000, local_port: 7000, enabled: true, label: None },
        ];
        let st = ControlState { hosts: vec![a, b], ..Default::default() };
        let msg = build_forwards_msg(&st, 9005);
        assert_eq!(msg.forward_port, 9005);
        // Only the two enabled rules, tagged with host id.
        assert_eq!(msg.rules.len(), 2);
        assert!(msg.rules.iter().any(|r| r.host_id == "a" && r.local_port == 8080 && r.remote_port == 3000));
        assert!(msg.rules.iter().any(|r| r.host_id == "b" && r.local_port == 7000));
        assert!(!msg.rules.iter().any(|r| r.local_port == 9)); // disabled excluded
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p control-server build_forwards_msg`
Expected: FAIL — `build_forwards_msg` not found.

- [ ] **Step 3: Add tag constants, `build_forwards_msg`, and `write_forwards`**

In `crates/control-server/src/mediaplane.rs`, after the existing `const T_MODE: u8 = 4;` line, add:

```rust
/// Server→viewer tag 5: the desired forward set (`[5][u32be len][JSON ForwardsMsg]`).
const T_FORWARDS: u8 = 5;
/// Viewer→server tag 2: a forward rule's status changed (`[2][u32be len][JSON ForwardStatusMsg]`).
const T_FORWARD_STATUS: u8 = 2;
```

Add these free functions near `write_mode` (they use the existing `write_json_frame`):

```rust
/// Build the viewer's desired forward set: the union of every host's *enabled* rules,
/// each tagged with its host id, plus the data port.
fn build_forwards_msg(state: &wire::ControlState, forward_port: u16) -> wire::forward::ForwardsMsg {
    let rules = state
        .hosts
        .iter()
        .flat_map(|h| {
            h.forwards.iter().filter(|f| f.enabled).map(move |f| wire::forward::ForwardRule {
                host_id: h.id.clone(),
                id: f.id.clone(),
                remote_port: f.remote_port,
                local_port: f.local_port,
            })
        })
        .collect();
    wire::forward::ForwardsMsg { forward_port, rules }
}

/// Push the current desired forward set to the viewer (port-1 tag 5).
fn write_forwards(viewer: &Viewer, state: &wire::ControlState, forward_port: u16) {
    write_json_frame(viewer, T_FORWARDS, &build_forwards_msg(state, forward_port));
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p control-server build_forwards_msg`
Expected: PASS.

- [ ] **Step 5: Capture `forward_port` and push on viewer connect**

In `pub fn spawn`, next to `let video_port = cfg.listen.video;`, add:

```rust
    let forward_port = cfg.listen.forward;
```

In the port-1 accept thread, this `forward_port` must be captured. Find the `let (viewer, handle, app, encoders) = (...)` tuple that clones state into the accept `std::thread::spawn` (~line 117) and add `forward_port` is `Copy`, so it is captured by the `move` closure directly — no clone needed. Then inside the connect handler, after the clipboard-offer block (right before the `read_viewer_input` thread is spawned, ~line 156), add:

```rust
                        // Push the desired forward set so the viewer opens its listeners.
                        write_forwards(&viewer, &app.store.get(), forward_port);
```

- [ ] **Step 6: Re-push forwards on every store change**

In the store-subscribe watch thread (the one that calls `prime_viewer` on selection change, ~line 171), capture `forward_port` (it is `Copy`; add it to the closure's move set implicitly by referencing it). Inside the `Ok(_) | Err(RecvError::Lagged(_))` arm, BEFORE the `let sel = app.store.selected();` line, add:

```rust
                        // A config change (or any mutation) may have altered forwards —
                        // re-push the full set; the viewer reconciles idempotently.
                        if viewer.lock().unwrap().is_some() {
                            write_forwards(&viewer, &app.store.get(), forward_port);
                        }
```

- [ ] **Step 7: Handle tag 2 (forward status) and clear on disconnect**

In `fn read_viewer_input`, add a match arm alongside `T_VIDEO` and `T_CLIPBOARD` (before `_ => break`):

```rust
            T_FORWARD_STATUS => {
                if let Ok(msg) = serde_json::from_slice::<wire::forward::ForwardStatusMsg>(&body) {
                    app.forwards.report(msg);
                }
            }
```

After the `while` loop in `read_viewer_input` (the function's end), add:

```rust
    // Viewer gone → its listeners are gone; drop all runtime status so the UI shows
    // every rule as offline until a viewer reconnects and re-reports.
    app.forwards.clear();
```

- [ ] **Step 8: Build to verify all wiring compiles**

Run: `cargo build -p control-server`
Expected: builds clean. (If the store-subscribe closure complains `forward_port` is not captured, add `let forward_port = forward_port;` just inside that `std::thread::spawn(move || {` body — but as a `Copy` value referenced in the closure it is captured automatically.)

- [ ] **Step 9: Commit**

```bash
git add crates/control-server/src/mediaplane.rs
git commit -m "feat(control-server): media-plane forward control (push tag 5, handle tag 2)"
```

---

### Task 6: Media plane data listener (`:9005`) + splice + port publish

**Files:**
- Modify: `crates/control-server/src/mediaplane.rs` (capture a tokio `Handle`; spawn the forward-data listener; `read_forward_header`, `splice_forward`, `serve_forward`)
- Modify: `compose.yaml` (publish 9005)
- Modify: `Dockerfile` (`EXPOSE` 9005)

**Interfaces:**
- Consumes: `wire::forward::ForwardHeader`, `app.dial_host(&Host)` (async), `app.store.get()`, `app.forwards.conn_opened/conn_closed`, `ListenConfig.forward`.
- Produces: `fn read_forward_header(stream: &mut TcpStream) -> std::io::Result<wire::forward::ForwardHeader>`; `fn splice_forward(a: TcpStream, b: TcpStream)`; `fn serve_forward(stream, app, rt_handle)`.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` at the bottom of `crates/control-server/src/mediaplane.rs`:

```rust
    #[test]
    fn forward_header_frames_round_trip() {
        use std::io::Write;
        use std::net::{TcpListener, TcpStream};
        let hdr = wire::forward::ForwardHeader { token: None, host_id: "h".into(), id: "f22".into(), remote_port: 22 };
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            read_forward_header(&mut s).unwrap()
        });
        let mut c = TcpStream::connect(addr).unwrap();
        let body = serde_json::to_vec(&hdr).unwrap();
        c.write_all(&(body.len() as u32).to_be_bytes()).unwrap();
        c.write_all(&body).unwrap();
        assert_eq!(h.join().unwrap(), hdr);
    }

    #[test]
    fn splice_forward_pipes_both_ways() {
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};
        // "upstream" = an echo server.
        let echo = TcpListener::bind("127.0.0.1:0").unwrap();
        let echo_addr = echo.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = echo.accept().unwrap();
            let mut buf = [0u8; 64];
            let n = s.read(&mut buf).unwrap();
            s.write_all(&buf[..n]).unwrap();
        });
        // A pair standing in for the viewer-facing client socket.
        let front = TcpListener::bind("127.0.0.1:0").unwrap();
        let front_addr = front.local_addr().unwrap();
        let mut client = TcpStream::connect(front_addr).unwrap();
        let (server_side, _) = front.accept().unwrap();
        let upstream = TcpStream::connect(echo_addr).unwrap();
        std::thread::spawn(move || splice_forward(server_side, upstream));
        client.write_all(b"ping").unwrap();
        let mut got = [0u8; 4];
        client.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"ping");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p control-server forward_header_frames_round_trip splice_forward_pipes_both_ways`
Expected: FAIL — functions not found.

- [ ] **Step 3: Add `read_forward_header` and `splice_forward`**

In `crates/control-server/src/mediaplane.rs`, add:

```rust
/// Read the data-plane header: `[u32be len][JSON ForwardHeader]` (len capped at 64 KiB).
fn read_forward_header(stream: &mut TcpStream) -> std::io::Result<wire::forward::ForwardHeader> {
    let mut lb = [0u8; 4];
    stream.read_exact(&mut lb)?;
    let len = u32::from_be_bytes(lb) as usize;
    if len > 64 * 1024 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "forward header too large"));
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Bidirectionally pipe two TCP streams until both close. Each direction runs in its own
/// thread and half-closes its peer's write side on EOF so the reverse direction drains.
fn splice_forward(a: TcpStream, b: TcpStream) {
    let (mut a_rd, mut b_wr) = match (a.try_clone(), b.try_clone()) {
        (Ok(x), Ok(y)) => (x, y),
        _ => return,
    };
    let (mut b_rd, mut a_wr) = (b, a);
    let t = std::thread::spawn(move || {
        let _ = std::io::copy(&mut a_rd, &mut b_wr);
        let _ = b_wr.shutdown(std::net::Shutdown::Write);
    });
    let _ = std::io::copy(&mut b_rd, &mut a_wr);
    let _ = a_wr.shutdown(std::net::Shutdown::Write);
    let _ = t.join();
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p control-server forward_header_frames_round_trip splice_forward_pipes_both_ways`
Expected: PASS (2 tests).

- [ ] **Step 5: Add `serve_forward` and the listener; capture a runtime `Handle`**

In `pub fn spawn`, at the very top of the function body (it is called from the async `main`, so a runtime exists), add:

```rust
    let rt_handle = tokio::runtime::Handle::current();
```

Add the forward-data listener thread (place it near the port-1 listener block, after it):

```rust
    // Port `forward` (data plane): one TCP connection per forwarded local socket.
    {
        let (app, rt_handle) = (app.clone(), rt_handle.clone());
        std::thread::spawn(move || match TcpListener::bind(("0.0.0.0", forward_port)) {
            Ok(l) => {
                tracing::info!("forward data plane listening on 0.0.0.0:{forward_port}");
                for stream in l.incoming().flatten() {
                    let _ = stream.set_nodelay(true);
                    let (app, rt_handle) = (app.clone(), rt_handle.clone());
                    std::thread::spawn(move || serve_forward(stream, app, rt_handle));
                }
            }
            Err(e) => tracing::error!("forward bind {forward_port} failed: {e}"),
        });
    }
```

Add `serve_forward`:

```rust
/// One forwarded connection: read the header, resolve + dial the clone port, reply a
/// status byte, then splice. `0x00` = connected, non-zero = dial failed.
fn serve_forward(mut stream: TcpStream, app: App, rt: tokio::runtime::Handle) {
    let header = match read_forward_header(&mut stream) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!("forward header read failed: {e}");
            return;
        }
    };
    let host = app.store.get().hosts.into_iter().find(|h| h.id == header.host_id);
    let Some(host) = host else {
        let _ = stream.write_all(&[1u8]);
        return;
    };
    let dial = rt.block_on(app.dial_host(&host));
    let addr = format!("{dial}:{}", header.remote_port);
    match TcpStream::connect(&addr) {
        Ok(upstream) => {
            let _ = upstream.set_nodelay(true);
            if stream.write_all(&[0u8]).is_err() {
                return;
            }
            app.forwards.conn_opened(&header.host_id, &header.id);
            splice_forward(stream, upstream);
            app.forwards.conn_closed(&header.host_id, &header.id);
        }
        Err(e) => {
            tracing::info!("forward dial {addr} failed: {e}");
            let _ = stream.write_all(&[1u8]);
        }
    }
}
```

`serve_forward` reads `header.id` (defined on `ForwardHeader` in Task 2) to attribute active-conn counts to the right rule.

- [ ] **Step 6: Publish the port**

In `compose.yaml`, change the ports block (~line 39-40) from:

```yaml
      # 9000 web/API, 9001 video, 9002 per-clone MCP, 9003 global MCP.
      - "9000-9003:9000-9003"
```

to:

```yaml
      # 9000 web/API, 9001 video, 9002 per-clone MCP, 9003 global MCP, 9005 forward.
      - "9000-9003:9000-9003"
      - "9005:9005"
```

In `Dockerfile` (~line 122), change:

```dockerfile
# 9000 web/API, 9001 video, 9002 per-clone MCP, 9003 global MCP.
EXPOSE 9000-9003
```

to:

```dockerfile
# 9000 web/API, 9001 video, 9002 per-clone MCP, 9003 global MCP, 9005 forward data plane.
EXPOSE 9000-9003 9005
```

- [ ] **Step 7: Build + run the media-plane tests**

Run: `cargo test -p control-server forward_header_frames_round_trip splice_forward_pipes_both_ways; and cargo build -p control-server`
Expected: tests PASS; control-server builds clean.

- [ ] **Step 8: Commit**

```bash
git add crates/control-server/src/mediaplane.rs compose.yaml Dockerfile
git commit -m "feat(control-server): forward data listener :9005 + splice; publish port"
```

---

### Task 7: Viewer forward module — `ForwardManager` (reconcile + listeners + tunnels)

**Files:**
- Create: `crates/viewer/src/forward.rs`

**Interfaces:**
- Consumes: `wire::forward::{ForwardRule, ForwardState, ForwardStatusMsg, ForwardHeader}`.
- Produces: `pub type StatusReport = Arc<dyn Fn(ForwardStatusMsg) + Send + Sync>;` and `pub struct ForwardManager` with `pub fn new(report: StatusReport) -> Self` and `pub fn reconcile(&self, rules: Vec<ForwardRule>, forward_addr: String)`.

- [ ] **Step 1: Write the failing test**

Create `crates/viewer/src/forward.rs` with the test module (and a `use super::*;`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Mutex;

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    /// A stub control-server data port: accept one conn, read the framed header, reply
    /// 0x00, then echo everything.
    fn spawn_stub() -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut lb = [0u8; 4];
            s.read_exact(&mut lb).unwrap();
            let mut body = vec![0u8; u32::from_be_bytes(lb) as usize];
            s.read_exact(&mut body).unwrap();
            let _hdr: wire::forward::ForwardHeader = serde_json::from_slice(&body).unwrap();
            s.write_all(&[0u8]).unwrap();
            let mut buf = [0u8; 64];
            let n = s.read(&mut buf).unwrap();
            s.write_all(&buf[..n]).unwrap();
        });
        addr
    }

    #[test]
    fn reconcile_binds_and_tunnels() {
        let stub = spawn_stub();
        let local = free_port();
        let reports: Arc<Mutex<Vec<ForwardStatusMsg>>> = Arc::new(Mutex::new(Vec::new()));
        let r2 = reports.clone();
        let mgr = ForwardManager::new(Arc::new(move |m| r2.lock().unwrap().push(m)));
        mgr.reconcile(
            vec![ForwardRule { host_id: "h".into(), id: "f".into(), remote_port: 3000, local_port: local }],
            stub,
        );
        // Give the listener a moment to bind.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let mut c = TcpStream::connect(("127.0.0.1", local)).unwrap();
        c.write_all(b"hey").unwrap();
        let mut got = [0u8; 3];
        c.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hey");
        assert!(reports.lock().unwrap().iter().any(|m| matches!(m.state, ForwardState::Listening)));
    }

    #[test]
    fn reconcile_reports_bind_conflict() {
        let local = free_port();
        let _hog = TcpListener::bind(("127.0.0.1", local)).unwrap(); // hold the port
        let reports: Arc<Mutex<Vec<ForwardStatusMsg>>> = Arc::new(Mutex::new(Vec::new()));
        let r2 = reports.clone();
        let mgr = ForwardManager::new(Arc::new(move |m| r2.lock().unwrap().push(m)));
        mgr.reconcile(
            vec![ForwardRule { host_id: "h".into(), id: "f".into(), remote_port: 1, local_port: local }],
            "127.0.0.1:1".into(),
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(reports.lock().unwrap().iter().any(|m| matches!(m.state, ForwardState::Error)));
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p viewer forward::tests`
Expected: FAIL — `ForwardManager` not found. (The module isn't declared in `main.rs` yet; declaring it is Task 8, but the test can compile once the type exists — to run this task's tests, temporarily add `mod forward;` to `main.rs`. Add it now; Task 8 uses it.)

Add `mod forward;` to `crates/viewer/src/main.rs` alongside `mod config;` etc.

- [ ] **Step 3: Implement `ForwardManager`**

Insert ABOVE the test module in `crates/viewer/src/forward.rs`:

```rust
//! Local port-forward listeners. The net thread hands us the server's desired rule set
//! (port-1 tag 5) via [`ForwardManager::reconcile`]; we keep one `127.0.0.1` listener
//! thread per rule, and per accepted socket open a data connection to the control-server
//! (`forward_addr`, its `:forward` port), send a [`ForwardHeader`], and splice. Status
//! (listening / bind error) is reported back through the `report` closure, which the net
//! thread turns into a port-1 tag-2 frame. Plain std threads/sockets — no async, no GTK.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wire::forward::{ForwardHeader, ForwardRule, ForwardState, ForwardStatusMsg};

/// Reports a rule's status back toward the server (the net thread frames it as tag 2).
pub type StatusReport = Arc<dyn Fn(ForwardStatusMsg) + Send + Sync>;

struct ListenerHandle {
    rule: ForwardRule,
    stop: Arc<AtomicBool>,
}

pub struct ForwardManager {
    report: StatusReport,
    active: Mutex<HashMap<String, ListenerHandle>>, // rule id → its listener
}

impl ForwardManager {
    pub fn new(report: StatusReport) -> Self {
        Self { report, active: Mutex::new(HashMap::new()) }
    }

    /// Make the running listeners match `rules`: stop any that were removed or whose
    /// `(local_port, remote_port, host_id)` changed; start listeners for new rules;
    /// leave unchanged rules running. `forward_addr` is `host:port` of the server's data
    /// port. Idempotent — a reconnect that pushes the same set is a no-op.
    pub fn reconcile(&self, rules: Vec<ForwardRule>, forward_addr: String) {
        let wanted: HashMap<String, ForwardRule> =
            rules.into_iter().map(|r| (r.id.clone(), r)).collect();
        let mut active = self.active.lock().unwrap();
        // Stop removed/changed listeners.
        active.retain(|id, h| {
            let keep = wanted.get(id).is_some_and(|w| *w == h.rule);
            if !keep {
                h.stop.store(true, Ordering::SeqCst);
            }
            keep
        });
        // Start new listeners.
        for (id, rule) in wanted {
            if active.contains_key(&id) {
                continue;
            }
            let stop = Arc::new(AtomicBool::new(false));
            let handle = ListenerHandle { rule: rule.clone(), stop: stop.clone() };
            let report = self.report.clone();
            let fa = forward_addr.clone();
            std::thread::spawn(move || run_listener(rule, fa, report, stop));
            active.insert(id, handle);
        }
    }
}

fn run_listener(rule: ForwardRule, forward_addr: String, report: StatusReport, stop: Arc<AtomicBool>) {
    let bind = format!("127.0.0.1:{}", rule.local_port);
    let listener = match TcpListener::bind(&bind) {
        Ok(l) => l,
        Err(e) => {
            report(ForwardStatusMsg {
                host_id: rule.host_id.clone(),
                id: rule.id.clone(),
                state: ForwardState::Error,
                error: Some(format!("{bind}: {e}")),
            });
            return;
        }
    };
    listener.set_nonblocking(true).ok();
    report(ForwardStatusMsg {
        host_id: rule.host_id.clone(),
        id: rule.id.clone(),
        state: ForwardState::Listening,
        error: None,
    });
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        match listener.accept() {
            Ok((sock, _)) => {
                let (fa, rule) = (forward_addr.clone(), rule.clone());
                std::thread::spawn(move || tunnel(sock, fa, rule));
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(_) => return,
        }
    }
}

fn tunnel(local: TcpStream, forward_addr: String, rule: ForwardRule) {
    let mut up = match TcpStream::connect(&forward_addr) {
        Ok(s) => s,
        Err(_) => return,
    };
    up.set_nodelay(true).ok();
    let hdr = ForwardHeader {
        token: None,
        host_id: rule.host_id,
        id: rule.id,
        remote_port: rule.remote_port,
    };
    let Ok(body) = serde_json::to_vec(&hdr) else { return };
    if up.write_all(&(body.len() as u32).to_be_bytes()).is_err() || up.write_all(&body).is_err() {
        return;
    }
    let mut status = [0u8; 1];
    if up.read_exact(&mut status).is_err() || status[0] != 0 {
        return; // dial failed server-side
    }
    splice(local, up);
}

fn splice(a: TcpStream, b: TcpStream) {
    let (mut a_rd, mut b_wr) = match (a.try_clone(), b.try_clone()) {
        (Ok(x), Ok(y)) => (x, y),
        _ => return,
    };
    let (mut b_rd, mut a_wr) = (b, a);
    let t = std::thread::spawn(move || {
        let _ = std::io::copy(&mut a_rd, &mut b_wr);
        let _ = b_wr.shutdown(std::net::Shutdown::Write);
    });
    let _ = std::io::copy(&mut b_rd, &mut a_wr);
    let _ = a_wr.shutdown(std::net::Shutdown::Write);
    let _ = t.join();
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p viewer forward::tests`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/viewer/src/forward.rs crates/viewer/src/main.rs
git commit -m "feat(viewer): ForwardManager — reconcile local listeners + tunnel to server"
```

---

### Task 8: Viewer net-thread integration — dispatch tag 5, wire status to tag 2

**Files:**
- Modify: `crates/viewer/src/main.rs` (construct `ForwardManager` in `run_gui`; capture into the net thread; handle tag 5 in the read loop; extend the `matches!` guard)

**Interfaces:**
- Consumes: `forward::{ForwardManager, StatusReport}` (Task 7), `send_tagged` (existing, main.rs:1424), `wire::forward::{ForwardsMsg, ForwardStatusMsg}`.
- Produces: viewer-side handling of server→viewer tag 5 and viewer→server tag 2.

- [ ] **Step 1: Add the `wire::forward` import**

In `crates/viewer/src/main.rs`, near `use wire::viewer::ModeMsg;`, add:

```rust
use wire::forward::{ForwardsMsg, ForwardStatusMsg};
```

- [ ] **Step 2: Construct the manager and its status reporter in `run_gui`**

In `fn run_gui`, right after `let writer: Writer = Arc::new(Mutex::new(None));` (~line 173), add:

```rust
    // Port-forward manager: reports status back as port-1 tag-2 frames via `writer`.
    let fwd_mgr: Arc<forward::ForwardManager> = {
        let writer = writer.clone();
        let report: forward::StatusReport = Arc::new(move |msg: ForwardStatusMsg| {
            if let Ok(json) = serde_json::to_string(&msg) {
                send_tagged(&writer, 2, json);
            }
        });
        Arc::new(forward::ForwardManager::new(report))
    };
```

- [ ] **Step 3: Capture the manager + address into the net thread**

Find the net-thread setup tuple (~line 182): `let (aus, srcs, writer, inbox, cursors, reported, warp, addr) = (...);` and add `fwd_mgr` to both sides:

```rust
        let (aus, srcs, writer, inbox, cursors, reported, warp, addr, fwd_mgr) =
            (aus.clone(), srcs.clone(), writer.clone(), inbox.clone(), cursors.clone(), reported.clone(), warp.clone(), addr.clone(), fwd_mgr.clone());
```

- [ ] **Step 4: Handle tag 5 in the read loop**

In the read loop, extend the tag guard. Change:

```rust
                            if matches!(tag[0], 1 | 2 | 3 | 4) {
```

to:

```rust
                            if matches!(tag[0], 1 | 2 | 3 | 4 | 5) {
```

Then, in the `if/else` chain that dispatches by tag (after the `else if tag[0] == 3 { ... }` block and BEFORE the final `else if let Ok(c) = serde_json::from_slice::<CursorMeta>(&body)` cursor block), insert:

```rust
                                } else if tag[0] == 5 {
                                    // Desired forward set: reconcile local listeners. The
                                    // data port lives on the same host as the video port.
                                    if let Ok(m) = serde_json::from_slice::<ForwardsMsg>(&body) {
                                        let server = addr.lock().unwrap().clone();
                                        let host = server
                                            .rsplit_once(':')
                                            .map(|(h, _)| h.to_string())
                                            .unwrap_or(server);
                                        let forward_addr = format!("{host}:{}", m.forward_port);
                                        fwd_mgr.reconcile(m.rules, forward_addr);
                                    }
```

(The existing cursor block starts with `} else if let Ok(c) = ...` — your new block sits just before it, closing with `}` handled by that `else if`.)

- [ ] **Step 5: Update the module doc comment (tags)**

In the `crates/viewer/src/main.rs` header doc, update the port-1 framing note to mention tag 5 (server→viewer forwards) and viewer→server tag 2 (forward status). Change the line describing viewer→server tags to:

```rust
//! viewer→server: `[u8 tag][u32be len][json]` (0 input, 1 clipboard, 2 forward-status). Auto-reconnects.
```

- [ ] **Step 6: Build the viewer**

Run: `cargo build -p viewer`
Expected: builds clean.

- [ ] **Step 7: Commit**

```bash
git add crates/viewer/src/main.rs
git commit -m "feat(viewer): dispatch forwards config (tag 5) + report status (tag 2)"
```

---

### Task 9: Frontend API + host type

**Files:**
- Modify: `frontend/app/lib/types.ts` (import `PortForward` from wire; add `forwards?` to `Host`)
- Modify: `frontend/app/lib/api.ts` (add `putForwards`)

**Interfaces:**
- Consumes: generated `~/lib/wire/PortForward` (Task 1).
- Produces: `Host.forwards?: PortForward[]`; `putForwards(hostId, forwards)`.

- [ ] **Step 1: Add `forwards` to the hand-written `Host`**

In `frontend/app/lib/types.ts`, add an import at the top (near the other imports):

```ts
import type { PortForward } from "~/lib/wire/PortForward";
```

Inside `interface Host`, before the closing `}` (after the `unread?` field ~line 88), add:

```ts
  /** Local port-forward rules; the native viewer runs the listeners. Live status
   *  arrives separately via the `forwards` SSE event, keyed by host id then rule id. */
  forwards?: PortForward[];
```

- [ ] **Step 2: Add `putForwards` to the API client**

In `frontend/app/lib/api.ts`, add near the other host mutations (after `deleteHost`):

```ts
/** Replace a host's port-forward rules. New rules omit `id` (server derives it as
 *  `f<localPort>`). 400 on a local-port conflict (validated server-side); the UI
 *  refreshes from the next `/events` frame. */
export const putForwards = (
  hostId: string,
  forwards: Array<{ id?: string; remotePort: number; localPort: number; enabled: boolean; label?: string }>,
) => putJson(`/api/hosts/${encodeURIComponent(hostId)}/forwards`, { forwards });
```

- [ ] **Step 3: Typecheck**

Run: `cd frontend; and bun run typecheck`
Expected: no type errors.

- [ ] **Step 4: Commit**

```bash
git add frontend/app/lib/types.ts frontend/app/lib/api.ts
git commit -m "feat(frontend): Host.forwards type + putForwards API"
```

---

### Task 10: `PortForwardModal` component + Storybook story

**Files:**
- Create: `frontend/app/components/PortForwardModal.tsx`
- Create: `frontend/app/stories/PortForwardModal.stories.tsx`

**Interfaces:**
- Consumes: `Host` (types.ts), `PortForward` + `ForwardRuntime` (wire), `putForwards` is called by the parent (Task 11) via `onSubmit`.
- Produces: `PortForwardModal` with props `{ host, runtime, busy, error, onClose, onSubmit }` where `onSubmit(forwards: Array<{ id?; remotePort; localPort; enabled; label? }>) => void`.

- [ ] **Step 1: Write the component**

Create `frontend/app/components/PortForwardModal.tsx`:

```tsx
// Configure a host's local port forwards (remote clone port → 127.0.0.1:<local> on the
// machine running the native viewer). Mirrors the change-account modal shell. Live
// status (listening / error / offline) is merged from the `forwards` SSE event by rule id.
import { useState } from "react";

import type { Host } from "~/lib/types";
import type { ForwardRuntime } from "~/lib/wire/ForwardRuntime";

type Row = { id?: string; remotePort: string; localPort: string; label: string; enabled: boolean };

function toRows(host: Host): Row[] {
  return (host.forwards ?? []).map((f) => ({
    id: f.id,
    remotePort: String(f.remotePort),
    localPort: String(f.localPort),
    label: f.label ?? "",
    enabled: f.enabled,
  }));
}

function statusFor(runtime: ForwardRuntime[], id?: string): ForwardRuntime | undefined {
  return id ? runtime.find((r) => r.id === id) : undefined;
}

function Badge({ rt }: { rt?: ForwardRuntime }) {
  const state = rt?.state ?? "offline";
  const color =
    state === "listening"
      ? "bg-emerald-100 text-emerald-700 dark:bg-emerald-900/40 dark:text-emerald-300"
      : state === "error"
        ? "bg-red-100 text-red-700 dark:bg-red-900/40 dark:text-red-300"
        : "bg-slate-100 text-slate-500 dark:bg-slate-700 dark:text-slate-400";
  return (
    <span className={`rounded px-1.5 py-0.5 text-[10px] font-medium ${color}`} title={rt?.error ?? ""}>
      {state}
      {rt && rt.activeConns > 0 ? ` · ${rt.activeConns}` : ""}
    </span>
  );
}

export function PortForwardModal({
  host,
  runtime,
  busy,
  error,
  onClose,
  onSubmit,
}: {
  host: Host;
  runtime: ForwardRuntime[];
  busy: boolean;
  error: string | null;
  onClose: () => void;
  onSubmit: (
    forwards: Array<{ id?: string; remotePort: number; localPort: number; enabled: boolean; label?: string }>,
  ) => void;
}) {
  const [rows, setRows] = useState<Row[]>(() => toRows(host));

  const update = (i: number, patch: Partial<Row>) =>
    setRows((rs) => rs.map((r, j) => (j === i ? { ...r, ...patch } : r)));
  const remove = (i: number) => setRows((rs) => rs.filter((_, j) => j !== i));
  const add = () =>
    setRows((rs) => [...rs, { remotePort: "", localPort: "", label: "", enabled: true }]);

  const submit = () => {
    const forwards = rows.map((r) => ({
      id: r.id,
      remotePort: Number(r.remotePort),
      localPort: Number(r.localPort),
      enabled: r.enabled,
      label: r.label.trim() || undefined,
    }));
    onSubmit(forwards);
  };

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-slate-900/30 p-4" onClick={onClose}>
      <div
        className="w-full max-w-lg rounded-xl border border-slate-200 bg-white p-5 shadow-xl dark:border-slate-700 dark:bg-slate-800"
        onClick={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          if (e.key === "Escape") onClose();
        }}
      >
        <h3 className="text-sm font-semibold text-slate-900 dark:text-slate-100">
          Port forwards · <span className="text-emerald-700 dark:text-emerald-400">{host.displayName ?? host.id}</span>
        </h3>
        <p className="mt-1 text-xs text-slate-500 dark:text-slate-400">
          Expose a port inside this host at <code>127.0.0.1:&lt;local&gt;</code> on the machine running the viewer.
        </p>

        {error ? <p className="mt-3 text-xs text-red-600 dark:text-red-400">{error}</p> : null}

        <div className="mt-4 space-y-2">
          <div className="grid grid-cols-[1fr_1fr_1fr_auto_auto] gap-2 text-[11px] font-medium uppercase tracking-wide text-slate-400">
            <span>Remote</span>
            <span>Local</span>
            <span>Label</span>
            <span>On</span>
            <span></span>
          </div>
          {rows.map((r, i) => (
            <div key={i} className="grid grid-cols-[1fr_1fr_1fr_auto_auto] items-center gap-2">
              <input
                inputMode="numeric"
                value={r.remotePort}
                onChange={(e) => update(i, { remotePort: e.target.value, id: undefined })}
                placeholder="3000"
                className="rounded-md border border-slate-300 px-2 py-1 text-sm dark:border-slate-600 dark:bg-slate-900 dark:text-slate-100"
              />
              <div className="flex items-center gap-1">
                <input
                  inputMode="numeric"
                  value={r.localPort}
                  onChange={(e) => update(i, { localPort: e.target.value, id: undefined })}
                  placeholder="8080"
                  className="w-full rounded-md border border-slate-300 px-2 py-1 text-sm dark:border-slate-600 dark:bg-slate-900 dark:text-slate-100"
                />
                <Badge rt={statusFor(runtime, r.id)} />
              </div>
              <input
                value={r.label}
                onChange={(e) => update(i, { label: e.target.value })}
                placeholder="dev server"
                className="rounded-md border border-slate-300 px-2 py-1 text-sm dark:border-slate-600 dark:bg-slate-900 dark:text-slate-100"
              />
              <input
                type="checkbox"
                checked={r.enabled}
                onChange={(e) => update(i, { enabled: e.target.checked })}
                className="size-4"
              />
              <button
                type="button"
                onClick={() => remove(i)}
                className="cursor-pointer rounded px-1.5 py-1 text-xs text-red-600 hover:bg-red-50 dark:text-red-400 dark:hover:bg-red-950/40"
              >
                ✕
              </button>
            </div>
          ))}
          <button
            type="button"
            onClick={add}
            className="cursor-pointer text-xs text-emerald-700 hover:underline dark:text-emerald-400"
          >
            + Add forward
          </button>
        </div>

        <div className="mt-5 flex justify-end gap-2">
          <button
            type="button"
            onClick={onClose}
            className="rounded-md px-3 py-1.5 text-sm text-slate-600 hover:bg-slate-100 dark:text-slate-300 dark:hover:bg-slate-800"
          >
            Cancel
          </button>
          <button
            type="button"
            disabled={busy}
            onClick={submit}
            className="rounded-md bg-emerald-600 px-3 py-1.5 text-sm font-medium text-white hover:bg-emerald-500 disabled:opacity-50"
          >
            {busy ? "Saving…" : "Save"}
          </button>
        </div>
      </div>
    </div>
  );
}
```

- [ ] **Step 2: Write the Storybook story**

Create `frontend/app/stories/PortForwardModal.stories.tsx`:

```tsx
import type { Meta, StoryObj } from "@storybook/react-vite";

import { PortForwardModal } from "~/components/PortForwardModal";
import type { Host } from "~/lib/types";

const host: Host = {
  id: "pega-abc",
  host: "pega-abc",
  port: 3389,
  username: "",
  password: "",
  managed: true,
  forwards: [
    { id: "f8080", remotePort: 3000, localPort: 8080, enabled: true, label: "dev server" },
    { id: "f5433", remotePort: 5432, localPort: 5433, enabled: false, label: null },
  ],
};

const meta: Meta<typeof PortForwardModal> = {
  title: "Modals/PortForwardModal",
  component: PortForwardModal,
  args: {
    host,
    runtime: [
      { id: "f8080", state: "listening", activeConns: 2 },
      { id: "f5433", state: "error", error: "127.0.0.1:5433: address already in use", activeConns: 0 },
    ],
    busy: false,
    error: null,
    onClose: () => {},
    onSubmit: () => {},
  },
};
export default meta;

type Story = StoryObj<typeof PortForwardModal>;
export const Default: Story = {};
export const WithError: Story = { args: { error: "local port 8080 is already in use" } };
```

- [ ] **Step 3: Typecheck**

Run: `cd frontend; and bun run typecheck`
Expected: no type errors. (Optionally spot-check visually: `cd frontend; and bun run storybook` → open Modals/PortForwardModal.)

- [ ] **Step 4: Commit**

```bash
git add frontend/app/components/PortForwardModal.tsx frontend/app/stories/PortForwardModal.stories.tsx
git commit -m "feat(frontend): PortForwardModal + story"
```

---

### Task 11: Wire the modal into the dashboard, menu, and SSE

**Files:**
- Modify: `frontend/app/routes/_index.tsx` (SSE `forwards` listener; `forwardHost` state; render modal; pass callback + forwards down)
- Modify: `frontend/app/components/Sidebar.tsx` (new `onPortForwardHost` prop → `SidebarHost`)
- Modify: `frontend/app/components/SidebarHost.tsx` (new `onPortForward` prop; menu item)

**Interfaces:**
- Consumes: `putForwards` (Task 9), `PortForwardModal` (Task 10), `ForwardRuntime` (wire).
- Produces: end-to-end UI flow.

- [ ] **Step 1: Add the menu item in `SidebarHost`**

In `frontend/app/components/SidebarHost.tsx`:

Add `onPortForward: () => void;` to `interface SidebarHostProps` (after `onChangeAccount`).

Add `onPortForward` to the `OverflowMenu` inline prop type and destructure (the block starting `function OverflowMenu({ hostId, managed, busy, onCommit, onChangeAccount, onDelete }: {...})`):

```tsx
function OverflowMenu({
  hostId,
  managed,
  busy,
  onCommit,
  onChangeAccount,
  onPortForward,
  onDelete,
}: {
  hostId: string;
  managed: boolean;
  busy: boolean;
  onCommit: () => void;
  onChangeAccount: () => void;
  onPortForward: () => void;
  onDelete: () => void;
}) {
```

In the menu body's `managed` block, add the item (after "Change account…"):

```tsx
              {item("Change account…", onChangeAccount)}
              {item("Port forward…", onPortForward)}
```

In `SidebarHost`'s render of `<OverflowMenu .../>` (~line 356), pass it through:

```tsx
          onPortForward={onPortForward}
```

And add `onPortForward` to `SidebarHost`'s destructured props (the `export function SidebarHost({ ... })` list).

- [ ] **Step 2: Thread the prop through `Sidebar`**

In `frontend/app/components/Sidebar.tsx`:

Add to the props interface (after `onChangeAccountHost`):

```tsx
  /** Open the port-forward editor for a host. */
  onPortForwardHost: (host: Host) => void;
```

Destructure `onPortForwardHost` in the `Sidebar({ ... })` signature, and pass it to each `<SidebarHost>` (next to `onChangeAccount={() => onChangeAccountHost(host)}`):

```tsx
                    onPortForward={() => onPortForwardHost(host)}
```

- [ ] **Step 3: Subscribe to the `forwards` SSE event**

In `frontend/app/routes/_index.tsx`:

Add the import:

```tsx
import type { ForwardRuntime } from "~/lib/wire/ForwardRuntime";
import { putForwards } from "~/lib/api";
import { PortForwardModal } from "~/components/PortForwardModal";
```

In `useLiveState`, add a `forwards` state + listener alongside `stats`:

```tsx
  const [forwards, setForwards] = useState<Record<string, ForwardRuntime[]>>({});
```

and inside the `useEffect`, after the `stats` listener:

```tsx
    es.addEventListener("forwards", (e) => {
      try {
        setForwards(JSON.parse((e as MessageEvent).data));
      } catch {
        // ignore malformed frame
      }
    });
```

and change the return to `return { state, stats, forwards };`.

In `Home`, change `const { state, stats } = useLiveState(loaderData);` to `const { state, stats, forwards } = useLiveState(loaderData);` and pass `forwards` to `<Dashboard ... forwards={forwards} />`.

- [ ] **Step 4: Accept `forwards` in `Dashboard` and add modal state**

Add `forwards` to the `Dashboard` prop type and destructure:

```tsx
function Dashboard({
  state,
  stats,
  cloneCpus,
  forwards,
}: {
  state: ControlState;
  stats: Record<string, ContainerStats>;
  cloneCpus: number;
  forwards: Record<string, ForwardRuntime[]>;
}) {
```

Add modal state near `commitHost`/`changeHost`:

```tsx
  const [forwardHost, setForwardHost] = useState<Host | null>(null);
  const [forwarding, setForwarding] = useState(false);
  const [forwardError, setForwardError] = useState<string | null>(null);
```

- [ ] **Step 5: Pass the callback into `Sidebar` and render the modal**

In the `<Sidebar ... />` element (near `onChangeAccountHost={(host) => setChangeHost(host)}`), add:

```tsx
          onPortForwardHost={(host) => {
            setForwardError(null);
            setForwardHost(host);
          }}
```

Near the `{commitHost ? (<CommitImageModal .../>) : null}` block, add:

```tsx
      {forwardHost ? (
        <PortForwardModal
          host={state.hosts.find((h) => h.id === forwardHost.id) ?? forwardHost}
          runtime={forwards[forwardHost.id] ?? []}
          busy={forwarding}
          error={forwardError}
          onClose={() => setForwardHost(null)}
          onSubmit={(list) => {
            setForwarding(true);
            setForwardError(null);
            putForwards(forwardHost.id, list)
              .then(() => setForwardHost(null))
              .catch((e: Error) => setForwardError(e.message))
              .finally(() => setForwarding(false));
          }}
        />
      ) : null}
```

> Note: the modal reads `host` from the live `state.hosts` so that after a successful save the rule list reflects the persisted `id`s on next open; while open, live status flows via the `runtime` prop from the `forwards` SSE map.

- [ ] **Step 6: Typecheck + build**

Run: `cd frontend; and bun run typecheck; and bun run build`
Expected: no type errors; build succeeds.

- [ ] **Step 7: Commit**

```bash
git add frontend/app/routes/_index.tsx frontend/app/components/Sidebar.tsx frontend/app/components/SidebarHost.tsx
git commit -m "feat(frontend): wire PortForwardModal into menu + forwards SSE"
```

---

### Task 12: End-to-end manual verification

**Files:** none (verification only).

- [ ] **Step 1: Build everything**

Run: `cargo build --workspace; and cd frontend; and bun run build`
Expected: clean build across the workspace + frontend.

- [ ] **Step 2: Bring up the stack and a test service inside a clone**

Start the control-server + a managed clone per the normal dev flow (see [docs/DEPLOY.md](../../DEPLOY.md)). Inside a running clone, start a throwaway HTTP server:

```bash
# in the clone (via chat/agent or a shell): serve on :8000
python3 -m http.server 8000
```

- [ ] **Step 3: Add a forward from the UI**

In the frontend, open that host's ⋯ menu → **Port forward…** → add `Remote 8000 → Local 18000`, Save. Expected: the row shows a green **listening** badge within a second (viewer bound `127.0.0.1:18000`).

- [ ] **Step 4: Exercise the tunnel**

On the machine running the native viewer:

Run: `curl -s http://127.0.0.1:18000/ | head`
Expected: the clone's `http.server` directory listing HTML. The badge's active-conn count briefly increments.

- [ ] **Step 5: Verify "any host, always on"**

Select a *different* host in the UI (change `selected`). Re-run the curl from Step 4.
Expected: still succeeds — the forward is independent of the displayed host.

- [ ] **Step 6: Verify the local-bind error path (Case 2)**

On the viewer machine, occupy a port: `python3 -m http.server 19000` (leave running). In the UI add a forward `Remote 8000 → Local 19000`, Save.
Expected: the row shows a red **error** badge; hovering shows `127.0.0.1:19000: address already in use`. (This came viewer → server tag 2 → `forwards` SSE.)

- [ ] **Step 7: Verify the config-conflict error path (Case 1)**

Add two rules with the same Local port (e.g. two rows both `→ 20000`), Save.
Expected: the modal shows an inline error (`local port 20000 is already in use`) and nothing is persisted — the `PUT` returned 400 synchronously.

- [ ] **Step 8: Verify the remote-down path**

Add a forward to a remote port with nothing listening (e.g. `Remote 65000 → Local 21000`), Save (badge → listening). Then `curl -s http://127.0.0.1:21000/`.
Expected: curl fails/closes immediately (server dialed the clone, got refused, returned a non-zero status byte), but the rule's badge stays **listening** (connection-level failure, not rule-level).

- [ ] **Step 9: Verify persistence + offline**

Close the native viewer. Expected: badges go **offline** (grey). Restart the control-server, restart the viewer. Expected: the saved rules re-appear and re-bind to **listening** without re-entering them (config persisted in `state.json`, re-pushed on connect).

- [ ] **Step 10: Final commit (if any verification fixes were needed)**

```bash
git add -A
git commit -m "test(port-forward): E2E verification pass"
```

---

## Self-Review notes

- **Spec coverage:** local-forwarding/any-host/persist/approach-1/config-status-split → Tasks 1–6 (server), 7–8 (viewer), 9–11 (frontend); two-layer errors → Task 4 (Case 1) + Task 5/7 (Case 2 relay) + Task 12 Steps 6–7; defaults (127.0.0.1, TCP-only, managed-only menu, token field reserved) → Tasks 4/7/11; testing + edge cases → per-task unit tests + Task 12.
- **Type consistency:** `ForwardHeader` gains `id` in Task 6 (used by `serve_forward` conn-counting and written by the viewer `tunnel` in Task 7) — both sides agree. `ForwardState` variants `listening|error|offline` are consistent across Rust (`lowercase` serde) and the TS badge. Rule `id` is `f{local_port}` everywhere it's derived (Task 4) and only ever compared, never re-derived, elsewhere.
- **Known simplification (documented, not a gap):** `forward_addr` host is parsed from `server_addr` via `rsplit_once(':')`, which assumes an IPv4/hostname `host:port` (matches the viewer's existing `config::DEFAULT_SERVER_ADDR` shape); bracketed IPv6 literals are out of scope for v1, consistent with the rest of the viewer's address handling.
