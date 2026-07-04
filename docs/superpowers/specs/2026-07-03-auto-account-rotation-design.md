# "auto" account selection = a live group of all imported accounts

**Date:** 2026-07-03
**Status:** Approved design, pending implementation plan
**Scope:** Claude (`claude.rs`) and Codex (`codex.rs`) — both are symmetric twins; every change lands in both.

## Problem

Today `"auto"` (the default Claude/Codex account selection) picks the single best account **once, at provision time**, pins the clone to that concrete email, and never revisits it. Continuous rebalancing only happens for clones bound to a named `group:<name>`. A separate, off-by-default global toggle (`auto_swap_on_exhaustion`) bolts on a one-shot hot-swap when a clone's account is exhausted, but it also (incorrectly) swaps clones the operator pinned to a fixed account.

We want `"auto"` to instead mean **"rotate this clone across all currently-imported accounts,"** i.e. behave exactly like a named group whose member list is the full account set — kept live as accounts are added/removed.

## Design

### 1. One unified eligibility threshold

Collapse the two existing thresholds into a single "exhausted" predicate used by both auto and named-group rotation.

An account is **exhausted** (cannot receive a clone; a clone on it must move) when:

```
5h utilization > 80%   OR   7d utilization >= 95%
```

Equivalently it is **eligible** when `5h <= 80% && 7d < 95%`.

Constant changes (both `claude.rs` and `codex.rs`):

- `SESSION_HEADROOM_PCT: 40.0 → 20.0` — so `(100 - five) < SESSION_HEADROOM_PCT` ⟺ `five > 80`.
- `SEVEN_DAY_CAP_PCT`: unchanged at `95.0`.
- `ROTATE_MAX_FIVE_HOUR_PCT` (`90.0`): **removed** — group rotation now uses the exhausted predicate instead of this separate cap.

This value already matches `score_accounts`'s `eligible` computation (`100 - five >= SESSION_HEADROOM_PCT && seven < SEVEN_DAY_CAP_PCT`), so the initial `"auto"` seed pick (`best_scored`) and the rotator stay consistent by construction.

**New shared helpers** (mirroring the existing `five_hour_pct`):

```rust
fn seven_day_pct(app: &App, email: &str) -> f64 { /* 7d pct, 0.0 if unknown */ }

/// An account whose 5h usage exceeds the session cap or whose 7d usage hit the weekly cap.
fn exhausted(app: &App, email: &str) -> bool {
    (100.0 - five_hour_pct(app, email)) < SESSION_HEADROOM_PCT
        || seven_day_pct(app, email) >= SEVEN_DAY_CAP_PCT
}
```

> ⚠️ **Behavior change for named groups.** Named groups previously rotated a clone only at 90% 5h and ignored the 7d window. They now rotate at 80% 5h **and** honor the 95% 7d cap, so they spread earlier and more often. Every move cold-starts that clone's Anthropic prompt cache. This is the intended consequence of unifying the threshold ("move group logic also to exhausted").

### 2. `"auto"` as a virtual pool in the existing rotator

`eligible_group_accounts` switches its filter from `five_hour_pct <= ROTATE_MAX_FIVE_HOUR_PCT` to `!exhausted(app, email)`.

`rotate_once` gains a second bucket alongside named groups. Refactor its per-group body into a shared helper so both buckets share the sticky placement path:

```rust
async fn rotate_pool(app: &App, label: &str, members: &[String], clones: &[Host]) {
    let eligible: Vec<String> = members.iter().filter(|e| !exhausted(app, e)).cloned().collect();
    if eligible.is_empty() {
        tracing::info!("rotate: pool '{label}' has no eligible account; leaving {} clone(s)", clones.len());
        return; // all exhausted → leave clones on their current account (unchanged group behavior)
    }
    let usage: HashMap<String, f64> = eligible.iter().map(|e| (e.clone(), five_hour_pct(app, e))).collect();
    for (host, email) in assign_rotation(clones, &eligible, &usage) {
        // ...existing push_account_to_clone + store update + STAGGER...
    }
}

pub async fn rotate_once(app: &App) {
    let cfg = app.config();
    let mut named: HashMap<String, Vec<Host>> = HashMap::new();
    let mut auto: Vec<Host> = Vec::new();
    for h in &app.store.get().hosts {
        if !h.managed { continue; }
        if let Some(g) = &h.claude_group {
            named.entry(g.clone()).or_default().push(h.clone());
        } else if h.claude_selection.as_deref() == Some(AUTO) {
            auto.push(h.clone());
        }
    }
    for (gname, clones) in named {
        let Some(group) = cfg.clone_groups.iter().find(|g| g.name == gname) else { continue };
        rotate_pool(app, &gname, &group.accounts, &clones).await;
    }
    if !auto.is_empty() {
        rotate_pool(app, "auto", &app.claude.emails(), &auto).await; // live pool = all imported accounts
    }
}
```

