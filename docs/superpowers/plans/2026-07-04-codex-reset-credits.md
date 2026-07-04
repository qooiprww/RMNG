# Codex Reset Credits Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show each managed Codex account's remaining rate-limit reset credits in the usage widget, and — behind an off-by-default setting — auto-spend one banked reset when the whole managed Codex fleet is >95% weekly with no 7d reset within 24h.

**Architecture:** Extends RMNG's existing server-owned Codex path. The control-server poller (`codex.rs`) already fetches `wham/usage` per account and maps it into the shared `ClaudeUsage` wire type the frontend renders. We add: (1) parse `rate_limit_reset_credits.available_count` into `ClaudeUsage.reset_credits` for display; (2) a pure fleet-gate predicate + a `consume_reset` HTTP call wired into the poll loop; (3) a persisted cooldown (`ControlState.codex_reset_marks`) written through `state::mutate`; (4) a settings toggle. Claude accounts leave the new field `null` — this is deliberately Codex-only.

**Tech Stack:** Rust (control-server + `wire` crate with `ts-rs` TS generation, `reqwest`, `serde_json`), React/React-Router + Tailwind frontend.

## Global Constraints

- **Branch:** all work on `feat/codex-reset-credits` (already checked out).
- **Spec:** `docs/superpowers/specs/2026-07-04-codex-reset-credits-design.md` — authoritative for behavior.
- **Setting default OFF:** `codex.auto_reset` defaults to `false`.
- **Thresholds are compile-time constants**, not config: reuse existing `SEVEN_DAY_CAP_PCT: f64 = 95.0`; add `RESET_MIN_HEADROOM_SECS: i64 = 24 * 3600`.
- **Endpoints (unofficial ChatGPT backend):**
  - Read: `GET https://chatgpt.com/backend-api/wham/usage` (existing `USAGE_URL`).
  - Consume: `POST https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume`, body `{ "redeem_request_id": "<id>" }`, response `{ "code": "reset|nothingToReset|noCredit|alreadyRedeemed", "windows_reset": <i64> }`. Headers mirror `fetch_usage`: `Authorization: Bearer <token>`, `ChatGPT-Account-Id: <account_id>`.
- **Time units:** rate-limit window reset times are **epoch seconds** in the raw API and in `CodexResetMark.window_resets_at`; `now_ms()` returns ms — divide by 1000 for gate math. (`ClaudeUsageWindow.resets_at` remains an ISO string, used only for display; the gate never touches it.)
- **Reserve-before-POST:** the cooldown mark is written *before* the consume request, so no outcome (including a timeout) can double-spend. One consume per poll pass, max.
- **ts-rs regeneration:** the `wire` crate's TS bindings are emitted by its tests. After any `wire` struct change run `cargo test -p wire` and commit the regenerated `frontend/app/lib/wire/*.ts` alongside the Rust change. Never hand-edit generated `wire/*.ts`.
- **`i64` maps to `bigint` in generated TS** (see `ClaudeUsage.lastUpdated: bigint`). Frontend must `Number(...)` these before numeric comparison.
- **Rust tests:** run `cargo test -p wire` and `cargo test -p control-server`. If the local box can't build `control-server`'s system deps, use the CT 106 W6800 build box (see project memory `ct106-w6800-build`).

---

### Task 1: `reset_credits` display field on `ClaudeUsage`

**Files:**
- Modify: `crates/wire/src/control.rs` (`ClaudeUsage` struct ~line 309; `controlstate_roundtrip_camelcase` test ~line 526)
- Regenerate: `frontend/app/lib/wire/ClaudeUsage.ts`

**Interfaces:**
- Produces: `ClaudeUsage.reset_credits: Option<i64>` (serialized `resetCredits`, TS `bigint | null`). Consumed by Tasks 4 (backend sets it) and 8 (frontend reads it).

- [ ] **Step 1: Add the field to the struct**

In `crates/wire/src/control.rs`, inside `pub struct ClaudeUsage`, immediately after the `spend` field, add:

```rust
    /// Codex only: banked rate-limit reset credits ("usage resets") left on the
    /// account. `None` for Claude (no such concept) and when usage is unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_credits: Option<i64>,
```

- [ ] **Step 2: Update the existing round-trip test to carry the field**

In the `controlstate_roundtrip_camelcase` test, in the `ClaudeUsage { … }` literal, add `reset_credits: Some(3),` after `spend: None,`. Then, after the existing `assert!(s.contains("\"fiveHour\""));` line, add:

```rust
        assert!(s.contains("\"resetCredits\":3"));
```

- [ ] **Step 3: Run the test — it must fail to compile first, then pass**

Run: `cargo test -p wire controlstate_roundtrip_camelcase`
Expected: PASS (the struct now has the field and the JSON contains `"resetCredits":3`). If it fails to compile because another `ClaudeUsage { … }` literal in the crate now lacks the field, add `reset_credits: None,` to that literal (e.g. the sample fixture near line 539) and re-run.

- [ ] **Step 4: Regenerate the TS binding**

Run: `cargo test -p wire`
Then: `grep resetCredits frontend/app/lib/wire/ClaudeUsage.ts`
Expected: a line like `resetCredits?: bigint | null,` (or `resetCredits: bigint | null,`) is present.

- [ ] **Step 5: Commit**

```bash
git add crates/wire/src/control.rs frontend/app/lib/wire/ClaudeUsage.ts
git commit -m "feat(wire): add ClaudeUsage.reset_credits (codex resets-left)"
```

