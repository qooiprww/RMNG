# Codex reset credits — display + gated fleet auto-reset

**Date:** 2026-07-04
**Status:** Approved design, pending implementation plan
**Scope:** Codex only (`codex.rs`, the Codex half of the shared `ClaudeUsage` wire type, the accounts widget, and settings). Claude has no equivalent of "reset credits," so this is deliberately *not* a symmetric `claude.rs`/`codex.rs` change — Claude accounts leave the new field `null`.

## Problem

OpenAI's Codex (ChatGPT-plan auth) grants a small pool of **rate-limit reset credits** ("usage resets"): banked tokens that, when consumed, reset an account's rate-limit window(s). They are scarce (a handful per account), expire ~30 days after grant, and today RMNG neither shows how many are left nor uses them.

We want two things:

1. **Display** — surface each managed Codex account's remaining reset count in the existing AI-usage widget.
2. **Gated auto-use** — behind an off-by-default setting, automatically spend one reset when the *entire* managed Codex fleet is weekly-capped with no near-term relief, so clones don't sit dead for days.

## Backend facts (verified)

The Codex poller already fetches `GET https://chatgpt.com/backend-api/wham/usage` per account (`codex.rs` `USAGE_URL`, `fetch_usage`). That response carries, in addition to the rate-limit windows RMNG already parses:

```jsonc
"rate_limit_reset_credits": { "available_count": 4 }   // integer; the "resets left" number
```

The **consume** endpoint (reverse-engineered from the `0.142.5` binary and corroborated by openai/codex PR #28143 — `codex-rs/backend-client/src/client/rate_limit_resets.rs`):

```
POST https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume
Headers:  Authorization: Bearer <access_token>
          ChatGPT-Account-Id: <account_id>
          Content-Type: application/json
Body:     { "redeem_request_id": "<uuid>" }          # idempotency key; reuse on retry
Response: { "code": "<reset|nothingToReset|noCredit|alreadyRedeemed>", "windows_reset": <i64> }
```

> **Unofficial-API risk:** the exact request field name (`redeem_request_id`), and *which* windows a reset clears (5h only / 7d / both), are unverified against a live call. The design is robust to either answer (see §1), but a single real consume is the only way to confirm the request shape and observe the effect — and it costs one of the ~4 available credits. Called out in Testing.

## Design

### 1. Trigger semantics — fleet gate, one credit per poll, persisted cooldown

Evaluated inside the existing poller `poll_inner` (`codex.rs`), once per pass, *after* all accounts' usage has been fetched and the `Vec<ClaudeUsage>` views built. Cadence is the existing `codex.poll_secs` (floored at 15s; default 600s).

The **fleet gate** fires when ALL of:

- `cfg.codex.auto_reset == true` — the setting (default **false**).
- The managed Codex fleet is non-empty AND **every** account has `seven_day.pct > 95` (reuse existing `SEVEN_DAY_CAP_PCT = 95.0`).
- **No** account's 7d window resets within 24h — for every account, `seven_day.resets_at - now >= 24h` (new `RESET_MIN_HEADROOM_SECS = 24*3600`). An account with unknown/absent `seven_day` fails the gate (we never fire on incomplete data).

When the gate fires, pick **one** account to spend on:

- eligible = `reset_credits > 0` AND **not on cooldown for its current 7d window** (see below);
- among eligible, choose **max `reset_credits`**; tie-break by **soonest `seven_day.resets_at`**;
- if no account is eligible (all out of credits or all on cooldown), do nothing.

Then, in order: **reserve** a cooldown mark for that account (§2) via `state::mutate` with a fresh idempotency key, *then* issue `consume_reset(account)` with that key, and on `code == reset` immediately re-poll that one account (§3). **At most one consume per poll pass**, always. The next scheduled poll re-fetches the whole fleet and re-evaluates from fresh data — this is the convergence step.

**Reserve-before-POST (safety ordering):** the mark is written *before* the request, so the account is on cooldown for this window regardless of the request's outcome or failure. This is deliberately conservative for a scarce, irreversible spend: a timeout with unknown outcome cannot cause a double-spend, and a transport error costs at most one skipped attempt for that account this window — the *other* still-capped accounts remain eligible next poll, so a single failed consume never blocks the whole fleet. `noCredit` / `nothingToReset` outcomes keep the mark too (don't retry this window); only genuinely unexpected states are logged loudly.

