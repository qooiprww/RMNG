# Codex Account Subsystem Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a server-owned **Codex** (OpenAI/ChatGPT) account subsystem to the control-server with full parity to the existing Claude subsystem (import-from-signed-in-clone, usage polling, assign-at-clone, swap, recommended scoring, groups + rotation, auto-swap) and independent coexistence — one clone can hold a Claude account **and** a Codex account at the same time.

**Architecture:** A new `crates/control-server/src/codex.rs` mirrors `claude.rs` (the token/JWT/usage mechanics differ, so it is a sibling, not a provider-generic rewrite). Side-effect-free helpers shared by both modules are extracted into a new `crates/control-server/src/clone_ops.rs` (including a new hand-rolled JWT decoder and a provider-aware view-merge that fixes the two-poller clobber). New wire fields (`Host.codex*`, `CodexConfig`, `codex_groups`) flow to the frontend via ts-rs. The Claude subsystem is unchanged except the poller merge fix, which is a no-op while only one provider has accounts.

**Tech Stack:** Rust (Cargo workspace: `wire`, `control-server`), Axum web API, serde/serde_json, ts-rs (TypeScript binding generation), Remix/React + TypeScript frontend (`bun`), bash guest scripts run over `docker exec`.

## Global Constraints

Every task's requirements implicitly include this section. Values are copied verbatim from `CODEX_PARITY.md` (verified 2026-07-02 against the working tree and the `openai/codex` source).

- **Codex auth file (in-clone):** `~/.codex/auth.json`, shape
  `{ "OPENAI_API_KEY": null, "tokens": { "id_token": "<JWT>", "access_token": "<JWT>", "refresh_token": "<opaque>", "account_id": "<uuid>" }, "last_refresh": "<RFC3339>" }`.
- **OAuth refresh:** `POST https://auth.openai.com/oauth/token`, `client_id = "app_EMoamEEZ73f0CkXaXp7hrann"`, `grant_type=refresh_token`. Response `{id_token?, access_token?, refresh_token?}` — **no `expires_in`**; expiry is decoded from the access-token JWT `exp` claim (seconds → ms). Refresh tokens are **single-use / rotating** (same hazard as Claude → keep the refresh gate + clone-can't-refresh model).
- **Injected auth.json (server → clone):** the real `access_token` + `id_token` + `account_id`, but `refresh_token: ""` and `last_refresh: <now RFC3339>`. This defeats the CLI's 8-day self-refresh fallback; the 5-min-before-exp trigger is never reached because the server re-pushes with a 60-min lead.
- **Usage:** `GET https://chatgpt.com/backend-api/wham/usage`, headers `Authorization: Bearer <access_token>` + `ChatGPT-Account-Id: <account_id>`. Map `rate_limit.primary_window` / `secondary_window` to the 5h / weekly bars **by `limit_window_seconds`** (≈18000 = 5h, ≈604800 = weekly), **not** by field order.
- **Identity:** codex accounts are keyed by **ChatGPT email** for assignment (parity with claude). Wire id is `codex:<account_id>`. `account_id` comes from `tokens.account_id`, falling back to the id_token claim `https://api.openai.com/auth`.`chatgpt_account_id`. Plan comes from the id_token claim `https://api.openai.com/auth`.`chatgpt_plan_type`. Email comes from the id_token claim `email`.
- **Store:** 0600 `codex-accounts.json` in `data_dir` (env override `RMNG_CODEX_ACCOUNTS_FILE`). `REFRESH_LEAD_MS = 60 * 60 * 1000` (60 min).
- **Install:** `CODEX_NON_INTERACTIVE=1 curl -fsSL https://chatgpt.com/codex/install.sh | sh` → standalone binary at `~/.local/bin/codex` (no node). **Warn-only** on failure (NOT load-bearing like the claude install).
- **No new crate dependencies.** JWT decode is hand-rolled base64url in `clone_ops.rs`. RFC3339 formatting reuses the existing `crate::docker::epoch_to_rfc3339`.
- **MSRV:** repo `rust-version` (currently 1.85). No feature requiring a newer toolchain.
- **Linux / Claude behavior unchanged.** The only edits to `claude.rs` are: (a) importing the moved shared helpers, (b) routing its two view-publish sites through `replace_provider_views`, (c) adding a `provider != Codex` filter to `five_hour_pct` and `auto_swap_exhausted`. All are no-ops while only Claude has accounts. No wire-protocol, decode-pipeline, pointer-lock, or keymap changes.
- **ts-rs exports:** every wire type that crosses to the browser derives `ts_rs::TS` with `#[ts(export, export_to = "../../../frontend/app/lib/wire/")]`. Regenerate by running `cargo test -p wire`.
- **Greenfield / compat:** all new `Host`/config fields are `Option` / `#[serde(default)]` — old `state.json` / `config.json` parse unchanged. No data migration.

## Reconciliations with the spec prose (read before Task 4)

`CODEX_PARITY.md` §Design was written against an earlier signature set. Reconciled against the current working tree:

1. The spec's `run_clone_op(ssh, ctid, user, script, op, extra)` is stale. The **current** signature is `crate::provision::run_clone_op(app, container, op, extra)` and it hardcodes `IMPORT_SCRIPT = include_str!("../scripts/claude-import.sh")`, running it via `app.docker.exec_script`. The generalization in this plan is `crate::clone_ops::run_clone_op(app, container, script, op, extra)`; `provision::run_clone_op` becomes a thin wrapper that passes `IMPORT_SCRIPT`, so **`claude.rs` is not touched** for this and all its existing callers keep working.
2. The spec lists an `sq` shell-quote helper to extract. **It does not exist** in the working tree (the `docker exec bash -s -- <args>` path passes args positionally, so nothing needs shell-quoting). Do not create it.
3. The spec says the JWT helper "matches orchestrate.rs's hand-rolled encode." `orchestrate.rs` is retired; the standard-base64 **encoder** now lives in `crate::provision::b64_encode`. The JWT **decoder** (base64url) is genuinely new — write it in `clone_ops.rs`.
4. RFC3339 formatting for `last_refresh` reuses the existing `crate::docker::epoch_to_rfc3339(secs: i64) -> String` (make it `pub(crate)`), rather than adding `chrono`.

## File Structure

**New files:**
- `crates/control-server/src/clone_ops.rs` — side-effect-free helpers shared by `claude.rs` and `codex.rs`: `now_ms`, `extract_json`, `snippet`, `rand_u64`, `shuffle`, generalized `run_clone_op`, new `jwt_claims` / `jwt_exp_ms` / `b64url_decode`, new `replace_provider_views`.
- `crates/control-server/src/codex.rs` — the Codex sibling of `claude.rs` (~700 lines): store, import, refresh, usage, scoring, groups, rotator, poller, auto-swap.
- `crates/control-server/scripts/codex-import.sh` — guest script (sibling of `claude-import.sh`) for `~/.codex/auth.json` ops.

**Modified files:**
- `crates/wire/src/control.rs` — `Host` gains `codex_account_email` / `codex_group` / `codex_selection`.
- `crates/wire/src/config.rs` — new `CodexConfig`; `AppConfig` + `AppConfigRedacted` gain `codex` + `codex_groups`.
- `crates/wire/src/lib.rs` — re-export `CodexConfig`.
- `crates/control-server/src/config.rs` — test only (deep_merge already covers the new non-secret fields).
- `crates/control-server/src/docker.rs` — `epoch_to_rfc3339` → `pub(crate)`.
- `crates/control-server/src/provision.rs` — `run_clone_op` delegates to `clone_ops::run_clone_op`.
- `crates/control-server/src/claude.rs` — use moved helpers; route view-publish through `replace_provider_views`; provider filters.
- `crates/control-server/src/app.rs` — `pub codex: Arc<CodexStore>`.
- `crates/control-server/src/main.rs` — `mod clone_ops; mod codex;` + spawn `codex::run_poller` / `codex::run_rotator`.
- `crates/control-server/src/web.rs` — `/api/codex/*` routes + handlers; `clone()` parses `codexAccount`.
- `crates/control-server/src/jobs.rs` — `CloneSpec.codex_account`; codex assignment block in `run_clone`.
- `crates/control-server/src/mcp.rs` — `codex_recommended` + `codex_swap` tools.
- `template/setup/30-user.sh` — install `codex` after the claude install.
- `frontend/app/lib/types.ts`, `frontend/app/lib/api.ts`, `frontend/app/routes/_index.tsx`, `frontend/app/components/{ImportAccountModal,CloneModal,ChangeAccountModal,SettingsPanel}.tsx`.
- `docs/API.md`, `docs/PROTOCOL.md`, `docs/SCRIPTS.md`, `crates/wire/README.md`.

**Dependency order (task order):** wire types (1–2) → control-server merge test (3) → shared helpers + coexistence fix (4–5) → guest script (6) → codex.rs backend (7–9) → app/main/endpoints/jobs/mcp wiring (10–12) → provisioning (13) → frontend (14–17) → docs (18).

---

### Task 1: Wire — `Host` codex fields

**Files:**
- Modify: `crates/wire/src/control.rs` (the `Host` struct ~62-143; the `ClaudeUsage.assignable` doc ~246-247; the `tests` module ~366-394)

**Interfaces:**
- Produces: three new optional `Host` fields consumed by `codex.rs`, `jobs.rs`, `web.rs`, and the frontend:
  - `codex_account_email: Option<String>` (JSON `codexAccountEmail`)
  - `codex_group: Option<String>` (JSON `codexGroup`)
  - `codex_selection: Option<String>` (JSON `codexSelection`)

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `crates/wire/src/control.rs`:

```rust
    #[test]
    fn host_codex_fields_camelcase() {
        let h = Host {
            id: "h".into(),
            host: "1.2.3.4".into(),
            port: 3389,
            claude_account_email: Some("a@b.c".into()),
            codex_account_email: Some("z@openai.com".into()),
            codex_group: Some("team".into()),
            codex_selection: Some("group:team".into()),
            ..Default::default()
        };
        let v = serde_json::to_value(&h).unwrap();
        assert_eq!(v["codexAccountEmail"], "z@openai.com");
        assert_eq!(v["codexGroup"], "team");
        assert_eq!(v["codexSelection"], "group:team");
        // Claude fields still present and untouched.
        assert_eq!(v["claudeAccountEmail"], "a@b.c");
        // Omitted codex fields are not serialized.
        let bare = Host { id: "h2".into(), ..Default::default() };
        let bv = serde_json::to_value(&bare).unwrap();
        assert!(bv.get("codexAccountEmail").is_none());
        // Round-trips.
        let back: Host = serde_json::from_value(v).unwrap();
        assert_eq!(back.codex_selection.as_deref(), Some("group:team"));
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p wire host_codex_fields_camelcase`
Expected: FAIL — `Host` has no field `codex_account_email` (compile error).

- [ ] **Step 3: Add the fields**

In `crates/wire/src/control.rs`, immediately after the `claude_selection` field (~line 117) inside `struct Host`, add:

```rust
    /// Email of the imported Codex (ChatGPT) account whose token is written into this
    /// clone's `~/.codex/auth.json`. Independent of `claude_account_email` — a clone can
    /// hold both. `None` when no Codex account is assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_account_email: Option<String>,
    /// Name of the Codex group this clone is balanced within (sticky, like `claude_group`);
    /// `None` when bound to a single fixed Codex account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_group: Option<String>,
    /// The operator's Codex *selection* verbatim: `"auto"`, `"none"`, `"group:<name>"`, or
    /// an account email — the Codex twin of `claude_selection`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_selection: Option<String>,
```

Also update the `ClaudeUsage.assignable` doc comment (~line 246) from "true for every imported Claude account … Codex accounts never." to reflect parity:

```rust
    /// Whether the account can run a clone: true for every imported account of either
    /// provider (the server owns each account's token lifecycle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignable: Option<bool>,
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p wire host_codex_fields_camelcase`
Expected: PASS.

- [ ] **Step 5: Regenerate the TS binding and confirm the suite is green**

Run: `cargo test -p wire`
Expected: PASS (all tests). This regenerates `frontend/app/lib/wire/Host.ts` with the three `codex*` fields (`string | null`).

- [ ] **Step 6: Commit**

```bash
git add crates/wire/src/control.rs frontend/app/lib/wire/Host.ts
git commit -m "feat(wire): Host codex account fields (codexAccountEmail/Group/Selection)"
```

---

### Task 2: Wire — `CodexConfig` + `AppConfig`/`AppConfigRedacted` codex fields

**Files:**
- Modify: `crates/wire/src/config.rs` (add `CodexConfig` after `ClaudeConfig` ~215; extend `AppConfig` ~221 + its `Default` ~276 + `redacted()` ~327; extend `AppConfigRedacted` ~351; add tests)
- Modify: `crates/wire/src/lib.rs` (re-export `CodexConfig`)

**Interfaces:**
- Consumes: `CloneGroup` (reused verbatim for codex groups), `ClaudeConfig` (the struct to mirror).
- Produces:
  - `CodexConfig { poll_secs: u64, pinned_email: Option<String>, auto_swap_on_exhaustion: bool, usage_polling: bool }` — TS-exported, `Default` = `{ poll_secs: 600, pinned_email: None, auto_swap_on_exhaustion: false, usage_polling: true }`.
  - `AppConfig.codex: CodexConfig`, `AppConfig.codex_groups: Vec<CloneGroup>` (both `#[serde(default)]`).
  - `AppConfigRedacted.codex: CodexConfig`, `AppConfigRedacted.codex_groups: Vec<CloneGroup>`.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `crates/wire/src/config.rs`:

```rust
    #[test]
    fn codex_config_defaults_and_passthrough() {
        // Defaults: 600s poll, no pinned email, no auto-swap, usage polling ON.
        let c = AppConfig::default();
        assert_eq!(c.codex.poll_secs, 600);
        assert!(c.codex.pinned_email.is_none());
        assert!(!c.codex.auto_swap_on_exhaustion);
        assert!(c.codex.usage_polling, "usage_polling defaults to true");
        assert!(c.codex_groups.is_empty());
        // Missing keys fall back to defaults (older config.json stays valid).
        let d: AppConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(d.codex.poll_secs, 600);
        assert!(d.codex.usage_polling);
        assert!(d.codex_groups.is_empty());
        // usage_polling can be turned off from JSON (camelCase).
        let off: AppConfig =
            serde_json::from_str(r#"{ "codex": { "pollSecs": 300, "usagePolling": false } }"#).unwrap();
        assert_eq!(off.codex.poll_secs, 300);
        assert!(!off.codex.usage_polling);
        // Redaction passes codex + codex_groups through (non-secret).
        let r = AppConfig {
            codex: CodexConfig { poll_secs: 120, usage_polling: false, ..Default::default() },
            codex_groups: vec![CloneGroup { name: "g".into(), accounts: vec!["z@o".into()] }],
            ..Default::default()
        }
        .redacted();
        assert_eq!(r.codex.poll_secs, 120);
        assert!(!r.codex.usage_polling);
        assert_eq!(r.codex_groups.len(), 1);
        // Round-trips as camelCase.
        let v = serde_json::to_value(&CodexConfig::default()).unwrap();
        assert!(v.get("usagePolling").is_some());
        assert!(v.get("autoSwapOnExhaustion").is_some());
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p wire codex_config_defaults_and_passthrough`
Expected: FAIL — `CodexConfig` unknown; `AppConfig` has no `codex` field (compile error).

- [ ] **Step 3: Add `CodexConfig`**

In `crates/wire/src/config.rs`, immediately after the `ClaudeConfig` `Default` impl (~line 215), add:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "../../../frontend/app/lib/wire/")]
pub struct CodexConfig {
    /// Usage poll interval (seconds, floored at 15 by the poller).
    pub poll_secs: u64,
    /// Account email pinned to the top of the usage list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_email: Option<String>,
    /// Hot-swap a clone to another account when its usage is exhausted.
    #[serde(default)]
    pub auto_swap_on_exhaustion: bool,
    /// Poll the ChatGPT usage endpoint. When false, the poller still refreshes + pushes
    /// tokens and publishes base views (with an explanatory `error`), but skips the usage
    /// fetch — an escape hatch if the unofficial `/wham/usage` shape drifts.
    #[serde(default = "default_true")]
    pub usage_polling: bool,
}

fn default_true() -> bool {
    true
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self { poll_secs: 600, pinned_email: None, auto_swap_on_exhaustion: false, usage_polling: true }
    }
}
```

- [ ] **Step 4: Wire `codex` + `codex_groups` into `AppConfig` / `AppConfigRedacted`**

In `struct AppConfig` (~line 254), immediately after the `pub claude: ClaudeConfig,` field, add:

```rust
    #[serde(default)]
    pub codex: CodexConfig,
```

Immediately after the `pub clone_groups: Vec<CloneGroup>,` field (~line 258), add:

```rust
    /// Named Codex account pools a clone can be bound to for rotation (members are emails
    /// of imported Codex accounts, from the server's `codex-accounts.json`).
    #[serde(default)]
    pub codex_groups: Vec<CloneGroup>,