---

### Task 2: `CodexResetMark` struct + `codex_reset_marks` on `ControlState`

**Files:**
- Modify: `crates/wire/src/control.rs` (new struct near the other `#[ts]` structs; new field on `ControlState` ~line 357)
- Regenerate: `frontend/app/lib/wire/CodexResetMark.ts`, `frontend/app/lib/wire/ControlState.ts`

**Interfaces:**
- Produces: `wire::CodexResetMark { account_id: String, window_resets_at: i64, consumed_at: i64, redeem_request_id: String }` and `ControlState.codex_reset_marks: Vec<CodexResetMark>`. Consumed by Tasks 5 (predicate reads marks) and 7 (poll loop writes marks).

- [ ] **Step 1: Add the struct**

In `crates/wire/src/control.rs`, directly **above** `pub struct ControlState`, add:

```rust
/// One recorded auto-consumed (or reserved) Codex reset. Persisted in `ControlState`
/// so a server restart can't re-spend on an account already reset this 7d window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "../../../frontend/app/lib/wire/")]
pub struct CodexResetMark {
    pub account_id: String,
    /// The 7d window (its `resets_at` epoch **seconds**) this reset was spent against —
    /// the cooldown key. An account is on cooldown while its current 7d window matches.
    pub window_resets_at: i64,
    /// Wall-clock ms when the mark was reserved / consume attempted (audit / UI tooltip).
    pub consumed_at: i64,
    /// Idempotency key sent to `/consume` for this reservation (audit; enables a future
    /// safe same-key retry — v1 does not retry within a window).
    pub redeem_request_id: String,
}
```

- [ ] **Step 2: Add the field to `ControlState`**

In `pub struct ControlState`, after the `claude_accounts` field, add:

```rust
    /// Codex auto-reset bookkeeping (cooldown). Non-secret; changes at most once per
    /// account per week, so it belongs in `state.json` (unlike per-tick stats).
    #[serde(default)]
    pub codex_reset_marks: Vec<CodexResetMark>,
```

- [ ] **Step 3: Write a round-trip test**

In the `#[cfg(test)] mod tests` block of `control.rs`, add:

```rust
    #[test]
    fn codex_reset_marks_roundtrip_camelcase() {
        let st = ControlState {
            codex_reset_marks: vec![CodexResetMark {
                account_id: "codex:acc-1".into(),
                window_resets_at: 1783392770,
                consumed_at: 1783168000000,
                redeem_request_id: "abc123".into(),
            }],
            ..Default::default()
        };
        let s = serde_json::to_string(&st).unwrap();
        assert!(s.contains("\"codexResetMarks\""));
        assert!(s.contains("\"windowResetsAt\":1783392770"));
        assert!(s.contains("\"redeemRequestId\":\"abc123\""));
        let back: ControlState = serde_json::from_str(&s).unwrap();
        assert_eq!(st, back);
    }
```

- [ ] **Step 4: Run the test**

Run: `cargo test -p wire codex_reset_marks_roundtrip_camelcase`
Expected: PASS.

- [ ] **Step 5: Regenerate TS + verify**

Run: `cargo test -p wire`
Then: `grep -l . frontend/app/lib/wire/CodexResetMark.ts && grep codexResetMarks frontend/app/lib/wire/ControlState.ts`
Expected: `CodexResetMark.ts` exists and `ControlState.ts` references `codexResetMarks`.

- [ ] **Step 6: Commit**

```bash
git add crates/wire/src/control.rs frontend/app/lib/wire/CodexResetMark.ts frontend/app/lib/wire/ControlState.ts
git commit -m "feat(wire): persist codex_reset_marks on ControlState"
```

---

### Task 3: `auto_reset` setting on `CodexConfig`

**Files:**
- Modify: `crates/wire/src/config.rs` (`CodexConfig` struct ~line 244; its `Default` impl ~line 263; config tests ~line 599)
- Regenerate: `frontend/app/lib/wire/CodexConfig.ts`

**Interfaces:**
- Produces: `CodexConfig.auto_reset: bool` (serialized `autoReset`), default `false`. Consumed by Task 7 (`cfg.codex.auto_reset` gates the poller) and Task 9 (settings toggle).

- [ ] **Step 1: Add the field**

In `crates/wire/src/config.rs`, inside `pub struct CodexConfig`, after the `usage_polling` field, add:

```rust
    /// When true, auto-spend one banked reset credit once every managed Codex account
    /// is over the weekly cap with no 7d reset within 24h (see `codex.rs` fleet gate).
    #[serde(default)]
    pub auto_reset: bool,
```

- [ ] **Step 2: Update the `Default` impl**

In `impl Default for CodexConfig`, change the returned literal to include the new field:

```rust
        Self { poll_secs: 600, pinned_email: None, usage_polling: true, auto_reset: false }
```

- [ ] **Step 3: Extend the config tests**

Find the test that asserts codex defaults (near `assert!(c.codex.usage_polling, …)`, ~line 601) and add after it:

```rust
        assert!(!c.codex.auto_reset, "auto_reset defaults to false");
```

Then find the test that toggles codex config from JSON (the block that sets `usage_polling` false via camelCase, ~line 608-612) and, in that JSON string, add `"autoReset":true` to the `codex` object, then assert after the existing usage_polling assertion:

```rust
        assert!(off.codex.auto_reset, "autoReset parses from camelCase JSON");
```

