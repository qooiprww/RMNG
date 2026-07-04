# "auto" account rotation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Redefine the `"auto"` Claude/Codex account selection to rotate a clone across *all currently-imported accounts* (like a group whose membership is the full account set), unify auto and named-group rotation onto one "exhausted" threshold, and remove the standalone `auto_swap_on_exhaustion` toggle.

**Architecture:** The existing sticky rotator (`rotate_once` → `assign_rotation`) gains a second bucket: managed clones with `claude_selection == "auto"` and no named group are rotated as one pool whose members are the live `app.claude.emails()`. Group and auto eligibility both switch from the old 90%-5h cap to a single `exhausted` predicate (`5h > 80% OR 7d ≥ 95%`). The one-shot `auto_swap_exhausted` path and its config toggle are deleted, so pinned-to-a-fixed-account clones are never moved. Every change is mirrored symmetrically in `claude.rs` and `codex.rs`.

**Tech Stack:** Rust (control-server crate, wire crate), ts-rs (`#[ts(export)]` generates the frontend TS types), React/TypeScript frontend (bun, react-router, tsc).

## Global Constraints

- **Exhausted predicate (verbatim):** an account is exhausted when `(100.0 - five) < SESSION_HEADROOM_PCT || seven >= SEVEN_DAY_CAP_PCT`, with `SESSION_HEADROOM_PCT = 20.0` and `SEVEN_DAY_CAP_PCT = 95.0`. Equivalently: `5h > 80%` OR `7d ≥ 95%`.
- **Symmetry:** `crates/control-server/src/claude.rs` and `crates/control-server/src/codex.rs` are twins. Codex uses `provider == Some(wire::Provider::Codex)` in usage filters, `app.codex.emails()`, and the `codex_*` host fields (`codex_group`, `codex_selection`, `codex_account_email`) / `codex_groups` config. Claude uses `provider != Some(Codex)`, `app.claude.emails()`, and the `claude_*` fields.
- **Legacy hosts:** a clone rotates in the auto pool only when `claude_selection == "auto"` (Codex: `codex_selection == "auto"`). Hosts with selection `None` (pre-dating the field) are treated as pinned and never moved.
- **Commits:** conventional-commit style, and every commit message MUST end with the trailer `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>` (shown in each commit step below).
- **Build/test commands:** `cargo test -p control-server`, `cargo test -p wire`, `cargo build`, and `cd frontend && bun run typecheck`.

---

### Task 1: Claude — unified `exhausted` threshold + eligibility helpers

**Files:**
- Modify: `crates/control-server/src/claude.rs` (const at :49, helpers near :677-701, tests at :998+)