Properties this inherits from the existing `assign_rotation` (unchanged):

- **Sticky:** a clone keeps its current account while it stays eligible; it moves only when its account exhausts or leaves the pool (for auto: is unimported/deleted). No move purely to even out spread.
- **Load-balanced placement:** homeless clones land on the eligible account with the fewest assigned clones, then lowest 5h usage, random tiebreak.
- **All-exhausted:** clones stay put (no valid target), same as groups today.
- **Live membership:** the auto pool is recomputed from `app.claude.emails()` every pass, so adding/removing accounts is reflected without touching clones' stored state.

### 3. Provisioning unchanged

`resolve_assignment("auto")` still returns `Assignment::Account(best_scored(...))`, so a new auto clone is seeded with one concrete account immediately and `jobs.rs` records `claude_selection = "auto"`, `claude_group = None`. The rotator then takes over by keying on `claude_selection == "auto"`. No new state fields; existing `"auto"` hosts begin rotating on the next pass with no migration.

### 4. Remove the standalone exhaustion swap

Delete entirely (both `claude.rs` and `codex.rs`):

- `auto_swap_exhausted` function.
- Its call site in `poll_once` (`if cfg.claude.auto_swap_on_exhaustion { auto_swap_exhausted(app).await; }`).
- `auto_swap_on_exhaustion` field on `ClaudeConfig` and `CodexConfig` (`crates/wire/src/config.rs`), plus its two `Default` initializers and the `assert!(!c.codex.auto_swap_on_exhaustion)` test line. Config does **not** use `deny_unknown_fields`, so a leftover `autoSwapOnExhaustion` key in an existing `config.json` is ignored on load — no migration needed.
- The two Settings checkboxes and their two `autoSwapOnExhaustion: false` default-state entries in `frontend/app/components/SettingsPanel.tsx`. Removing the Rust field regenerates the TS `ClaudeConfig`/`CodexConfig` types without it, so any remaining reference is a compile error to clean up.

**Consequence (intended):** a clone pinned to a specific account (`claude_selection == <email>`, no group) is now **never** moved by the server — the rotator only touches named-group and `"auto"` clones. This resolves the current bug where pinned clones were swapped when the toggle was on.

### 5. Frontend label

In `frontend/app/components/AccountGroupSelect.tsx`, change the option text `Auto (best account)` → `Auto (all accounts)` and update the component's header comment to describe auto as "rotate across all imported accounts" rather than "server picks the best account."

## Decisions made (conservative defaults — flag if wrong)

- **Legacy hosts** with `claude_selection == None` (created before that field existed) are treated as **pinned**, not auto: the rotator keys strictly on `claude_selection == "auto"`. A clone whose intent is ambiguous is never silently moved. Making legacy `None` rotate would be a one-line relaxation later if wanted.
- **Codex parity:** all of the above is applied identically in `codex.rs` using `app.codex.emails()` and the Codex config/settings, keeping the two paths symmetric.

## Non-goals

- No change to how tokens are refreshed/pushed (`push_stale_tokens`, `push_account_to_clone`).
- No change to named-group *configuration* (create/edit/delete groups) — only their rotation threshold.
- No new UI beyond the label tweak and the removed checkboxes.

## Testing

Rust unit tests in both `claude.rs` and `codex.rs`:

- `exhausted` boundary: 80% 5h not exhausted; >80% exhausted; 95% 7d exhausted; 94.9% not.
- Auto rotation: a clone on a now-exhausted account moves to the least-loaded eligible account; a clone on a still-eligible account stays put (sticky).
- Auto pool tracks live membership: removing an account makes clones on it homeless; adding one makes it an eligible target.
- All-exhausted: auto clones stay put.
- Pinned clone (`selection == email`) and legacy `None` clone are untouched by `rotate_once`.
- Named-group rotation now triggers at the exhausted threshold (regression-updates existing group tests referencing the 90% cap).

Frontend: existing type-check / build must pass after the `ClaudeConfig`/`CodexConfig` field removal.