(If the JSON literal is `{"codex":{"pollSecs":300,"usagePolling":false}}`, change it to `{"codex":{"pollSecs":300,"usagePolling":false,"autoReset":true}}`.)

- [ ] **Step 4: Run the tests**

Run: `cargo test -p wire -- codex`
Expected: PASS (default false; camelCase `autoReset` round-trips).

- [ ] **Step 5: Regenerate TS + verify**

Run: `cargo test -p wire`
Then: `grep autoReset frontend/app/lib/wire/CodexConfig.ts`
Expected: `autoReset: boolean,` present.

- [ ] **Step 6: Commit**

```bash
git add crates/wire/src/config.rs frontend/app/lib/wire/CodexConfig.ts
git commit -m "feat(wire): add codex.auto_reset setting (default off)"
```

---

### Task 4: Parse `rate_limit_reset_credits` into the usage view

**Files:**
- Modify: `crates/control-server/src/codex.rs` (`RawUsage` ~line 447; `to_usage` ~line 485; `codex_base` just below)
- Test: same file's `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `ClaudeUsage.reset_credits` (Task 1).
- Produces: `to_usage()` now populates `reset_credits`; a new `RawResetCredits` type and `RawUsage.rate_limit_reset_credits` field used by Task 5's `gate_facts`.

- [ ] **Step 1: Write the failing test**

In the `mod tests` block of `codex.rs`, add:

```rust
    #[test]
    fn to_usage_reads_reset_credits() {
        let raw: RawUsage = serde_json::from_str(
            r#"{"plan_type":"pro","rate_limit":{"secondary_window":{"used_percent":96,"limit_window_seconds":604800,"reset_at":1783392770}},"rate_limit_reset_credits":{"available_count":4}}"#,
        )
        .unwrap();
        let u = to_usage(&sample_account(), raw);
        assert_eq!(u.reset_credits, Some(4));
        assert_eq!(u.seven_day.unwrap().pct, 96.0);
    }

    #[test]
    fn to_usage_absent_reset_credits_is_none() {
        let raw: RawUsage = serde_json::from_str(r#"{"rate_limit":{}}"#).unwrap();
        assert_eq!(to_usage(&sample_account(), raw).reset_credits, None);
    }
```

- [ ] **Step 2: Run it — expect a compile failure**

Run: `cargo test -p control-server to_usage_reads_reset_credits`
Expected: FAIL to compile — `RawUsage` has no `rate_limit_reset_credits`, and `ClaudeUsage` construction in `to_usage`/`codex_base` will error once the field is required-in-test.

- [ ] **Step 3: Add the raw type + field**

In `codex.rs`, add near `RawUsage` (after the `struct RawUsage { … }` block):

```rust
#[derive(Deserialize)]
struct RawResetCredits {
    #[serde(default)]
    available_count: Option<i64>,
}
```

And add a field to `struct RawUsage`:

```rust
    #[serde(default)]
    rate_limit_reset_credits: Option<RawResetCredits>,
```

- [ ] **Step 4: Set `reset_credits` in `to_usage` and `codex_base`**

In `to_usage`, capture the count before building `ClaudeUsage` (place it before the `ClaudeUsage { … }` return):

```rust
    let reset_credits =
        raw.rate_limit_reset_credits.as_ref().and_then(|c| c.available_count);
```

Then add `reset_credits,` to the returned `ClaudeUsage { … }` literal (after `spend: None,`). In `codex_base`, add `reset_credits: None,` to its `ClaudeUsage { … }` literal.

- [ ] **Step 5: Run the tests to green**

Run: `cargo test -p control-server to_usage`
Expected: PASS (both new tests).

- [ ] **Step 6: Commit**

```bash
git add crates/control-server/src/codex.rs
git commit -m "feat(codex): parse rate_limit_reset_credits into usage view"
```

---

### Task 5: Fleet-gate predicate (pure, unit-tested)

**Files:**
- Modify: `crates/control-server/src/codex.rs` (new `RESET_MIN_HEADROOM_SECS` const near the other consts; new `FleetFacts` struct, `gate_facts`, `choose_reset_target`, `prune_marks`)
- Test: same file's `mod tests`

**Interfaces:**
- Consumes: `wire::CodexResetMark` (Task 2), `RawUsage`/`RawResetCredits` (Task 4), `SEVEN_DAY_CAP_PCT`.
- Produces:
  - `fn gate_facts(account_id: &str, raw: &RawUsage) -> Option<FleetFacts>`
  - `fn choose_reset_target(facts: &[FleetFacts], account_count: usize, marks: &[wire::CodexResetMark], now_secs: i64, enabled: bool) -> Option<String>` (returns the chosen account id)
  - `fn prune_marks(marks: &mut Vec<wire::CodexResetMark>, now_secs: i64)`
  - `struct FleetFacts { account_id: String, seven_pct: f64, seven_reset_at: i64, reset_credits: i64 }`
  Consumed by Task 7.

- [ ] **Step 1: Write the failing tests**

In `mod tests`, add:

