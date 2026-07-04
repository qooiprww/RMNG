# Dynamic Monitor Layouts & Layout Presets — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let an operator store multiple named monitor-layout presets and switch the active one from the web sidebar; switching live-reconfigures every running clone's monitors **without closing any running programs**, and every attached viewer reflows immediately.

**Architecture:** Presets live in `AppConfig` (`layout_presets` + `active_layout`). A new `POST /api/layout/activate` persists the choice and pushes a new `ServerMsg::SetMonitors` over the existing clone unix socket to every running daemon (and on each daemon's `Hello`). The clone-daemon diffs the desired monitor set against its live Mutter `RecordVirtual` streams — adding/stopping/recreating only the changed ones (**Approach A**) — so gnome-shell never restarts. The server rebuilds per-monitor encoders as sizes change and re-broadcasts the layout; the viewer reconciles its per-monitor windows.

**Tech Stack:** Rust 2024 (workspace crates `wire`, `media`, `control-server`, `clone-daemon`, `viewer`), zbus (Mutter D-Bus), GStreamer/VA-API, PipeWire; React 19 + React Router 7 + Tailwind v4 (Bun); ts-rs (Rust→TS type generation); Docker; Proxmox LXC.

## Global Constraints

- **No app loss:** switching a layout MUST NOT restart gnome-headless or the clone session. The existing `apply-monitors.sh` path (restarts GNOME) is deleted, not reused.
- **Fleet-wide scope:** activating a preset applies to **all running clones** (one global active layout). No per-clone layouts.
- **Approach A only:** count/resolution changes are done by diffing `RecordVirtual` streams on the live Mutter session. No session-rebuild fallback.
- **Server is source of truth:** the active layout is pushed to daemons over the clone socket on `Hello` and on activation. Baked `RMNG_MONITORS` is only a pre-connect boot default.
- **No-env invariant:** the control-server reads config only from `config.json` (no new `RMNG_*` env). Config changes go through `PUT /api/config` / `config::save`.
- **ts-rs generation:** wire types deriving `TS` regenerate `frontend/app/lib/wire/*.ts` on `cargo test -p wire` (or the build). Never hand-edit generated files; DO hand-edit the parallel hand-written `frontend/app/lib/types.ts`.
- **Naming:** existing clone-provisioning `presets` (env/Linear) are untouched and keep the name "clone presets". The new ones are **layout presets** everywhere (`layout_presets`, `active_layout`, `/api/layout/activate`).
- **Refresh rate:** all modes stay `@60.000` (matches `build_modes` / `apply_layout`).
- **Spec:** `docs/superpowers/specs/2026-07-04-dynamic-layout-presets-design.md`.

---

## Phase 0 — Mutter live-reconfiguration spike (de-risk Approach A)

This phase validates the core assumption before any product code. It is exploratory (concrete experiments), not TDD. It runs against a real clone from the **stock** template on a Proxmox CT (create the CT now per Phase 7 Task 7.1–7.3 if none exists, or use an existing clone on CT 105/106). Capture findings in a scratch note; they parameterize Phase 3.

### Task 0.1: Prove live RecordVirtual add / stop / resize on a started session

**Files:**
- Create: `/tmp/spike/mutter-reconfig.md` (findings note — not committed)

- [ ] **Step 1: Open a shell in a running clone and start a probe app**

Pick a running clone container `<c>` (e.g. from `docker ps` on the CT). Launch a GUI app as the clone user so we can prove it survives:
```bash
docker exec -u 1000 -e XDG_RUNTIME_DIR=/run/user/1000 -e DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus <c> \
  bash -lc 'gnome-text-editor & echo started pid $!'
```
Record the pid.

- [ ] **Step 2: Enumerate the current virtual monitors + connectors**

```bash
docker exec -u 1000 -e XDG_RUNTIME_DIR=/run/user/1000 -e DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus <c> \
  gdbus call --session --dest org.gnome.Mutter.DisplayConfig \
  --object-path /org/gnome/Mutter/DisplayConfig \
  --method org.gnome.Mutter.DisplayConfig.GetCurrentState
```
Note the serial (leading `uint32`) and each monitor's connector name (`Meta-0`, `Meta-1`, …) and available modes. Record the exact text shape so Task 3.2 can parse connectors.

- [ ] **Step 3: Add a virtual monitor live via a throwaway ScreenCast RecordVirtual**

Write a tiny Rust or Python probe (or reuse the daemon's zbus proxies interactively) that, on the **same** ScreenCast session the daemon holds, calls `RecordVirtual` with a new size (e.g. 1280×720). Because the daemon owns the live session, the cleanest probe is to add a temporary `--spike-add <WxH>` subcommand to `rmng-clone-daemon` that connects to the session bus, creates a ScreenCast session, RecordVirtuals the size, Starts, waits, then exits — OR observe whether the daemon's existing session accepts a second RecordVirtual after Start. The decision to record: **does `RecordVirtual` succeed after `Session.Start()`?** (Y/N). GNOME 48 (Ubuntu 26.04) is expected Y — this is how gnome-remote-desktop adds RDP monitors.

- [ ] **Step 4: Confirm the probe app never closed**

```bash
docker exec <c> pgrep -a gnome-text-editor
```
Expected: the pid from Step 1 is still alive. This is the load-bearing property.

- [ ] **Step 5: Prove Stream.Stop removes a virtual monitor and the PipeWire node vanishes**

Call `org.gnome.Mutter.ScreenCast.Stream.Stop` on one stream path; re-run GetCurrentState (Step 2). Record: does the connector disappear? Does the app relocate to a remaining monitor (still alive)?

- [ ] **Step 6: Record whether capture ends when a node vanishes**

With the daemon running (`journalctl --user -u rmng-clone-daemon` inside the clone, or `docker logs`), stop a stream and observe whether `capture_pw::run` for that node logs an exit / error. Record: **does raw-PW capture self-terminate on node destroy, or is an explicit shutdown flag needed?** (This decides Task 3.3's capture-stop mechanism.)

- [ ] **Step 7: Write findings to `/tmp/spike/mutter-reconfig.md`**

Record answers to: (a) RecordVirtual-after-Start works?; (b) Stream.Stop removes the monitor?; (c) apps survive add+remove?; (d) capture_pw self-terminates on node destroy?; (e) the exact `GetCurrentState` text shape for connector parsing; (f) whether `ApplyMonitorsConfig` accepts the new/changed connector set. **If (a) or (b) is N**, STOP and escalate — Approach A needs the design revisited (the user chose "A only", so a negative result is a real blocker to surface, not silently work around).

---

## Phase 1 — Wire types & config foundation

All types live in the `wire` crate and regenerate TS. Do this phase first — every later phase consumes these types.

### Task 1.1: Add the `LayoutPreset` wire type

**Files:**
- Modify: `crates/wire/src/control.rs` (add struct near `MonitorSpec`, ~line 30)
- Test: `crates/wire/src/control.rs` (`#[cfg(test)]` module at bottom)
- Regenerates: `frontend/app/lib/wire/LayoutPreset.ts`

**Interfaces:**
- Produces: `pub struct LayoutPreset { pub name: String, pub monitors: Vec<MonitorSpec> }` (Serialize/Deserialize, camelCase, `TS` export).

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)]` module in `crates/wire/src/control.rs`:
```rust
    #[test]
    fn layout_preset_roundtrip_camelcase() {
        let p = LayoutPreset {
            name: "Dual 1440p".into(),
            monitors: vec![MonitorSpec { width: 2560, height: 1440, x: 0, y: 0, primary: true }],
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["name"], "Dual 1440p");
        assert_eq!(v["monitors"][0]["width"], 2560);
        let back: LayoutPreset = serde_json::from_value(v).unwrap();
        assert_eq!(back, p);
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p wire layout_preset_roundtrip_camelcase`
Expected: FAIL — `cannot find type LayoutPreset`.

- [ ] **Step 3: Add the type**

Insert after the `MonitorSpec` struct (after line ~30) in `crates/wire/src/control.rs`:
```rust
/// A named monitor-layout preset: a full arrangement the operator can switch to.
/// Distinct from clone-provisioning `Preset` (env/Linear) — this is display geometry only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "../../../frontend/app/lib/wire/")]
pub struct LayoutPreset {
    pub name: String,
    pub monitors: Vec<MonitorSpec>,
}
```

- [ ] **Step 4: Run the test**

Run: `cargo test -p wire layout_preset_roundtrip_camelcase`
Expected: PASS. Confirm `frontend/app/lib/wire/LayoutPreset.ts` was written (ts-rs emits on test run).

- [ ] **Step 5: Commit**

```bash
git add crates/wire/src/control.rs frontend/app/lib/wire/LayoutPreset.ts
git commit -m "feat(wire): add LayoutPreset type"
```

### Task 1.2: AppConfig — supersede `monitors` with `layout_presets` + `active_layout`

**Files:**
- Modify: `crates/wire/src/config.rs:271-441` (`AppConfig`, `Default`, `effective_monitors`, `redacted`, `AppConfigRedacted`)
- Test: `crates/wire/src/config.rs` (`#[cfg(test)]`)
- Regenerates: `frontend/app/lib/wire/AppConfigRedacted.ts`

**Interfaces:**
- Consumes: `LayoutPreset` (Task 1.1).
- Produces: `AppConfig.layout_presets: Vec<LayoutPreset>`, `AppConfig.active_layout: String`; `AppConfig::effective_monitors() -> Vec<MonitorSpec>` now resolves the active preset. **The legacy `monitors` field is KEPT (unused, transitional) this task so control-server keeps compiling; it is removed in Task 6.4 after all readers migrate.**

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)]` module in `crates/wire/src/config.rs`:
```rust
    #[test]
    fn effective_monitors_from_active_preset() {
        let mut c = AppConfig::default();
        c.layout_presets = vec![
            LayoutPreset { name: "A".into(), monitors: vec![
                MonitorSpec { width: 1920, height: 1080, x: 0, y: 0, primary: true }] },
            LayoutPreset { name: "B".into(), monitors: vec![
                MonitorSpec { width: 3840, height: 2160, x: 0, y: 0, primary: true }] },
        ];
        c.active_layout = "B".into();
        assert_eq!(c.effective_monitors(), c.layout_presets[1].monitors);
    }

    #[test]
    fn effective_monitors_defaults_when_empty() {
        // No presets → dual-1440p default (unchanged behavior).
        let c = AppConfig::default();
        assert_eq!(c.effective_monitors().len(), 2);
        assert!(c.effective_monitors()[0].primary);
    }

    #[test]
    fn effective_monitors_falls_back_to_first_when_active_missing() {
        let mut c = AppConfig::default();
        c.layout_presets = vec![LayoutPreset {
            name: "Only".into(),
            monitors: vec![MonitorSpec { width: 1280, height: 720, x: 0, y: 0, primary: true }],
        }];
        c.active_layout = "Nonexistent".into();
        assert_eq!(c.effective_monitors(), c.layout_presets[0].monitors);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p wire effective_monitors`
Expected: FAIL — `no field layout_presets` / `active_layout`.

- [ ] **Step 3: Edit AppConfig struct**

In `crates/wire/src/config.rs`, **keep** the existing `monitors` field (line 299-300) and mark it transitional, then add the two new fields immediately after it:
```rust
    /// DEPRECATED (transitional): superseded by `layout_presets` + `active_layout`. Still
    /// read from legacy config.json for the one-shot migration (Task 1.5); removed in Task 6.4.
    #[serde(default)]
    pub monitors: Vec<MonitorSpec>,
    /// Named monitor-layout presets. The operator switches the active one from the
    /// sidebar (`POST /api/layout/activate`); the active preset drives `effective_monitors()`.
    #[serde(default)]
    pub layout_presets: Vec<LayoutPreset>,
    /// Name of the active layout preset (the fleet-wide live layout).
    #[serde(default)]
    pub active_layout: String,
```
Add the import at the top of the file if not present (find the existing `use crate::control::{...}` line and add `LayoutPreset`):
```rust
use crate::control::{LayoutPreset, MonitorSpec};
```
(If the existing import already lists `MonitorSpec`, just add `LayoutPreset` to the braces.)

- [ ] **Step 4: Update `Default for AppConfig`**

In the `impl Default for AppConfig` block, keep `monitors: Vec::new(),` and add after it:
```rust
            layout_presets: Vec::new(),
            active_layout: String::new(),
```

- [ ] **Step 5: Rewrite `effective_monitors`**

Replace the body of `effective_monitors` (lines 385-394):
```rust
    /// The active preset's monitors. Falls back to the first preset, then to a dual
    /// 2560×1440 side-by-side default (primary on the right) when no presets exist.
    pub fn effective_monitors(&self) -> Vec<MonitorSpec> {
        if let Some(p) = self.layout_presets.iter().find(|p| p.name == self.active_layout) {
            return p.monitors.clone();
        }
        if let Some(p) = self.layout_presets.first() {
            return p.monitors.clone();
        }
        vec![
            MonitorSpec { width: 2560, height: 1440, x: 2560, y: 0, primary: true },
            MonitorSpec { width: 2560, height: 1440, x: 0, y: 0, primary: false },
        ]
    }
```

- [ ] **Step 6: Update `redacted()` and `AppConfigRedacted`**

In `redacted()` (line 405), keep `monitors: self.monitors.clone(),` and add after it:
```rust
            layout_presets: self.layout_presets.clone(),
            active_layout: self.active_layout.clone(),
```
In `struct AppConfigRedacted` (line 431), keep `pub monitors: Vec<MonitorSpec>,` and add after it:
```rust
    pub layout_presets: Vec<LayoutPreset>,
    pub active_layout: String,
```

- [ ] **Step 7: Run the tests + fix the `monitors_csv` tests**

Run: `cargo test -p wire effective_monitors`
Expected: PASS (3 tests). Now `cargo build -p control-server` — `monitors_csv` reads `effective_monitors()` which now reads presets, so the two existing tests that set `AppConfig.monitors` fail. Update them in `crates/control-server/src/provision.rs`:
- `cfg_with_monitors` (line ~831): set `c.layout_presets = vec![LayoutPreset { name: "T".into(), monitors: mons }]; c.active_layout = "T".into();` instead of `c.monitors = mons;`.
- `monitors_csv_format` / `monitors_csv_falls_back_to_default` keep their assertions (the CSV output is unchanged; only the setup path changes).
Run: `cargo test -p control-server monitors_csv` → PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/wire/src/config.rs frontend/app/lib/wire/AppConfigRedacted.ts frontend/app/lib/wire/LayoutPreset.ts
git commit -m "feat(wire): AppConfig layout_presets + active_layout, effective_monitors from active"
```

### Task 1.3: Add `ServerMsg::SetMonitors`

**Files:**
- Modify: `crates/wire/src/socket.rs:145-185` (`ServerMsg` enum) + imports
- Test: `crates/wire/src/socket.rs` (`#[cfg(test)]`)

**Interfaces:**
- Produces: `ServerMsg::SetMonitors { monitors: Vec<MonitorSpec> }` — server→daemon "apply this layout live".

- [ ] **Step 1: Write the failing test**

Add to `crates/wire/src/socket.rs` tests (create a `#[cfg(test)] mod tests` if none exists):
```rust
    #[test]
    fn server_msg_set_monitors_tag() {
        use crate::control::MonitorSpec;
        let m = ServerMsg::SetMonitors {
            monitors: vec![MonitorSpec { width: 1920, height: 1080, x: 0, y: 0, primary: true }],
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["t"], "set_monitors");
        assert_eq!(v["monitors"][0]["width"], 1920);
        let back: ServerMsg = serde_json::from_value(v).unwrap();
        assert_eq!(back, m);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p wire server_msg_set_monitors_tag`
Expected: FAIL — `no variant SetMonitors`.

- [ ] **Step 3: Add the variant**

In `crates/wire/src/socket.rs`, add to the `ServerMsg` enum (after `Input(InputMsg),`):
```rust
    /// Apply a new monitor layout live (no session restart). The daemon diffs against
    /// its current virtual monitors and adds/stops/recreates only the changed ones.
    SetMonitors { monitors: Vec<crate::control::MonitorSpec> },
```

- [ ] **Step 4: Run the test**

Run: `cargo test -p wire server_msg_set_monitors_tag`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/wire/src/socket.rs
git commit -m "feat(wire): ServerMsg::SetMonitors variant"
```

### Task 1.4: ControlState — mirror active layout + preset names for the sidebar

**Files:**
- Modify: `crates/wire/src/control.rs:327-335` (`ControlState`)
- Test: `crates/wire/src/control.rs` (extend `controlstate_roundtrip_camelcase`)
- Regenerates: `frontend/app/lib/wire/ControlState.ts`

**Interfaces:**
- Produces: `ControlState.active_layout: String`, `ControlState.layout_preset_names: Vec<String>` — the SSE-live values the sidebar switcher renders from.

- [ ] **Step 1: Write the failing test**

Add to `crates/wire/src/control.rs` tests:
```rust
    #[test]
    fn controlstate_layout_fields_camelcase() {
        let st = ControlState {
            active_layout: "Dual 1440p".into(),
            layout_preset_names: vec!["Dual 1440p".into(), "Single 4K".into()],
            ..Default::default()
        };
        let v = serde_json::to_value(&st).unwrap();
        assert_eq!(v["activeLayout"], "Dual 1440p");
        assert_eq!(v["layoutPresetNames"][1], "Single 4K");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p wire controlstate_layout_fields_camelcase`
Expected: FAIL — `no field active_layout`.

- [ ] **Step 3: Add the fields**

In `struct ControlState` (after the `monitors` field, ~line 333) add:
```rust
    /// Name of the active layout preset (mirrored from config so the sidebar switcher
    /// updates live over `/events`). Empty when no presets exist.
    #[serde(default)]
    pub active_layout: String,
    /// Names of all layout presets, in config order — the sidebar's segmented buttons.
    #[serde(default)]
    pub layout_preset_names: Vec<String>,
```

- [ ] **Step 4: Run the test**

Run: `cargo test -p wire controlstate_layout_fields_camelcase`
Expected: PASS. Confirm `frontend/app/lib/wire/ControlState.ts` regenerated with the two fields.

- [ ] **Step 5: Commit**

```bash
git add crates/wire/src/control.rs frontend/app/lib/wire/ControlState.ts
git commit -m "feat(wire): ControlState active_layout + layout_preset_names (sidebar SSE)"
```

### Task 1.5: Config migration — legacy `monitors` → "Default" preset

**Files:**
- Modify: `crates/control-server/src/config.rs` (`migrate_legacy`, ~lines 40-110)
- Test: `crates/control-server/src/config.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: `AppConfig.layout_presets` / `active_layout` (Task 1.2). The `migrate_legacy(raw: &serde_json::Value, cfg: &mut AppConfig) -> bool` fn already exists (returns true if it changed anything, triggering a one-time re-save).

- [ ] **Step 1: Write the failing test**

Add to `crates/control-server/src/config.rs` tests:
```rust
    #[test]
    fn migrates_legacy_monitors_into_default_preset() {
        // Simulate an old config.json with a top-level `monitors` array and no presets.
        let raw: serde_json::Value = serde_json::json!({
            "monitors": [
                { "width": 3440, "height": 1440, "x": 0, "y": 0, "primary": true }
            ]
        });
        let mut cfg = AppConfig::default(); // layout_presets empty, active_layout ""
        let changed = migrate_legacy(&raw, &mut cfg);
        assert!(changed);
        assert_eq!(cfg.layout_presets.len(), 1);
        assert_eq!(cfg.layout_presets[0].name, "Default");
        assert_eq!(cfg.layout_presets[0].monitors[0].width, 3440);
        assert_eq!(cfg.active_layout, "Default");
    }

    #[test]
    fn migration_noop_when_presets_present() {
        let raw: serde_json::Value = serde_json::json!({ "monitors": [] });
        let mut cfg = AppConfig::default();
        cfg.layout_presets = vec![wire::LayoutPreset {
            name: "X".into(),
            monitors: vec![wire::MonitorSpec { width: 800, height: 600, x: 0, y: 0, primary: true }],
        }];
        cfg.active_layout = "X".into();
        // Migration must not clobber an already-migrated config.
        let _ = migrate_legacy(&raw, &mut cfg);
        assert_eq!(cfg.layout_presets.len(), 1);
        assert_eq!(cfg.layout_presets[0].name, "X");
    }
```
(Adjust the `wire::LayoutPreset` / `wire::MonitorSpec` paths to match how `config.rs` imports `wire` types — grep the file's existing `use wire::` lines.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p control-server migrates_legacy_monitors`
Expected: FAIL — migration doesn't create the preset yet.

- [ ] **Step 3: Add the migration branch**

Inside `migrate_legacy` (before its final `return changed;`), add:
```rust
    // Legacy single `monitors` array → a "Default" layout preset (one-shot). Only when
    // the new `layout_presets` is still empty (don't clobber an already-migrated config).
    if cfg.layout_presets.is_empty() {
        if let Some(mons) = raw.get("monitors").and_then(|m| m.as_array()) {
            if !mons.is_empty() {
                if let Ok(parsed) = serde_json::from_value::<Vec<wire::MonitorSpec>>(
                    serde_json::Value::Array(mons.clone()),
                ) {
                    cfg.layout_presets = vec![wire::LayoutPreset { name: "Default".into(), monitors: parsed }];
                    if cfg.active_layout.is_empty() {
                        cfg.active_layout = "Default".into();
                    }
                    changed = true;
                }
            }
        }
    }
```
(`changed` is the fn's existing accumulator boolean — confirm its name by reading the fn head; rename if the local is called something else.)

- [ ] **Step 4: Run the tests**

Run: `cargo test -p control-server migrat`
Expected: PASS (both migration tests).

- [ ] **Step 5: Commit**

```bash
git add crates/control-server/src/config.rs
git commit -m "feat(config): migrate legacy monitors into a Default layout preset"
```

### Task 1.6: `merge_update` — keep `active_layout` consistent after preset edits

**Files:**
- Modify: `crates/control-server/src/config.rs:458-474` (`merge_update`)
- Test: `crates/control-server/src/config.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: `merge_update(base, incoming) -> Result<AppConfig>`. After a settings PUT that changes `layout_presets`, `active_layout` must still name an existing preset (else point to the first).

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn merge_reconciles_active_layout_when_active_preset_removed() {
        let mut base = AppConfig::default();
        base.layout_presets = vec![
            wire::LayoutPreset { name: "A".into(), monitors: vec![ms(1920,1080)] },
            wire::LayoutPreset { name: "B".into(), monitors: vec![ms(3840,2160)] },
        ];
        base.active_layout = "B".into();
        // The UI removes preset "B", sending only "A".
        let incoming = serde_json::json!({
            "layoutPresets": [ { "name": "A", "monitors": [
                { "width": 1920, "height": 1080, "x": 0, "y": 0, "primary": true } ] } ]
        });
        let merged = merge_update(&base, incoming).unwrap();
        assert_eq!(merged.layout_presets.len(), 1);
        assert_eq!(merged.active_layout, "A"); // reconciled off the removed "B"
    }
```
Add a small helper near the tests if not present:
```rust
    fn ms(w: u32, h: u32) -> wire::MonitorSpec { wire::MonitorSpec { width: w, height: h, x: 0, y: 0, primary: true } }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p control-server merge_reconciles_active_layout`
Expected: FAIL — `active_layout` stays `"B"`.

- [ ] **Step 3: Reconcile in `merge_update`**

In `merge_update`, after `let mut merged: AppConfig = serde_json::from_value(cur)?;` and after the presets merge block, before `enforce_categories`, add:
```rust
    // Keep active_layout valid after preset edits: if it no longer names a preset,
    // point it at the first (or clear it when there are none).
    if !merged.layout_presets.iter().any(|p| p.name == merged.active_layout) {
        merged.active_layout =
            merged.layout_presets.first().map(|p| p.name.clone()).unwrap_or_default();
    }
```
Note: generic `deep_merge` replaces the `layout_presets` array wholesale (the UI always sends the complete list), so no special-case merge like `presets` is needed.

- [ ] **Step 4: Run the test**

Run: `cargo test -p control-server merge_reconciles_active_layout`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/control-server/src/config.rs
git commit -m "feat(config): reconcile active_layout after layout-preset edits"
```

---

## Phase 2 — Control-server control plane

Adds the fleet-wide dispatch, the activate endpoint, the encoder prune, and the ControlState mirror. Consumes Phase 1 types.

### Task 2.1: `MediaHandle::set_monitors_all` + push `SetMonitors` on `Hello`

**Files:**
- Modify: `crates/control-server/src/mediaplane.rs:159-178` (add method to `impl MediaHandle`)
- Modify: `crates/control-server/src/mediaplane.rs:672-676` (the `DaemonMsg::Hello` arm in `serve_clone`)

**Interfaces:**
- Consumes: `ServerMsg::SetMonitors` (Task 1.3), `Conn::send` (existing), `AppConfig::effective_monitors` (Task 1.2).
- Produces: `MediaHandle::set_monitors_all(&self, monitors: &[MonitorSpec]) -> Vec<(String, Result<(), String>)>`.

- [ ] **Step 1: Add the broadcast method**

In `impl MediaHandle` (after `send_input`, ~line 166) add:
```rust
    /// Push a live layout to **every** connected clone-daemon. Best-effort; returns a
    /// per-clone result so the caller can report partial failures. Cheap: `Conn::send`
    /// is a single non-blocking `sendmsg`.
    pub fn set_monitors_all(
        &self,
        monitors: &[wire::MonitorSpec],
    ) -> Vec<(String, Result<(), String>)> {
        let conns: Vec<(String, std::sync::Arc<Conn>)> =
            self.conns.lock().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        conns
            .into_iter()
            .map(|(id, c)| {
                let r = c
                    .send(&ServerMsg::SetMonitors { monitors: monitors.to_vec() })
                    .map_err(|e| e.to_string());
                (id, r)
            })
            .collect()
    }
```
(Add `use wire::MonitorSpec;` to the `use wire::{...}` group at the top if not already imported — the file already imports `MonitorPlacement, ServerMsg` from `wire`.)

- [ ] **Step 2: Push the active layout on Hello**

In `serve_clone`, the `DaemonMsg::Hello` arm currently reads:
```rust
            Ok((DaemonMsg::Hello(h), _)) => {
                tracing::info!("clone-daemon '{}' connected", h.clone_id);
                handle.conns.lock().unwrap().insert(h.clone_id.clone(), conn.clone());
                clone_id = Some(h.clone_id);
            }
```
Replace its body with:
```rust
            Ok((DaemonMsg::Hello(h), _)) => {
                tracing::info!("clone-daemon '{}' connected", h.clone_id);
                handle.conns.lock().unwrap().insert(h.clone_id.clone(), conn.clone());
                // Correct a clone that booted with a stale baked RMNG_MONITORS: push the
                // current active layout so it live-reconfigures to match the fleet.
                let mons = app.config().effective_monitors();
                if let Err(e) = conn.send(&ServerMsg::SetMonitors { monitors: mons }) {
                    tracing::warn!("SetMonitors on Hello for '{}' failed: {e}", h.clone_id);
                }
                clone_id = Some(h.clone_id);
            }
```

- [ ] **Step 3: Build**

Run: `cargo build -p control-server`
Expected: compiles. (No unit test here — exercised by the integration/E2E phases; `Conn` I/O isn't unit-testable without a socket.)

- [ ] **Step 4: Commit**

```bash
git add crates/control-server/src/mediaplane.rs
git commit -m "feat(mediaplane): set_monitors_all + push active layout on daemon Hello"
```

### Task 2.2: `POST /api/layout/activate`

**Files:**
- Modify: `crates/control-server/src/web.rs:45-80` (route table) + a new handler + req struct
- Test: (behavioral — covered by Phase 7 E2E; add a name-validation unit test if a router test harness exists, else skip)

**Interfaces:**
- Consumes: `config::merge_update`/`config::save` (or direct field set), `app.media.set_monitors_all` (Task 2.1), `app.store.mutate`, `mirror_layout_to_state` (Task 2.4).
- Produces: `POST /api/layout/activate` body `{ "name": string }` → `200 { ok, applied: string[], errors: string[] }`.

- [ ] **Step 1: Add the route**

In `router(...)`, after the `/api/monitors/apply` line (which Task 6.1 later removes), add:
```rust
        .route("/api/layout/activate", post(layout_activate))
```

- [ ] **Step 2: Add the request struct + handler**

Near the other handlers in `web.rs`:
```rust
#[derive(Deserialize)]
struct LayoutActivateReq {
    name: String,
}

/// `POST /api/layout/activate` — make `name` the active layout preset and live-apply it
/// to every running clone (no session restart). Persists config, mirrors the active
/// name into ControlState (so all sidebars update over SSE), then pushes `SetMonitors`
/// to each daemon. Best-effort per clone; partial failures are reported.
async fn layout_activate(
    State(app): State<App>,
    Json(req): Json<LayoutActivateReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // 1. Validate + persist the active_layout.
    let mut cfg = app.config();
    if !cfg.layout_presets.iter().any(|p| p.name == req.name) {
        return Err((StatusCode::BAD_REQUEST, format!("unknown layout preset '{}'", req.name)));
    }
    cfg.active_layout = req.name.clone();
    crate::config::save(&cfg).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    *app.cfg.write().unwrap() = cfg.clone();

    // 2. Mirror into ControlState for the sidebar (SSE broadcast).
    crate::web::mirror_layout_to_state(&app);

    // 3. Live-apply to all running clones.
    let monitors = cfg.effective_monitors();
    let results = app.media.set_monitors_all(&monitors);
    let mut applied = Vec::new();
    let mut errors = Vec::new();
    for (id, r) in results {
        match r {
            Ok(()) => applied.push(id),
            Err(e) => errors.push(format!("{id}: {e}")),
        }
    }
    Ok(Json(serde_json::json!({ "ok": true, "applied": applied, "errors": errors })))
}
```
(If `mirror_layout_to_state` is defined in `web.rs` itself, call it unqualified as `mirror_layout_to_state(&app)`.)

- [ ] **Step 3: Build**

Run: `cargo build -p control-server` (will fail until Task 2.4 defines `mirror_layout_to_state` — do Task 2.4 next, then return here to build). Expected after 2.4: compiles.

- [ ] **Step 4: Commit** (after Task 2.4 lands so it compiles)

```bash
git add crates/control-server/src/web.rs
git commit -m "feat(web): POST /api/layout/activate — fleet-wide live layout switch"
```

### Task 2.3: Prune encoders when the selected clone's monitor set shrinks

**Files:**
- Modify: `crates/control-server/src/mediaplane.rs:730-737` (the `DaemonMsg::Layout` arm in `serve_clone`)

**Interfaces:**
- Consumes: `Encoders` map (existing). New/resized monitors are handled lazily by `encoder_for`; this task handles **removal**.

- [ ] **Step 1: Extend the Layout arm**

The current `DaemonMsg::Layout` arm:
```rust
            Ok((DaemonMsg::Layout { monitors: l }, _)) => {
                if let Some(id) = clone_id.clone() {
                    handle.layout.lock().unwrap().insert(id.clone(), l.clone());
                    if app.store.selected().as_deref() == Some(id.as_str()) {
                        broadcast_json(&viewers, T_LAYOUT, &l);
                    }
                }
            }
```
Replace with:
```rust
            Ok((DaemonMsg::Layout { monitors: l }, _)) => {
                if let Some(id) = clone_id.clone() {
                    handle.layout.lock().unwrap().insert(id.clone(), l.clone());
                    if app.store.selected().as_deref() == Some(id.as_str()) {
                        // Drop encoders for monitors that no longer exist on the selected
                        // clone (added/resized ones are (re)built lazily by encoder_for on
                        // the next frame). Prevents stale encoders lingering after a switch.
                        let live: std::collections::HashSet<u32> = l.iter().map(|m| m.id).collect();
                        encoders.lock().unwrap().retain(|mid, _| live.contains(mid));
                        broadcast_json(&viewers, T_LAYOUT, &l);
                    }
                }
            }
```

- [ ] **Step 2: Build**

Run: `cargo build -p control-server`
Expected: compiles.

- [ ] **Step 3: Commit**

```bash
git add crates/control-server/src/mediaplane.rs
git commit -m "feat(mediaplane): prune stale encoders on selected clone's layout change"
```

### Task 2.4: Mirror active layout + preset names into ControlState (boot, config PUT, activate)

**Files:**
- Modify: `crates/control-server/src/web.rs` (add `pub(crate) fn mirror_layout_to_state`; call from `config_put`)
- Modify: `crates/control-server/src/main.rs` (or wherever `App` is built / server starts — call once at boot after config load)

**Interfaces:**
- Consumes: `app.config()`, `app.store.mutate`.
- Produces: `pub(crate) fn mirror_layout_to_state(app: &App)` — copies `config.active_layout` + preset names into `ControlState`.

- [ ] **Step 1: Add the mirror helper**

In `web.rs`:
```rust
/// Copy the config's active layout + preset names into ControlState so the sidebar
/// switcher renders + highlights over the live `/events` SSE. Idempotent; call after any
/// change to `layout_presets` / `active_layout` and once at boot.
pub(crate) fn mirror_layout_to_state(app: &App) {
    let cfg = app.config();
    let active = cfg.active_layout.clone();
    let names: Vec<String> = cfg.layout_presets.iter().map(|p| p.name.clone()).collect();
    app.store.mutate(|s| {
        s.active_layout = active.clone();
        s.layout_preset_names = names.clone();
    });
}
```

- [ ] **Step 2: Call it from `config_put`**

In `config_put`, after `*app.cfg.write().unwrap() = merged.clone();` (near the end, before building the response), add:
```rust
    // Keep the sidebar's live layout list/active marker in sync with the just-saved presets.
    mirror_layout_to_state(&app);
```

- [ ] **Step 3: Seed at boot**

Find where the server builds `App` and starts (grep `mediaplane::spawn(` or `router(` in `main.rs`). After `App` is constructed and before/after `mediaplane::spawn(app.clone())`, add a one-time seed:
```rust
    crate::web::mirror_layout_to_state(&app);
```
(This makes ControlState carry the presets immediately on a fresh boot, before any activate.)

- [ ] **Step 4: Build + re-build Task 2.2**

Run: `cargo build -p control-server`
Expected: compiles (this defines the symbol Task 2.2 referenced). Commit Task 2.2 now if not yet committed.

- [ ] **Step 5: Commit**

```bash
git add crates/control-server/src/web.rs crates/control-server/src/main.rs
git commit -m "feat(web): mirror active layout + preset names into ControlState (SSE)"
```

### Task 2.5: New clones bake the active layout; drop the per-clone apply_monitors call

**Files:**
- Modify: `crates/control-server/src/jobs.rs` (~line 353 — the `if !app.config().monitors.is_empty()` block that calls `apply_monitors`)

**Interfaces:**
- New clones already bake `RMNG_MONITORS` from `monitors_csv()` → `effective_monitors()` (now the active preset) via the template's build ARG only if provisioning writes it; the live correction comes from the `Hello` push (Task 2.1). The restart-based `apply_monitors` call must go.

- [ ] **Step 1: Remove the apply_monitors call**

Delete the whole post-create block in `jobs.rs` that reads `if !app.config().monitors.is_empty() { ... apply_monitors ... }` (it restarts the desktop — the no-app-loss violation). New clones get the correct layout via the daemon's `Hello` → server `SetMonitors` (Task 2.1); the baked `RMNG_MONITORS` default is only a pre-connect placeholder. (`provision::apply_monitors` itself is removed in Task 6.1; do this task before 6.1 so nothing calls a deleted fn.)

- [ ] **Step 2: Build**

Run: `cargo build -p control-server`
Expected: compiles.

- [ ] **Step 3: Commit**

```bash
git add crates/control-server/src/jobs.rs
git commit -m "refactor(jobs): drop restart-based apply_monitors on clone create (live push instead)"
```

---

## Phase 3 — Clone-daemon live reconfiguration (Approach A)

The heart of the feature. Uses Phase 0 findings. Restructures `run_shipping` so the Mutter `Session` and per-monitor capture are owned by a reconfigurable controller reachable by a control channel.

### Task 3.1: Make the Mutter `Session` reconfigurable (hold proxies; add stream helpers)

**Files:**
- Modify: `crates/clone-daemon/src/mutter.rs:84-190` (add `Stream.Stop`; store ScreenCast session proxy + per-stream proxies in `Session`; add `add_monitor` / `stop_monitor`)
- Test: none (D-Bus I/O; validated by Phase 0 spike + E2E)

**Interfaces:**
- Consumes: existing `ScreenCastSessionProxy`, `ScreenCastStreamProxy`, `VirtualMonitor`.
- Produces:
  - `Session` gains `sc: ScreenCastSessionProxy<'static>`, `conn` (already present), and per-monitor a stored `stream_path`.
  - `Session::add_monitor(&mut self, monitor_id: u32, w: u32, h: u32) -> Result<VirtualMonitor>`.
  - `Session::stop_monitor(&mut self, monitor_id: u32) -> Result<()>` (calls `Stream.Stop`, removes from `self.monitors`).

- [ ] **Step 1: Add `Stop` to the Stream proxy**

In the `ScreenCastStream` proxy trait (line 107-110), add:
```rust
    fn stop(&self) -> zbus::Result<()>;
```

- [ ] **Step 2: Store the ScreenCast session proxy in `Session`**

Change `struct Session` (line 124-128) to:
```rust
pub struct Session {
    pub conn: zbus::Connection,
    pub rd: RemoteDesktopSessionProxy<'static>,
    /// Kept alive so we can RecordVirtual more monitors live (add) after Start.
    pub sc: ScreenCastSessionProxy<'static>,
    pub cursor_mode: u32,
    pub monitors: Vec<VirtualMonitor>,
}
```
In `setup_with_cursor_mode`, keep `sc_session` (currently local) and put it into the returned `Session`, and record `cursor_mode`:
```rust
    Ok(Session { conn, rd: rd_session, sc: sc_session, cursor_mode, monitors })
```

- [ ] **Step 3: Add `add_monitor`**

Add to `impl Session` (create the impl block if none exists):
```rust
impl Session {
    /// RecordVirtual a new virtual monitor on the live session and resolve its PipeWire
    /// node id. Appends to `self.monitors`. (Mutter accepts RecordVirtual after Start —
    /// validated in the Phase 0 spike; this is how gnome-remote-desktop adds RDP monitors.)
    pub async fn add_monitor(&mut self, monitor_id: u32, w: u32, h: u32) -> Result<VirtualMonitor> {
        let mut props: HashMap<&str, Value<'_>> = HashMap::new();
        props.insert("cursor-mode", Value::from(self.cursor_mode));
        props.insert("is-platform", Value::from(true));
        props.insert("modes", Value::new(build_modes(w, h)));
        let stream_path = self.sc.record_virtual(props).await.context("RecordVirtual (add)")?;
        let stream = ScreenCastStreamProxy::builder(&self.conn).path(stream_path.clone())?.build().await?;
        let mut added = stream.receive_pipe_wire_stream_added().await?;
        let sig = added.next().await.context("PipeWireStreamAdded (add)")?;
        let node_id = sig.args().context("PipeWireStreamAdded args")?.node_id;
        let vm = VirtualMonitor { monitor_id, stream_path: stream_path.to_string(), node_id, width: w, height: h };
        self.monitors.push(vm.clone());
        tracing::info!(monitor_id, node_id, w, h, "virtual monitor added live");
        Ok(vm)
    }

    /// Stop one virtual monitor's stream (removing the Mutter output) and forget it.
    pub async fn stop_monitor(&mut self, monitor_id: u32) -> Result<()> {
        if let Some(pos) = self.monitors.iter().position(|m| m.monitor_id == monitor_id) {
            let vm = self.monitors.remove(pos);
            let stream = ScreenCastStreamProxy::builder(&self.conn).path(vm.stream_path.clone())?.build().await?;
            stream.stop().await.context("Stream.Stop")?;
            tracing::info!(monitor_id, node_id = vm.node_id, "virtual monitor stopped live");
        }
        Ok(())
    }
}
```
Add `#[derive(Clone)]` to `VirtualMonitor` if not already (extraction shows it already derives `Clone`). Ensure `use futures::StreamExt;` is in scope (it is, per line 10).

- [ ] **Step 4: Update all `Session` constructors**

`setup_with_cursor_mode` is the only constructor. Ensure the returned struct includes `sc` and `cursor_mode` (Step 2). Build:
Run: `cargo build -p clone-daemon`
Expected: compiles (the `run_capture_test` + MCP paths that read `session.rd`/`session.monitors` are unaffected by the added fields).

- [ ] **Step 5: Commit**

```bash
git add crates/clone-daemon/src/mutter.rs
git commit -m "feat(daemon/mutter): reconfigurable Session — add/stop virtual monitors live"
```

### Task 3.2: Read real connector names from `GetCurrentState` for `apply_layout`

**Files:**
- Modify: `crates/clone-daemon/src/main.rs:234-286` (`apply_layout`, `display_config_serial`)
- Test: `crates/clone-daemon/src/main.rs` (`#[cfg(test)]` — parse a captured GetCurrentState blob)

**Interfaces:**
- Produces: `fn parse_connectors(get_current_state_stdout: &str) -> Vec<(String /*connector*/, u32 /*w*/, u32 /*h*/)>` and an updated `apply_layout` that maps desired monitors to real connector names (by matching current mode `WxH`), instead of hard-coding `Meta-<i>`.

- [ ] **Step 1: Paste a real GetCurrentState blob from the spike**

From Phase 0 Step 2, save the exact `gdbus ... GetCurrentState` stdout to use as the test fixture. (The structure: a serial `uint32`, then an array of monitors each with a connector string and a list of modes with `WxH@rate`.)

- [ ] **Step 2: Write the failing test**

```rust
    #[test]
    fn parses_connectors_and_current_modes() {
        // Fixture captured from `gdbus ... GetCurrentState` on a 2-monitor clone (Phase 0).
        let blob = r#"<PASTE THE REAL BLOB FROM THE SPIKE HERE>"#;
        let conns = parse_connectors(blob);
        assert_eq!(conns.len(), 2);
        assert_eq!(conns[0].0, "Meta-0");
        assert!(conns.iter().any(|(_, w, h)| *w == 2560 && *h == 1440));
    }
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p clone-daemon parses_connectors`
Expected: FAIL — `parse_connectors` undefined.

- [ ] **Step 4: Implement `parse_connectors` + use it in `apply_layout`**

Add `parse_connectors` (a text parser over the gdbus GVariant text — the exact regex/split derives from the spike blob; match each monitor's connector name to its current mode `WxH`). Then change `apply_layout` to:
1. read the full `GetCurrentState` stdout (not just the serial),
2. build a `Vec<(connector, w, h)>` from it,
3. for each desired `MonitorCfg`, pick the connector whose `(w,h)` matches (consuming matched connectors so duplicates map 1:1 in order),
4. emit the `ApplyMonitorsConfig` logical-monitor array using those real connector names + `'{w}x{h}@60.000'` mode ids.

Keep `apply_layout` best-effort (log + continue on failure), as today. Refactor `display_config_serial` to share the single `GetCurrentState` call (return `(serial, stdout)` or add a `get_current_state() -> Option<(u32, String)>` helper).

- [ ] **Step 5: Run the test**

Run: `cargo test -p clone-daemon parses_connectors`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/clone-daemon/src/main.rs
git commit -m "feat(daemon): map layout to real Mutter connectors via GetCurrentState"
```

### Task 3.3: Reconfigurable capture controller + the diff

**Files:**
- Modify: `crates/clone-daemon/src/main.rs:290-456` (`run_shipping` — own `Session` + capture handles; replace the final `pending()` with a control loop)
- Modify: `crates/clone-daemon/src/main.rs:501-546` (`spawn_pw_monitor` — return a stop handle)
- Test: `crates/clone-daemon/src/main.rs` (`#[cfg(test)]` — pure diff function)

**Interfaces:**
- Consumes: `Session::add_monitor` / `stop_monitor` (3.1), `apply_layout` (3.2), `parse_monitors`/`MonitorCfg`.
- Produces:
  - `fn diff_monitors(current: &[VirtualMonitor], desired: &[MonitorCfg]) -> MonitorDiff` where `struct MonitorDiff { keep: Vec<(u32 /*slot*/, u32 /*existing node_id*/)>, add: Vec<(u32 /*slot*/, u32 /*w*/, u32 /*h*/)>, stop: Vec<u32 /*monitor_id to stop*/> }`.
  - A `tokio::sync::mpsc` channel carrying `Vec<MonitorCfg>` from the reader thread to the controller.
  - `spawn_pw_monitor(...) -> Arc<AtomicBool>` (a `stop` flag the capture loop checks; set to stop it — belt-and-suspenders even if node-vanish already ends `capture_pw::run`, per spike finding 0.1(d)).

- [ ] **Step 1: Write the failing diff test**

```rust
    #[test]
    fn diff_keeps_same_size_adds_new_stops_surplus() {
        let cur = vec![
            vm(0, 100, 1920, 1080),
            vm(1, 101, 2560, 1440),
        ];
        // Desired: slot 0 same (1920x1080 reused), slot 1 resized to 3840x2160 (add),
        // and the old 2560x1440 stream is surplus (stop).
        let desired = vec![mc(1920, 1080), mc(3840, 2160)];
        let d = diff_monitors(&cur, &desired);
        assert_eq!(d.keep, vec![(0, 100)]);        // slot 0 reuses node 100
        assert_eq!(d.add, vec![(1, 3840, 2160)]);  // slot 1 needs a new stream
        assert_eq!(d.stop, vec![1]);               // old monitor_id 1 (2560x1440) stops
    }
```
Add helpers near the tests:
```rust
    fn vm(id: u32, node: u32, w: u32, h: u32) -> mutter::VirtualMonitor {
        mutter::VirtualMonitor { monitor_id: id, stream_path: String::new(), node_id: node, width: w, height: h }
    }
    fn mc(w: u32, h: u32) -> MonitorCfg { MonitorCfg { w, h, x: 0, y: 0, primary: false } }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p clone-daemon diff_keeps_same_size`
Expected: FAIL — `diff_monitors` undefined.

- [ ] **Step 3: Implement `diff_monitors`**

A greedy match by `WxH`: iterate desired slots; for each, find an unused current monitor with the same `(w,h)` → `keep(slot, node_id)`; else `add(slot, w, h)`. Any current monitor never matched → `stop(monitor_id)`.
```rust
struct MonitorDiff {
    /// (desired slot index, reused existing node_id)
    keep: Vec<(u32, u32)>,
    /// (desired slot index, width, height) — needs a fresh RecordVirtual
    add: Vec<(u32, u32, u32)>,
    /// monitor_id of a current stream to Stop (surplus / resized-away)
    stop: Vec<u32>,
}

fn diff_monitors(current: &[mutter::VirtualMonitor], desired: &[MonitorCfg]) -> MonitorDiff {
    let mut used = vec![false; current.len()];
    let mut keep = Vec::new();
    let mut add = Vec::new();
    for (slot, d) in desired.iter().enumerate() {
        let slot = slot as u32;
        if let Some(i) = current.iter().enumerate().position(|(i, c)| {
            !used[i] && c.width == d.w && c.height == d.h
        }) {
            used[i] = true;
            keep.push((slot, current[i].node_id));
        } else {
            add.push((slot, d.w, d.h));
        }
    }
    let stop = current.iter().enumerate().filter(|(i, _)| !used[*i]).map(|(_, c)| c.monitor_id).collect();
    MonitorDiff { keep, add, stop }
}
```

- [ ] **Step 4: Run the diff test**

Run: `cargo test -p clone-daemon diff_keeps_same_size`
Expected: PASS.

- [ ] **Step 5: Give `spawn_pw_monitor` a stop handle**

Change its signature to return `Arc<AtomicBool>` and have `on_frame` early-return + the loop break when the flag is set. Minimal change: create `let stop = Arc::new(AtomicBool::new(false));` clone it into the thread; inside `on_frame`, `if stop.load(Ordering::Relaxed) { return; }`; after `capture_pw::run(...)` returns (it returns when the node vanishes per spike), the thread ends. Return `stop` so the controller can also proactively signal it. (If Phase 0 finding (d) says capture self-terminates reliably, the flag is just belt-and-suspenders; keep it — it also lets us stop a reused-but-now-removed node deterministically.)

- [ ] **Step 6: Restructure `run_shipping` into a controller loop**

Refactor so `run_shipping` keeps `session: Session`, a `capture: HashMap<u32 /*monitor_id*/, CaptureHandle>` (GStreamer `Pipeline` for embedded, or `Arc<AtomicBool>` stop-flag for raw-PW), and the current `Vec<MonitorCfg>`. Replace the terminal `futures::future::pending().await` with:
```rust
    // Control loop: apply live monitor reconfigurations pushed by the server.
    let mut current_cfg = monitors.to_vec();
    while let Some(desired) = reconfig_rx.recv().await {
        if let Err(e) = reconfigure(&mut session, &mut capture, &transport, &latest, &in_flight, embedded, &current_cfg, &desired).await {
            tracing::warn!("reconfigure failed: {e:#}");
            continue;
        }
        current_cfg = desired;
    }
```
Add `async fn reconfigure(...)` that:
1. `let d = diff_monitors(&session.monitors, &desired);`
2. for each `stop` id: signal/stop its capture handle, `session.stop_monitor(id).await`, remove from `in_flight`/`capture`.
3. for each `add(slot, w, h)`: `let vm = session.add_monitor(slot, w, h).await?;` create its `in_flight` gate + spawn capture (embedded → `capture::start_capture`, raw-PW → `spawn_pw_monitor`) and store the handle in `capture`.
4. Re-key: the daemon must renumber `monitor_id` = slot for every monitor (kept + added) so the viewer's windows track slots. Since `keep` reuses an existing stream but its slot may differ, set each kept `VirtualMonitor.monitor_id` to its new slot and re-tag its capture (the capture closure captures `mid` — for kept-but-renumbered monitors, stop+respawn the capture with the new mid, OR carry slot separately). Simplest correct approach: **stop and respawn capture for any monitor whose slot changed**, so the `mid` passed to `ship_frame` always equals the slot. In practice most switches change sizes/counts (already churned) and slots stay aligned; handle the reorder case by respawning.
5. `apply_layout(&desired).await;` (positions/primary via real connectors — Task 3.2).
6. Re-send the layout: build `Vec<MonitorPlacement>` from `desired` (id = slot) and `transport.send(&DaemonMsg::Layout { monitors })`.

Factor the per-monitor capture-spawn (embedded vs raw-PW) used at startup (lines 405-425) into a helper `spawn_capture_for(mon, embedded, transport, latest, gate) -> CaptureHandle` and reuse it in both startup and `reconfigure`.

- [ ] **Step 7: Wire the reader thread → controller channel**

Add a `tokio::sync::mpsc::channel::<Vec<MonitorCfg>>(4)`; move `reconfig_rx` into the control loop (Step 6) and clone `reconfig_tx` into the `ServerMsg` reader thread (Task 3.4 adds the arm). Because the reader is a `std::thread` and the sender is a tokio mpsc, use `reconfig_tx.blocking_send(...)` there.

- [ ] **Step 8: Build**

Run: `cargo build -p clone-daemon`
Expected: compiles. Run `cargo test -p clone-daemon` — the diff + connector tests pass.

- [ ] **Step 9: Commit**

```bash
git add crates/clone-daemon/src/main.rs
git commit -m "feat(daemon): live monitor reconfigure controller (diff + add/stop streams)"
```

### Task 3.4: `SetMonitors` arm in the daemon's ServerMsg reader

**Files:**
- Modify: `crates/clone-daemon/src/main.rs:363-398` (the `ServerMsg` reader thread)

**Interfaces:**
- Consumes: `reconfig_tx` (Task 3.3), `ServerMsg::SetMonitors` (Task 1.3).

- [ ] **Step 1: Add the arm**

In the reader `match transport.recv()`, replace the catch-all `Ok(_) => {}` with:
```rust
                    Ok(ServerMsg::SetMonitors { monitors }) => {
                        // Convert wire MonitorSpec → daemon MonitorCfg and hand to the
                        // async reconfigure controller. Guarantees exactly one primary.
                        let mut mons: Vec<MonitorCfg> = monitors
                            .iter()
                            .map(|m| MonitorCfg {
                                w: m.width, h: m.height, x: m.x as i32, y: m.y as i32, primary: m.primary,
                            })
                            .collect();
                        if mons.is_empty() {
                            mons.push(MonitorCfg { w: 1920, h: 1080, x: 0, y: 0, primary: true });
                        }
                        if !mons.iter().any(|m| m.primary) {
                            mons[0].primary = true;
                        }
                        if reconfig_tx.blocking_send(mons).is_err() {
                            tracing::warn!("reconfigure channel closed; ignoring SetMonitors");
                        }
                    }
                    Ok(_) => {} // Subscribe/FrameRequest — not used by the daemon
```
(Note `MonitorSpec.x/y` are `u32`; cast to `i32` for `MonitorCfg`.)

- [ ] **Step 2: Build**

Run: `cargo build -p clone-daemon`
Expected: compiles.

- [ ] **Step 3: Commit**

```bash
git add crates/clone-daemon/src/main.rs
git commit -m "feat(daemon): handle ServerMsg::SetMonitors → live reconfigure"
```

### Task 3.5: Refresh the daemon MCP's monitor snapshot after reconfigure

**Files:**
- Modify: `crates/clone-daemon/src/main.rs:436-448` + `crates/clone-daemon/src/mcp.rs:73` (share live monitors)

**Interfaces:**
- The MCP task holds a `session.monitors.clone()` snapshot (screenshot/geometry). After a reconfigure it goes stale. Make it read a shared `Arc<Mutex<Vec<VirtualMonitor>>>` the controller updates.

- [ ] **Step 1: Introduce a shared monitors handle**

Create `let live_monitors = Arc::new(Mutex::new(session.monitors.clone()));` before spawning capture. The `reconfigure` fn updates it at the end (`*live_monitors.lock().unwrap() = session.monitors.clone();`). Pass `live_monitors.clone()` to `mcp::serve` instead of the `&mons` snapshot; update `mcp::serve`'s signature to take `Arc<Mutex<Vec<VirtualMonitor>>>` and lock it per request.

- [ ] **Step 2: Build**

Run: `cargo build -p clone-daemon`
Expected: compiles.

- [ ] **Step 3: Commit**

```bash
git add crates/clone-daemon/src/main.rs crates/clone-daemon/src/mcp.rs
git commit -m "feat(daemon): MCP reads live monitor set after reconfigure"
```

---

## Phase 4 — Viewer live reflow

The viewer already lazily builds a window per `monitor_id` and reads tag-3 layout. Resize is handled by the decoder automatically (caps carry no dimensions; `vah264dec` renegotiates from the SPS at the new IDR and the `Picture` auto-fits via `intrinsic_*`). This phase adds **removal** and **main-window robustness**.

### Task 4.1: Store the window + pipeline handles per monitor

**Files:**
- Modify: `crates/viewer/src/main.rs:388-405` (`MonitorWindow`)
- Modify: `crates/viewer/src/main.rs:928-976` (`make_decoder` — return the pipeline instead of leaking it)
- Modify: `crates/viewer/src/main.rs` (`make_monitor_window` — store the `ApplicationWindow` + pipeline)

**Interfaces:**
- Produces: `MonitorWindow` gains `window: gtk4::ApplicationWindow` and `pipeline: gst::Pipeline`; `make_decoder` returns `(AppSrc, gdk::Paintable, gst::Pipeline)`.

- [ ] **Step 1: Return the pipeline from `make_decoder`**

In `make_decoder` (and the `make_decoder_yuv444` twin), replace `std::mem::forget(pipeline); Ok((appsrc, paintable))` with `Ok((appsrc, paintable, pipeline))` and change the return type to `Result<(AppSrc, gdk::Paintable, gst::Pipeline)>`. Update the caller (`make_monitor_window`) to receive the pipeline.

- [ ] **Step 2: Store handles on `MonitorWindow`**

Add to `struct MonitorWindow`:
```rust
    /// The window itself, so a reconfigure can destroy it when its monitor is removed.
    window: gtk4::ApplicationWindow,
    /// The decode pipeline, stopped (not leaked) on window teardown.
    pipeline: gst::Pipeline,
```
In `make_monitor_window`, put the built `window.clone()` and `pipeline` into the returned `MonitorWindow`.

- [ ] **Step 3: Build**

Run: `cargo build -p viewer`
Expected: compiles (a window is now retained by `MonitorWindow`, in addition to GTK's list — fine).

- [ ] **Step 4: Commit**

```bash
git add crates/viewer/src/main.rs
git commit -m "feat(viewer): retain window + pipeline handles per monitor"
```

### Task 4.2: Reconcile windows on layout change (destroy removed monitors)

**Files:**
- Modify: `crates/viewer/src/main.rs:499-559` (the 8 ms tick's `reported`-reading block)

**Interfaces:**
- Consumes: `reported: ReportedLayout`, `windows: Windows`, `srcs: VideoSrcs`. Runs on the GTK main thread (windows are `!Send`).

- [ ] **Step 1: Add the reconcile in the tick**

In the tick, in the block that reads `reported` (after it refreshes `layout`), add a reconcile that destroys windows whose `monitor_id` is no longer present:
```rust
            // Reconcile windows against the reported layout: destroy any monitor window
            // whose id vanished (a preset with fewer monitors). New ids are still built
            // lazily on their first AU; resized ids keep their window (the decoder
            // renegotiates from the new SPS). Runs on the GTK main thread.
            {
                let rep = reported.lock().unwrap();
                if !rep.is_empty() {
                    let live: std::collections::HashSet<u32> = rep.iter().map(|m| m.id).collect();
                    let mut w = windows.borrow_mut();
                    let mut srcs = srcs.lock().unwrap();
                    let gone: Vec<u32> = w.keys().copied().filter(|id| !live.contains(id)).collect();
                    for id in gone {
                        if let Some(win) = w.remove(&id) {
                            let _ = win.pipeline.set_state(gst::State::Null);
                            win.window.destroy();
                            srcs.remove(&id);
                        }
                    }
                }
            }
```
Place this **after** the existing `layout` refresh block so `layout` (drag routing) already reflects the new set. Guard against destroying the last window / the main window — handled in Task 4.3.

- [ ] **Step 2: Build**

Run: `cargo build -p viewer`
Expected: compiles.

- [ ] **Step 3: Commit**

```bash
git add crates/viewer/src/main.rs
git commit -m "feat(viewer): destroy monitor windows removed from the layout"
```

### Task 4.3: Main-window (close/Settings) survives reconfigure

**Files:**
- Modify: `crates/viewer/src/main.rs:499-535` (main-window designation), `:673-753` (deletable/settings/close closures)

**Interfaces:**
- Guarantee: exactly one live window always carries the close button + Settings, and it is never destroyed by Task 4.2.

- [ ] **Step 1: Never destroy the main window in reconcile**

Track the main window's `monitor_id` (the primary at build time). In Task 4.2's `gone` loop, skip the main id even if it's absent from the new layout — instead, if the primary monitor is being removed, **re-designate**: pick the lowest surviving `monitor_id` as the new main and move the close/Settings affordance there. Minimal robust rule for v1: **keep the main window alive and repurpose it** — if its slot disappears, reassign it to render the lowest surviving id (its appsrc/pipeline get swapped to that id's stream). If that is too invasive, the acceptable v1 fallback is: main window = the lowest monitor_id currently present; ensure the lowest id's window has `deletable(true)` + the Settings button by building those on **every** window but only *showing* them on the current-lowest (toggle visibility in the tick).

Choose the visibility-toggle approach for v1 (simplest, avoids stream swapping):
- Build the close button (`deletable`) and Settings button on **every** monitor window (not just primary).
- In `connect_close_request`, quit only if this window is the current lowest-id present; otherwise `Stop`.
- In the tick, after reconcile, compute `main_id = min(windows.keys())` and set each window's headerbar Settings button + `set_deletable` so only `main_id`'s window shows them.

- [ ] **Step 2: Replace the frozen `primary` gates**

Where `deletable(primary)`, the `if primary { settings … }`, and the close closure use the captured `primary` bool, replace with a shared `Rc<Cell<u32>> main_id` the tick updates, and have each window compare its own `mid` to `main_id.get()`.

- [ ] **Step 3: Build + manual sanity**

Run: `cargo build -p viewer`
Expected: compiles. (Behavioral verification is in Phase 7 E2E — remove the primary monitor and confirm the viewer keeps a working close/Settings control.)

- [ ] **Step 4: Commit**

```bash
git add crates/viewer/src/main.rs
git commit -m "feat(viewer): keep close/Settings affordance across layout reconfigure"
```

---

## Phase 5 — Frontend (layout-presets editor + segmented sidebar switcher)

### Task 5.1: API client — `activateLayout`; drop `applyMonitors`

**Files:**
- Modify: `frontend/app/lib/api.ts:148-165`

**Interfaces:**
- Produces: `export const activateLayout = (name: string) => postJson("/api/layout/activate", { name }) as Promise<{ ok: boolean; applied: string[]; errors: string[] }>`.
- Removes: `applyMonitors` (route deleted in Phase 6).

- [ ] **Step 1: Replace `applyMonitors`**

Delete the `applyMonitors` export and add:
```ts
/** Make `name` the active layout preset and live-apply it to all running clones. */
export const activateLayout = (name: string) =>
  postJson("/api/layout/activate", { name }) as Promise<{
    ok: boolean;
    applied: string[];
    errors: string[];
  }>;
```

- [ ] **Step 2: Commit**

```bash
git add frontend/app/lib/api.ts
git commit -m "feat(frontend/api): activateLayout; drop applyMonitors"
```
(TypeScript won't type-check standalone yet — `applyMonitors` is still referenced in SettingsPanel/_index until Tasks 5.2/5.4. Land those before running `bun run build`.)

### Task 5.2: Settings — replace the Monitors section with a Layout Presets editor

**Files:**
- Modify: `frontend/app/components/SettingsPanel.tsx` (props, state, load, save, the Monitors `<Section>`, remove `applyNow`)

**Interfaces:**
- Consumes: `AppConfigRedacted.layoutPresets` / `.activeLayout` (generated by Task 1.2), `MonitorsEditor`, `putConfig`.
- Produces: a `layoutPresets` array in the config `save()` patch.

- [ ] **Step 1: Remove monitor-apply plumbing from props/state**

Delete `applyMonitors` from `SettingsPanelProps`, and delete `applying`/`applyMsg` state + the whole `applyNow()` function (lines 426-456).

- [ ] **Step 2: Replace `monitors` state with `layoutPresets` state**

Replace the `const [monitors, setMonitors] = useState<Mon[]>([]);` line with:
```tsx
  const [layoutPresets, setLayoutPresets] = useState<{ name: string; monitors: Mon[] }[]>([]);
```
Add editor helpers (mirroring the clone-`presets` pattern at lines 298-316):
```tsx
  const addLayoutPreset = () =>
    setLayoutPresets((ps) => [
      ...ps,
      { name: "", monitors: [{ width: 1920, height: 1080, x: 0, y: 0, primary: true }] },
    ]);
  const rmLayoutPreset = (i: number) => setLayoutPresets((ps) => ps.filter((_, j) => j !== i));
  const setLayoutPresetName = (i: number, name: string) =>
    setLayoutPresets((ps) => ps.map((p, j) => (j === i ? { ...p, name } : p)));
  const setLayoutPresetMonitors = (i: number, mons: Mon[]) =>
    setLayoutPresets((ps) => ps.map((p, j) => (j === i ? { ...p, monitors: mons } : p)));
```

- [ ] **Step 3: Seed from config in `load()`**

In `load(c)`, replace the `setMonitors(...)` block with:
```tsx
    setLayoutPresets(
      c.layoutPresets.length
        ? c.layoutPresets.map((p) => ({ name: p.name, monitors: p.monitors.map((m) => ({ ...m })) }))
        : [{ name: "Default", monitors: [{ width: 1920, height: 1080, x: 0, y: 0, primary: true }] }],
    );
```

- [ ] **Step 4: Emit `layoutPresets` in `save()`**

In `save()`'s `patch`, replace the `monitors: monitors.map(...)` field with:
```tsx
        layoutPresets: layoutPresets
          .filter((p) => p.name.trim())
          .map((p) => ({
            name: p.name.trim(),
            monitors: p.monitors.map((m) => ({
              width: Math.max(1, m.width),
              height: Math.max(1, m.height),
              x: Math.max(0, m.x),
              y: Math.max(0, m.y),
              primary: m.primary,
            })),
          })),
```

- [ ] **Step 5: Replace the Monitors `<Section>` render (lines 505-518)**

```tsx
            {/* Layout presets — named monitor arrangements; switch the active one from
                the sidebar. Each preset uses the same editor as before. */}
            <Section
              title="Layout presets"
              effect="immediate"
              hint="Named monitor arrangements. Switch the active preset from the sidebar — running clones reconfigure live without closing apps."
            >
              <div className="space-y-3">
                {layoutPresets.length === 0 ? (
                  <p className="text-xs text-slate-400 dark:text-slate-500">No layout presets.</p>
                ) : null}
                {layoutPresets.map((p, i) => (
                  <div key={i} className="rounded border border-slate-200 p-3 dark:border-slate-700">
                    <div className="mb-2 flex items-center gap-2">
                      <input
                        className={input}
                        placeholder="preset name (e.g. Dual 1440p)"
                        value={p.name}
                        onChange={(e) => setLayoutPresetName(i, e.target.value)}
                      />
                      <button
                        type="button"
                        onClick={() => rmLayoutPreset(i)}
                        className="rounded px-2 py-1 text-xs text-slate-500 hover:bg-slate-100 dark:text-slate-400 dark:hover:bg-slate-800"
                      >
                        Remove
                      </button>
                    </div>
                    <MonitorsEditor
                      monitors={p.monitors}
                      onChange={(mons) => setLayoutPresetMonitors(i, mons)}
                    />
                  </div>
                ))}
                <button
                  type="button"
                  onClick={addLayoutPreset}
                  className="rounded border border-slate-300 px-2 py-1 text-xs text-slate-600 hover:bg-slate-50 dark:border-slate-600 dark:text-slate-300 dark:hover:bg-slate-800"
                >
                  + Add layout preset
                </button>
              </div>
            </Section>
```
(`MonitorsEditor` is now called **without** `onApply`/`applying`/`applyMsg` — its apply block is removed in Task 6.3.)

- [ ] **Step 6: Commit** (compiles after Task 5.4 removes the prop at the call site)

```bash
git add frontend/app/components/SettingsPanel.tsx
git commit -m "feat(settings): layout-presets editor replaces the single Monitors section"
```

### Task 5.3: Sidebar — segmented layout switcher

**Files:**
- Modify: `frontend/app/components/Sidebar.tsx` (props + render under the `rmng control` header)

**Interfaces:**
- Consumes: `presetNames: string[]`, `activeLayout: string`, `onActivateLayout: (name: string) => void` (new props).

- [ ] **Step 1: Add props**

Add to `SidebarProps`:
```tsx
  /** Layout preset names (config order) — the segmented switcher buttons. */
  presetNames: string[];
  /** The active preset name (highlighted). */
  activeLayout: string;
  /** Activate a layout preset (live-applies to all running clones). */
  onActivateLayout: (name: string) => void;
```
Destructure them in the component signature.

- [ ] **Step 2: Render the switcher**

Immediately after the `rmng control` header `</div>` (the block at lines 111-124), add:
```tsx
      {presetNames.length > 0 ? (
        <div className="px-1">
          <div className="mb-1 text-[11px] font-semibold uppercase tracking-wide text-slate-400 dark:text-slate-500">
            Layout
          </div>
          <div className="flex flex-wrap gap-1">
            {presetNames.map((name) => {
              const active = name === activeLayout;
              return (
                <button
                  key={name}
                  type="button"
                  onClick={() => onActivateLayout(name)}
                  aria-pressed={active}
                  className={`rounded px-2 py-1 text-xs font-medium ${
                    active
                      ? "bg-emerald-600 text-white"
                      : "border border-slate-300 text-slate-600 hover:bg-slate-100 dark:border-slate-600 dark:text-slate-300 dark:hover:bg-slate-800"
                  }`}
                >
                  {name}
                </button>
              );
            })}
          </div>
        </div>
      ) : null}
```

- [ ] **Step 3: Commit**

```bash
git add frontend/app/components/Sidebar.tsx
git commit -m "feat(sidebar): segmented layout-preset switcher"
```

### Task 5.4: Wire it up in the route (state types + render props)

**Files:**
- Modify: `frontend/app/lib/types.ts` (hand-written `ControlState` — add the two fields)
- Modify: `frontend/app/routes/_index.tsx` (pass props to `<Sidebar>` + `<SettingsPanel>`)

**Interfaces:**
- Consumes: `activateLayout` (Task 5.1), `state.activeLayout`, `state.layoutPresetNames`, `cfg` (already fetched).

- [ ] **Step 1: Add fields to the hand-written ControlState**

In `frontend/app/lib/types.ts`, the `ControlState` type (lines ~174-181) — add:
```ts
  activeLayout: string;
  layoutPresetNames: string[];
```
If `emptyState` is defined in the same file, add `activeLayout: "", layoutPresetNames: []` to it.

- [ ] **Step 2: Pass props to `<Sidebar>`**

In `_index.tsx` at the `<Sidebar>` render (lines 295-325), add:
```tsx
          presetNames={state.layoutPresetNames ?? []}
          activeLayout={state.activeLayout ?? ""}
          onActivateLayout={(name) => run(activateLayout(name))}
```
Import `activateLayout` from `~/lib/api` (add to the existing api import block, lines 16-37), and remove the `applyMonitors` import.

- [ ] **Step 3: Remove `applyMonitors` from `<SettingsPanel>`**

In the `<SettingsPanel>` render (lines 403-427), delete the `applyMonitors={applyMonitors}` prop line.

- [ ] **Step 4: Build the frontend**

Run: `cd frontend && bun run build`
Expected: type-checks + builds with no errors. If `bun` is not on PATH, use the absolute path per the deploy notes.

- [ ] **Step 5: Commit**

```bash
git add frontend/app/lib/types.ts frontend/app/routes/_index.tsx
git commit -m "feat(frontend): wire layout switcher + presets editor into the dashboard"
```

---

## Phase 6 — Remove the restart-based path & update docs

### Task 6.1: Delete `apply-monitors.sh`, `provision::apply_monitors`, and `/api/monitors/apply`

**Files:**
- Delete: `crates/control-server/scripts/apply-monitors.sh`
- Modify: `crates/control-server/src/provision.rs` (remove `APPLY_MONITORS_SCRIPT` const + `apply_monitors` fn; keep `monitors_csv`)
- Modify: `crates/control-server/src/web.rs` (remove the `/api/monitors/apply` route + `monitors_apply` handler)

- [ ] **Step 1: Remove the route + handler**

Delete `.route("/api/monitors/apply", post(monitors_apply))` and the entire `monitors_apply` async fn (lines 619-641).

- [ ] **Step 2: Remove provision pieces**

Delete `const APPLY_MONITORS_SCRIPT` (line 31) and the `apply_monitors` fn (lines 756-782) from `provision.rs`. Keep `monitors_csv` (still used to bake `RMNG_MONITORS` defaults at provision).

- [ ] **Step 3: Delete the script**

```bash
git rm crates/control-server/scripts/apply-monitors.sh
```

- [ ] **Step 4: Build**

Run: `cargo build -p control-server`
Expected: compiles. `rg apply_monitors crates/ frontend/` returns nothing.

- [ ] **Step 5: Commit**

```bash
git add -A crates/control-server
git commit -m "refactor: remove restart-based apply-monitors path (superseded by live switch)"
```

### Task 6.2: Remove the `onApply` block from `MonitorsEditor`

**Files:**
- Modify: `frontend/app/components/MonitorsEditor.tsx:69-172`

**Interfaces:**
- `MonitorsEditor` becomes purely `{ monitors, onChange }`. Drop `onApply`/`applying`/`applyMsg` props + the apply `<div>` (lines 153-169).

- [ ] **Step 1: Trim the props + apply block**

Remove `onApply?`, `applying`, `applyMsg` from the component's props type and signature, and delete the trailing `{onApply ? (...) : null}` block (lines 153-169).

- [ ] **Step 2: Build**

Run: `cd frontend && bun run build`
Expected: builds (no remaining `onApply` references — Task 5.2 already dropped them at the call site).

- [ ] **Step 3: Commit**

```bash
git add frontend/app/components/MonitorsEditor.tsx
git commit -m "refactor(MonitorsEditor): drop apply-to-running-clones block"
```

### Task 6.3: Update protocol/API/dev docs

**Files:**
- Modify: `docs/PROTOCOL.md` (ServerMsg table: add `set_monitors`; config schema: `monitors` → `layout_presets` + `active_layout`; RMNG_MONITORS note)
- Modify: `docs/API.md` (remove `/api/monitors/apply`; add `POST /api/layout/activate`; config field change)
- Modify: `docs/DEVELOPMENT.md` (if it documents the monitors-apply flow)

- [ ] **Step 1: Edit the docs**

- `docs/PROTOCOL.md`: in the `ServerMsg` list (~line 90), add `set_monitors {monitors: MonitorSpec[]}` (live layout). In the config schema table (~line 141), replace the `monitors` row with `layout_presets` (`LayoutPreset[]`) + `active_layout` (string, the active preset). Note that `RMNG_MONITORS` is now only a boot default, corrected by the server's `SetMonitors` on `Hello`.
- `docs/API.md`: remove the `POST /api/monitors/apply` row + section; add `POST /api/layout/activate` — body `{ name }` → `{ ok, applied, errors }`.

- [ ] **Step 2: Commit**

```bash
git add docs/PROTOCOL.md docs/API.md docs/DEVELOPMENT.md
git commit -m "docs: document layout presets, /api/layout/activate, ServerMsg::SetMonitors"
```

### Task 6.4: Remove the transitional `monitors` field

**Files:**
- Modify: `crates/wire/src/config.rs` (drop `AppConfig.monitors` + `AppConfigRedacted.monitors` + the `redacted()` line + `Default` line)
- Modify: `crates/control-server/src/config.rs` (migration reads `raw["monitors"]`, not the struct field — unaffected; confirm)
- Regenerates: `frontend/app/lib/wire/AppConfigRedacted.ts`

**Interfaces:**
- By now every reader uses `layout_presets`/`effective_monitors()`; the legacy field is dead and safe to delete. Migration (Task 1.5) reads the legacy value from raw JSON, so removing the struct field does not break it.

- [ ] **Step 1: Remove the field**

Delete `pub monitors: Vec<MonitorSpec>,` from `AppConfig` and `AppConfigRedacted`, the `monitors: Vec::new(),` line from `Default`, and `monitors: self.monitors.clone(),` from `redacted()`.

- [ ] **Step 2: Build + test**

Run: `cargo build --workspace && cargo test -p wire && cargo test -p control-server`
Expected: clean. `rg '\.monitors\b' crates/wire crates/control-server` shows only `ControlState.monitors` (the unrelated dead SSE field, left as-is) and test/migration references to `raw["monitors"]`.

- [ ] **Step 3: Commit**

```bash
git add crates/wire/src/config.rs frontend/app/lib/wire/AppConfigRedacted.ts
git commit -m "refactor(config): remove transitional AppConfig.monitors field"
```

### Task 6.5: Full workspace build + test gate

- [ ] **Step 1: Build everything**

Run: `cargo build --workspace`
Expected: clean build.

- [ ] **Step 2: Test everything**

Run: `cargo test --workspace`
Expected: all green (Phase 1/3 unit tests + existing tests). Fix any regressions before proceeding.

- [ ] **Step 3: Frontend build**

Run: `cd frontend && bun run build`
Expected: clean.

- [ ] **Step 4: Commit any fixes**

```bash
git add -A && git commit -m "chore: workspace build + test green for layout presets" || echo "nothing to commit"
```

---

## Phase 7 — Proxmox LXC deployment + E2E headless-viewer test

Deploys the branch build to a fresh CT and validates the whole chain end-to-end. Uses the **Docker product model** (control-server container; clone-daemon injected from the image; stock template). Host: `root@10.0.0.100` (pegaswarm). This dev box (`10.0.0.187`, Intel iHD VA-API) runs the headless viewer against `CT:9001`.

### Task 7.1: Create + provision the CT

**Files:** none (infrastructure)

- [ ] **Step 1: Pick the next free CTID + create the CT**

```bash
CTID=$(ssh root@10.0.0.100 'pvesh get /cluster/nextid')
echo "using CTID=$CTID"
ssh root@10.0.0.100 "pct create $CTID local:vztmpl/ubuntu-26.04-standard_26.04-1_amd64.tar.zst \
  --hostname rmng-layout-e2e --cores 8 --memory 16384 --swap 8192 \
  --rootfs local-lvm:40 --net0 name=eth0,bridge=vmbr0,ip=dhcp \
  --features nesting=1,keyctl=1,fuse=1 --unprivileged 1 --onboot 0"
```

- [ ] **Step 2: Add the node-side conf (render node, AppArmor, mounts)** per PROXMOX-LXC.md §1

```bash
ssh root@10.0.0.100 "cat >> /etc/pve/lxc/$CTID.conf <<'EOF'
dev0: /dev/dri/renderD128,mode=0666
lxc.apparmor.profile: unconfined
lxc.mount.entry: /dev/null sys/module/apparmor/parameters/enabled none bind,optional 0 0
lxc.mount.auto: cgroup:mixed proc:rw sys:mixed
EOF"
ssh root@10.0.0.100 "pct start $CTID && sleep 5 && pct exec $CTID -- ip -4 addr show eth0 | grep inet"
```
Record the CT's DHCP IP as `CT_IP`. (Host keyring sysctls §1b are already raised on 10.0.0.100 — verified; `/dev/dri/renderD128` exists.)

- [ ] **Step 3: Install Docker in the CT** per §2

```bash
ssh root@10.0.0.100 "pct exec $CTID -- bash -lc 'apt-get update && apt-get install -y curl ca-certificates && curl -fsSL https://get.docker.com | sh'"
ssh root@10.0.0.100 "pct exec $CTID -- bash -lc 'docker info | grep -i \"storage driver\"; ls -l /dev/dri/renderD128; docker run --rm hello-world | head -3'"
```
Expected: storage driver `overlay2`/`overlayfs` (NOT vfs), the render node present, hello-world runs. If vfs, recheck `features: nesting=1` + CT restart.

### Task 7.2: Build the branch image and load it into the CT

- [ ] **Step 1: Build the control-server image locally from the branch**

```bash
cd /home/pegasis/Projects/RMNG
docker build -t rmng:layout-e2e .
```
Expected: multi-stage build succeeds (compiles `clone-daemon` + `control-server`, bundles the frontend, produces the image carrying our modified clone-daemon at `/usr/local/share/rmng/clone-daemon`).

- [ ] **Step 2: Ship the image into the CT**

```bash
docker save rmng:layout-e2e | ssh root@10.0.0.100 "pct exec $CTID -- docker load"
ssh root@10.0.0.100 "pct exec $CTID -- docker images | grep rmng"
```

- [ ] **Step 3: Run the control-server container**

```bash
ssh root@10.0.0.100 "pct exec $CTID -- docker run -d --name rmng --privileged --init --pid host --restart unless-stopped \
  -v /var/run/docker.sock:/var/run/docker.sock \
  -v rmng-data:/data -v rmng-sock:/srv/rmng-sock \
  -p 9000-9003:9000-9003 -p 9005:9005 -p 445:445 rmng:layout-e2e"
sleep 5
curl -s -o /dev/null -w "web http=%{http_code}\n" http://$CT_IP:9000/
```
Expected: `web http=200`. (Use `$CT_IP` from Task 7.1.)

### Task 7.3: Run setup, create layout presets, create a clone, open an app

- [ ] **Step 1: Finish the setup wizard via API**

Confirm env, then pull the stock template and latch setup:
```bash
curl -s http://$CT_IP:9000/api/setup/env | head -c 400; echo
# Pull the stock clone template (binary-less; our clone-daemon is injected from the image).
curl -s -X POST http://$CT_IP:9000/api/images/pull -H 'content-type: application/json' \
  -d '{"reference":"pegasis0/rmng-template:latest"}'
# Wait for the pull op to finish (watch /events or poll /api/images), then finish setup:
curl -s -X PUT http://$CT_IP:9000/api/config -H 'content-type: application/json' \
  -d '{"setupComplete":true,"docker":{"cloneCpus":4,"cloneMemoryMb":8192}}'
```
(`cloneCpus:4` because the CT has 8 cores — a `cloneCpus` above the CT's cores 400s the clone create, per the small-CT gotcha.)

- [ ] **Step 2: Create two single-monitor presets (deterministic E2E dims) + a multi-monitor one**

```bash
curl -s -X PUT http://$CT_IP:9000/api/config -H 'content-type: application/json' -d '{
  "layoutPresets": [
    { "name": "Single 1080p", "monitors": [ {"width":1920,"height":1080,"x":0,"y":0,"primary":true} ] },
    { "name": "Single 4K",    "monitors": [ {"width":3840,"height":2160,"x":0,"y":0,"primary":true} ] },
    { "name": "Dual 1080p",   "monitors": [
        {"width":1920,"height":1080,"x":0,"y":0,"primary":true},
        {"width":1920,"height":1080,"x":1920,"y":0,"primary":false} ] }
  ],
  "activeLayout": "Single 1080p"
}'
curl -s http://$CT_IP:9000/api/config | python3 -c 'import sys,json;c=json.load(sys.stdin);print("presets:",[p["name"] for p in c["layoutPresets"]],"active:",c["activeLayout"])'
```
Expected: three presets listed, active `Single 1080p`.

- [ ] **Step 3: Create a plain clone + wait for it to register**

```bash
curl -s -X POST http://$CT_IP:9000/api/clone -H 'content-type: application/json' \
  -d '{"image":"pegasis0/rmng-template:latest","plain":{"title":"layout-e2e","message":"idle"}}'
# Poll until the clone's host shows up + its daemon connects (state.hosts[].id). Watch /events:
curl -s -N http://$CT_IP:9000/events | head -c 1200
```
Expected: a host appears (id `<hostnamePrefix>layout-e2e`); it becomes selected/streamable once its daemon sends `Hello` (which triggers the server's `SetMonitors` push — the clone comes up already on `Single 1080p`). Note the clone container name from `docker ps`.

- [ ] **Step 4: Open a GUI app on the clone (to prove apps survive)**

```bash
C=<clone-container-name>
ssh root@10.0.0.100 "pct exec $CTID -- docker exec -u 1000 \
  -e XDG_RUNTIME_DIR=/run/user/1000 -e DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus \
  $C bash -lc 'gnome-text-editor & echo pid $!'"
ssh root@10.0.0.100 "pct exec $CTID -- docker exec $C pgrep -a gnome-text-editor"
```
Record the pid — it must still be alive after every switch below.

### Task 7.4: E2E — headless viewer decode + live switch + apps-survive assertions

**Files:**
- Build: `crates/viewer` (local, Intel iHD)

- [ ] **Step 1: Build the viewer locally + activate the clone**

```bash
cd /home/pegasis/Projects/RMNG
cargo build --release -p viewer
# Ensure the clone is the selected/streamed host:
curl -s -X POST http://$CT_IP:9000/api/activate -H 'content-type: application/json' -d '{"id":"<clone-host-id>"}' >/dev/null
```

- [ ] **Step 2: Dump a frame at the current (1080p) layout**

```bash
RMNG_VIDEO=$CT_IP:9001 RMNG_DUMP=/tmp/e2e-1080.png ./target/release/rmng-viewer --headless
python3 -c 'from PIL import Image; print("dims", Image.open("/tmp/e2e-1080.png").size)'
```
Expected: the PNG decodes and prints `dims (1920, 1080)`. (Single-monitor preset ⇒ unambiguous dimensions. If PIL is unavailable, `file /tmp/e2e-1080.png` prints the geometry.)

- [ ] **Step 3: Switch to Single 4K and re-dump**

```bash
curl -s -X POST http://$CT_IP:9000/api/layout/activate -H 'content-type: application/json' -d '{"name":"Single 4K"}'
sleep 3   # allow the daemon to reconfigure + the encoder/decoder to renegotiate
RMNG_VIDEO=$CT_IP:9001 RMNG_DUMP=/tmp/e2e-4k.png ./target/release/rmng-viewer --headless
python3 -c 'from PIL import Image; print("dims", Image.open("/tmp/e2e-4k.png").size)'
```
Expected: `dims (3840, 2160)` — proves the daemon reconfigured the virtual monitor live, the server rebuilt the encoder, and the viewer decoded the new resolution. **End-to-end path validated.**

- [ ] **Step 4: Assert the app never closed**

```bash
ssh root@10.0.0.100 "pct exec $CTID -- docker exec $C pgrep -a gnome-text-editor"
```
Expected: the SAME pid from Task 7.3 Step 4 — the app survived the resolution switch (Global Constraint: no app loss).

- [ ] **Step 5: Switch to Dual 1080p (add a monitor) and verify multi-monitor decode + app survival**

```bash
curl -s -X POST http://$CT_IP:9000/api/layout/activate -H 'content-type: application/json' -d '{"name":"Dual 1080p"}'
sleep 3
# Headless viewer reports per-monitor fps for BOTH monitors; run ~5s and check the log.
RMNG_VIDEO=$CT_IP:9001 timeout 6 ./target/release/rmng-viewer --headless 2>&1 | grep -i "monitor" | head
ssh root@10.0.0.100 "pct exec $CTID -- docker exec $C pgrep -a gnome-text-editor"
curl -s http://$CT_IP:9000/api/config | python3 -c 'import sys,json;print("active:",json.load(sys.stdin)["activeLayout"])'
```
Expected: the headless log shows decoded frames for **two** monitor ids; the app pid is still alive; active layout is `Dual 1080p`.

- [ ] **Step 6: Switch back to Single 1080p (remove a monitor) and confirm clean teardown**

```bash
curl -s -X POST http://$CT_IP:9000/api/layout/activate -H 'content-type: application/json' -d '{"name":"Single 1080p"}'
sleep 3
RMNG_VIDEO=$CT_IP:9001 RMNG_DUMP=/tmp/e2e-back.png ./target/release/rmng-viewer --headless
python3 -c 'from PIL import Image; print("dims", Image.open("/tmp/e2e-back.png").size)'
ssh root@10.0.0.100 "pct exec $CTID -- docker exec $C pgrep -a gnome-text-editor"
```
Expected: `dims (1920, 1080)`; the app still alive. Removing a monitor did not crash the daemon or lose the app.

- [ ] **Step 7 (manual, GUI viewer): confirm window reflow + main-window survival**

On this dev box (or any GTK host on the LAN), run the **windowed** viewer against `$CT_IP:9001`, then repeat the switches from the web UI sidebar. Confirm: windows appear/disappear as monitors are added/removed, the surviving window keeps a working close/Settings control, and no window shows a stale/black frame after a resize. (This validates Phase 4's GTK reconcile, which headless mode does not exercise.)

- [ ] **Step 8: Record results + tear down the CT**

Write pass/fail for each assertion into the PR description. Then clean up the throwaway CT:
```bash
ssh root@10.0.0.100 "pct stop $CTID && pct destroy $CTID"
```
(Skip destroy if you want to keep it for further manual testing — note it in the PR.)

---

## Self-review checklist (completed by the plan author)

- **Spec coverage:** data model (Ph1), control plane + fleet apply (Ph2), daemon Approach-A reconfigure (Ph0 spike + Ph3), server encoder add/drop + viewer reflow (Ph2.3 + Ph4), frontend editor + segmented switcher (Ph5), removal of restart path (Ph6), deploy + E2E (Ph7). All spec sections map to tasks.
- **Approach A commitment:** Phase 0 gates it; a negative spike result is an explicit escalation (no silent fallback), matching the user's "A only" choice.
- **Type consistency:** `LayoutPreset { name, monitors }`, `AppConfig.layout_presets`/`active_layout`, `ServerMsg::SetMonitors { monitors }`, `ControlState.active_layout`/`layout_preset_names`, `MediaHandle::set_monitors_all`, `mirror_layout_to_state`, `activateLayout`, `diff_monitors`/`MonitorDiff{keep,add,stop}` are used identically across every task that references them.
- **Fixtures to fill from the spike:** Task 3.2 Step 1 (real `GetCurrentState` blob) and Task 0.1 findings are the only "paste the captured value" placeholders — inherent to the D-Bus text format and explicitly gathered in Phase 0, not vague TODOs.