**Cooldown = once per account per 7d window.** An account is on cooldown iff a persisted mark exists with `account_id == a.id && window_resets_at == a.seven_day.resets_at`. When that account's 7d window rolls (its `resets_at` advances to a later epoch), the old mark no longer matches and the account becomes eligible again.

**Why this is safe regardless of what a reset actually clears:** the per-account-per-window cooldown bounds total spend at **one credit per account per 7d window**.
- If consuming drops that account's 7d below 95%, the gate clears next poll → only **1** credit spent (ideal case).
- If a reset only clears the 5h window and 7d stays >95%, the gate stays hot and the poller walks *one account per poll* until each has spent its single per-window credit or run out — bounded at ≤ (fleet size) credits/week, never a runaway loop.

Spend policy is "**cooldown only, spend down to zero**" — no reserve credit is kept (per decision). Thresholds (95%, 24h) are compile-time constants matching the existing `codex.rs` style, not config.

### 2. Persisted cooldown in `state.json`

Cooldown lives in `ControlState` (persisted atomically + broadcast by `state::mutate`, exactly like `claude_accounts`/`operations`). It changes at most once per account per week, so it does **not** violate the "keep per-tick churn out of `state.json`" rule that excludes volatile stats.

```rust
// wire/src/control.rs — new struct, ts-rs exported to frontend
#[serde(rename_all = "camelCase")]
pub struct CodexResetMark {
    pub account_id: String,
    /// The 7d window (its resets_at epoch, seconds) this reset was spent against — the cooldown key.
    pub window_resets_at: i64,
    /// Wall-clock ms when the mark was reserved / consume attempted (audit / future UI tooltip).
    pub consumed_at: i64,
    /// Idempotency key sent to `/consume` for this reservation (audit; enables a safe
    /// same-key retry if ever needed — v1 does not retry within a window).
    pub redeem_request_id: String,
}

// on ControlState:
#[serde(default)]
pub codex_reset_marks: Vec<CodexResetMark>,
```

- **Write path:** the mark is reserved (append, or replace any same-account mark) **through `state::mutate`** *before* the consume POST (§1 reserve-before-POST) — no bespoke save logic; persistence + broadcast come for free. Surviving restart means a mid-week server bounce cannot re-spend on an already-reset (or already-attempted) account.
- **Pruning:** opportunistically drop marks whose `account_id` matches no current account, or whose `window_resets_at < now` (window already elapsed), so the Vec can't grow unbounded.
- This replaces the in-memory cooldown map from earlier drafts entirely — `codex_reset_marks` is the single source of truth.
- The marks are broadcast (non-secret, tiny); the frontend may later cross-reference them ("auto-reset used this week"), but v1 does not depend on that.

### 3. Immediate best-effort re-poll after a successful consume

Without this, a consumed account keeps showing >95% for up to `poll_secs` (10 min) and we never confirm the effect.

On `code == reset`, re-fetch **only that account's** `/wham/usage`, update its stored `ClaudeUsage` view (so `replace_provider_views` / the widget reflect it within a second), and log `before → after` (7d pct, `resets_at`, `available_count`). This is also where we learn empirically which window the reset cleared.

Guardrails:

- **Display + confirmation only** — it does **not** re-arm the gate to spend a second credit in the same pass. The one-consume-per-pass rule and the freshly-written cooldown mark both prevent that.
- **Best-effort** — the usage endpoint may be eventually consistent. If the re-poll still shows old numbers, that's fine; we do not act on it and we do **not** retry-loop (no endpoint hammering). The next scheduled full poll is authoritative.

### 4. Wire + config changes

- **`wire/src/control.rs`**
  - `ClaudeUsage`: add `#[serde(default, skip_serializing_if = "Option::is_none")] reset_credits: Option<i64>` (camelCase `resetCredits`). Claude accounts leave it `None`.
  - Add `CodexResetMark` struct (ts-rs exported) and `codex_reset_marks: Vec<CodexResetMark>` on `ControlState`.
- **`wire/src/config.rs`** — add `#[serde(default)] auto_reset: bool` to `CodexConfig` (default **false**). Update the config round-trip tests in that file to assert the default and JSON toggling, mirroring the existing `usage_polling` tests.
- ts-rs regenerates `frontend/app/lib/wire/ClaudeUsage.ts`, `CodexResetMark.ts`, `ControlState.ts` — never hand-edited.