```rust
    fn facts(id: &str, pct: f64, reset_at: i64, credits: i64) -> FleetFacts {
        FleetFacts { account_id: id.into(), seven_pct: pct, seven_reset_at: reset_at, reset_credits: credits }
    }
    const DAY: i64 = 24 * 3600;

    #[test]
    fn gate_fires_picks_max_credits_when_all_capped_and_far() {
        let now = 1_000_000;
        let f = vec![
            facts("codex:a", 96.0, now + 2 * DAY, 1),
            facts("codex:b", 99.0, now + 3 * DAY, 4),
        ];
        assert_eq!(choose_reset_target(&f, 2, &[], now, true), Some("codex:b".into()));
    }

    #[test]
    fn gate_blocked_when_setting_off() {
        let now = 1_000_000;
        let f = vec![facts("codex:a", 99.0, now + 2 * DAY, 4)];
        assert_eq!(choose_reset_target(&f, 1, &[], now, false), None);
    }

    #[test]
    fn gate_blocked_when_any_account_below_cap() {
        let now = 1_000_000;
        let f = vec![facts("codex:a", 96.0, now + 2 * DAY, 4), facts("codex:b", 90.0, now + 2 * DAY, 4)];
        assert_eq!(choose_reset_target(&f, 2, &[], now, true), None);
    }

    #[test]
    fn gate_blocked_when_any_resets_within_24h() {
        let now = 1_000_000;
        let f = vec![facts("codex:a", 99.0, now + 2 * DAY, 4), facts("codex:b", 99.0, now + 3600, 4)];
        assert_eq!(choose_reset_target(&f, 2, &[], now, true), None);
    }

    #[test]
    fn gate_blocked_when_facts_incomplete() {
        // Only 1 of 2 accounts reported fresh usage → never fire on partial data.
        let now = 1_000_000;
        let f = vec![facts("codex:a", 99.0, now + 2 * DAY, 4)];
        assert_eq!(choose_reset_target(&f, 2, &[], now, true), None);
    }

    #[test]
    fn gate_skips_accounts_out_of_credit_or_on_cooldown() {
        let now = 1_000_000;
        let f = vec![
            facts("codex:a", 99.0, now + 2 * DAY, 0), // no credit
            facts("codex:b", 99.0, now + 2 * DAY, 2), // on cooldown this window
        ];
        let marks = vec![wire::CodexResetMark {
            account_id: "codex:b".into(),
            window_resets_at: now + 2 * DAY,
            consumed_at: 0,
            redeem_request_id: "x".into(),
        }];
        assert_eq!(choose_reset_target(&f, 2, &marks, now, true), None);
    }

    #[test]
    fn cooldown_clears_when_window_rolls() {
        let now = 1_000_000;
        let f = vec![facts("codex:b", 99.0, now + 9 * DAY, 2)]; // new window resets_at
        let marks = vec![wire::CodexResetMark {
            account_id: "codex:b".into(),
            window_resets_at: now + 2 * DAY, // stale window
            consumed_at: 0,
            redeem_request_id: "x".into(),
        }];
        assert_eq!(choose_reset_target(&f, 1, &marks, now, true), Some("codex:b".into()));
    }

    #[test]
    fn prune_drops_elapsed_windows() {
        let now = 1_000_000;
        let mut marks = vec![
            wire::CodexResetMark { account_id: "a".into(), window_resets_at: now - 10, consumed_at: 0, redeem_request_id: "x".into() },
            wire::CodexResetMark { account_id: "b".into(), window_resets_at: now + 10, consumed_at: 0, redeem_request_id: "y".into() },
        ];
        prune_marks(&mut marks, now);
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].account_id, "b");
    }

    #[test]
    fn gate_facts_extracts_weekly_window_and_credits() {
        let raw: RawUsage = serde_json::from_str(
            r#"{"rate_limit":{"primary_window":{"used_percent":10,"limit_window_seconds":18000,"reset_at":111},"secondary_window":{"used_percent":97,"limit_window_seconds":604800,"reset_at":222}},"rate_limit_reset_credits":{"available_count":3}}"#,
        ).unwrap();
        let ff = gate_facts("codex:a", &raw).unwrap();
        assert_eq!(ff.seven_pct, 97.0);
        assert_eq!(ff.seven_reset_at, 222);
        assert_eq!(ff.reset_credits, 3);
    }
```

- [ ] **Step 2: Run — expect failure**

Run: `cargo test -p control-server -- gate_ choose_reset prune_ cooldown_`
Expected: FAIL to compile (`FleetFacts`, `choose_reset_target`, `prune_marks`, `gate_facts` undefined).

- [ ] **Step 3: Add the const**

Near the other `codex.rs` consts (below `SEVEN_DAY_CAP_PCT`), add:

```rust
/// Auto-reset only fires when every account's 7d window is at least this far from
/// resetting (spec: "more than 24h from the next 7d reset").
const RESET_MIN_HEADROOM_SECS: i64 = 24 * 3600;
```

- [ ] **Step 4: Implement the predicate + helpers**

Add (near the usage-mapping helpers, after `codex_base`):