```

In `impl Default for AppConfig` (~line 277), add after `claude: ClaudeConfig::default(),`:

```rust
            codex: CodexConfig::default(),
```

and after `clone_groups: Vec::new(),`:

```rust
            codex_groups: Vec::new(),
```

In `AppConfig::redacted()` (~line 327), add after `claude: self.claude.clone(),`:

```rust
            codex: self.codex.clone(),
```

and after `clone_groups: self.clone_groups.clone(),`:

```rust
            codex_groups: self.codex_groups.clone(),
```

In `struct AppConfigRedacted` (~line 351), add after `pub claude: ClaudeConfig,`:

```rust
    pub codex: CodexConfig,
```

and after `pub clone_groups: Vec<CloneGroup>,`:

```rust
    pub codex_groups: Vec<CloneGroup>,
```

- [ ] **Step 5: Re-export `CodexConfig`**

In `crates/wire/src/lib.rs`, add `CodexConfig` to the `pub use config::{…}` list (alphabetical, after `ClaudeConfig`):

```rust
pub use config::{
    AppConfig, AppConfigRedacted, ChromaMode, ClaudeConfig, CloneGroup, CodexConfig,
    ConfigPutResponse, DockerConfig, EnvCheckRow, EnvVar, ImageInfo, ListenConfig, Preset,
    PresetRedacted, SetupEnv,
};
```

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo test -p wire codex_config_defaults_and_passthrough`
Expected: PASS.

- [ ] **Step 7: Regenerate TS bindings and confirm the suite is green**

Run: `cargo test -p wire`
Expected: PASS. New `frontend/app/lib/wire/CodexConfig.ts`; `AppConfigRedacted.ts` gains `codex: CodexConfig` + `codexGroups: Array<CloneGroup>`.

- [ ] **Step 8: Commit**

```bash
git add crates/wire/src/config.rs crates/wire/src/lib.rs frontend/app/lib/wire/
git commit -m "feat(wire): CodexConfig + AppConfig.codex/codexGroups"
```

---

### Task 3: control-server config merge — codex_groups wholesale-replace test

**Files:**
- Modify: `crates/control-server/src/config.rs` (add a test next to `merge_replaces_clone_groups_wholesale` ~183)

**Interfaces:**
- Consumes: `merge_update` (existing), `wire::CloneGroup`, `wire::CodexConfig`. No production code changes — `deep_merge` already merges the new non-secret fields generically; this task pins that behavior with a characterization test so a future refactor can't silently break codex config persistence.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `crates/control-server/src/config.rs` (note: bring `CodexConfig` into scope — the test module already `use`s the config types; add it if the module does not glob-import):

```rust
    #[test]
    fn merge_replaces_codex_groups_and_config() {
        use wire::CodexConfig;
        let mut base = AppConfig::default();
        base.codex_groups = vec![CloneGroup { name: "old".into(), accounts: vec!["a@o".into()] }];
        base.codex = CodexConfig { poll_secs: 600, usage_polling: true, ..Default::default() };
        // Editor sends the full group list + a codex config patch.
        let incoming = serde_json::json!({
            "codexGroups": [{ "name": "team", "accounts": ["a@o", "b@o"] }],
            "codex": { "pollSecs": 300, "usagePolling": false, "autoSwapOnExhaustion": true },
        });
        let merged = merge_update(&base, incoming).unwrap();
        assert_eq!(merged.codex_groups.len(), 1);
        assert_eq!(merged.codex_groups[0].name, "team");
        assert_eq!(merged.codex_groups[0].accounts, vec!["a@o".to_string(), "b@o".to_string()]);
        assert_eq!(merged.codex.poll_secs, 300);
        assert!(!merged.codex.usage_polling);
        assert!(merged.codex.auto_swap_on_exhaustion);
        // An empty array clears all codex groups.
        let cleared = merge_update(&merged, serde_json::json!({ "codexGroups": [] })).unwrap();
        assert!(cleared.codex_groups.is_empty());
        // Claude groups are untouched by a codex-only patch.
        let mut with_claude = base.clone();
        with_claude.clone_groups = vec![CloneGroup { name: "cl".into(), accounts: vec!["c@a".into()] }];
        let m2 = merge_update(&with_claude, serde_json::json!({ "codexGroups": [] })).unwrap();
        assert_eq!(m2.clone_groups.len(), 1, "codex patch must not disturb claude groups");
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p control-server merge_replaces_codex_groups_and_config`
Expected: FAIL — `AppConfig` in this crate resolves to `wire::AppConfig`, which now has the fields (from Task 2), so the failure here is the *assertion* wiring, not compilation. If Task 2 is complete this test may actually PASS immediately (deep_merge is generic). That is acceptable — its purpose is to lock the behavior. If it fails, the failure message identifies the exact merge gap to fix.

- [ ] **Step 3: No production change expected**

`deep_merge` (config.rs ~525) already recurses objects and replaces arrays/scalars, and `merge_update` round-trips through `serde_json::to_value(base)` → `deep_merge` → `from_value`, which now includes `codex`/`codexGroups`. If the test passes, proceed. If it fails, the only legitimate fix is that a new field was not `#[serde(default)]` — re-check Task 2.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p control-server merge_replaces_codex_groups_and_config`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/control-server/src/config.rs
git commit -m "test(config): pin codex config + codexGroups merge behavior"
```

---

### Task 4: Shared helpers — `clone_ops.rs` (extract + JWT + generalized run_clone_op)

**Files:**
- Create: `crates/control-server/src/clone_ops.rs`
- Modify: `crates/control-server/src/main.rs` (add `mod clone_ops;` after `mod claude;` ~line 11)
- Modify: `crates/control-server/src/docker.rs` (make `epoch_to_rfc3339` `pub(crate)` ~1657)
- Modify: `crates/control-server/src/provision.rs` (`run_clone_op` ~831 delegates to `clone_ops::run_clone_op`)
- Modify: `crates/control-server/src/claude.rs` (delete the private copies of `now_ms` ~58, `extract_json` ~216, `snippet` ~385, `rand_u64` ~689, `shuffle` ~700; import them from `clone_ops`)

**Interfaces:**
- Produces (all `pub(crate)` in `clone_ops`):
  - `fn now_ms() -> i64`
  - `fn extract_json(s: &str) -> &str`
  - `fn snippet(s: &str) -> String`
  - `fn rand_u64() -> u64`
  - `fn shuffle<T>(v: &mut [T])`
  - `async fn run_clone_op(app: &App, container: &str, script: &str, op: &str, extra: &[&str]) -> anyhow::Result<String>`
  - `fn jwt_claims(token: &str) -> Option<serde_json::Value>`
  - `fn jwt_exp_ms(token: &str) -> Option<i64>`
- Consumes: `crate::docker::{CLONE_USER, epoch_to_rfc3339}` (epoch_to_rfc3339 used by `codex.rs`, not here, but promoted in this task), `crate::app::App`.
- Note: `replace_provider_views` is added in Task 5, not here.

- [ ] **Step 1: Create `clone_ops.rs` with the helpers + JWT decoder + unit tests**

Create `crates/control-server/src/clone_ops.rs`:

```rust
//! Side-effect-free helpers shared by the `claude` and `codex` account subsystems.
//!
//! These were private to `claude.rs` when Claude was the only provider; `codex.rs`
//! needs the identical logic, so they live here (moved verbatim — no behavior change).
//! Two are new for Codex: a hand-rolled JWT claim decoder (`jwt_claims` / `jwt_exp_ms`;
//! the Codex OAuth response carries no `expires_in`, so expiry is read from the
//! access-token JWT `exp`) and the generalized `run_clone_op` (parameterized by guest
//! script, so each provider runs its own import script).

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};

use crate::app::App;
use crate::docker::CLONE_USER;

/// Milliseconds since the Unix epoch (0 if the clock is before the epoch).
pub(crate) fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// The `{…}` substring of `s` (login-shell noise can wrap the JSON), else trimmed `s`.
pub(crate) fn extract_json(s: &str) -> &str {
    match (s.find('{'), s.rfind('}')) {
        (Some(a), Some(b)) if b >= a => &s[a..=b],
        _ => s.trim(),
    }
}

/// A short `: <prefix>` of an error body for log lines (empty stays empty).
pub(crate) fn snippet(s: &str) -> String {
    if s.is_empty() { String::new() } else { format!(": {}", &s[..s.len().min(120)]) }
}

/// Non-cryptographic randomness from `/dev/urandom` (mirrors `files::rand_hex`),
/// enough to shuffle/tiebreak rotation; falls back to the clock.
pub(crate) fn rand_u64() -> u64 {
    use std::io::Read;
    let mut buf = [0u8; 8];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)).is_ok() {
        u64::from_le_bytes(buf)
    } else {
        now_ms() as u64
    }
}

/// In-place Fisher–Yates shuffle.
pub(crate) fn shuffle<T>(v: &mut [T]) {
    for i in (1..v.len()).rev() {
        let j = (rand_u64() % (i as u64 + 1)) as usize;
        v.swap(i, j);
    }
}

/// Run one import-script op (`status`|`read`|`clear`|`apply`) inside clone `container`
/// via `docker exec bash -s`, returning its raw stdout+stderr. `script` is the guest
/// script body (`include_str!`); `extra` are extra positional args (e.g. the base64
/// credentials for `apply`). Script args: `<user> <op> [extra…]`. Generalized from the
/// original claude-only `provision::run_clone_op` so each provider passes its own script.
pub(crate) async fn run_clone_op(
    app: &App,
    container: &str,
    script: &str,
    op: &str,
    extra: &[&str],
) -> Result<String> {
    let mut args: Vec<String> = vec![CLONE_USER.to_string(), op.to_string()];
    args.extend(extra.iter().map(|s| s.to_string()));

    let mut out = String::new();
    let code = app
        .docker
        .exec_script(container, script, &[], &args, |_stream, line| {
            out.push_str(line);
            out.push('\n');
        })
        .await?;

    if code == 0 {
        Ok(out)
    } else {
        bail!("clone op '{op}' failed in {container} (exit {code}): {}", out.trim());
    }
}

/// Decode a JWT's payload claims (the middle `.`-delimited segment, base64url, no
/// padding) into a JSON value. `None` if the token isn't a well-formed three-segment JWT
/// or the payload isn't valid base64url-encoded JSON. Hand-rolled base64url decode — no
/// new dependency (the standard-base64 *encoder* lives in `provision::b64_encode`).
pub(crate) fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = b64url_decode(payload)?;
    serde_json::from_slice(&bytes).ok()
}

/// The `exp` claim (seconds since epoch) of `token`, as epoch **milliseconds**. `None`
/// if the token has no numeric `exp` claim.
pub(crate) fn jwt_exp_ms(token: &str) -> Option<i64> {
    let exp = jwt_claims(token)?.get("exp")?.as_i64()?;
    Some(exp * 1000)
}

/// Decode base64url (RFC 4648 §5: `-`/`_`, padding optional). `None` on any invalid
/// character or a truncated 1-char final quantum.
fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let s = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for c in s.chunks(4) {
        if c.len() == 1 {
            return None; // a lone trailing char is not valid base64
        }
        let b0 = val(c[0])?;
        let b1 = val(c[1])?;
        out.push((b0 << 2) | (b1 >> 4));
        if c.len() >= 3 {
            let b2 = val(c[2])?;
            out.push(((b1 & 0x0f) << 4) | (b2 >> 2));
            if c.len() == 4 {
                let b3 = val(c[3])?;
                out.push(((b2 & 0x03) << 6) | b3);
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_strips_shell_noise() {
        assert_eq!(extract_json("noise {\"a\":1} tail"), "{\"a\":1}");
        assert_eq!(extract_json("  bare text  "), "bare text");
    }

    #[test]
    fn b64url_roundtrip_via_standard_encoder() {
        // Derive base64url from the existing standard-base64 encoder (+→-, /→_, drop =).
        for sample in ["", "f", "fo", "foo", "foob", "fooba", "foobar", "?>? subtle/+bytes"] {
            let std_b64 = crate::provision::b64_encode(sample.as_bytes());
            let url = std_b64.trim_end_matches('=').replace('+', "-").replace('/', "_");
            assert_eq!(b64url_decode(&url).unwrap(), sample.as_bytes(), "sample {sample:?}");
        }
        // Invalid input rejected.
        assert!(b64url_decode("A").is_none());
        assert!(b64url_decode("****").is_none());
    }

    #[test]
    fn jwt_claims_and_exp() {
        let payload = r#"{"exp":2000000000,"email":"a@openai.com","https://api.openai.com/auth":{"chatgpt_plan_type":"plus","chatgpt_account_id":"acc-1"}}"#;
        let b64 = crate::provision::b64_encode(payload.as_bytes());
        let url = b64.trim_end_matches('=').replace('+', "-").replace('/', "_");
        let jwt = format!("eyJhbGciOiJub25lIn0.{url}.sig");
        let claims = jwt_claims(&jwt).unwrap();
        assert_eq!(claims["email"], "a@openai.com");
        assert_eq!(claims["https://api.openai.com/auth"]["chatgpt_plan_type"], "plus");
        assert_eq!(claims["https://api.openai.com/auth"]["chatgpt_account_id"], "acc-1");
        assert_eq!(jwt_exp_ms(&jwt), Some(2_000_000_000_000));
        // Non-JWT input yields no claims.
        assert!(jwt_claims("not-a-jwt").is_none());
        assert!(jwt_exp_ms("a.b").is_none());
    }
}
```

- [ ] **Step 2: Register the module and promote `epoch_to_rfc3339`**

In `crates/control-server/src/main.rs`, add after `mod claude;` (~line 11):

```rust
mod clone_ops;
```

In `crates/control-server/src/docker.rs` (~line 1657), change the `epoch_to_rfc3339` signature from `fn epoch_to_rfc3339(secs: i64) -> String` to:

```rust
pub(crate) fn epoch_to_rfc3339(secs: i64) -> String {
```

- [ ] **Step 3: Run the new tests to verify they pass**

Run: `cargo test -p control-server clone_ops::`
Expected: PASS (`extract_json_strips_shell_noise`, `b64url_roundtrip_via_standard_encoder`, `jwt_claims_and_exp`).

- [ ] **Step 4: Delegate `provision::run_clone_op` to `clone_ops` and de-dup `claude.rs`**