### 5. Backend `codex.rs` changes

- **Parse:** extend `RawUsage` with `rate_limit_reset_credits: Option<RawResetCredits>` where `RawResetCredits { available_count: Option<i64> }`; `to_usage()` sets `ClaudeUsage.reset_credits`. `codex_base()` leaves it `None`.
- **New `consume_reset(http, token, account_id) -> Result<ConsumeOutcome>`** — POST as specified in "Backend facts," parsing the `code` enum. Mirrors `fetch_usage`'s header/timeout/error-snippet conventions.
- **Idempotency:** generate a UUID per consume, persisted on the reserved mark (`redeem_request_id`) *before* the POST (see §1 reserve-before-POST). v1 does not retry within a window — the reserved mark makes the account ineligible until its 7d window rolls — so the key exists for audit and for a future safe same-key retry, not for v1 retry logic.
- **Gate + spend + re-poll:** implemented after the views loop in `poll_inner`, calling into a pure, unit-testable predicate (see Testing).
- **Constants:** reuse `SEVEN_DAY_CAP_PCT`; add `RESET_MIN_HEADROOM_SECS`.
- **Interaction with `usage_polling=false`:** when usage polling is disabled, we have no window data, so the gate can never fire — auto-reset is implicitly inert. No special-casing needed beyond the gate's "absent `seven_day` fails" rule.

### 6. Frontend changes

- **`components/ClaudeAccountsPanel.tsx` (`Row`)** — for Codex accounts (`a.provider === "codex"`) with `a.resetCredits != null`, render a small muted badge near the 7d bar, e.g. `⟳ 3 left` (rose when `0`). Follows existing lucide-at-`size-4` / `text-[10px]` conventions; no layout restructure.
- **`components/SettingsPanel.tsx`** — a toggle **"Auto-use Codex reset credits"** bound to `codex.autoReset`, default off, with a one-line explainer: *"When every Codex account is over 95% weekly and none reset within 24h, spend one banked reset to bring an account back."* Wired the same way as the existing `codex.usagePolling` / poll-interval settings.

## Decisions made (flag if wrong)

- **Trigger is a fleet-wide gate**, not per-account: fires only when *all* managed Codex accounts are >95% weekly AND *none* reset within 24h.
- **One credit per poll pass**; converge across polls. Pick max-credits account, tie-break soonest reset.
- **Cooldown = once per account per 7d window**, persisted in `state.json` via `codex_reset_marks`.
- **No reserve** — will spend the last credit ("cooldown only" spend policy).
- **Immediate best-effort re-poll** of the consumed account after `code == reset`.
- **Thresholds (95% / 24h) are constants**, not config; only the on/off toggle is configurable.
- **Setting default OFF.**

## Non-goals

- No manual "use a reset now" button in the UI (display + auto only).
- No per-account enable/disable — a single global toggle.
- No Claude-side equivalent (Claude has no reset credits).
- No configurable thresholds or reserve count in v1.
- No historical/graph view of credit consumption (the `consumed_at` on marks is groundwork, not a v1 feature).

## Testing

- **Gate predicate (Rust unit tests)** — factor the decision into a pure function over `(&[ClaudeUsage], &[CodexResetMark], now, cfg.auto_reset)` returning `Option<chosen_account_id>`. Table tests:
  - not all accounts >95% → `None`;
  - one account resets in <24h → `None`;
  - all eligible, differing credits → picks max-credits (and tie-break by soonest reset);
  - chosen account already has a matching-window mark → skipped (next-best or `None`);
  - all `reset_credits == 0` → `None`;
  - `auto_reset == false` → `None`;
  - account with absent `seven_day` → `None`.
- **Cooldown lifecycle** — mark blocks re-spend within the same window; mark for a rolled-forward `window_resets_at` no longer blocks; pruning removes stale/orphan marks.
- **Usage parse** — a captured `wham/usage` JSON with `rate_limit_reset_credits.available_count` maps to `ClaudeUsage.reset_credits`; absence maps to `None`.
- **Config** — `codex.auto_reset` default-false + JSON round-trip (extend existing `config.rs` tests).
- **The one real risk (manual, costs 1 credit):** validate `read/display + gate` end-to-end without spending; then do a single guarded live `consume_reset` to confirm the request field name and observe which window(s) reset, and bake that observation into a code comment. Do this deliberately, not in CI.