```rust
/// Per-account inputs the fleet gate needs, extracted from a fresh raw usage fetch
/// (epoch-seconds based, so the gate never round-trips the display ISO string).
struct FleetFacts {
    account_id: String,
    seven_pct: f64,
    seven_reset_at: i64,
    reset_credits: i64,
}

/// Extract gate facts from a raw usage response. `None` if the weekly window or its
/// reset epoch is missing — such an account can't be confirmed, so the gate won't fire.
fn gate_facts(account_id: &str, raw: &RawUsage) -> Option<FleetFacts> {
    let rl = raw.rate_limit.as_ref()?;
    // Weekly window = the one whose length is nearer a week than 5h (never by field order).
    let seven = [rl.primary_window.as_ref(), rl.secondary_window.as_ref()]
        .into_iter()
        .flatten()
        .find(|w| {
            let s = w.limit_window_seconds.unwrap_or(0);
            (s - 604_800).abs() <= (s - 18_000).abs()
        })?;
    Some(FleetFacts {
        account_id: account_id.to_string(),
        seven_pct: seven.used_percent.unwrap_or(0.0),
        seven_reset_at: seven.reset_at?,
        reset_credits: raw
            .rate_limit_reset_credits
            .as_ref()
            .and_then(|c| c.available_count)
            .unwrap_or(0),
    })
}

/// The fleet gate. Returns the account id to spend one reset on, or `None`.
fn choose_reset_target(
    facts: &[FleetFacts],
    account_count: usize,
    marks: &[wire::CodexResetMark],
    now_secs: i64,
    enabled: bool,
) -> Option<String> {
    if !enabled || account_count == 0 || facts.len() != account_count {
        return None; // off, no accounts, or incomplete fresh data → never fire.
    }
    let all_capped = facts.iter().all(|f| f.seven_pct > SEVEN_DAY_CAP_PCT);
    let none_soon = facts
        .iter()
        .all(|f| f.seven_reset_at - now_secs >= RESET_MIN_HEADROOM_SECS);
    if !all_capped || !none_soon {
        return None;
    }
    let mut eligible: Vec<&FleetFacts> = facts
        .iter()
        .filter(|f| {
            f.reset_credits > 0
                && !marks.iter().any(|m| {
                    m.account_id == f.account_id && m.window_resets_at == f.seven_reset_at
                })
        })
        .collect();
    // Most credits first; tie-break by soonest reset.
    eligible.sort_by(|a, b| {
        b.reset_credits
            .cmp(&a.reset_credits)
            .then(a.seven_reset_at.cmp(&b.seven_reset_at))
    });
    eligible.first().map(|f| f.account_id.clone())
}

/// Drop marks whose 7d window has already elapsed (account is now in a new window).
fn prune_marks(marks: &mut Vec<wire::CodexResetMark>, now_secs: i64) {
    marks.retain(|m| m.window_resets_at > now_secs);
}
```

- [ ] **Step 5: Run to green**

Run: `cargo test -p control-server -- gate_ choose_reset prune_ cooldown_`
Expected: PASS (all predicate tests).

- [ ] **Step 6: Commit**

```bash
git add crates/control-server/src/codex.rs
git commit -m "feat(codex): fleet-gate predicate for auto-reset (pure, tested)"
```

---

### Task 6: `consume_reset` HTTP call + outcome parsing

**Files:**
- Modify: `crates/control-server/src/codex.rs` (new `CONSUME_URL` const; `ConsumeOutcome` enum; `parse_consume_outcome`; `new_request_id`; `consume_reset`)
- Test: same file's `mod tests`

**Interfaces:**
- Produces:
  - `enum ConsumeOutcome { Reset, NothingToReset, NoCredit, AlreadyRedeemed, Unknown(String) }`
  - `fn parse_consume_outcome(body: &str) -> ConsumeOutcome`
  - `fn new_request_id() -> String`
  - `async fn consume_reset(http: &reqwest::Client, token: &str, account_id: &str, request_id: &str) -> Result<ConsumeOutcome>`
  Consumed by Task 7.

- [ ] **Step 1: Write the failing test (parsing is the unit-testable seam)**

In `mod tests`, add:

```rust
    #[test]
    fn parse_consume_outcomes() {
        assert_eq!(parse_consume_outcome(r#"{"code":"reset","windows_reset":2}"#), ConsumeOutcome::Reset);
        assert_eq!(parse_consume_outcome(r#"{"code":"noCredit"}"#), ConsumeOutcome::NoCredit);
        assert_eq!(parse_consume_outcome(r#"{"code":"alreadyRedeemed"}"#), ConsumeOutcome::AlreadyRedeemed);
        assert_eq!(parse_consume_outcome(r#"{"code":"nothingToReset"}"#), ConsumeOutcome::NothingToReset);
        assert_eq!(parse_consume_outcome(r#"{"code":"wat"}"#), ConsumeOutcome::Unknown("wat".into()));
        assert_eq!(parse_consume_outcome("not json"), ConsumeOutcome::Unknown(String::new()));
    }

    #[test]
    fn request_id_is_nonempty_and_varies() {
        let a = new_request_id();
        let b = new_request_id();
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
    }
```

- [ ] **Step 2: Run — expect failure**

Run: `cargo test -p control-server -- parse_consume request_id_is`
Expected: FAIL to compile (symbols undefined).

- [ ] **Step 3: Implement**

Add the const near `USAGE_URL`:

```rust
const CONSUME_URL: &str = "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume";
```

Add the enum + functions (near `fetch_usage`):