In `crates/control-server/src/provision.rs`, replace the body of `run_clone_op` (~831-849) with a thin delegation (keeps the exact public signature, so `claude.rs`'s callers are untouched):

```rust
pub async fn run_clone_op(app: &App, container: &str, op: &str, extra: &[&str]) -> Result<String> {
    crate::clone_ops::run_clone_op(app, container, IMPORT_SCRIPT, op, extra).await
}
```

In `crates/control-server/src/claude.rs`:
- Delete the private `fn now_ms()` (~58-60), `fn extract_json()` (~216-221), `fn snippet()` (~385-387), `fn rand_u64()` (~689-697), `fn shuffle<T>()` (~700-705).
- Add an import near the top of the file (after the existing `use crate::app::App;` ~line 33):

```rust
use crate::clone_ops::{extract_json, now_ms, rand_u64, shuffle, snippet};
```

- [ ] **Step 5: Run the whole crate test suite (claude tests must stay green)**

Run: `cargo test -p control-server`
Expected: PASS — no warnings about unused imports; all existing `claude::tests` still pass (they exercise the moved helpers indirectly). If `rustc` warns that any of the imported names is unused, that name still has a private definition left in `claude.rs` — delete it.

- [ ] **Step 6: Commit**

```bash
git add crates/control-server/src/clone_ops.rs crates/control-server/src/main.rs \
        crates/control-server/src/docker.rs crates/control-server/src/provision.rs \
        crates/control-server/src/claude.rs
git commit -m "refactor: extract clone_ops shared helpers + hand-rolled JWT decode"
```

---

### Task 5: Coexistence — `replace_provider_views` + Claude poller merge fix

**Files:**
- Modify: `crates/control-server/src/clone_ops.rs` (add `replace_provider_views` + a unit test)
- Modify: `crates/control-server/src/claude.rs` (route `poll_inner`'s two publish sites through it ~502 and ~546-561; add `provider != Codex` filter to `five_hour_pct` ~719 and `auto_swap_exhausted` ~964-967)

**Interfaces:**
- Produces: `pub(crate) fn replace_provider_views(app: &App, provider: wire::Provider, views: Vec<wire::ClaudeUsage>, pinned: Option<&str>)` — replaces exactly the rows of `provider` in `ControlState.claude_accounts` (sorting them pinned-first-then-email), leaving every other provider's rows intact. Used by BOTH pollers so they never clobber each other.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `crates/control-server/src/clone_ops.rs`:

```rust
    #[test]
    fn replace_provider_views_preserves_other_provider() {
        use wire::{ClaudeUsage, Provider};
        fn view(email: &str, provider: Provider) -> ClaudeUsage {
            ClaudeUsage {
                id: format!("{email}|{provider:?}"),
                email: email.into(),
                provider: Some(provider),
                active: false,
                assignable: Some(true),
                error: None,
                stale: None,
                last_updated: 0,
                five_hour: None,
                seven_day: None,
                spend: None,
            }
        }
        let app = crate::app::App::test_app();
        // Seed: two claude, one codex.
        app.store.mutate(|s| {
            s.claude_accounts =
                vec![view("a@c", Provider::Claude), view("b@c", Provider::Claude), view("z@o", Provider::Codex)];
        });
        // A codex poll publishes a new codex set (pinned y@o first).
        replace_provider_views(
            &app,
            Provider::Codex,
            vec![view("z@o", Provider::Codex), view("y@o", Provider::Codex)],
            Some("y@o"),
        );
        let st = app.store.get();
        // Both claude rows still present.
        assert_eq!(st.claude_accounts.iter().filter(|u| u.provider == Some(Provider::Claude)).count(), 2);
        // Codex rows are the new set, pinned first.
        let codex: Vec<_> = st
            .claude_accounts
            .iter()
            .filter(|u| u.provider == Some(Provider::Codex))
            .map(|u| u.email.as_str())
            .collect();
        assert_eq!(codex, vec!["y@o", "z@o"]);
        // An empty codex publish drops all codex rows but keeps claude.
        replace_provider_views(&app, Provider::Codex, vec![], None);
        let st2 = app.store.get();
        assert_eq!(st2.claude_accounts.len(), 2);
        assert!(st2.claude_accounts.iter().all(|u| u.provider == Some(Provider::Claude)));
    }
```

This test needs a test-only `App` constructor. Add it in this step too — in `crates/control-server/src/app.rs`, add inside `impl App` (gated to tests):

```rust
    /// A minimal App backed by a throwaway temp data dir, for unit tests in sibling
    /// modules (state + stores are file-isolated; Docker is constructed I/O-free).
    #[cfg(test)]
    pub fn test_app() -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rmng-cloneops-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store =
            std::sync::Arc::new(crate::state::StateStore::load(dir.join("state.json")).unwrap());
        let cfg = wire::AppConfig { data_dir: dir.to_string_lossy().into_owned(), ..Default::default() };
        Self::new(store, cfg)
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p control-server replace_provider_views_preserves_other_provider`
Expected: FAIL — `replace_provider_views` not found.

- [ ] **Step 3: Implement `replace_provider_views`**

Add to `crates/control-server/src/clone_ops.rs` (below `shuffle`, above the JWT section):

```rust
/// Stable ordering rank for a provider so a merged `claude_accounts` list groups Claude
/// rows before Codex rows deterministically regardless of which poller wrote last.
fn provider_rank(p: Option<wire::Provider>) -> u8 {
    match p {
        Some(wire::Provider::Claude) => 0,
        Some(wire::Provider::Codex) => 1,
        None => 2,
    }
}

/// Publish `views` (all of `provider`) into `ControlState.claude_accounts`, replacing
/// exactly this provider's existing rows and leaving every other provider's rows intact.
/// `views` are sorted pinned-email-first then alphabetical; the combined list is then
/// stable-sorted by provider rank so grouping is deterministic. This is what lets the
/// Claude and Codex pollers coexist without clobbering each other (each poller previously
/// did `s.claude_accounts = views`, which would erase the other provider).
pub(crate) fn replace_provider_views(
    app: &App,
    provider: wire::Provider,
    mut views: Vec<wire::ClaudeUsage>,
    pinned: Option<&str>,
) {
    views.sort_by(|a, b| {
        let ap = Some(a.email.as_str()) == pinned;
        let bp = Some(b.email.as_str()) == pinned;
        if ap != bp {
            return if ap { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater };
        }
        a.email.cmp(&b.email)
    });
    app.store.mutate(|s| {
        let mut merged: Vec<wire::ClaudeUsage> =
            s.claude_accounts.iter().filter(|u| u.provider != Some(provider)).cloned().collect();
        merged.extend(views.iter().cloned());
        merged.sort_by_key(|u| provider_rank(u.provider));
        s.claude_accounts = merged;
    });
}
```

Add `use wire;` is unnecessary (the crate re-exports at `wire::`); the function already references `wire::` paths.

- [ ] **Step 4: Route Claude's `poll_inner` through it + add provider filters**

In `crates/control-server/src/claude.rs`:

(a) In `poll_inner`, replace the empty-accounts early clear (~502):

```rust
    if accts.is_empty() {
        crate::clone_ops::replace_provider_views(app, wire::Provider::Claude, Vec::new(), None);
        return Ok(false);
    }
```

(b) Replace the publish site (~546-561). Delete the `for v in &mut views { v.assignable = Some(true); }` + the pinned `views.sort_by(...)` + `app.store.mutate(|s| s.claude_accounts = views);` block, and replace with:

```rust
    // Every imported account can run a clone (the server owns its token lifecycle).
    for v in &mut views {
        v.assignable = Some(true);
    }
    let cfg = app.config();
    crate::clone_ops::replace_provider_views(
        app,
        wire::Provider::Claude,
        views,
        cfg.claude.pinned_email.as_deref(),
    );
```

(Keep the subsequent `push_stale_tokens(app).await;` and `if cfg.claude.auto_swap_on_exhaustion { … }` lines. Note `cfg` is now bound above; remove the later duplicate `let cfg = app.config();` if one remains so there is a single binding.)

(c) In `five_hour_pct` (~719), add the provider filter so a Codex account sharing an email can't be mistaken for the Claude one:

```rust
fn five_hour_pct(app: &App, email: &str) -> f64 {
    app.store
        .get()
        .claude_accounts
        .iter()
        .filter(|u| u.provider != Some(wire::Provider::Codex))
        .find(|u| u.email == email)
        .and_then(|u| u.five_hour.as_ref())
        .map(|w| w.pct)
        .unwrap_or(0.0)
}
```

(d) In `auto_swap_exhausted` (~964-967), filter the usage map to non-Codex:

```rust
    let usage: HashMap<String, &ClaudeUsage> = st
        .claude_accounts
        .iter()
        .filter(|u| u.provider != Some(wire::Provider::Codex))
        .map(|u| (u.email.clone(), u))
        .collect();
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p control-server`
Expected: PASS — `replace_provider_views_preserves_other_provider` plus all existing `claude::tests`. (The claude assignment tests do not touch the store, so they are unaffected.)

- [ ] **Step 6: Commit**

```bash
git add crates/control-server/src/clone_ops.rs crates/control-server/src/app.rs \
        crates/control-server/src/claude.rs
git commit -m "fix(claude): provider-aware view merge so codex poller can coexist"
```

---

### Task 6: Guest script — `codex-import.sh`

**Files:**
- Create: `crates/control-server/scripts/codex-import.sh`

**Interfaces:**
- Produces: a guest script with the same op contract as `claude-import.sh` but targeting `~/.codex/auth.json`:
  - `status` — print `~/.codex/auth.json` (or `{}` if absent); never fails.
  - `read` — print `~/.codex/auth.json` (fails if absent).
  - `clear` — delete it, print `CLEARED`.
  - `apply <b64>` — write it 0600 from base64 arg `$3`, print `OK`.
- Consumed by `codex.rs` via `include_str!("../scripts/codex-import.sh")` (Task 7).

- [ ] **Step 1: Write the script**

Create `crates/control-server/scripts/codex-import.sh`:

```bash
#!/usr/bin/env bash
# Runs INSIDE the target clone container (the control-server streams this over
# `docker exec bash -s`). Executes a Codex credential op as the clone user, printing
# the raw result to stdout. Sibling of claude-import.sh; targets ~/.codex/auth.json.
#
#   codex-import.sh <user> status|read|clear|apply [b64]
#     status — contents of ~/.codex/auth.json, or `{}` if absent (never fails the script;
#              codex has no clean JSON `login status`, so identity is read from the file)
#     read   — contents of ~/.codex/auth.json (fails if absent)
#     clear  — delete that auth file, then print CLEARED
#     apply  — write ~/.codex/auth.json from base64 arg $3 (the full JSON: real access +
#              id token + account_id, refresh_token empty, last_refresh now), print OK.
#              Does NOT restart anything — codex re-reads auth.json per invocation.
set -euo pipefail
USER="${1:-rmng}"; OP="$2"
# Force bash with an explicit PATH rather than the user's login shell (clones default to
# fish, which prints tty/parse noise). Mirrors claude-import.sh exactly.
inct() { runuser -l "$USER" -s /bin/bash -c "export PATH=\$HOME/.local/bin:\$PATH; $1"; }
case "$OP" in
  status) inct 'cat "$HOME/.codex/auth.json" 2>/dev/null || echo "{}"' ;;
  read)   inct 'cat "$HOME/.codex/auth.json"' ;;
  clear)  inct 'rm -f "$HOME/.codex/auth.json"'; echo CLEARED ;;
  apply)  B64="$3"; inct "umask 077; mkdir -p \"\$HOME/.codex\"; echo '$B64' | base64 -d > \"\$HOME/.codex/auth.json\"; chmod 600 \"\$HOME/.codex/auth.json\"; echo OK" ;;
  *)      echo "unknown op: $OP" >&2; exit 2 ;;
esac
```

- [ ] **Step 2: Static-check the script**

Run: `bash -n crates/control-server/scripts/codex-import.sh`
Expected: no output, exit 0 (valid syntax).

- [ ] **Step 3: Behavioral smoke test against a temp HOME (no container needed)**

The script's core is the `case` dispatch. Verify `apply` → `read` round-trips and `clear` works by invoking it as the current user against a throwaway HOME. Run:

```bash
TMPH="$(mktemp -d)"; \
B64="$(printf '{"OPENAI_API_KEY":null,"tokens":{"access_token":"eyJx"}}' | base64)"; \
HOME="$TMPH" runuser() { shift 3; bash -lc "$@"; }; \
export -f runuser 2>/dev/null; \
HOME="$TMPH" bash crates/control-server/scripts/codex-import.sh "$(id -un)" apply "$B64" && \
HOME="$TMPH" bash crates/control-server/scripts/codex-import.sh "$(id -un)" read && \
HOME="$TMPH" bash crates/control-server/scripts/codex-import.sh "$(id -un)" clear; \
rm -rf "$TMPH"
```

Expected output: `OK`, then the JSON `{"OPENAI_API_KEY":null,"tokens":{"access_token":"eyJx"}}`, then `CLEARED`.
Note: this shims `runuser` (unavailable/again privileged on macOS) so the `inct` helper runs the command in the current shell against `$TMPH`. This validates the op dispatch and the apply/read/clear file handling; the real `runuser -l` path is exercised on staging (Verification §3).

- [ ] **Step 4: Commit**

```bash
git add crates/control-server/scripts/codex-import.sh
git commit -m "feat(scripts): codex-import.sh guest ops for ~/.codex/auth.json"
```

---

### Task 7: `codex.rs` part 1 — store + import (JWT identity)

**Files:**
- Create: `crates/control-server/src/codex.rs` (first section)
- Modify: `crates/control-server/src/main.rs` (add `mod codex;` after `mod clone_ops;`)
- Modify: `crates/control-server/src/app.rs` (add `pub codex: Arc<CodexStore>` + construct it in `App::new`)

**Interfaces:**
- Produces (used by later tasks + `web.rs`):
  - `pub struct StoredCodexAccount { id, email, account_id, plan, active, access_token, id_token, refresh_token, expires_at }`
  - `pub struct CodexStore` with `load(data_dir) -> Self`, `forget_pushed(&self, host_id)`, and the same private helpers as `ClaudeStore` (`snapshot`, `get_by_email`, `emails`, `update_account`, `save`, and the fields `accounts`/`last_good`/`polling`/`refresh_gate`/`pushed`).
  - `pub struct CodexAuth { email, plan, account_id, id_token, access_token, refresh_token }`
  - `pub async fn check_clone_auth(app, host) -> Result<CodexAuth>`
  - `pub struct ImportResult { email, cleared }`
  - `pub async fn import_clone_account(app, host) -> Result<ImportResult>`
- Consumes: `crate::clone_ops::{now_ms, extract_json, run_clone_op, jwt_claims}`, `crate::app::App`, `wire::{Host, Provider}`.

- [ ] **Step 1: Write the failing test (auth parse + identity + store round-trip)**

Create `crates/control-server/src/codex.rs` with the header, imports, constants, the store, the import structs, `check_clone_auth`/`import_clone_account` (implemented in Step 3), and this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn jwt_with(payload: &str) -> String {
        let b64 = crate::provision::b64_encode(payload.as_bytes());
        let url = b64.trim_end_matches('=').replace('+', "-").replace('/', "_");
        format!("eyJhbGciOiJub25lIn0.{url}.sig")
    }

    fn sample_account() -> StoredCodexAccount {
        StoredCodexAccount {
            id: "codex:acc-1".into(),
            email: "z@openai.com".into(),
            account_id: "acc-1".into(),
            plan: "plus".into(),
            active: false,
            access_token: "eyJaccess".into(),
            id_token: "eyJid".into(),
            refresh_token: "rt-1".into(),
            expires_at: 0,
        }
    }

    #[test]
    fn parses_codex_auth_identity() {
        let id_token = jwt_with(
            r#"{"email":"z@openai.com","exp":2000000000,"https://api.openai.com/auth":{"chatgpt_plan_type":"plus","chatgpt_account_id":"acc-1"}}"#,
        );
        let file = format!(
            r#"{{"OPENAI_API_KEY":null,"tokens":{{"id_token":"{id_token}","access_token":"eyJaccess","refresh_token":"rt-1","account_id":"acc-1"}},"last_refresh":"2026-07-01T00:00:00Z"}}"#
        );
        let auth = parse_codex_auth(&file).unwrap();
        assert_eq!(auth.email, "z@openai.com");
        assert_eq!(auth.plan, "plus");
        assert_eq!(auth.account_id, "acc-1");
        assert_eq!(auth.refresh_token, "rt-1");
        assert_eq!(auth.access_token, "eyJaccess");
    }

    #[test]
    fn rejects_api_key_login() {
        // A codex CLI signed in with an API key has OPENAI_API_KEY set — not importable.
        let file = r#"{"OPENAI_API_KEY":"sk-proj-xxx","tokens":null}"#;
        assert!(parse_codex_auth(file).is_err());
        // Missing tokens block is also an error.
        assert!(parse_codex_auth(r#"{"OPENAI_API_KEY":null}"#).is_err());
    }

    #[test]
    fn store_upsert_roundtrip() {
        let dir = std::env::temp_dir().join(format!("rmng-codex-store-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = CodexStore::load(dir.to_str().unwrap());
        store.update_account(&sample_account()).unwrap();
        // Second store loading the same file sees the account.
        let reloaded = CodexStore::load(dir.to_str().unwrap());
        assert_eq!(reloaded.emails(), vec!["z@openai.com".to_string()]);
        assert_eq!(reloaded.get_by_email("z@openai.com").unwrap().account_id, "acc-1");
        std::fs::remove_dir_all(&dir).ok();
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p control-server codex::tests`
Expected: FAIL — `codex` module not declared / `parse_codex_auth` missing (compile error). First add `mod codex;` to `main.rs` after `mod clone_ops;` so the module is compiled.

- [ ] **Step 3: Implement part 1 of `codex.rs`**

Prepend to `crates/control-server/src/codex.rs` (before the test module):

```rust
//! Codex (OpenAI/ChatGPT) accounts — the sibling of `claude.rs`. Same server-owned
//! single-token model: the server holds each account's OAuth pair in the 0600 store
//! `codex-accounts.json`, refreshes access tokens itself (expiry decoded from the
//! access-token JWT — the Codex OAuth response has no `expires_in`), injects only the
//! short-lived access + id token + account_id into a clone's `~/.codex/auth.json` with an
//! empty refresh token, and re-pushes on every rotation. Importing harvests the OAuth
//! triple from a clone already signed in to Codex via ChatGPT, then clears the clone's
//! auth.json so its CLI can never rotate the refresh token the server now owns.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use wire::{ClaudeSpend, ClaudeUsage, ClaudeUsageWindow, CloneGroup, Host};

use crate::app::App;
use crate::clone_ops::{extract_json, jwt_claims, now_ms, rand_u64, run_clone_op, shuffle, snippet};

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Refresh an access token this far before its expiry (must exceed the worst-case
/// poll gap). Matches claude's lead.
const REFRESH_LEAD_MS: i64 = 60 * 60 * 1000;
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const STAGGER: Duration = Duration::from_millis(400);

// scoring knobs — copied from claude.rs (identical semantics).
const SESSION_HEADROOM_PCT: f64 = 40.0;
const SEVEN_DAY_CAP_PCT: f64 = 95.0;
const ROTATE_MAX_FIVE_HOUR_PCT: f64 = 90.0;
const ROTATE_SECS: u64 = 600;

const IMPORT_SCRIPT: &str = include_str!("../scripts/codex-import.sh");

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredCodexAccount {
    /// `codex:<account_id>`.
    pub id: String,
    pub email: String,
    pub account_id: String,
    #[serde(default)]
    pub plan: String,
    #[serde(default)]
    pub active: bool,
    pub access_token: String,
    #[serde(default)]
    pub id_token: String,
    pub refresh_token: String,
    #[serde(default)]
    pub expires_at: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct AccountsFile {
    #[serde(default)]
    accounts: Vec<StoredCodexAccount>,
}

pub struct CodexStore {
    accounts: Mutex<Vec<StoredCodexAccount>>,
    last_good: Mutex<HashMap<String, ClaudeUsage>>,
    path: PathBuf,
    polling: Mutex<bool>,
    refresh_gate: tokio::sync::Mutex<()>,
    pushed: Mutex<HashMap<String, String>>,
}

impl CodexStore {
    pub fn load(data_dir: &str) -> Self {
        let path = std::env::var("RMNG_CODEX_ACCOUNTS_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| Path::new(data_dir).join("codex-accounts.json"));
        let accounts = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<AccountsFile>(&s).ok())
            .map(|f| f.accounts)
            .unwrap_or_default();
        Self {
            accounts: Mutex::new(accounts),
            last_good: Mutex::new(HashMap::new()),
            path,
            polling: Mutex::new(false),
            refresh_gate: tokio::sync::Mutex::new(()),
            pushed: Mutex::new(HashMap::new()),
        }
    }

    fn save(&self, accounts: &[StoredCodexAccount]) -> Result<()> {
        if let Some(d) = self.path.parent() {
            std::fs::create_dir_all(d).ok();
        }
        let tmp = self.path.with_extension(format!("tmp.{}", std::process::id()));
        let body =
            serde_json::to_string_pretty(&AccountsFile { accounts: accounts.to_vec() })? + "\n";
        std::fs::write(&tmp, body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).ok();
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    fn snapshot(&self) -> Vec<StoredCodexAccount> {
        self.accounts.lock().unwrap().clone()
    }

    fn get_by_email(&self, email: &str) -> Option<StoredCodexAccount> {
        self.accounts.lock().unwrap().iter().find(|a| a.email == email).cloned()
    }

    fn emails(&self) -> Vec<String> {
        self.accounts.lock().unwrap().iter().map(|a| a.email.clone()).collect()
    }

    fn update_account(&self, acct: &StoredCodexAccount) -> Result<()> {
        let mut accounts = self.accounts.lock().unwrap();
        match accounts.iter_mut().find(|a| a.id == acct.id) {
            Some(existing) => *existing = acct.clone(),
            None => accounts.push(acct.clone()),
        }
        self.save(&accounts)
    }

    pub fn forget_pushed(&self, host_id: &str) {
        self.pushed.lock().unwrap().remove(host_id);
    }
}

// --- import from a signed-in clone ----------------------------------------

/// The on-disk `~/.codex/auth.json` shape.
#[derive(Deserialize)]
struct CodexAuthFile {
    #[serde(rename = "OPENAI_API_KEY", default)]
    openai_api_key: Option<String>,
    #[serde(default)]
    tokens: Option<CodexTokens>,
}

#[derive(Deserialize)]
struct CodexTokens {
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
}

/// The identity + tokens harvested from a signed-in clone.
pub struct CodexAuth {
    pub email: String,
    pub plan: String,
    pub account_id: String,
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
}

pub struct ImportResult {
    pub email: String,
    pub cleared: bool,
}

/// Parse + validate a `~/.codex/auth.json` body into a [`CodexAuth`]. Requires a
/// ChatGPT login (`OPENAI_API_KEY` null/absent) with a full `tokens` block, and decodes
/// the id_token JWT for email / plan / account_id (account_id falls back to the JWT claim
/// when absent from `tokens`).
fn parse_codex_auth(raw: &str) -> Result<CodexAuth> {
    let file: CodexAuthFile = serde_json::from_str(extract_json(raw))
        .map_err(|_| anyhow::anyhow!("couldn't parse ~/.codex/auth.json"))?;
    if file.openai_api_key.as_deref().is_some_and(|k| !k.is_empty()) {
        bail!("this clone is signed in to Codex with an API key, not a ChatGPT subscription");
    }
    let tokens = file.tokens.context("~/.codex/auth.json has no tokens block (not signed in?)")?;
    let (Some(id_token), Some(access_token), Some(refresh_token)) =
        (tokens.id_token, tokens.access_token, tokens.refresh_token)
    else {
        bail!("~/.codex/auth.json is missing its id/access/refresh tokens");
    };
    let claims = jwt_claims(&id_token).context("codex id_token is not a decodable JWT")?;
    let auth_ns = claims.get("https://api.openai.com/auth");
    let email = claims
        .get("email")
        .and_then(|v| v.as_str())
        .context("codex id_token has no email claim")?
        .to_string();
    let plan = auth_ns
        .and_then(|a| a.get("chatgpt_plan_type"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let account_id = tokens
        .account_id
        .filter(|s| !s.is_empty())
        .or_else(|| {
            auth_ns
                .and_then(|a| a.get("chatgpt_account_id"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .context("codex auth has no account_id (in tokens or id_token claim)")?;
    Ok(CodexAuth { email, plan, account_id, id_token, access_token, refresh_token })
}

/// Confirm clone `host` is signed in to Codex via ChatGPT and return its identity +
/// tokens. Reads `~/.codex/auth.json` (codex has no clean JSON `login status`).
pub async fn check_clone_auth(app: &App, host: &Host) -> Result<CodexAuth> {
    if !host.managed {
        bail!("host '{}' is not a managed clone; only clones can be imported", host.id);
    }
    let raw = run_clone_op(app, &host.id, IMPORT_SCRIPT, "status", &[]).await?;
    parse_codex_auth(&raw).map_err(|e| {
        anyhow::anyhow!("{e} — is codex installed and signed in on '{}'?", host.id)
    })
}

/// Import a Codex account from a signed-in clone: harvest the OAuth triple, upsert into
/// the 0600 store (by id), then delete the clone's auth.json so its CLI can't rotate the
/// refresh token the server now owns.
pub async fn import_clone_account(app: &App, host: &Host) -> Result<ImportResult> {
    if !host.managed {
        bail!("host '{}' is not a managed clone; only clones can be imported", host.id);
    }
    let auth = check_clone_auth(app, host).await?;
    let stored = StoredCodexAccount {
        id: format!("codex:{}", auth.account_id),
        email: auth.email.clone(),
        account_id: auth.account_id,
        plan: auth.plan,
        active: false,
        access_token: auth.access_token,
        id_token: auth.id_token,
        refresh_token: auth.refresh_token,
        expires_at: 0,
    };
    {
        let mut accts = app.codex.accounts.lock().unwrap();
        let mut by_id: HashMap<String, StoredCodexAccount> =
            accts.drain(..).map(|a| (a.id.clone(), a)).collect();
        by_id.insert(stored.id.clone(), stored);
        let mut next: Vec<_> = by_id.into_values().collect();
        next.sort_by(|a, b| a.email.cmp(&b.email));
        app.codex.save(&next)?;
        *accts = next;
    }
    let cleared = match run_clone_op(app, &host.id, IMPORT_SCRIPT, "clear", &[]).await {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("codex import: clearing '{}' auth.json failed: {e}", host.id);
            false
        }
    };
    app.codex.forget_pushed(&host.id);
    tracing::info!("imported Codex account {} from '{}' (cleared={cleared})", auth.email, host.id);
    Ok(ImportResult { email: auth.email, cleared })
}
```

Note: some imported names (`ClaudeSpend`, `ClaudeUsageWindow`, `CloneGroup`, `rand_u64`, `shuffle`, `snippet`, `STAGGER`, scoring consts) are used only by Tasks 8–9. To keep this task's build warning-free, add `#![allow(dead_code)]`-free scaffolding is NOT desired — instead, add a temporary `#[allow(unused_imports)]` on the `use crate::clone_ops::…` line and a `#[allow(dead_code)]` on the constants block, with a `// TODO(task-8/9): consumed by usage + scoring` comment. Remove both allows in Task 9.

- [ ] **Step 4: Add the `CodexStore` to `App`**

In `crates/control-server/src/app.rs`:
- Add the field to `struct App` after `pub claude: Arc<ClaudeStore>,`:

```rust
    /// Codex secret store + usage cache (sibling of `claude`).
    pub codex: Arc<crate::codex::CodexStore>,
```

- In `App::new`, after `let claude = Arc::new(ClaudeStore::load(&cfg.data_dir));`, add:

```rust
        let codex = Arc::new(crate::codex::CodexStore::load(&cfg.data_dir));
```

- Add `codex,` to the struct literal returned by `App::new` (after `claude,`).

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p control-server codex::tests`
Expected: PASS (`parses_codex_auth_identity`, `rejects_api_key_login`, `store_upsert_roundtrip`).

- [ ] **Step 6: Commit**

```bash
git add crates/control-server/src/codex.rs crates/control-server/src/main.rs \
        crates/control-server/src/app.rs
git commit -m "feat(codex): store + import from signed-in clone (JWT identity)"
```

---

### Task 8: `codex.rs` part 2 — refresh, token push, usage

**Files:**
- Modify: `crates/control-server/src/codex.rs` (add the refresh/token/usage section + tests)

**Interfaces:**
- Consumes: Task 7's `StoredCodexAccount` / `CodexStore` / constants; `crate::clone_ops::jwt_exp_ms`; `crate::provision::b64_encode`; `crate::docker::epoch_to_rfc3339`.
- Produces (used by Task 9 + Tasks 10–12):
  - `pub async fn fresh_access_token(app, email) -> Result<(StoredCodexAccount, bool)>` (whole account — apply needs id_token + account_id)
  - `pub async fn apply_clone_token(app, host_id, acct: &StoredCodexAccount) -> Result<()>`
  - `pub async fn clear_clone_token(app, host_id) -> Result<()>`
  - `pub async fn push_account_to_clone(app, host_id, email) -> Result<()>`
  - `pub async fn push_stale_tokens(app)`
  - `fn to_usage(acct, RawUsage) -> ClaudeUsage`, `fn codex_base(acct) -> ClaudeUsage`, `async fn fetch_usage(http, token, account_id) -> Result<RawUsage>`

- [ ] **Step 1: Write the failing tests (auth.json shape + usage mapping by window seconds + expiry from JWT)**

Add these to the `codex::tests` module:

```rust
    #[test]
    fn injected_auth_json_shape() {
        let j = auth_json(&sample_account());
        let v: serde_json::Value = serde_json::from_str(&j).unwrap();
        assert!(v["OPENAI_API_KEY"].is_null());
        assert_eq!(v["tokens"]["access_token"], "eyJaccess");
        assert_eq!(v["tokens"]["id_token"], "eyJid");
        assert_eq!(v["tokens"]["account_id"], "acc-1");
        // Refresh token is emptied — the clone can never rotate the server-owned token.
        assert_eq!(v["tokens"]["refresh_token"], "");
        // last_refresh is a present RFC3339 string (defeats the CLI's 8-day fallback).
        assert!(v["last_refresh"].as_str().is_some_and(|s| s.contains('T')));
    }

    #[test]
    fn usage_maps_by_window_seconds_not_order() {
        // primary=5h, secondary=weekly.
        let body = r#"{"plan_type":"plus","rate_limit":{
            "primary_window":{"used_percent":12.0,"limit_window_seconds":18000,"reset_at":"2026-07-03T00:00:00Z"},
            "secondary_window":{"used_percent":3.0,"limit_window_seconds":604800,"reset_at":"2026-07-10T00:00:00Z"}
        }}"#;
        let u = to_usage(&sample_account(), serde_json::from_str(body).unwrap());
        assert_eq!(u.five_hour.as_ref().unwrap().pct, 12.0);
        assert_eq!(u.seven_day.as_ref().unwrap().pct, 3.0);
        assert_eq!(u.provider, Some(wire::Provider::Codex));
        assert!(u.spend.is_none());
        // Swapped field order: still classified by limit_window_seconds.
        let swapped = r#"{"rate_limit":{
            "primary_window":{"used_percent":3.0,"limit_window_seconds":604800},
            "secondary_window":{"used_percent":12.0,"limit_window_seconds":18000}
        }}"#;
        let u2 = to_usage(&sample_account(), serde_json::from_str(swapped).unwrap());
        assert_eq!(u2.five_hour.as_ref().unwrap().pct, 12.0);
        assert_eq!(u2.seven_day.as_ref().unwrap().pct, 3.0);
    }

    #[test]
    fn expiry_decoded_from_access_jwt() {
        // apply_expiry_from_jwt sets expires_at from the access token's exp claim.
        let mut acct = sample_account();
        acct.access_token = jwt_with(r#"{"exp":2000000000}"#);
        set_expiry_from_access(&mut acct);
        assert_eq!(acct.expires_at, 2_000_000_000_000);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p control-server codex::tests::injected_auth_json_shape`
Expected: FAIL — `auth_json` / `to_usage` / `set_expiry_from_access` missing (compile error).

- [ ] **Step 3: Implement the refresh/token/usage section**

Add to `codex.rs` (after `import_clone_account`, before the tests):

```rust
// --- token refresh + push -------------------------------------------------

fn is_expired(expires_at: i64) -> bool {
    now_ms() + REFRESH_LEAD_MS >= expires_at
}

/// Set `acct.expires_at` from its access-token JWT `exp` claim; if the token isn't a
/// decodable JWT, fall back to a conservative 55-minute lifetime so the account still
/// refreshes before the CLI's 5-minute trigger.
fn set_expiry_from_access(acct: &mut StoredCodexAccount) {
    acct.expires_at =
        crate::clone_ops::jwt_exp_ms(&acct.access_token).unwrap_or_else(|| now_ms() + 55 * 60 * 1000);
}

#[derive(Deserialize)]
struct RefreshResp {
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
}

/// Refresh `acct`'s access token unconditionally (rotates the single-use refresh token).
/// The OAuth response carries no `expires_in`, so expiry is decoded from the new access
/// token's JWT. Mutates `acct` in place; the caller persists.
async fn refresh_account(http: &reqwest::Client, acct: &mut StoredCodexAccount) -> Result<()> {
    let resp = http
        .post(OAUTH_TOKEN_URL)
        .timeout(FETCH_TIMEOUT)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": acct.refresh_token,
            "client_id": OAUTH_CLIENT_ID,
        }))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        bail!("refresh {}{}", status.as_u16(), snippet(&text));
    }
    let data: RefreshResp = resp.json().await?;
    if let Some(a) = data.access_token {
        acct.access_token = a;
    }
    if let Some(i) = data.id_token {
        acct.id_token = i;
    }
    if let Some(r) = data.refresh_token {
        acct.refresh_token = r;
    }
    set_expiry_from_access(acct);
    Ok(())
}

/// `email`'s current account, refreshed (and persisted) first if within
/// [`REFRESH_LEAD_MS`] of expiry. Returns `(account, rotated)`. Runs under the store's
/// refresh gate so concurrent callers can't burn the same single-use refresh token.
pub async fn fresh_access_token(app: &App, email: &str) -> Result<(StoredCodexAccount, bool)> {
    let _gate = app.codex.refresh_gate.lock().await;
    let mut acct = app
        .codex
        .get_by_email(email)
        .with_context(|| format!("no imported Codex account for '{email}'"))?;
    if !is_expired(acct.expires_at) {
        return Ok((acct, false));
    }
    refresh_account(&app.http, &mut acct).await?;
    app.codex.update_account(&acct)?;
    Ok((acct, true))
}

/// The `~/.codex/auth.json` body that runs codex under `acct`'s current tokens. The
/// refresh token is emptied and `last_refresh` set to now so the clone's CLI never tries
/// to rotate or abandon the server-owned token (see the module + PROTOCOL docs).
fn auth_json(acct: &StoredCodexAccount) -> String {
    let last_refresh = crate::docker::epoch_to_rfc3339(now_ms() / 1000);
    format!(
        r#"{{"OPENAI_API_KEY":null,"tokens":{{"id_token":"{id}","access_token":"{access}","refresh_token":"","account_id":"{acct_id}"}},"last_refresh":"{last_refresh}"}}"#,
        id = acct.id_token,
        access = acct.access_token,
        acct_id = acct.account_id,
    )
}

/// Install `acct`'s tokens into clone `host_id`'s `~/.codex/auth.json`. Sanity-checks the
/// access token is a JWT (`eyJ…`). Best-effort hot-swap; codex re-reads auth.json per call.
pub async fn apply_clone_token(app: &App, host_id: &str, acct: &StoredCodexAccount) -> Result<()> {
    if !acct.access_token.starts_with("eyJ") {
        bail!("refusing to apply a non-JWT codex access token");
    }
    let b64 = crate::provision::b64_encode(auth_json(acct).as_bytes());
    let out = run_clone_op(app, host_id, IMPORT_SCRIPT, "apply", &[&b64]).await?;
    if out.contains("OK") {
        Ok(())
    } else {
        bail!("codex token apply produced unexpected output: {}", out.trim());
    }
}

/// Remove clone `host_id`'s `~/.codex/auth.json`, leaving it with no Codex token.
pub async fn clear_clone_token(app: &App, host_id: &str) -> Result<()> {
    let out = run_clone_op(app, host_id, IMPORT_SCRIPT, "clear", &[]).await?;
    if out.contains("CLEARED") {
        Ok(())
    } else {
        bail!("codex token clear produced unexpected output: {}", out.trim());
    }
}

/// Refresh-if-needed and install `email`'s tokens into clone `host_id`, recording the
/// push. If the refresh rotated the token, fan it out to the account's other clones.
pub async fn push_account_to_clone(app: &App, host_id: &str, email: &str) -> Result<()> {
    let (acct, rotated) = fresh_access_token(app, email).await?;
    apply_clone_token(app, host_id, &acct).await?;
    app.codex.pushed.lock().unwrap().insert(host_id.to_string(), acct.access_token.clone());
    if rotated {
        let app = app.clone();
        tokio::spawn(async move { push_stale_tokens(&app).await });
    }
    Ok(())
}

/// Reconcile pass: every clone assigned a Codex account gets that account's current
/// access token, unless the last successful push already delivered it. Mirrors
/// `claude::push_stale_tokens`, reading `Host.codex_account_email`.
pub async fn push_stale_tokens(app: &App) {
    let mut first = true;
    for host in app.store.get().hosts {
        let Some(email) = host.codex_account_email.as_deref() else { continue };
        if !host.managed {
            continue;
        }
        let Some(acct) = app.codex.get_by_email(email) else { continue };
        let stale = app.codex.pushed.lock().unwrap().get(&host.id) != Some(&acct.access_token);
        if !stale {
            continue;
        }
        if !first {
            tokio::time::sleep(STAGGER).await;
        }
        first = false;
        match apply_clone_token(app, &host.id, &acct).await {
            Ok(()) => {
                app.codex.pushed.lock().unwrap().insert(host.id.clone(), acct.access_token.clone());
                tracing::info!("pushed fresh codex token ({email}) to {}", host.id);
            }
            Err(e) => tracing::warn!(
                "pushing codex token ({email}) to {} failed (retried next pass): {e}",
                host.id
            ),
        }
    }
}

// --- usage fetch + mapping -------------------------------------------------

#[derive(Deserialize)]
struct RawRateWindow {
    #[serde(default)]
    used_percent: Option<f64>,
    #[serde(default)]
    limit_window_seconds: Option<i64>,
    #[serde(default)]
    reset_at: Option<String>,
}
#[derive(Deserialize)]
struct RawRateLimit {
    #[serde(default)]
    primary_window: Option<RawRateWindow>,
    #[serde(default)]
    secondary_window: Option<RawRateWindow>,
}
#[derive(Deserialize)]
struct RawUsage {
    #[serde(default)]
    plan_type: Option<String>,
    #[serde(default)]
    rate_limit: Option<RawRateLimit>,
}

async fn fetch_usage(http: &reqwest::Client, token: &str, account_id: &str) -> Result<RawUsage> {
    let resp = http
        .get(USAGE_URL)
        .timeout(FETCH_TIMEOUT)
        .header("Authorization", format!("Bearer {token}"))
        .header("ChatGPT-Account-Id", account_id)
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        bail!("usage {}{}", status.as_u16(), snippet(&text));
    }
    Ok(resp.json().await?)
}

/// A rolling window whose `limit_window_seconds` is nearer 5h (18000s) than a week
/// (604800s) maps to the 5h bar, else the weekly bar — never by field order.
fn window_of(w: RawRateWindow) -> Option<(bool, ClaudeUsageWindow)> {
    let secs = w.limit_window_seconds?;
    let is_five = (secs - 18_000).abs() <= (secs - 604_800).abs();
    Some((is_five, ClaudeUsageWindow { pct: w.used_percent.unwrap_or(0.0).round(), resets_at: w.reset_at }))
}

fn to_usage(acct: &StoredCodexAccount, raw: RawUsage) -> ClaudeUsage {
    let mut five_hour = None;
    let mut seven_day = None;
    if let Some(rl) = raw.rate_limit {
        for w in [rl.primary_window, rl.secondary_window].into_iter().flatten() {
            if let Some((is_five, win)) = window_of(w) {
                if is_five {
                    five_hour = Some(win);
                } else {
                    seven_day = Some(win);
                }
            }
        }
    }
    let _ = raw.plan_type; // plan is stored on the account, not the usage view
    ClaudeUsage {
        id: acct.id.clone(),
        email: acct.email.clone(),
        provider: Some(wire::Provider::Codex),
        active: acct.active,
        assignable: None,
        error: None,
        stale: None,
        last_updated: now_ms(),
        five_hour,
        seven_day,
        spend: None,
    }
}

fn codex_base(acct: &StoredCodexAccount) -> ClaudeUsage {
    ClaudeUsage {
        id: acct.id.clone(),
        email: acct.email.clone(),
        provider: Some(wire::Provider::Codex),
        active: acct.active,
        assignable: None,
        error: None,
        stale: None,
        last_updated: now_ms(),
        five_hour: None,
        seven_day: None,
        spend: None,
    }
}
```

`ClaudeSpend` is imported but codex has no spend line; drop `ClaudeSpend` from the `use wire::{…}` list if `rustc` warns it is unused.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p control-server codex::tests`
Expected: PASS (the three new tests + Task 7's).

- [ ] **Step 5: Commit**

```bash
git add crates/control-server/src/codex.rs
git commit -m "feat(codex): refresh (JWT-exp), token push, usage mapping by window"
```

---

### Task 9: `codex.rs` part 3 — scoring, groups, rotator, poller, auto-swap

**Files:**
- Modify: `crates/control-server/src/codex.rs` (add the scoring/groups/rotator/poller/auto-swap section + assignment tests; remove the temporary `#[allow(...)]`s from Task 7)
- Modify: `crates/control-server/src/main.rs` (spawn `codex::run_poller` + `codex::run_rotator`)

**Interfaces:**
- Consumes: Tasks 7–8 (`CodexStore`, `push_account_to_clone`, `clear_clone_token`, `fetch_usage`, `fresh_access_token`, `to_usage`, `codex_base`), `crate::clone_ops::{rand_u64, shuffle, replace_provider_views}`, `cfg.codex` / `cfg.codex_groups`, `Host.codex_account_email` / `Host.codex_group`.
- Produces (used by `web.rs`, `jobs.rs`, `mcp.rs`, `main.rs`):
  - `pub fn normalize_selection(requested: Option<&str>) -> String`
  - `pub fn recommend(app) -> Option<String>`
  - `pub fn resolve_clone_account(app, requested: Option<&str>) -> Option<String>`
  - `pub enum Assignment { Account(String), Group { name, initial }, None }`
  - `pub fn resolve_assignment(app, requested: Option<&str>) -> Option<Assignment>`
  - `pub async fn rotate_once(app)`, `pub async fn run_rotator(app: App)`
  - `pub async fn poll_once(app) -> Result<bool>`, `pub async fn run_poller(app: App)`

- [ ] **Step 1: Write the failing tests (assignment rules with codex host fields)**

Add to `codex::tests`:

```rust
    fn clone_host(id: &str, cur: Option<&str>) -> Host {
        Host { id: id.into(), managed: true, codex_account_email: cur.map(str::to_string), ..Default::default() }
    }

    #[test]
    fn assignment_uses_codex_account_field() {
        // Sticky keep: a clone on an eligible account stays; a homeless clone lands in-set.
        let eligible = ["a@o".to_string(), "b@o".to_string()];
        let clones = [clone_host("c1", Some("a@o")), clone_host("c2", Some("z@gone"))];
        for _ in 0..50 {
            let got = assign_rotation(&clones, &eligible, &HashMap::new());
            let by_id: HashMap<_, _> = got.iter().map(|(h, e)| (h.id.clone(), e.clone())).collect();
            assert_eq!(by_id["c1"], "a@o");
            assert_eq!(by_id["c2"], "b@o");
        }
    }

    #[test]
    fn assignment_distinct_when_enough_accounts() {
        let eligible = ["a@o".to_string(), "b@o".to_string(), "c@o".to_string()];
        let clones = [clone_host("c1", None), clone_host("c2", None), clone_host("c3", None)];
        for _ in 0..50 {
            let got = assign_rotation(&clones, &eligible, &HashMap::new());
            let mut emails: Vec<_> = got.iter().map(|(_, e)| e.clone()).collect();
            emails.sort();
            emails.dedup();
            assert_eq!(emails.len(), 3);
        }
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p control-server codex::tests::assignment_uses_codex_account_field`
Expected: FAIL — `assign_rotation` missing (compile error).

- [ ] **Step 3: Implement the scoring/groups/rotator/poller section**

Add to `codex.rs` (after `codex_base`, before the tests). This is a faithful port of `claude.rs`'s scoring/groups/rotator/poller/auto-swap with exactly these substitutions vs. `claude.rs`:

| claude.rs | codex.rs |
|---|---|
| `app.claude` | `app.codex` |
| `Host.claude_account_email` | `Host.codex_account_email` |
| `Host.claude_group` | `Host.codex_group` |
| `cfg.clone_groups` | `cfg.codex_groups` |
| `cfg.claude.pinned_email` / `.auto_swap_on_exhaustion` / `.poll_secs` | `cfg.codex.pinned_email` / `.auto_swap_on_exhaustion` / `.poll_secs` |
| usage-map filter `provider != Some(Codex)` | `provider == Some(Codex)` |
| `s.claude_accounts = views` (both publish sites) | `replace_provider_views(app, Provider::Codex, …)` |
| `apply_clone_token(app, id, &token)` (token string) | `push_account_to_clone` path only (apply takes the whole account) |

Write it in full:

```rust
// --- scoring + assignment (mirrors claude.rs) -----------------------------

const AUTO: &str = "auto";
const NONE: &str = "none";

pub fn normalize_selection(requested: Option<&str>) -> String {
    let want = requested.unwrap_or("").trim();
    if want.is_empty() { AUTO.to_string() } else { want.to_string() }
}

struct Scored {
    email: String,
    score: f64,
    eligible: bool,
}

fn clamp01(n: f64) -> f64 {
    n.clamp(0.0, 1.0)
}

fn score_accounts(app: &App) -> Vec<Scored> {
    let st = app.store.get();
    let usage: HashMap<&str, &ClaudeUsage> = st
        .claude_accounts
        .iter()
        .filter(|u| u.provider == Some(wire::Provider::Codex))
        .map(|u| (u.email.as_str(), u))
        .collect();
    let mut clones: HashMap<&str, u32> = HashMap::new();
    for h in &st.hosts {
        if let Some(e) = &h.codex_account_email {
            *clones.entry(e.as_str()).or_insert(0) += 1;
        }
    }
    app.codex
        .emails()
        .into_iter()
        .map(|email| {
            let u = usage.get(email.as_str());
            let five = u.and_then(|u| u.five_hour.as_ref()).map(|w| w.pct).unwrap_or(0.0);
            let seven = u.and_then(|u| u.seven_day.as_ref()).map(|w| w.pct).unwrap_or(0.0);
            let headroom = clamp01((100.0 - five) / 100.0);
            let n = *clones.get(email.as_str()).unwrap_or(&0) as f64;
            let score = headroom - 0.5 * n;
            let eligible = (100.0 - five >= SESSION_HEADROOM_PCT) && seven < SEVEN_DAY_CAP_PCT;
            Scored { email, score, eligible }
        })
        .collect()
}

fn best_scored(app: &App) -> Option<String> {
    let scored = score_accounts(app);
    if scored.is_empty() {
        return None;
    }
    let mut pool: Vec<&Scored> = scored.iter().filter(|s| s.eligible).collect();
    if pool.is_empty() {
        pool = scored.iter().collect();
    }
    pool.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    pool.first().map(|s| s.email.clone())
}

pub fn recommend(app: &App) -> Option<String> {
    best_scored(app)
}

pub fn resolve_clone_account(app: &App, requested: Option<&str>) -> Option<String> {
    let emails = app.codex.emails();
    if emails.is_empty() {
        return None;
    }
    let want = requested.unwrap_or("").trim();
    if !want.is_empty() && want != AUTO {
        if let Some(hit) = emails.iter().find(|e| e.as_str() == want) {
            return Some(hit.clone());
        }
        tracing::warn!("codex account '{want}' not imported; using recommended");
    }
    best_scored(app)
}

pub enum Assignment {
    Account(String),
    Group { name: String, initial: String },
    None,
}

pub fn resolve_assignment(app: &App, requested: Option<&str>) -> Option<Assignment> {
    let want = requested.unwrap_or("").trim();
    if want.eq_ignore_ascii_case(NONE) {
        return Some(Assignment::None);
    }
    if let Some(name) = want.strip_prefix("group:") {
        let name = name.trim();
        let initial = pick_group_account(app, name)?;
        return Some(Assignment::Group { name: name.to_string(), initial });
    }
    resolve_clone_account(app, requested).map(Assignment::Account)
}

fn clone_counts(app: &App) -> HashMap<String, u32> {
    let mut m = HashMap::new();
    for h in &app.store.get().hosts {
        if let Some(e) = &h.codex_account_email {
            *m.entry(e.clone()).or_insert(0) += 1;
        }
    }
    m
}

fn five_hour_pct(app: &App, email: &str) -> f64 {
    app.store
        .get()
        .claude_accounts
        .iter()
        .filter(|u| u.provider == Some(wire::Provider::Codex))
        .find(|u| u.email == email)
        .and_then(|u| u.five_hour.as_ref())
        .map(|w| w.pct)
        .unwrap_or(0.0)
}

fn eligible_group_accounts(app: &App, group: &CloneGroup) -> Vec<String> {
    let known = app.codex.emails();
    group
        .accounts
        .iter()
        .filter(|email| known.iter().any(|k| &k == email))
        .filter(|email| five_hour_pct(app, email) <= ROTATE_MAX_FIVE_HOUR_PCT)
        .cloned()
        .collect()
}

fn pick_group_account(app: &App, group_name: &str) -> Option<String> {
    let cfg = app.config();
    let group = cfg.codex_groups.iter().find(|g| g.name == group_name)?;
    let counts = clone_counts(app);
    let mut pool = eligible_group_accounts(app, group);
    if pool.is_empty() {
        let known = app.codex.emails();
        pool = group.accounts.iter().filter(|e| known.iter().any(|k| &k == e)).cloned().collect();
    }
    shuffle(&mut pool);
    pool.into_iter().min_by_key(|email| {
        let load = *counts.get(email).unwrap_or(&0);
        let pct = five_hour_pct(app, email).round() as u32;
        (load, pct)
    })
}

fn assign_rotation(
    clones: &[Host],
    eligible: &[String],
    usage: &HashMap<String, f64>,
) -> Vec<(Host, String)> {
    let mut used: HashMap<String, u32> = HashMap::new();
    let mut out: Vec<(Host, String)> = Vec::with_capacity(clones.len());
    let mut homeless: Vec<Host> = Vec::new();
    for c in clones {
        match &c.codex_account_email {
            Some(e) if eligible.contains(e) => {
                *used.entry(e.clone()).or_insert(0) += 1;
                out.push((c.clone(), e.clone()));
            }
            _ => homeless.push(c.clone()),
        }
    }
    shuffle(&mut homeless);
    for host in homeless {
        let pick = eligible
            .iter()
            .min_by_key(|email| {
                let load = *used.get(*email).unwrap_or(&0);
                let pct = usage.get(*email).copied().unwrap_or(0.0).round() as u32;
                (load, pct, rand_u64() as u32)
            })
            .expect("eligible is non-empty")
            .clone();
        *used.entry(pick.clone()).or_insert(0) += 1;
        out.push((host, pick));
    }
    out
}

pub async fn rotate_once(app: &App) {
    let cfg = app.config();
    if cfg.codex_groups.is_empty() {
        return;
    }
    let mut by_group: HashMap<String, Vec<Host>> = HashMap::new();
    for h in &app.store.get().hosts {
        if let (Some(g), true) = (&h.codex_group, h.managed) {
            by_group.entry(g.clone()).or_default().push(h.clone());
        }
    }
    for (gname, clones) in by_group {
        let Some(group) = cfg.codex_groups.iter().find(|g| g.name == gname) else {
            continue;
        };
        let eligible = eligible_group_accounts(app, group);
        if eligible.is_empty() {
            tracing::info!(
                "codex rotate: group '{gname}' has no account <= {ROTATE_MAX_FIVE_HOUR_PCT}% 5h; leaving {} clone(s)",
                clones.len()
            );
            continue;
        }
        let usage: HashMap<String, f64> =
            eligible.iter().map(|e| (e.clone(), five_hour_pct(app, e))).collect();
        for (host, email) in assign_rotation(&clones, &eligible, &usage) {
            if host.codex_account_email.as_deref() == Some(email.as_str()) {
                continue;
            }
            match push_account_to_clone(app, &host.id, &email).await {
                Ok(()) => {
                    tracing::info!(
                        "codex rotate: {} {} -> {}",
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
                Err(e) => tracing::warn!("codex rotate: applying {email} to {} failed: {e}", host.id),
            }
            tokio::time::sleep(STAGGER).await;
        }
    }
}

pub async fn run_rotator(app: App) {
    tokio::time::sleep(Duration::from_secs(30)).await;
    loop {
        rotate_once(&app).await;
        tokio::time::sleep(Duration::from_secs(ROTATE_SECS)).await;
    }
}

async fn auto_swap_exhausted(app: &App) {
    let st = app.store.get();
    let usage: HashMap<String, &ClaudeUsage> = st
        .claude_accounts
        .iter()
        .filter(|u| u.provider == Some(wire::Provider::Codex))
        .map(|u| (u.email.clone(), u))
        .collect();
    let exhausted = |email: &str| -> bool {
        usage.get(email).is_some_and(|u| {
            let five = u.five_hour.as_ref().map(|w| w.pct).unwrap_or(0.0);
            let seven = u.seven_day.as_ref().map(|w| w.pct).unwrap_or(0.0);
            (100.0 - five) < SESSION_HEADROOM_PCT || seven >= SEVEN_DAY_CAP_PCT
        })
    };
    for host in &st.hosts {
        if host.codex_group.is_some() {
            continue;
        }
        if !host.managed {
            continue;
        }
        let Some(cur) = &host.codex_account_email else { continue };
        if !exhausted(cur) {
            continue;
        }
        let Some(next) = best_scored(app) else { continue };
        if &next == cur || exhausted(&next) {
            continue;
        }
        match push_account_to_clone(app, &host.id, &next).await {
            Ok(()) => {
                tracing::info!("codex auto-swapped {} from {cur} to {next}", host.id);
                let id = host.id.clone();
                app.store.mutate(|s| {
                    if let Some(h) = s.hosts.iter_mut().find(|h| h.id == id) {
                        h.codex_account_email = Some(next);
                    }
                });
            }
            Err(e) => tracing::warn!("codex auto-swap of {} failed: {e}", host.id),
        }
    }
}

// --- poller ----------------------------------------------------------------

pub async fn poll_once(app: &App) -> Result<bool> {
    {
        let mut p = app.codex.polling.lock().unwrap();
        if *p {
            return Ok(false);
        }
        *p = true;
    }
    let result = poll_inner(app).await;
    *app.codex.polling.lock().unwrap() = false;
    result
}

async fn poll_inner(app: &App) -> Result<bool> {
    let accts = app.codex.snapshot();
    let cfg = app.config();
    if accts.is_empty() {
        crate::clone_ops::replace_provider_views(app, wire::Provider::Codex, Vec::new(), None);
        return Ok(false);
    }

    let usage_polling = cfg.codex.usage_polling;
    let mut any429 = false;
    let mut views = Vec::with_capacity(accts.len());

    for (i, acct) in accts.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(STAGGER).await;
        }
        let outcome = async {
            let (fresh, _) = fresh_access_token(app, &acct.email).await?;
            if !usage_polling {
                // Refresh + push still happen; skip the usage fetch, publish a base view.
                let mut b = codex_base(acct);
                b.error = Some("usage polling disabled (codex.usagePolling=false)".into());
                return Ok::<_, anyhow::Error>(b);
            }
            let raw = fetch_usage(&app.http, &fresh.access_token, &fresh.account_id).await?;
            Ok::<_, anyhow::Error>(to_usage(acct, raw))
        }
        .await;
        match outcome {
            Ok(u) => {
                app.codex.last_good.lock().unwrap().insert(acct.id.clone(), u.clone());
                views.push(u);
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("429") {
                    any429 = true;
                }
                let prev = app.codex.last_good.lock().unwrap().get(&acct.id).cloned();
                views.push(match prev {
                    Some(mut p) => {
                        p.stale = Some(true);
                        p
                    }
                    None => {
                        let mut b = codex_base(acct);
                        b.error = Some(msg);
                        b
                    }
                });
            }
        }
    }

    for v in &mut views {
        v.assignable = Some(true);
    }
    crate::clone_ops::replace_provider_views(
        app,
        wire::Provider::Codex,
        views,
        cfg.codex.pinned_email.as_deref(),
    );

    push_stale_tokens(app).await;

    if cfg.codex.auto_swap_on_exhaustion {
        auto_swap_exhausted(app).await;
    }
    Ok(any429)
}

pub async fn run_poller(app: App) {
    const MAX_BACKOFF: Duration = Duration::from_secs(30 * 60);
    let mut backoff: u32 = 0;
    loop {
        let any429 = match poll_once(&app).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("codex usage poll failed: {e}");
                false
            }
        };
        let base = Duration::from_secs(app.config().codex.poll_secs.max(15));
        let delay = if any429 {
            backoff = (backoff + 1).min(8);
            let escalate = backoff.saturating_sub(2);
            (base * 2u32.pow(escalate)).min(MAX_BACKOFF)
        } else {
            backoff = 0;
            base
        };
        if any429 {
            tracing::warn!("codex usage rate-limited (429); next poll in {}s", delay.as_secs());
        }
        tokio::time::sleep(delay).await;
    }
}
```

Now remove the temporary `#[allow(unused_imports)]` / `#[allow(dead_code)]` added in Task 7 Step 3 — every imported helper and constant is now used.

- [ ] **Step 4: Spawn the codex loops**

In `crates/control-server/src/main.rs`, after the two `claude::` spawns (~129-130), add:

```rust
    tokio::spawn(codex::run_poller(app.clone()));
    tokio::spawn(codex::run_rotator(app.clone()));
```

- [ ] **Step 5: Run the full crate suite**

Run: `cargo test -p control-server`
Expected: PASS — the two new assignment tests plus every prior test. Zero warnings (the temp allows are gone; all imports/consts are live).

- [ ] **Step 6: Commit**

```bash
git add crates/control-server/src/codex.rs crates/control-server/src/main.rs
git commit -m "feat(codex): scoring, groups, rotator, poller, auto-swap + loops"
```

---

### Task 10: Endpoints — `/api/codex/*` routes + handlers

**Files:**
- Modify: `crates/control-server/src/web.rs` (add 6 routes ~64-69; add handlers + request structs after the Claude handlers ~853)

**Interfaces:**
- Consumes: `crate::codex::{check_clone_auth, import_clone_account, poll_once, recommend, resolve_assignment, normalize_selection, clear_clone_token, push_account_to_clone, Assignment}`; `host_by_id` (existing, ~857).
- Produces: HTTP routes `POST /api/codex/import/check`, `POST /api/codex/import`, `POST /api/codex/refresh`, `GET /api/codex/recommended`, `POST /api/codex/swap`, `POST /api/codex/rotate`.

- [ ] **Step 1: Write the failing test (router builds with codex routes)**

`web.rs` has no per-route test harness, so gate on a compile-level assertion that the handlers and request structs are well-formed by adding a small serde test for the swap request in the `web.rs` tests module (create one if absent at end of file):

```rust
#[cfg(test)]
mod codex_route_tests {
    use super::*;

    #[test]
    fn codex_swap_req_parses() {
        let r: CodexSwapReq =
            serde_json::from_str(r#"{ "host": "pega-1", "account": "group:team" }"#).unwrap();
        assert_eq!(r.host, "pega-1");
        assert_eq!(r.account, "group:team");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p control-server codex_route_tests`
Expected: FAIL — `CodexSwapReq` not found.

- [ ] **Step 3: Add the routes**

In `crates/control-server/src/web.rs`, in the router builder, after the `.route("/api/claude/rotate", post(claude_rotate))` line (~69), add:

```rust
        .route("/api/codex/import/check", post(codex_import_check))
        .route("/api/codex/import", post(codex_import))
        .route("/api/codex/refresh", post(codex_refresh))
        .route("/api/codex/recommended", get(codex_recommended))
        .route("/api/codex/swap", post(codex_swap))
        .route("/api/codex/rotate", post(codex_rotate))
```

- [ ] **Step 4: Add the handlers**

In `crates/control-server/src/web.rs`, after `claude_rotate` (~853, before `// --- per-host chat ---`), add:

```rust
// --- Codex accounts --------------------------------------------------------

#[derive(Deserialize)]
struct CodexImportReq {
    host: String,
}

/// `POST /api/codex/import/check` — confirm a clone is signed in to Codex via ChatGPT and
/// report its identity so the UI can show it before importing.
async fn codex_import_check(State(app): State<App>, Json(req): Json<CodexImportReq>) -> JsonResult {
    let host = host_by_id(&app, &req.host)
        .ok_or_else(|| err_json(StatusCode::BAD_REQUEST, format!("unknown host '{}'", req.host)))?;
    let auth = crate::codex::check_clone_auth(&app, &host)
        .await
        .map_err(|e| err_json(StatusCode::BAD_GATEWAY, e))?;
    Ok(Json(json!({
        "ok": true,
        "email": auth.email,
        "plan": auth.plan,
        "accountId": auth.account_id,
    })))
}

/// `POST /api/codex/import` — import a Codex account from a signed-in clone.
async fn codex_import(State(app): State<App>, Json(req): Json<CodexImportReq>) -> JsonResult {
    let host = host_by_id(&app, &req.host)
        .ok_or_else(|| err_json(StatusCode::BAD_REQUEST, format!("unknown host '{}'", req.host)))?;
    let res = crate::codex::import_clone_account(&app, &host)
        .await
        .map_err(|e| err_json(StatusCode::BAD_GATEWAY, e))?;
    let _ = crate::codex::poll_once(&app).await;
    Ok(Json(json!({ "ok": true, "email": res.email, "cleared": res.cleared })))
}

/// `POST /api/codex/refresh` — force one usage poll now.
async fn codex_refresh(State(app): State<App>) -> Json<serde_json::Value> {
    let any429 = crate::codex::poll_once(&app).await.unwrap_or(false);
    Json(json!({ "ok": true, "rateLimited": any429 }))
}

/// `GET /api/codex/recommended` — the account the clone dialog should pre-select.
async fn codex_recommended(State(app): State<App>) -> Json<serde_json::Value> {
    Json(json!({ "email": crate::codex::recommend(&app) }))
}

#[derive(Deserialize)]
struct CodexSwapReq {
    host: String,
    /// Account email, `auto`, `none`, or `group:<name>`.
    account: String,
}

/// `POST /api/codex/swap` — change a clone's Codex account/group.
async fn codex_swap(
    State(app): State<App>,
    Json(req): Json<CodexSwapReq>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let host = app
        .store
        .get()
        .hosts
        .into_iter()
        .find(|h| h.id == req.host)
        .ok_or_else(|| (StatusCode::BAD_REQUEST, format!("unknown host '{}'", req.host)))?;
    if !host.managed {
        return Err((StatusCode::BAD_REQUEST, format!("'{}' is not a managed clone", host.id)));
    }
    let assignment = crate::codex::resolve_assignment(&app, Some(&req.account))
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "no imported Codex accounts".into()))?;
    let selection = crate::codex::normalize_selection(Some(&req.account));
    let (group, email) = match assignment {
        crate::codex::Assignment::None => {
            crate::codex::clear_clone_token(&app, &host.id)
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
            app.codex.forget_pushed(&host.id);
            (None, None)
        }
        crate::codex::Assignment::Group { name, initial } => {
            crate::codex::push_account_to_clone(&app, &host.id, &initial)
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
            (Some(name), Some(initial))
        }
        crate::codex::Assignment::Account(a) => {
            crate::codex::push_account_to_clone(&app, &host.id, &a)
                .await
                .map_err(|e| (StatusCode::BAD_GATEWAY, e.to_string()))?;
            (None, Some(a))
        }
    };
    let (id, email_set, group_set, sel_set) =
        (host.id.clone(), email.clone(), group.clone(), selection.clone());
    app.store.mutate(|s| {
        if let Some(h) = s.hosts.iter_mut().find(|h| h.id == id) {
            h.codex_account_email = email_set;
            h.codex_group = group_set;
            h.codex_selection = Some(sel_set);
        }
    });
    Ok(Json(json!({ "ok": true, "account": email, "group": group, "selection": selection })))
}

/// `POST /api/codex/rotate` — run one Codex group-rotation pass immediately.
async fn codex_rotate(State(app): State<App>) -> Json<serde_json::Value> {
    crate::codex::rotate_once(&app).await;
    Json(json!({ "ok": true }))
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p control-server codex_route_tests`
Expected: PASS. Then `cargo build -p control-server` — clean.

- [ ] **Step 6: Commit**

```bash
git add crates/control-server/src/web.rs
git commit -m "feat(web): /api/codex/{import/check,import,refresh,recommended,swap,rotate}"
```

---

### Task 11: Clone-time Codex assignment — `CloneSpec` + `run_clone` + `clone()` parse

**Files:**
- Modify: `crates/control-server/src/jobs.rs` (`CloneSpec` ~49-61; codex block in `run_clone` after the claude block ~414)
- Modify: `crates/control-server/src/web.rs` (`clone()` ~204 parse `codexAccount`; set `codex_account` in the two `CloneSpec` literals ~243-252 and ~273-282)

**Interfaces:**
- Consumes: `crate::codex::{resolve_assignment, normalize_selection, push_account_to_clone, Assignment}`.
- Produces: `CloneSpec.codex_account: Option<String>` (has `Default` via `#[derive(Default)]`).

- [ ] **Step 1: Write the failing test**

Add to the `jobs::tests` module (which has `test_app()` ~616):

```rust
    #[test]
    fn clonespec_default_has_no_codex_account() {
        let spec = CloneSpec { new_hostname: "x".into(), ..Default::default() };
        assert!(spec.codex_account.is_none());
    }

    #[tokio::test]
    async fn run_clone_codex_none_leaves_no_email() {
        // With no imported codex accounts, resolve_assignment(None) → None, so a clone's
        // codex_account_email stays None (the block is a no-op) — independent of claude.
        let app = test_app();
        assert!(crate::codex::resolve_assignment(&app, None).is_none());
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p control-server clonespec_default_has_no_codex_account`
Expected: FAIL — `CloneSpec` has no field `codex_account`.

- [ ] **Step 3: Add the `CloneSpec` field**

In `crates/control-server/src/jobs.rs`, in `struct CloneSpec` (~55), after `pub claude_account: Option<String>,`, add:

```rust
    /// Requested Codex account: an email, `"auto"`, `"group:<name>"`, `"none"`, or `None`
    /// (= auto). Independent of `claude_account`.
    pub codex_account: Option<String>,
```

- [ ] **Step 4: Add the codex assignment block in `run_clone`**

In `crates/control-server/src/jobs.rs`, immediately after the claude assignment block closes (~414, right after its closing `}` and before the `// Kick off the agent:` comment ~416), add a parallel, independent block:

```rust
    // Assign a Codex account/group (or explicitly none), independently of Claude — a clone
    // can hold both. Same shape as the Claude block above, reading codex_* state.
    if let Some(assignment) = crate::codex::resolve_assignment(&app, spec.codex_account.as_deref()) {
        let selection = crate::codex::normalize_selection(spec.codex_account.as_deref());
        let (group, account) = match assignment {
            crate::codex::Assignment::Group { name, initial } => (Some(name), Some(initial)),
            crate::codex::Assignment::Account(a) => (None, Some(a)),
            crate::codex::Assignment::None => (None, None),
        };
        let id = spec.new_hostname.clone();
        let (email, group_set) = (account.clone(), group.clone());
        app.store.mutate(|s| {
            if let Some(h) = s.hosts.iter_mut().find(|h| h.id == id) {
                h.codex_selection = Some(selection.clone());
                h.codex_account_email = email.clone();
                h.codex_group = group_set.clone();
            }
        });
        match account {
            None => patch_op(&app, &op_id, |op| {
                op.log.push("codex account: none (no token installed)".into())
            }),
            Some(email) => {
                let label = match &group {
                    Some(g) => format!("{email} (group {g})"),
                    None => email.clone(),
                };
                match crate::codex::push_account_to_clone(&app, &spec.new_hostname, &email).await {
                    Ok(()) => patch_op(&app, &op_id, |op| {
                        op.log.push(format!("codex account: assigned {label}"))
                    }),
                    Err(e) => {
                        tracing::warn!("codex push_account_to_clone({}) failed: {e}", spec.new_hostname);
                        patch_op(&app, &op_id, |op| {
                            op.log.push(format!("codex account: failed to assign {label}: {e}"))
                        });
                    }
                }
            }
        }
    }
```

- [ ] **Step 5: Parse `codexAccount` in `web.rs::clone()` and thread it into both specs**

In `crates/control-server/src/web.rs`, in `clone()`, after `let claude_account = str_field("claudeAccount");` (~204), add:

```rust
    let codex_account = str_field("codexAccount");
```

In the plain-mode `CloneSpec { … }` literal (~243), add `codex_account,` after `claude_account,`. In the ticket/create-mode `CloneSpec { … }` literal (~273), add `codex_account,` after `claude_account,`. (Rust moves `codex_account` into the first literal that runs; the plain path `return`s before reaching the second, so a single binding is correct — mirrors how `claude_account` is already used in both literals.)

Wait — `claude_account` is used in both literals but the plain path returns early, so only one literal executes per call; the second literal's `claude_account` is only reached when the plain branch did not run. Confirm `codex_account` follows the identical pattern (it will, since both are `Option<String>` moved once per code path).

- [ ] **Step 6: Run the tests + build**

Run: `cargo test -p control-server clonespec_default_has_no_codex_account run_clone_codex_none_leaves_no_email` then `cargo build -p control-server`
Expected: PASS + clean build. (The `mcp.rs` `CloneSpec` literal, if any, uses `..Default::default()`, so the new field needs no change there — confirm with the build; if the build flags a missing field in a `CloneSpec` literal elsewhere, add `codex_account: None` or `..Default::default()` there.)

- [ ] **Step 7: Commit**

```bash
git add crates/control-server/src/jobs.rs crates/control-server/src/web.rs
git commit -m "feat(clone): assign a Codex account at clone time (independent of Claude)"
```

---

### Task 12: MCP tools — `codex_recommended` + `codex_swap`

**Files:**
- Modify: `crates/control-server/src/mcp.rs` (tool declarations ~140-152; dispatch arms ~288-335)

**Interfaces:**
- Consumes: `crate::codex::{recommend, resolve_assignment, normalize_selection, clear_clone_token, push_account_to_clone, Assignment}`; the existing `tool()` / `text()` helpers.
- Produces: two MCP tools mirroring `claude_recommended` / `claude_swap`.

- [ ] **Step 1: Add the tool declarations**

In `crates/control-server/src/mcp.rs`, in `tools_for` after the `claude_swap` tool push (~152), add:

```rust
        tools.push(tool("codex_recommended", "Recommended Codex account for a new clone", json!({}), json!([])));
        tools.push(tool(
            "codex_swap",
            "Hot-swap a clone's Codex account",
            json!({
                "clone": { "type": "string" },
                "account": {
                    "type": "string",
                    "description": "An account email, \"auto\" (server picks best), \"group:<name>\", or \"none\" (remove the clone's token)",
                },
            }),
            json!(["clone", "account"]),
        ));
```

- [ ] **Step 2: Add the dispatch arms**

In the tool-dispatch `match` (after the `"claude_swap" => { … }` arm closes ~335), add:

```rust
        "codex_recommended" => {
            Ok(text(json!({ "email": crate::codex::recommend(app) }).to_string()))
        }
        "codex_swap" => {
            let clone = args.get("clone").and_then(Value::as_str).ok_or("clone required")?;
            let account = args.get("account").and_then(Value::as_str).unwrap_or("auto");
            let host = app.store.get().hosts.into_iter().find(|h| h.id == clone).ok_or("unknown clone")?;
            if !host.managed {
                return Err("not a managed clone".into());
            }
            let assignment =
                crate::codex::resolve_assignment(app, Some(account)).ok_or("no imported Codex accounts")?;
            let selection = crate::codex::normalize_selection(Some(account));
            let (group, email) = match assignment {
                crate::codex::Assignment::None => {
                    crate::codex::clear_clone_token(app, &host.id).await.map_err(|e| e.to_string())?;
                    app.codex.forget_pushed(&host.id);
                    (None, None)
                }
                crate::codex::Assignment::Group { name, initial } => {
                    crate::codex::push_account_to_clone(app, &host.id, &initial)
                        .await
                        .map_err(|e| e.to_string())?;
                    (Some(name), Some(initial))
                }
                crate::codex::Assignment::Account(a) => {
                    crate::codex::push_account_to_clone(app, &host.id, &a)
                        .await
                        .map_err(|e| e.to_string())?;
                    (None, Some(a))
                }
            };
            let (id, email_set, group_set, sel_set) =
                (host.id.clone(), email.clone(), group.clone(), selection.clone());
            app.store.mutate(|s| {
                if let Some(h) = s.hosts.iter_mut().find(|h| h.id == id) {
                    h.codex_account_email = email_set;
                    h.codex_group = group_set;
                    h.codex_selection = Some(sel_set);
                }
            });
            Ok(text(match (&group, &email) {
                (Some(g), Some(e)) => format!("swapped {clone} codex → {e} (group {g})"),
                (None, Some(e)) => format!("swapped {clone} codex → {e}"),
                _ => format!("swapped {clone} codex → none (no token)"),
            }))
        }
```

- [ ] **Step 3: Build to verify it compiles**

Run: `cargo build -p control-server`
Expected: clean.

- [ ] **Step 4: Confirm the crate suite is green**

Run: `cargo test -p control-server`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/control-server/src/mcp.rs
git commit -m "feat(mcp): codex_recommended + codex_swap tools"
```

---

### Task 13: Provisioning — install `codex` in the clone template

**Files:**
- Modify: `template/setup/30-user.sh` (add the codex install after the claude install ~141)

**Interfaces:**
- Produces: `~/.local/bin/codex` in the built template image. **Warn-only** on failure (unlike the load-bearing claude install).

- [ ] **Step 1: Add the install line**

In `template/setup/30-user.sh`, immediately after the claude install line (~141: `runuser -u "$USERNAME" -- bash -lc 'set -o pipefail; command -v claude …'`), add:

```bash
# Codex CLI installs standalone (self-contained binary, no node) → ~/.local/bin/codex.
# Warn-only: unlike claude, the agent-wrapper does not require codex, so a failed install
# must not fail the template build. Idempotent (skips if already present).
log "install standalone codex CLI (no node)"
runuser -u "$USERNAME" -- bash -lc 'set -o pipefail; command -v codex >/dev/null 2>&1 || CODEX_NON_INTERACTIVE=1 curl -fsSL https://chatgpt.com/codex/install.sh | sh' \
  || warn "codex install failed; codex accounts will be unavailable on clones from this template"
```

Do NOT add `test -x "/home/$USERNAME/.local/bin/codex"` to the toolchain-assertion block (~219-221) — codex is warn-only, not load-bearing.

- [ ] **Step 2: Static-check the script**

Run: `bash -n template/setup/30-user.sh`
Expected: no output, exit 0.

- [ ] **Step 3: Confirm the warn-only contract in text**

Run: `grep -n "codex" template/setup/30-user.sh`
Expected: shows the install line + the `|| warn` fallback, and NO `test -x .../codex` assertion line. (This is the reviewer's check that the install cannot fail the build.)

- [ ] **Step 4: Commit**

```bash
git add template/setup/30-user.sh
git commit -m "feat(template): install standalone codex CLI (warn-only)"
```

Note in the commit body / PR: existing images need a template rebuild + publish (`scripts/publish-template.sh`) and a `POST /api/images/pull`, or a manual `codex` install — `binswap` only syncs `clone-daemon`/`agent-wrapper`, never new CLIs.

---

### Task 14: Frontend — types, api, and provider split in `_index.tsx`

**Files:**
- Modify: `frontend/app/lib/types.ts` (`Host` codex fields ~39-53; `ClonePayload` `codexAccount` ~35-41)
- Modify: `frontend/app/lib/api.ts` (codex functions mirroring the claude ones ~76-99; `ClonePayload` `codexAccount`)
- Modify: `frontend/app/routes/_index.tsx` (derive `codexAccounts`; pass to CloneModal, ChangeAccountModal, SettingsPanel; ImportAccountModal `onImported` refresh)

**Interfaces:**
- Consumes: the regenerated `frontend/app/lib/wire/{Host,CodexConfig,AppConfigRedacted}.ts` (Tasks 1–2).
- Produces (used by Tasks 15–17):
  - api: `refreshCodexUsage`, `checkCodexImport`, `importCodexAccount`, `recommendedCodexAccount`, `swapCodexAccount`.
  - `ClonePayload` gains `codexAccount?: string`.
  - `Host` (hand-written) gains `codexAccountEmail?`, `codexGroup?`, `codexSelection?`.
  - `_index.tsx` computes `codexAccounts` = `state.claudeAccounts.filter(a => a.assignable && a.provider === "codex")`.

- [ ] **Step 1: Add the hand-written `Host` codex fields + `ClonePayload.codexAccount`**

In `frontend/app/lib/types.ts`, in the `Host` interface after the `claudeSelection?` field (~53), add:

```ts
  /** Email of the imported Codex account whose token is written into this clone. */
  codexAccountEmail?: string;
  /** Name of the Codex group this clone is balanced within (absent = single account). */
  codexGroup?: string;
  /** Verbatim operator Codex pick: "auto" | "none" | "group:<name>" | email. */
  codexSelection?: string;
```

- [ ] **Step 2: Add the codex api functions + payload field**

In `frontend/app/lib/api.ts`, in the `ClonePayload` type (~35-41), add `codexAccount?: string;` to the final `& { … }` intersection alongside `claudeAccount?`:

```ts
) & { claudeAccount?: string; codexAccount?: string; preset?: string };
```

After the `swapClaudeAccount` definition (~99), add:

```ts
export const refreshCodexUsage = () => postJson("/api/codex/refresh", {});

export const checkCodexImport = (host: string) =>
  postJson("/api/codex/import/check", { host }) as Promise<{
    email: string;
    plan: string | null;
    accountId: string;
  }>;

export const importCodexAccount = (host: string) =>
  postJson("/api/codex/import", { host }) as Promise<{ email: string; cleared: boolean }>;

export const recommendedCodexAccount = () =>
  getJson("/api/codex/recommended") as Promise<{ email: string | null }>;

export const swapCodexAccount = (host: string, account: string) =>
  postJson("/api/codex/swap", { host, account }) as Promise<{
    ok: boolean;
    account: string | null;
    group: string | null;
    selection: string;
  }>;
```

- [ ] **Step 3: Derive `codexAccounts` and pass it down in `_index.tsx`**

In `frontend/app/routes/_index.tsx`:

- In the `<CloneModal>` element (~358), add a `codexAccounts` prop next to `accounts`:

```tsx
        accounts={(state.claudeAccounts ?? []).filter(
          (a) => a.assignable && a.provider !== "codex",
        )}
        codexAccounts={(state.claudeAccounts ?? []).filter(
          (a) => a.assignable && a.provider === "codex",
        )}
```

- In the `<SettingsPanel>` element (~373), add a `codexAccountEmails` prop next to `accountEmails`:

```tsx
        codexAccountEmails={(state.claudeAccounts ?? [])
          .filter((a) => a.provider === "codex")
          .map((a) => a.email)}
```

- In the `<ChangeAccountModal>` element (~422), add a `codexAccounts` prop next to `accounts`:

```tsx
        codexAccounts={(state.claudeAccounts ?? []).filter(
          (a) => a.assignable && a.provider === "codex",
        )}
```

- In `<ImportAccountModal>` (~411), the `onImported` currently runs `refreshClaudeUsage()`. Import `refreshCodexUsage` and refresh both providers after any import (the modal will import either):

```tsx
        onImported={() => {
          setImportOpen(false);
          run(refreshClaudeUsage());
          run(refreshCodexUsage());
        }}
```

Add `refreshCodexUsage` (and any other new codex api names used here) to the existing `import { … } from "~/lib/api"` statement.

Note: the new props (`codexAccounts`, `codexAccountEmails`) are added to the child components' prop types in Tasks 15–17. To keep this task's typecheck green, those children must already accept the props — so either (a) do Tasks 15–17 in the same branch before typechecking, or (b) add the optional props to the child signatures here as `codexAccounts?: ClaudeUsage[]` / `codexAccountEmails?: string[]` and consume them in 15–17. **Choose (a):** run the typecheck gate at the end of Task 17, not here. This task's own gate is limited to `types.ts` + `api.ts`.

- [ ] **Step 4: Typecheck the api + types layer**

Run: `cd frontend && bunx tsc --noEmit frontend/app/lib/api.ts frontend/app/lib/types.ts 2>/dev/null || bun run typecheck`
Expected: `api.ts` / `types.ts` themselves have no type errors. `_index.tsx` may report the not-yet-declared child props — that is expected and resolved by Task 17's full typecheck. (If your toolchain can't typecheck single files, skip to the Task 17 gate; do not treat the `_index.tsx` prop errors as failures here.)

- [ ] **Step 5: Commit**

```bash
git add frontend/app/lib/types.ts frontend/app/lib/api.ts frontend/app/routes/_index.tsx
git commit -m "feat(frontend): codex api + Host codex fields + provider split"
```

---

### Task 15: Frontend — `ImportAccountModal` provider toggle

**Files:**
- Modify: `frontend/app/components/ImportAccountModal.tsx`

**Interfaces:**
- Consumes: `checkClaudeImport`/`importClaudeAccount` (existing) and `checkCodexImport`/`importCodexAccount` (Task 14).
- Produces: a provider toggle (Claude | Codex) inside the modal; the check/import calls switch on it; codex shows `email` + `plan`.

- [ ] **Step 1: Add a provider toggle and branch the check/import calls**

In `frontend/app/components/ImportAccountModal.tsx`:

- Add provider state near the other `useState`s:

```tsx
  const [provider, setProvider] = useState<"claude" | "codex">("claude");
```

- Add the imports for the codex api functions to the existing `~/lib/api` import.

- Render a toggle above the host picker (two buttons; re-run the check when it changes):

```tsx
  <div className="mb-3 flex gap-2">
    {(["claude", "codex"] as const).map((p) => (
      <button
        key={p}
        type="button"
        onClick={() => setProvider(p)}
        className={
          "rounded px-3 py-1 text-sm " +
          (provider === p ? "bg-slate-800 text-white" : "bg-slate-100 text-slate-600")
        }
      >
        {p === "claude" ? "Claude" : "Codex"}
      </button>
    ))}
  </div>
```

- In the check effect, call the provider's check function and shape the identity display accordingly. Replace the `checkClaudeImport(hostId)` call with:

```tsx
    const check = provider === "codex" ? checkCodexImport : checkClaudeImport;
    check(hostId)
      .then((r) => {
        // codex returns { email, plan }, claude returns { email, subscriptionType }.
        const plan = "plan" in r ? r.plan : (r as { subscriptionType: string | null }).subscriptionType;
        setInfo({ email: r.email, plan });
      })
```

Adjust the local `info` state shape to `{ email: string; plan: string | null } | null` if it currently stores `subscriptionType`. Re-run the effect when `provider` changes (add `provider` to its dependency array along with the selected host).

- In the submit handler, branch the import:

```tsx
    const doImport = provider === "codex" ? importCodexAccount : importClaudeAccount;
    doImport(hostId).then((r) => onImported(r.email));
```

- Update the header text (~75) from "Import Claude account" to a provider-aware label:

```tsx
    {provider === "codex" ? "Import Codex account" : "Import Claude account"}
```

- [ ] **Step 2: Typecheck this component**

Run: `cd frontend && bun run typecheck`
Expected: `ImportAccountModal.tsx` has no type errors. (Other files may still error until Tasks 16–17; focus on this file's diagnostics.)

- [ ] **Step 3: Commit**

```bash
git add frontend/app/components/ImportAccountModal.tsx
git commit -m "feat(frontend): ImportAccountModal Claude|Codex provider toggle"
```

---

### Task 16: Frontend — `CloneModal` + `ChangeAccountModal` Codex pickers

**Files:**
- Modify: `frontend/app/components/CloneModal.tsx`
- Modify: `frontend/app/components/ChangeAccountModal.tsx`

**Interfaces:**
- Consumes: `AccountGroupSelect` (reused as-is), `recommendedCodexAccount`, `getConfig().codexGroups`, `ClaudeUsage` (provider-tagged).
- Produces: a second "Codex account" picker in each modal, feeding `codexAccount` into every clone payload / calling `swapCodexAccount`.

- [ ] **Step 1: `CloneModal` — add codex account state, prop, picker, and payload field**

In `frontend/app/components/CloneModal.tsx`:

- Add `codexAccounts: ClaudeUsage[]` to the props type (next to `accounts`), documented "Assignable Codex accounts".
- Add state mirroring the claude ones:

```tsx
  const [codexAccount, setCodexAccount] = useState("auto");
  const [codexRecommended, setCodexRecommended] = useState<string | null>(null);
  const [codexGroups, setCodexGroups] = useState<CloneGroup[]>([]);
```

- Where `getConfig()` populates `groups` (from `cloneGroups`), also set `setCodexGroups(c.codexGroups)`.
- Add a `recommendedCodexAccount()` effect mirroring the claude one (guarded by `codexAccounts.length`), pre-selecting into `codexAccount`.
- Render a second `AccountGroupSelect` immediately after the Claude one, shown only when codex accounts/groups exist:

```tsx
  {codexAccounts.length > 0 || codexGroups.length > 0 ? (
    <label className="mt-3 block text-xs font-medium text-slate-500">
      Codex account
      <AccountGroupSelect
        groups={codexGroups}
        accounts={codexAccounts}
        value={codexAccount}
        onChange={setCodexAccount}
        recommended={codexRecommended}
        className="mt-1 w-full ..."
      />
    </label>
  ) : null}
```

(Match the exact `className` used by the Claude `AccountGroupSelect` in this file.)

- Add `codexAccount` to **all three** `onClone(image, { … })` payload branches (plain, existing/ticket, create), next to `claudeAccount: account`:

```tsx
      claudeAccount: account,
      codexAccount,
```

- [ ] **Step 2: `ChangeAccountModal` — two pickers, independent values, provider-specific Apply**

In `frontend/app/components/ChangeAccountModal.tsx`:

- Add `codexAccounts: ClaudeUsage[]` to the props type.
- Add a `codexValue` state seeded from the host's codex selection, mirroring `currentValue` but for codex:

```tsx
function currentCodexValue(host: Host): string {
  if (host.codexSelection) return host.codexSelection;
  if (host.codexGroup) return `group:${host.codexGroup}`;
  return host.codexAccountEmail ?? "auto";
}
// ...
  const [codexValue, setCodexValue] = useState(() => currentCodexValue(host));
  const [codexGroups, setCodexGroups] = useState<CloneGroup[]>([]);
```

- In the `getConfig()` effect, also `setCodexGroups(c.codexGroups)`.
- Render a second `AccountGroupSelect` (labelled "Codex account") bound to `codexValue`/`codexGroups`/`codexAccounts`, shown when codex accounts/groups exist.
- Change the submit contract so the parent can apply whichever changed. Simplest parity-preserving approach: change `onSubmit` from `(value: string) => void` to `(claude: string, codex: string) => void`, and have the parent (`_index.tsx`) call `swapClaudeAccount` and/or `swapCodexAccount` for the value(s) that differ from the host's current selection. Update the Apply button:

```tsx
  onClick={() => onSubmit(value, codexValue)}
```

- In `frontend/app/routes/_index.tsx`, update the `<ChangeAccountModal onSubmit={…}>` handler to call both swaps as needed:

```tsx
        onSubmit={(claude, codex) => {
          setChanging(true);
          const jobs: Promise<unknown>[] = [];
          if (claude !== (changeHost.claudeSelection ?? changeHost.claudeAccountEmail ?? "auto"))
            jobs.push(swapClaudeAccount(changeHost.id, claude));
          if (codex !== (changeHost.codexSelection ?? changeHost.codexAccountEmail ?? "auto"))
            jobs.push(swapCodexAccount(changeHost.id, codex));
          Promise.allSettled(jobs).finally(() => {
            setChanging(false);
            setChangeHost(null);
          });
        }}
```

Import `swapCodexAccount` in `_index.tsx`.

- [ ] **Step 3: Typecheck**

Run: `cd frontend && bun run typecheck`
Expected: `CloneModal.tsx`, `ChangeAccountModal.tsx`, and the `_index.tsx` call-sites for these two modals have no type errors. (`SettingsPanel.tsx` may still error until Task 17.)

- [ ] **Step 4: Commit**

```bash
git add frontend/app/components/CloneModal.tsx frontend/app/components/ChangeAccountModal.tsx \
        frontend/app/routes/_index.tsx
git commit -m "feat(frontend): Codex account pickers in CloneModal + ChangeAccountModal"
```

---

### Task 17: Frontend — `SettingsPanel` Codex section + Codex groups

**Files:**
- Modify: `frontend/app/components/SettingsPanel.tsx`

**Interfaces:**
- Consumes: `codexAccountEmails: string[]` prop (Task 14), `getConfig().codex` + `.codexGroups`.
- Produces: a "Codex" settings section + a "Codex groups" editor; `save()` patches `codex` + `codexGroups`.

- [ ] **Step 1: Add the prop, state, seeding, JSX, and save-patch**

In `frontend/app/components/SettingsPanel.tsx`:

- Add `codexAccountEmails: string[];` to `SettingsPanelProps`.
- Add state mirroring the claude ones:

```tsx
  const [codex, setCodex] = useState({
    pollSecs: 600,
    pinnedEmail: "",
    autoSwapOnExhaustion: false,
    usagePolling: true,
  });
  const [codexGroups, setCodexGroups] = useState<{ name: string; accounts: string[] }[]>([]);
```

- In `load()`, seed both from config (next to the claude seeding):

```tsx
    setCodex({
      ...c.codex,
      pollSecs: Number(c.codex.pollSecs),
      pinnedEmail: c.codex.pinnedEmail ?? "",
    });
    setCodexGroups(c.codexGroups.map((g) => ({ name: g.name, accounts: [...g.accounts] })));
```

- Add a "Codex" `<Section>` mirroring the "Claude" section, with an extra usage-polling checkbox:

```tsx
  <Section title="Codex">
    <div className="grid grid-cols-2 gap-3">
      <Field label="Usage poll interval (s)">
        <input
          type="number"
          value={codex.pollSecs}
          onChange={(e) => setCodex({ ...codex, pollSecs: Number(e.target.value) || 0 })}
        />
      </Field>
      <Field label="Pinned account email">
        <input
          value={codex.pinnedEmail}
          onChange={(e) => setCodex({ ...codex, pinnedEmail: e.target.value })}
        />
      </Field>
      <label className="col-span-2 flex items-center gap-2 text-sm text-slate-600">
        <input
          type="checkbox"
          checked={codex.autoSwapOnExhaustion}
          onChange={(e) => setCodex({ ...codex, autoSwapOnExhaustion: e.target.checked })}
        />
        Auto-swap a clone to another account when usage is exhausted
      </label>
      <label className="col-span-2 flex items-center gap-2 text-sm text-slate-600">
        <input
          type="checkbox"
          checked={codex.usagePolling}
          onChange={(e) => setCodex({ ...codex, usagePolling: e.target.checked })}
        />
        Poll ChatGPT usage (uncheck if the usage endpoint drifts; refresh + push still run)
      </label>
    </div>
  </Section>
```

- Add a "Codex groups" `<Section>` mirroring "Claude groups", bound to `codexGroups` and fed by `codexAccountEmails`. Reuse the same helper pattern (`setGroupName`/`toggleGroupAccount`/`addGroup`) but pointed at the codex state — add codex variants (`setCodexGroupName`, `toggleCodexGroupAccount`, `addCodexGroup`) or generalize the existing helpers to take the setter. The checkbox list iterates `codexAccountEmails`.

- In `save()`, add both keys to the patch object (next to `claude` and `cloneGroups`):

```tsx
    codex: { ...codex, pinnedEmail: codex.pinnedEmail || null },
    codexGroups: codexGroups
      .filter((g) => g.name.trim())
      .map((g) => ({ name: g.name.trim(), accounts: [...new Set(g.accounts)] })),
```

- [ ] **Step 2: Full frontend typecheck (the gate for Tasks 14–17)**

Run: `cd frontend && bun run typecheck`
Expected: PASS — zero type errors across the whole frontend. This is the integration gate that closes out the `_index.tsx` cross-component props from Task 14.

- [ ] **Step 3: Commit**

```bash
git add frontend/app/components/SettingsPanel.tsx
git commit -m "feat(frontend): SettingsPanel Codex section + Codex groups editor"
```

---

### Task 18: Docs

**Files:**
- Modify: `docs/API.md` (codex endpoint block, mirroring the claude block)
- Modify: `docs/PROTOCOL.md` (a `codex-accounts` sibling section)
- Modify: `docs/SCRIPTS.md` (codex-import.sh row + section + provision note)
- Modify: `crates/wire/README.md` (note the new `Host.codex*` / `CodexConfig` / `codexGroups` types)

**Interfaces:** none (documentation).

- [ ] **Step 1: API.md — add the codex endpoint block**

In `docs/API.md`, directly after the Claude accounts endpoint section, add a parallel block documenting each endpoint with method, path, request body, and response:

```markdown
### Codex accounts

Codex (OpenAI/ChatGPT) accounts mirror the Claude endpoints. The server owns each
account's OAuth pair; clones receive only a short-lived injected `~/.codex/auth.json`.

- `POST /api/codex/import/check` — body `{ host }` → `{ ok, email, plan, accountId }`.
  Confirms the clone is signed in to Codex via ChatGPT (reads `~/.codex/auth.json`,
  decodes the id_token JWT). Errors if signed in with an API key.
- `POST /api/codex/import` — body `{ host }` → `{ ok, email, cleared }`. Harvests the
  OAuth triple, stores it (0600 `codex-accounts.json`), clears the clone's auth.json.
- `POST /api/codex/refresh` — force one usage poll → `{ ok, rateLimited }`.
- `GET  /api/codex/recommended` → `{ email }` — the account a new clone should pre-select.
- `POST /api/codex/swap` — body `{ host, account }` (`account` = email | `auto` | `none` |
  `group:<name>`) → `{ ok, account, group, selection }`.
- `POST /api/codex/rotate` — run one Codex group-rotation pass → `{ ok }`.

Clone creation (`POST /api/clone`) accepts an optional `codexAccount` alongside
`claudeAccount`; a clone can be assigned both independently.
```

- [ ] **Step 2: PROTOCOL.md — add the codex-accounts section**

In `docs/PROTOCOL.md`, add a sibling section to the Claude accounts one:

```markdown
<a id="codex-accounts"></a>
## Codex accounts

Server-owned single-token model, identical in spirit to Claude accounts.

- **Store:** `codex-accounts.json` (0600, in `data_dir`; override `RMNG_CODEX_ACCOUNTS_FILE`).
  Each record: `id` (`codex:<account_id>`), `email`, `account_id`, `plan`, `access_token`,
  `id_token`, `refresh_token`, `expires_at`.
- **Injected in-clone file:** `~/.codex/auth.json` = `{ "OPENAI_API_KEY": null, "tokens":
  { "id_token", "access_token", "refresh_token": "", "account_id" }, "last_refresh": <now> }`.
  The refresh token is emptied and `last_refresh` set to now so the clone's CLI never
  rotates the server-owned token. The server re-pushes on every refresh, with a 60-min lead.
- **Refresh:** `POST https://auth.openai.com/oauth/token` (client_id
  `app_EMoamEEZ73f0CkXaXp7hrann`). No `expires_in` — expiry is decoded from the access-token
  JWT `exp`. Refresh tokens are single-use / rotating.
- **Usage:** `GET https://chatgpt.com/backend-api/wham/usage` (Bearer + `ChatGPT-Account-Id`);
  windows map to 5h/weekly by `limit_window_seconds`. Disable with `codex.usagePolling=false`
  (refresh + push still run).
- **Config:** `CodexConfig { pollSecs, pinnedEmail, autoSwapOnExhaustion, usagePolling }` +
  `codexGroups: CloneGroup[]`. `Host` carries `codexAccountEmail` / `codexGroup` /
  `codexSelection`. Coexists with Claude: one clone can hold both.
```

- [ ] **Step 3: SCRIPTS.md — add codex-import.sh**

In `docs/SCRIPTS.md`, add a row/section for `codex-import.sh` mirroring `claude-import.sh` (ops: `status`/`read`/`clear`/`apply`; target `~/.codex/auth.json`; run over `docker exec`), and a provisioning note: codex is installed into the clone template by `template/setup/30-user.sh` (warn-only); existing images need a template rebuild + `POST /api/images/pull` or a manual install (binswap does not install CLIs).

- [ ] **Step 4: wire/README.md — note the new types**

In `crates/wire/README.md`, add `Host.codex*`, `CodexConfig`, and `AppConfig.codex` / `codexGroups` to wherever the Claude equivalents are listed.

- [ ] **Step 5: Sanity-check the docs reference real symbols**

Run:

```bash
grep -rn "usagePolling\|codexGroups\|codex-accounts.json\|ChatGPT-Account-Id\|/api/codex/" docs/ crates/wire/README.md
```

Expected: hits in each edited doc; the terms match the code (`usagePolling` in `CodexConfig`, `codexGroups` in `AppConfig`, the endpoint paths from Task 10).

- [ ] **Step 6: Commit**

```bash
git add docs/API.md docs/PROTOCOL.md docs/SCRIPTS.md crates/wire/README.md
git commit -m "docs: Codex accounts (API, protocol, scripts, wire types)"
```

---

## Final verification (run after all tasks)

1. `cargo build --workspace && cargo test --workspace` — clean build, all tests pass, ts-rs bindings regenerated.
2. `cd frontend && bun run typecheck` — zero type errors.
3. **Manual on staging** (CT with codex installed), per `CODEX_PARITY.md` §Verification:
   a. `codex login` (ChatGPT mode) → `~/.codex/auth.json` has `tokens` + null `OPENAI_API_KEY`.
   b. `POST /api/codex/import/check` → email/plan/accountId; `POST /api/codex/import` → account in 0600 `codex-accounts.json`, clone's auth.json cleared, ChatGPT-logo row with usage in the panel.
   c. **Decode the stored `access_token` `exp`** → confirm lifetime > `REFRESH_LEAD_MS` (60 min); shrink the lead if not.
   d. Swap a clone onto the account → its `~/.codex/auth.json` written with `refresh_token:""`; run `codex` in the clone → a successful API call (proves the CLI tolerates the empty refresh token; if not, switch `codex-import.sh apply` to a dead sentinel refresh token).
   e. Force `expires_at ≈ now` + `POST /api/codex/refresh` → server refreshes (rotates the refresh token) and the clone's auth.json changes.
   f. Two accounts in a `codexGroups` group + two bound clones → `POST /api/codex/rotate` re-balances; both stay authed.
   g. **Coexistence:** one clone with both a Claude and a Codex account → both credential files present; both pollers publish without clobbering (`/events` shows both provider rows persisting across polls).
   h. Set `codex.usagePolling=false` → accounts still listed; assignment/refresh/push unaffected.

---

## Self-Review (completed by plan author)

**Spec coverage** — every `CODEX_PARITY.md` §Changes item maps to a task: §1 shared helpers → Task 4; §2 codex.rs → Tasks 7–9 (+ store in 7, refresh/usage in 8, scoring/groups/rotator/poller/auto-swap in 9); poller clobber fix + provider-filter audit → Task 5; §3 wire types → Tasks 1–2 (+ config merge test in 3); §4 app/main/web/jobs/mcp → Tasks 7 (app), 9 (main spawns), 10 (web), 11 (jobs), 12 (mcp); §5 scripts + provisioning → Tasks 6 (codex-import.sh) + 13 (30-user.sh); §6 frontend → Tasks 14–17; §7 docs → Task 18.

**Stale-spec reconciliations** are called out up front (run_clone_op signature, absent `sq`, JWT decode is new, RFC3339 via existing `epoch_to_rfc3339`) so an implementer following the spec prose verbatim doesn't chase retired signatures.

**Type consistency** — `fresh_access_token` returns `(StoredCodexAccount, bool)` (not a token string) everywhere it is used (Tasks 8–9); `apply_clone_token` takes `&StoredCodexAccount` (needs id_token + account_id), consumed consistently by `push_account_to_clone` / `push_stale_tokens`; `replace_provider_views(app, provider, views, pinned)` signature is identical in its definition (Task 5) and both call-sites (claude Task 5, codex Task 9); wire field names (`codexAccountEmail`/`codexGroup`/`codexSelection`, `CodexConfig`, `codexGroups`) are spelled identically across Rust (snake_case) and the JSON/TS (camelCase) throughout.

**Placeholder scan** — no "TBD"/"add validation"/"similar to Task N (without code)". The only "port verbatim from claude.rs" instruction (Task 9) references an in-repo file with an explicit, exhaustive substitution table AND provides the full code, so it is not a placeholder.

**Known soft spots flagged for the executor:** (1) frontend cross-component props (Task 14 adds call-sites; Tasks 15–17 add the receiving prop types) — the typecheck gate is deliberately deferred to Task 17 Step 2, noted in Task 14 Step 3. (2) The codex-import.sh smoke test (Task 6 Step 3) shims `runuser` for local runs; the real `runuser -l` path is only exercised on staging. (3) Codex CLI tolerance of `refresh_token:""` and `/wham/usage` shape are hedged per the spec; the fallbacks (dead sentinel token; `usagePolling=false`) are wired.

## Execution Handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-03-codex-account-parity.md`. Two execution options:

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.

**2. Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints.

Which approach?