**Interfaces:**
- Produces (used by Tasks 2): `fn is_exhausted(five: f64, seven: f64) -> bool`, `fn exhausted(app: &App, email: &str) -> bool`, `fn seven_day_pct(app: &App, email: &str) -> f64`, `fn eligible_members(app: &App, members: &[String]) -> Vec<String>`.
- `SESSION_HEADROOM_PCT` changes value `40.0 → 20.0`. `ROTATE_MAX_FIVE_HOUR_PCT` stays (still referenced by `rotate_once`'s log until Task 2).

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `crates/control-server/src/claude.rs` (after the existing `assignment_degrades_with_single_eligible` test, before the closing `}`):

```rust
    #[test]
    fn exhaustion_threshold_is_80_5h_or_95_7d() {
        assert!(!is_exhausted(80.0, 0.0), "exactly 80% 5h is still eligible");
        assert!(is_exhausted(80.1, 0.0), "just over 80% 5h is exhausted");
        assert!(!is_exhausted(0.0, 94.9), "under the 7d cap is eligible");
        assert!(is_exhausted(0.0, 95.0), "hitting the 7d cap is exhausted");
        assert!(!is_exhausted(79.9, 94.9), "both under caps is eligible");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p control-server exhaustion_threshold_is_80_5h_or_95_7d`
Expected: FAIL to compile — `cannot find function is_exhausted in this scope`.

- [ ] **Step 3: Write minimal implementation**

3a. Change the constant at `crates/control-server/src/claude.rs:49`:

```rust
const SESSION_HEADROOM_PCT: f64 = 20.0;
```

3b. Add the new helpers immediately after `five_hour_pct` (currently ends at :688). Insert:

```rust
/// The 7d utilization for `email` from the latest usage view (0 if unknown).
fn seven_day_pct(app: &App, email: &str) -> f64 {
    app.store
        .get()
        .claude_accounts
        .iter()
        .filter(|u| u.provider != Some(wire::Provider::Codex))
        .find(|u| u.email == email)
        .and_then(|u| u.seven_day.as_ref())
        .map(|w| w.pct)
        .unwrap_or(0.0)
}

/// Whether an account is out of usable headroom: 5h over the session cap or 7d at the
/// weekly cap. Pure decision (see [`exhausted`] for the store-backed wrapper).
fn is_exhausted(five: f64, seven: f64) -> bool {
    (100.0 - five) < SESSION_HEADROOM_PCT || seven >= SEVEN_DAY_CAP_PCT
}

/// [`is_exhausted`] against `email`'s latest usage view.
fn exhausted(app: &App, email: &str) -> bool {
    is_exhausted(five_hour_pct(app, email), seven_day_pct(app, email))
}

/// Imported accounts among `members` that aren't exhausted — the usable rotation
/// targets for a pool. (Non-imported members have no token, so they're dropped.)
fn eligible_members(app: &App, members: &[String]) -> Vec<String> {
    let known = app.claude.emails();
    members
        .iter()
        .filter(|email| known.iter().any(|k| &k == email))
        .filter(|email| !exhausted(app, email))
        .cloned()
        .collect()
}
```

3c. Replace the body of `eligible_group_accounts` (currently :692-701) so it delegates to `eligible_members`:

```rust
/// Group members that are imported accounts and not exhausted. Missing usage counts as
/// eligible (0% util).
fn eligible_group_accounts(app: &App, group: &CloneGroup) -> Vec<String> {
    eligible_members(app, &group.accounts)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p control-server exhaustion_threshold_is_80_5h_or_95_7d`
Expected: PASS. No dead-code warnings — the new helpers chain into `eligible_group_accounts`, which `pick_group_account` and `rotate_once` already call.

Then run the full module to confirm nothing regressed: `cargo test -p control-server`
Expected: PASS (existing `assignment_*` tests unaffected; `eligible_group_accounts` output for the same inputs is unchanged except the threshold moved from 90% to the exhausted definition).

- [ ] **Step 5: Commit**

```bash
git add crates/control-server/src/claude.rs
git commit -m "refactor(claude): unify rotation eligibility on the exhausted threshold (80% 5h / 95% 7d)" \
  -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Claude — "auto" as a live group of all imported accounts

**Files:**
- Modify: `crates/control-server/src/claude.rs` (const at :52, `rotate_once` at :771-821, tests at :998+)

**Interfaces:**
- Consumes (Task 1): `exhausted`, `eligible_members`, `AUTO` (the `"auto"` sentinel const at :553).
- Produces: `fn auto_pool_clones(hosts: &[Host]) -> Vec<Host>`, `async fn rotate_pool(app: &App, label: &str, members: &[String], clones: &[Host])`. `rotate_once` now also rotates the auto pool. `ROTATE_MAX_FIVE_HOUR_PCT` is removed.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `crates/control-server/src/claude.rs` (after the test added in Task 1). Note the `Host` literal sets the fields `auto_pool_clones` inspects:

```rust
    fn host_sel(id: &str, managed: bool, group: Option<&str>, sel: Option<&str>) -> Host {
        Host {
            id: id.into(),
            managed,
            claude_group: group.map(str::to_string),
            claude_selection: sel.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn auto_pool_is_only_managed_ungrouped_auto_clones() {
        let hosts = vec![
            host_sel("auto1", true, None, Some("auto")),        // in
            host_sel("pinned", true, None, Some("me@x")),       // out: pinned to an email
            host_sel("legacy", true, None, None),               // out: legacy None == pinned
            host_sel("grouped", true, Some("g"), Some("auto")), // out: named group handles it
            host_sel("stopped", false, None, Some("auto")),     // out: unmanaged
        ];
        let picked: Vec<String> = auto_pool_clones(&hosts).into_iter().map(|h| h.id).collect();
        assert_eq!(picked, vec!["auto1"]);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p control-server auto_pool_is_only_managed_ungrouped_auto_clones`
Expected: FAIL to compile — `cannot find function auto_pool_clones in this scope`.

- [ ] **Step 3: Write minimal implementation**

3a. Remove the constant at `crates/control-server/src/claude.rs:52`:

```rust
// DELETE this line:
const ROTATE_MAX_FIVE_HOUR_PCT: f64 = 90.0;
```

(Also delete its now-stale doc comment on the preceding line.)

3b. Replace the entire `rotate_once` function (currently :771-821) with the following three items (`rotate_pool`, `auto_pool_clones`, `rotate_once`):

```rust
/// Rotate one pool of clones over candidate account emails `members`. Drops members
/// that aren't imported or are exhausted, sticky-assigns clones to the survivors
/// ([`assign_rotation`]), and pushes any moves (no agent-wrapper restart). `label` names
/// the pool in logs. Leaves clones untouched when no member is eligible.
async fn rotate_pool(app: &App, label: &str, members: &[String], clones: &[Host]) {
    let eligible = eligible_members(app, members);
    if eligible.is_empty() {
        tracing::info!("rotate: pool '{label}' has no eligible account; leaving {} clone(s)", clones.len());
        return;
    }
    let usage: HashMap<String, f64> =
        eligible.iter().map(|e| (e.clone(), five_hour_pct(app, e))).collect();
    for (host, email) in assign_rotation(clones, &eligible, &usage) {
        if host.claude_account_email.as_deref() == Some(email.as_str()) {
            continue; // unchanged (sticky keep) → no rewrite
        }
        match push_account_to_clone(app, &host.id, &email).await {
            Ok(()) => {
                tracing::info!(
                    "rotate[{label}]: {} {} -> {}",
                    host.id,
                    host.claude_account_email.as_deref().unwrap_or("none"),
                    email
                );
                let id = host.id.clone();
                app.store.mutate(|s| {
                    if let Some(h) = s.hosts.iter_mut().find(|h| h.id == id) {
                        h.claude_account_email = Some(email);
                    }
                });
            }
            Err(e) => tracing::warn!("rotate[{label}]: applying {email} to {} failed: {e}", host.id),
        }
        tokio::time::sleep(STAGGER).await; // gentle on the daemon
    }
}

/// Managed clones bound to the implicit "auto" pool: `claude_selection == "auto"` and
/// not in a named group. Legacy hosts with `claude_selection == None` are treated as
/// pinned (never rotated).
fn auto_pool_clones(hosts: &[Host]) -> Vec<Host> {
    hosts
        .iter()
        .filter(|h| {
            h.managed && h.claude_group.is_none() && h.claude_selection.as_deref() == Some(AUTO)
        })
        .cloned()
        .collect()
}

/// One rotation pass over every named group plus the implicit "auto" pool (all imported
/// accounts, recomputed live). Sticky: a clone moves only when its account exhausts or
/// leaves its pool. See [`rotate_pool`] / [`assign_rotation`].
pub async fn rotate_once(app: &App) {
    let cfg = app.config();
    let hosts = app.store.get().hosts;
    // Named groups.
    let mut by_group: HashMap<String, Vec<Host>> = HashMap::new();
    for h in &hosts {
        if let (Some(g), true) = (&h.claude_group, h.managed) {
            by_group.entry(g.clone()).or_default().push(h.clone());
        }
    }
    for (gname, clones) in by_group {
        let Some(group) = cfg.clone_groups.iter().find(|g| g.name == gname) else {
            continue; // group deleted → leave its clones on their current account
        };
        rotate_pool(app, &gname, &group.accounts, &clones).await;
    }
    // "auto" == a live group of all imported accounts.
    let auto = auto_pool_clones(&hosts);
    if !auto.is_empty() {
        rotate_pool(app, "auto", &app.claude.emails(), &auto).await;
    }
}
```

Note: the old `if cfg.clone_groups.is_empty() { return; }` early-return is intentionally gone — the auto pool must rotate even with no named groups.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p control-server auto_pool_is_only_managed_ungrouped_auto_clones`
Expected: PASS.

Then: `cargo test -p control-server` and `cargo build`
Expected: PASS / builds clean (no more `ROTATE_MAX_FIVE_HOUR_PCT`; `exhausted`/`eligible_members` from Task 1 are now used).

- [ ] **Step 5: Commit**

```bash
git add crates/control-server/src/claude.rs
git commit -m "feat(claude): rotate 'auto' clones across all imported accounts like a group" \
  -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Claude — remove the standalone `auto_swap_exhausted` path

**Files:**
- Modify: `crates/control-server/src/claude.rs` (module doc :12, `poll_once` tail :545-547, `auto_swap_exhausted` :923-968)

**Interfaces:**
- Removes: `async fn auto_swap_exhausted(app: &App)` and its call site. No new public surface. The config field `auto_swap_on_exhaustion` still exists (removed in Task 5) but is no longer read here.

- [ ] **Step 1: Delete the call site in `poll_once`**

Remove these lines (currently `crates/control-server/src/claude.rs:545-547`):

```rust
    if cfg.claude.auto_swap_on_exhaustion {
        auto_swap_exhausted(app).await;
    }
```

Check whether `cfg` is still used elsewhere in `poll_once` after this deletion; it is (the function uses `cfg` earlier), so leave the `let cfg = ...` binding. If the compiler warns `cfg` is unused, prefix with `_`, but it should remain used.

- [ ] **Step 2: Delete the function**

Remove the entire `auto_swap_exhausted` function and its doc comment (currently `crates/control-server/src/claude.rs:923-968`, the block starting `/// When a clone's assigned account is exhausted, hot-swap it to the best alternative.` through its closing `}`).

- [ ] **Step 3: Update the module doc**

At `crates/control-server/src/claude.rs:11-12`, change:

```rust
//! publishes a token-free `ClaudeUsage` view onto `ControlState.claudeAccounts`,
//! and (when enabled) auto-swaps a clone whose account is exhausted.
```

to:

```rust
//! publishes a token-free `ClaudeUsage` view onto `ControlState.claudeAccounts`.
//! Clones select an account via `"auto"` (rotated across all imported accounts by
//! [`rotate_once`]), a named group, or a pinned email.
```

- [ ] **Step 4: Verify build + tests**

Run: `cargo build` then `cargo test -p control-server`
Expected: builds clean, all tests PASS. (If a warning appears that `best_scored` is now unused, it is not — it's still used by `recommend`/`resolve_clone_account` for the initial seed pick.)

- [ ] **Step 5: Commit**

```bash
git add crates/control-server/src/claude.rs
git commit -m "refactor(claude): drop the standalone auto_swap_exhausted path (auto rotation replaces it)" \
  -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Codex — mirror the threshold, auto pool, and swap removal

**Files:**
- Modify: `crates/control-server/src/codex.rs` (const :32 & :34, helpers near :642-663, `rotate_once` :716-765, `auto_swap_exhausted` :775-818, `poll_once` tail :901-903, tests :934+)

**Interfaces:**
- Same shapes as Tasks 1-3 but in `codex.rs`: `is_exhausted`, `seven_day_pct`, `exhausted`, `eligible_members`, `auto_pool_clones`, `rotate_pool`; `rotate_once` rotates the Codex auto pool; `auto_swap_exhausted` removed. Uses `provider == Some(wire::Provider::Codex)`, `app.codex.emails()`, `codex_group`, `codex_selection`, `codex_account_email`, `cfg.codex_groups`.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `crates/control-server/src/codex.rs` (near the end, before the closing `}`):

```rust
    #[test]
    fn codex_exhaustion_threshold_is_80_5h_or_95_7d() {
        assert!(!is_exhausted(80.0, 0.0));
        assert!(is_exhausted(80.1, 0.0));
        assert!(!is_exhausted(0.0, 94.9));
        assert!(is_exhausted(0.0, 95.0));
    }

    fn host_sel(id: &str, managed: bool, group: Option<&str>, sel: Option<&str>) -> Host {
        Host {
            id: id.into(),
            managed,
            codex_group: group.map(str::to_string),
            codex_selection: sel.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn codex_auto_pool_is_only_managed_ungrouped_auto_clones() {
        let hosts = vec![
            host_sel("auto1", true, None, Some("auto")),
            host_sel("pinned", true, None, Some("me@x")),
            host_sel("legacy", true, None, None),
            host_sel("grouped", true, Some("g"), Some("auto")),
            host_sel("stopped", false, None, Some("auto")),
        ];
        let picked: Vec<String> = auto_pool_clones(&hosts).into_iter().map(|h| h.id).collect();
        assert_eq!(picked, vec!["auto1"]);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p control-server codex_`
Expected: FAIL to compile — `is_exhausted` / `auto_pool_clones` not found.

- [ ] **Step 3: Write the implementation**

3a. Change the constant at `crates/control-server/src/codex.rs:32` and delete `:34`:

```rust
const SESSION_HEADROOM_PCT: f64 = 20.0;   // was 40.0
// DELETE: const ROTATE_MAX_FIVE_HOUR_PCT: f64 = 90.0;
```

3b. Add helpers after `five_hour_pct` (ends :652):

```rust
fn seven_day_pct(app: &App, email: &str) -> f64 {
    app.store
        .get()
        .claude_accounts
        .iter()
        .filter(|u| u.provider == Some(wire::Provider::Codex))
        .find(|u| u.email == email)
        .and_then(|u| u.seven_day.as_ref())
        .map(|w| w.pct)
        .unwrap_or(0.0)
}

fn is_exhausted(five: f64, seven: f64) -> bool {
    (100.0 - five) < SESSION_HEADROOM_PCT || seven >= SEVEN_DAY_CAP_PCT
}

fn exhausted(app: &App, email: &str) -> bool {
    is_exhausted(five_hour_pct(app, email), seven_day_pct(app, email))
}

fn eligible_members(app: &App, members: &[String]) -> Vec<String> {
    let known = app.codex.emails();
    members
        .iter()
        .filter(|email| known.iter().any(|k| &k == email))
        .filter(|email| !exhausted(app, email))
        .cloned()
        .collect()
}
```

3c. Replace `eligible_group_accounts` (:654-663) with:

```rust
fn eligible_group_accounts(app: &App, group: &CloneGroup) -> Vec<String> {
    eligible_members(app, &group.accounts)
}
```

3d. Replace the entire `rotate_once` function (:716-765) with `rotate_pool` + `auto_pool_clones` + `rotate_once`:

```rust
async fn rotate_pool(app: &App, label: &str, members: &[String], clones: &[Host]) {
    let eligible = eligible_members(app, members);
    if eligible.is_empty() {
        tracing::info!("codex rotate: pool '{label}' has no eligible account; leaving {} clone(s)", clones.len());
        return;
    }
    let usage: HashMap<String, f64> =
        eligible.iter().map(|e| (e.clone(), five_hour_pct(app, e))).collect();
    for (host, email) in assign_rotation(clones, &eligible, &usage) {
        if host.codex_account_email.as_deref() == Some(email.as_str()) {
            continue;
        }
        match push_account_to_clone(app, &host.id, &email).await {
            Ok(()) => {
                tracing::info!(
                    "codex rotate[{label}]: {} {} -> {}",
                    host.id,
                    host.codex_account_email.as_deref().unwrap_or("none"),
                    email
                );
                let id = host.id.clone();
                app.store.mutate(|s| {
                    if let Some(h) = s.hosts.iter_mut().find(|h| h.id == id) {
                        h.codex_account_email = Some(email);
                    }
                });
            }
            Err(e) => tracing::warn!("codex rotate[{label}]: applying {email} to {} failed: {e}", host.id),
        }
        tokio::time::sleep(STAGGER).await;
    }
}

fn auto_pool_clones(hosts: &[Host]) -> Vec<Host> {
    hosts
        .iter()
        .filter(|h| {
            h.managed && h.codex_group.is_none() && h.codex_selection.as_deref() == Some(AUTO)
        })
        .cloned()
        .collect()
}

pub async fn rotate_once(app: &App) {
    let cfg = app.config();
    let hosts = app.store.get().hosts;
    let mut by_group: HashMap<String, Vec<Host>> = HashMap::new();
    for h in &hosts {
        if let (Some(g), true) = (&h.codex_group, h.managed) {
            by_group.entry(g.clone()).or_default().push(h.clone());
        }
    }
    for (gname, clones) in by_group {
        let Some(group) = cfg.codex_groups.iter().find(|g| g.name == gname) else {
            continue;
        };
        rotate_pool(app, &gname, &group.accounts, &clones).await;
    }
    let auto = auto_pool_clones(&hosts);
    if !auto.is_empty() {
        rotate_pool(app, "auto", &app.codex.emails(), &auto).await;
    }
}
```

3e. Delete the call in `poll_once` (:901-903):

```rust
    if cfg.codex.auto_swap_on_exhaustion {
        auto_swap_exhausted(app).await;
    }
```

3f. Delete the entire `auto_swap_exhausted` function (:775-818).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p control-server codex_` then `cargo build` then `cargo test -p control-server`
Expected: new codex tests PASS; builds clean; whole suite PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/control-server/src/codex.rs
git commit -m "feat(codex): mirror auto rotation + exhausted threshold; drop auto_swap_exhausted" \
  -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: Remove the `auto_swap_on_exhaustion` config field (Rust + generated TS + frontend refs)

**Files:**
- Modify: `crates/wire/src/config.rs` (ClaudeConfig :233-235 & Default :240; CodexConfig :253-255 & Default :269; test :593 & :597)
- Regenerate: `frontend/app/lib/wire/ClaudeConfig.ts`, `frontend/app/lib/wire/CodexConfig.ts` (via ts-rs)
- Modify: `frontend/app/stories/fixtures.ts:254,256`
- Modify: `frontend/app/components/SettingsPanel.tsx` (state :221 & :226; checkboxes :824-831 & :909-916)

**Interfaces:**
- Consumes: nothing reads `auto_swap_on_exhaustion` in Rust anymore (removed in Tasks 3 & 4) — this task is safe to run only after those.
- Produces: `ClaudeConfig`/`CodexConfig` (Rust + TS) without the field.

- [ ] **Step 1: Update the failing Rust config test**

In `crates/wire/src/config.rs`, edit the `codex_config_defaults_and_passthrough` test: remove the assertion at :597 and fix the comment at :593.

Change:
```rust
        // Defaults: 600s poll, no pinned email, no auto-swap, usage polling ON.
        let c = AppConfig::default();
        assert_eq!(c.codex.poll_secs, 600);
        assert!(c.codex.pinned_email.is_none());
        assert!(!c.codex.auto_swap_on_exhaustion);
        assert!(c.codex.usage_polling, "usage_polling defaults to true");
```
to:
```rust
        // Defaults: 600s poll, no pinned email, usage polling ON.
        let c = AppConfig::default();
        assert_eq!(c.codex.poll_secs, 600);
        assert!(c.codex.pinned_email.is_none());
        assert!(c.codex.usage_polling, "usage_polling defaults to true");
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p wire codex_config_defaults_and_passthrough`
Expected: still PASSES compile-wise at this point (field still exists). This step is a no-op check; proceed. (If you prefer strict red-green, run after Step 3a instead, where removing the field makes the untouched assertion a compile error.)

- [ ] **Step 3: Remove the Rust field**

3a. In `ClaudeConfig` (`crates/wire/src/config.rs:233-235`), delete:
```rust
    /// Hot-swap a clone to another account when its usage is exhausted.
    #[serde(default)]
    pub auto_swap_on_exhaustion: bool,
```
and update its `Default` (:240) to:
```rust
        Self { poll_secs: 600, pinned_email: None }
```

3b. In `CodexConfig` (`crates/wire/src/config.rs:253-255`), delete the same three lines, and update its `Default` (:269) to:
```rust
        Self { poll_secs: 600, pinned_email: None, usage_polling: true }
```

- [ ] **Step 4: Run Rust tests + regenerate TS bindings**

Run: `cargo test -p wire`
Expected: PASS. This also **regenerates** `frontend/app/lib/wire/ClaudeConfig.ts` and `CodexConfig.ts` (ts-rs writes exported types during the test run).

Verify the field is gone:
Run: `rg -n "autoSwapOnExhaustion" frontend/app/lib/wire/`
Expected: no matches. (If ts-rs did not rewrite them, manually delete the `autoSwapOnExhaustion: boolean,` line and its doc comment from both `ClaudeConfig.ts` and `CodexConfig.ts`.)

- [ ] **Step 5: Remove the frontend references**

5a. `frontend/app/stories/fixtures.ts`: delete `autoSwapOnExhaustion: false,` at :254 and remove `autoSwapOnExhaustion: false, ` from the inline codex object at :256 (leaving `codex: { pollSecs: BigInt(600), pinnedEmail: null, usagePolling: true },`).

5b. `frontend/app/components/SettingsPanel.tsx`: delete `autoSwapOnExhaustion: false,` from the `claude` state initializer (:221) and the `codex` state initializer (:226).

5c. `frontend/app/components/SettingsPanel.tsx`: delete the Claude checkbox block (:824-831):
```tsx
                <label className="col-span-2 flex items-center gap-2 text-sm text-slate-600 dark:text-slate-300">
                  <input
                    type="checkbox"
                    checked={claude.autoSwapOnExhaustion}
                    onChange={(e) => setClaude({ ...claude, autoSwapOnExhaustion: e.target.checked })}
                  />
                  Auto-swap a clone to another account when usage is exhausted
                </label>
```
and the Codex checkbox block (:909-916):
```tsx
                <label className="col-span-2 flex items-center gap-2 text-sm text-slate-600">
                  <input
                    type="checkbox"
                    checked={codex.autoSwapOnExhaustion}
                    onChange={(e) => setCodex({ ...codex, autoSwapOnExhaustion: e.target.checked })}
                  />
                  Auto-swap a clone to another account when usage is exhausted
                </label>
```

The hydration at :251-260 uses `...c.claude` / `...c.codex` spreads, so no change is needed there once the wire types drop the field.

- [ ] **Step 6: Verify frontend typechecks**

Run: `cd frontend && bun run typecheck`
Expected: PASS, no reference to `autoSwapOnExhaustion`.

Confirm nothing else references it:
Run: `rg -n "autoSwapOnExhaustion|auto_swap_on_exhaustion" crates/ frontend/`
Expected: no matches.

- [ ] **Step 7: Commit**

```bash
git add crates/wire/src/config.rs frontend/app/lib/wire/ClaudeConfig.ts frontend/app/lib/wire/CodexConfig.ts frontend/app/stories/fixtures.ts frontend/app/components/SettingsPanel.tsx
git commit -m "refactor(config): remove the auto_swap_on_exhaustion toggle (auto rotation replaces it)" \
  -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: Frontend copy — "auto" label and stale group hint

**Files:**
- Modify: `frontend/app/components/AccountGroupSelect.tsx` (option :34, header comment :1-4)
- Modify: `frontend/app/components/SettingsPanel.tsx` (Claude groups hint :838)

**Interfaces:** Copy-only; no behavior or type changes.

- [ ] **Step 1: Update the picker option + comment**

In `frontend/app/components/AccountGroupSelect.tsx:34`, change:
```tsx
      <option value="auto">Auto (best account)</option>
```
to:
```tsx
      <option value="auto">Auto (all accounts)</option>
```

And update the header comment (:1-3) from "auto" (server picks the best account) to reflect rotation:
```tsx
// A Claude-account picker shared by the clone modal and the per-host change control.
// Value is one of: "auto" (rotate across all imported accounts), "none" (install no
// token), an account email, or "group:<name>" (binds the clone to a named pool). The
// server rotates "auto" and group clones; a pinned email is left fixed.
```

- [ ] **Step 2: Update the stale Claude-groups hint**

In `frontend/app/components/SettingsPanel.tsx:838`, change the hint text from:
```tsx
              hint="A pool of accounts. A clone bound to a group keeps its account (preserving its prompt cache) until that account passes 90% 5h usage, then moves to the least-used member."
```
to:
```tsx
              hint="A pool of accounts. A clone bound to a group keeps its account (preserving its prompt cache) until that account is exhausted (80% 5h or 95% 7d), then moves to the least-used member."
```

- [ ] **Step 3: Verify frontend typechecks/builds**

Run: `cd frontend && bun run typecheck`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add frontend/app/components/AccountGroupSelect.tsx frontend/app/components/SettingsPanel.tsx
git commit -m "docs(frontend): relabel 'auto' as all-accounts rotation; fix group threshold hint" \
  -m "Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Final verification

- [ ] `cargo build` — clean.
- [ ] `cargo test -p control-server` — all pass (new: `exhaustion_threshold_is_80_5h_or_95_7d`, `auto_pool_is_only_managed_ungrouped_auto_clones`, `codex_exhaustion_threshold_is_80_5h_or_95_7d`, `codex_auto_pool_is_only_managed_ungrouped_auto_clones`).
- [ ] `cargo test -p wire` — all pass.
- [ ] `cd frontend && bun run typecheck` — clean.
- [ ] `rg -n "auto_swap_on_exhaustion|autoSwapOnExhaustion|ROTATE_MAX_FIVE_HOUR_PCT" crates/ frontend/` — no matches.

## Notes for the implementer

- **Why no App-level test for `rotate_once`:** it calls `push_account_to_clone`, which shells into a running clone over `docker exec` — not available in unit tests. The existing suite tests the pure `assign_rotation` (sticky placement, spread, degradation), which auto reuses unchanged; this plan adds pure tests for the two genuinely new decisions (the `is_exhausted` threshold and which clones join the auto pool). That matches the codebase's existing test boundary.
- **Behavior change to flag on review:** named groups now rotate at the exhausted threshold (80% 5h / 95% 7d) instead of the old 90% 5h cap, so they spread earlier and each move cold-starts that clone's Anthropic prompt cache. This is intentional per the spec.
- **No migration:** existing `"auto"` hosts (selection == "auto", group == None) begin rotating on the next pass; a leftover `autoSwapOnExhaustion` key in an existing `config.json` is ignored on load (config does not use `deny_unknown_fields`).