```rust
#[derive(Debug, PartialEq, Eq)]
enum ConsumeOutcome {
    Reset,
    NothingToReset,
    NoCredit,
    AlreadyRedeemed,
    Unknown(String),
}

fn parse_consume_outcome(body: &str) -> ConsumeOutcome {
    let code = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("code").and_then(|c| c.as_str()).map(str::to_string))
        .unwrap_or_default();
    match code.as_str() {
        "reset" => ConsumeOutcome::Reset,
        "nothingToReset" => ConsumeOutcome::NothingToReset,
        "noCredit" => ConsumeOutcome::NoCredit,
        "alreadyRedeemed" => ConsumeOutcome::AlreadyRedeemed,
        other => ConsumeOutcome::Unknown(other.to_string()),
    }
}

/// A 32-hex-char idempotency key (no `uuid` dep; `rand_u64` from `clone_ops`).
fn new_request_id() -> String {
    format!("{:016x}{:016x}", rand_u64(), rand_u64())
}

/// POST one reset-credit consume. Mirrors `fetch_usage` headers/timeout/error style.
async fn consume_reset(
    http: &reqwest::Client,
    token: &str,
    account_id: &str,
    request_id: &str,
) -> Result<ConsumeOutcome> {
    let resp = http
        .post(CONSUME_URL)
        .timeout(FETCH_TIMEOUT)
        .header("Authorization", format!("Bearer {token}"))
        .header("ChatGPT-Account-Id", account_id)
        .json(&serde_json::json!({ "redeem_request_id": request_id }))
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("consume {}{}", status.as_u16(), snippet(&text));
    }
    Ok(parse_consume_outcome(&text))
}
```

- [ ] **Step 4: Run to green**

Run: `cargo test -p control-server -- parse_consume request_id_is`
Expected: PASS. (`consume_reset` itself is exercised manually in Task 10 — it makes a real network call.)

- [ ] **Step 5: Commit**

```bash
git add crates/control-server/src/codex.rs
git commit -m "feat(codex): consume_reset endpoint + outcome parsing"
```

---

### Task 7: Wire the gate into the poll loop

**Files:**
- Modify: `crates/control-server/src/codex.rs` (`poll_inner` ~line 821)

**Interfaces:**
- Consumes: `gate_facts`, `choose_reset_target`, `prune_marks`, `consume_reset`, `new_request_id`, `ConsumeOutcome` (Tasks 5–6); `fresh_access_token`, `fetch_usage`, `to_usage`, `replace_provider_views`, `app.store.mutate`, `app.codex.last_good`.
- Produces: end-to-end auto-reset behavior. No new public symbols.

This task has no cheap unit test (it orchestrates async HTTP + shared state); its verification is `cargo build` + the manual run in Task 10. Keep each edit small and re-`cargo build -p control-server` after.

- [ ] **Step 1: Collect fleet facts during the fetch loop**

In `poll_inner`, before the `for (i, acct)` loop, add:

```rust
    let mut fleet: Vec<FleetFacts> = Vec::with_capacity(accts.len());
```

Inside the loop, change the success async block so it returns the gate facts alongside the view. Replace:

```rust
            let raw = fetch_usage(&app.http, &fresh.access_token, &fresh.account_id).await?;
            Ok::<_, anyhow::Error>(to_usage(acct, raw))
```

with:

```rust
            let raw = fetch_usage(&app.http, &fresh.access_token, &fresh.account_id).await?;
            let facts = gate_facts(&acct.id, &raw); // borrow before `raw` moves into to_usage
            Ok::<_, anyhow::Error>((to_usage(acct, raw), facts))
```

The disabled-polling early-return in that block currently returns a bare `b`; change it to `return Ok::<_, anyhow::Error>((b, None));`.

Update the `match outcome` arms: the `Ok(u)` arm becomes `Ok((u, facts))` — push the view as before **and** `if let Some(f) = facts { fleet.push(f); }`. The `Err(e)` arm is unchanged (no facts → account absent from `fleet`, so the gate can't fire on partial data).

- [ ] **Step 2: Set `assignable` and run the gate before publishing views**

The existing code sets `assignable = Some(true)` on each view and then calls `replace_provider_views(...)`. Insert the gate logic **between** the `assignable` loop and the `replace_provider_views` call:

```rust
    // --- fleet auto-reset gate ---------------------------------------------
    let now_secs = now_ms() / 1000;
    let marks = app.store.get().codex_reset_marks;
    if let Some(target_id) =
        choose_reset_target(&fleet, accts.len(), &marks, now_secs, cfg.codex.auto_reset)
    {
        if let (Some(target_facts), Some(acct)) = (
            fleet.iter().find(|f| f.account_id == target_id),
            accts.iter().find(|a| a.id == target_id),
        ) {
            let window = target_facts.seven_reset_at;
            let req_id = new_request_id();
            // Reserve the cooldown mark BEFORE the POST (no outcome can double-spend).
            app.store.mutate(|s| {
                s.codex_reset_marks.retain(|m| m.account_id != target_id);
                s.codex_reset_marks.push(wire::CodexResetMark {
                    account_id: target_id.clone(),
                    window_resets_at: window,
                    consumed_at: now_ms(),
                    redeem_request_id: req_id.clone(),
                });
                prune_marks(&mut s.codex_reset_marks, now_secs);
            });
            match fresh_access_token(app, &acct.email).await {
                Ok((fresh, _)) => {
                    match consume_reset(&app.http, &fresh.access_token, &fresh.account_id, &req_id)
                        .await
                    {
                        Ok(ConsumeOutcome::Reset) => {
                            tracing::info!(
                                "codex auto-reset consumed for {} (7d was {:.0}%); re-polling",
                                acct.email,
                                target_facts.seven_pct
                            );
                            // Best-effort immediate re-poll of just this account.
                            if let Ok(raw2) =
                                fetch_usage(&app.http, &fresh.access_token, &fresh.account_id).await
                            {
                                let u2 = to_usage(&acct, raw2);
                                tracing::info!(
                                    "codex auto-reset after: {} 7d={:?} credits={:?}",
                                    acct.email,
                                    u2.seven_day.as_ref().map(|w| w.pct),
                                    u2.reset_credits
                                );
                                app.codex.last_good.lock().unwrap().insert(acct.id.clone(), u2.clone());
                                if let Some(v) = views.iter_mut().find(|v| v.id == acct.id) {
                                    *v = u2;
                                    v.assignable = Some(true);
                                }
                            }
                        }
                        Ok(other) => tracing::warn!(
                            "codex auto-reset for {}: {:?} (mark kept, no retry this window)",
                            acct.email,
                            other
                        ),
                        Err(e) => tracing::warn!(
                            "codex auto-reset consume for {} failed: {e} (mark kept)",
                            acct.email
                        ),
                    }
                }
                Err(e) => tracing::warn!(
                    "codex auto-reset: token refresh for {} failed: {e} (mark kept)",
                    acct.email
                ),
            }
        }
    }
```

(Leave the subsequent `replace_provider_views(...)` and `push_stale_tokens(app).await;` calls as they are — they now publish the possibly-updated `views`.)

- [ ] **Step 3: Build**

Run: `cargo build -p control-server`
Expected: compiles clean. Fix any borrow error by ensuring `marks`/`target_facts` reads happen before the `app.store.mutate` / `views.iter_mut()` writes (they do in the snippet above).

- [ ] **Step 4: Run the full codex test suite (no regressions)**

Run: `cargo test -p control-server`
Expected: PASS (all prior tests still green; the new orchestration has no unit test but must not break existing ones).

- [ ] **Step 5: Commit**

```bash
git add crates/control-server/src/codex.rs
git commit -m "feat(codex): auto-consume one reset when the fleet is weekly-capped"
```

---

### Task 8: Reset-credits badge in the accounts widget

**Files:**
- Modify: `frontend/app/components/ClaudeAccountsPanel.tsx` (`Row`, ~line 84)
- Modify: `frontend/app/stories/fixtures.ts` (add `resetCredits` to a codex fixture, ~line 154+)

**Interfaces:**
- Consumes: `ClaudeUsage.resetCredits: bigint | null` (Task 1).

- [ ] **Step 1: Compute the display value at the top of `Row`**

In `ClaudeAccountsPanel.tsx`, change the `Row` signature body to compute a numeric credit count. Right after `function Row({ a, now }: { a: ClaudeUsage; now: number | null }) {`, add:

```tsx
  const resetCredits =
    a.provider === "codex" && a.resetCredits != null ? Number(a.resetCredits) : null;
```

- [ ] **Step 2: Render the badge in the header row**

In `Row`'s header `<div className="flex items-center gap-1.5">`, after the `{a.spend ? ( … ) : null}` block, add:

```tsx
        {resetCredits != null ? (
          <span
            className={`shrink-0 text-[10px] tabular-nums ${
              resetCredits === 0 ? "text-rose-400" : "text-slate-500 dark:text-slate-400"
            }`}
            title="Banked Codex rate-limit resets left"
          >
            ⟳ {resetCredits}
          </span>
        ) : null}
```

- [ ] **Step 3: Add a fixture value for Storybook**

In `frontend/app/stories/fixtures.ts`, find a codex account entry in the `claudeAccounts` array (`provider: "codex"`). Add `resetCredits: 3n,` to it (the `n` suffix — it's a `bigint`). If no codex entry exists in that array, add `resetCredits` to one Claude entry as `resetCredits: null,` is unnecessary; instead ensure at least one codex fixture carries `resetCredits: 3n` so the badge is visible in the story.

- [ ] **Step 4: Type-check the frontend**

Run: `cd frontend && npm run typecheck`
(If the project uses a different script, run `npx tsc --noEmit`.)
Expected: no type errors — `resetCredits` exists on `ClaudeUsage` and `Number(bigint)` is valid.

- [ ] **Step 5: Visual check (optional but recommended)**

Run the Storybook or dev server (`cd frontend && npm run storybook` or `npm run dev`) and confirm a codex account row shows `⟳ 3`, and `⟳ 0` renders rose. (See project memory `frontend-icons-dark-theme` for the light/dark conventions.)

- [ ] **Step 6: Commit**

```bash
git add frontend/app/components/ClaudeAccountsPanel.tsx frontend/app/stories/fixtures.ts
git commit -m "feat(frontend): show codex reset credits in the accounts widget"
```

---

### Task 9: Auto-reset toggle in Settings

**Files:**
- Modify: `frontend/app/components/SettingsPanel.tsx` (codex `useState` ~line 217; the codex controls block ~line 908)

**Interfaces:**
- Consumes: `CodexConfig.autoReset` (Task 3). The existing load (`...c.codex`) and save (`...codex`) spreads already carry the field once it exists — we only add the default to `useState` and the checkbox.

- [ ] **Step 1: Add `autoReset` to the codex form state**

In `SettingsPanel.tsx`, in the codex `useState` initializer (~line 217), add `autoReset: false,`:

```tsx
  const [codex, setCodex] = useState({
    pollSecs: 600,
    pinnedEmail: "",
    usagePolling: true,
    autoReset: false,
  });
```

- [ ] **Step 2: Add the checkbox after the `usagePolling` control**

Directly after the existing `usagePolling` `<label> … </label>` block (~line 908–915), add:

```tsx
                <label className="col-span-2 flex items-center gap-2 text-sm text-slate-600">
                  <input
                    type="checkbox"
                    checked={codex.autoReset}
                    onChange={(e) => setCodex({ ...codex, autoReset: e.target.checked })}
                  />
                  Auto-use Codex reset credits (when every account is &gt;95% weekly and none
                  reset within 24h, spend one banked reset to bring an account back)
                </label>
```

- [ ] **Step 3: Confirm load + save carry the field**

No code change needed, but verify: the config-load effect uses `setCodex({ ...c.codex, … })` (spread includes `autoReset`), and the save patch uses `codex: { ...codex, pinnedEmail: codex.pinnedEmail || null }` (spread includes `autoReset`). Read both lines to confirm the spreads are present; if either explicitly enumerates fields instead of spreading, add `autoReset: codex.autoReset` / `autoReset: c.codex.autoReset` accordingly.

- [ ] **Step 4: Type-check**

Run: `cd frontend && npm run typecheck` (or `npx tsc --noEmit`)
Expected: no type errors.

- [ ] **Step 5: Commit**

```bash
git add frontend/app/components/SettingsPanel.tsx
git commit -m "feat(frontend): add codex auto-reset settings toggle"
```

---

### Task 10: End-to-end verification (manual; one step spends a real credit)

**Files:** none (verification only).

This is the only place the unofficial `/consume` endpoint is exercised live. Do it deliberately — it spends one of the ~4 available credits and is the sole way to confirm the request shape and observe which window(s) a reset clears.

- [ ] **Step 1: Full build + test sweep**

Run: `cargo test -p wire && cargo test -p control-server && (cd frontend && npm run typecheck)`
Expected: all green.

- [ ] **Step 2: Display path (no spend) on a running instance**

Deploy the branch to a staging control-server with imported Codex accounts (see project memory `deploy-staging-ct106`). Open the UI; confirm each Codex account row shows `⟳ N` matching that account's real remaining resets (cross-check against the read-only `wham/usage` for one account). Confirm the Settings toggle loads as **off** and persists on/off across a reload.

- [ ] **Step 3: Gate dry-run with the setting OFF**

With `codex.auto_reset` off, confirm from logs that no consume is attempted even if an account is >95% weekly. (Grep the control-server log for `codex auto-reset`.) Expected: nothing.

- [ ] **Step 4: One guarded live consume (spends 1 credit)**

Only when you intend to spend one: enable the setting on an instance whose fleet genuinely meets the gate (all accounts >95% weekly, none resetting within 24h) — or temporarily lower `RESET_MIN_HEADROOM_SECS`/`SEVEN_DAY_CAP_PCT` in a throwaway build to force the gate on a single test account. Watch the log for:
  - `codex auto-reset consumed for <email> … re-polling`
  - `codex auto-reset after: <email> 7d=Some(<pct>) credits=Some(<n-1>)`
Record **which window dropped** (5h, 7d, or both) and confirm `credits` decremented by exactly 1. Bake the observed reset semantics into a code comment above `consume_reset`.

- [ ] **Step 5: Cooldown + persistence check**

Confirm a second poll in the same 7d window does **not** consume again for that account (log shows no repeat), and that `state.json` contains the `codexResetMarks` entry. Restart the control-server and confirm it still does not re-consume for that account/window (mark survived restart).

- [ ] **Step 6: Finalize**

If Step 4's observed semantics differ from the spec's assumption (e.g. a reset only clears the 5h window), note it in the spec's "Unofficial-API risk" section and confirm the per-window cooldown still bounds spend as designed (it does). Commit any comment/doc updates:

```bash
git add -A
git commit -m "docs(codex): record observed reset-credit semantics from live check"
```

---

## Self-Review

**1. Spec coverage:**
- Display of resets-left → Tasks 1, 4, 8. ✓
- Fleet gate (all >95% AND none <24h) → Task 5 (`choose_reset_target`). ✓
- One credit per poll, max-credits pick, tie-break → Task 5. ✓
- Cooldown once per account per 7d window, persisted in `state.json` → Tasks 2, 5 (`prune_marks`/cooldown filter), 7 (reserve via `state::mutate`). ✓
- Reserve-before-POST + idempotency key → Task 7 (mark written before `consume_reset`), Task 6 (`new_request_id`). ✓
- Immediate best-effort re-poll after `reset` → Task 7. ✓
- Setting default off → Tasks 3, 9. ✓
- Constants not config → Task 5 (`RESET_MIN_HEADROOM_SECS`), reuse `SEVEN_DAY_CAP_PCT`. ✓
- Testing (gate table tests, parse tests, config round-trip, the one live check) → Tasks 5, 6, 4, 3, 10. ✓
- Non-goals (no manual button, no per-account toggle, no Claude side) → respected; nothing added. ✓

**2. Placeholder scan:** No TBD/TODO/"handle errors"/"similar to Task N" — every code step carries real code. ✓

**3. Type consistency:** `reset_credits: Option<i64>` / `resetCredits: bigint | null` used consistently (Tasks 1→4→8, with `Number(...)` at the frontend). `CodexResetMark` field names (`account_id`, `window_resets_at`, `consumed_at`, `redeem_request_id`) identical across Tasks 2, 5, 7. `choose_reset_target` / `gate_facts` / `prune_marks` / `FleetFacts` / `ConsumeOutcome` / `consume_reset` signatures match between their defining task and Task 7's call sites. `CONSUME_URL`/`RESET_MIN_HEADROOM_SECS` defined once. ✓
