# Reset-Aware Auto Account Rotation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Claude and Codex auto rotation switch to the best saturated account instead of doing nothing when every candidate is near or over the configured usage thresholds.

**Architecture:** Keep the existing sticky eligible-account path unchanged. Add a saturated fallback path that ranks all imported candidates by known 5h reset time, 5h utilization, known 7d reset time, 7d utilization, assigned-clone load, and random tie-break; keep the current account if it is within the approved reset or utilization margin.

**Tech Stack:** Rust control-server modules, existing `wire::ClaudeUsage` usage cache, current clone token push/update paths, `cargo test -p control-server`.

---

### Task 1: Claude Saturated Rotation Helper

**Files:**
- Modify: `crates/control-server/src/claude.rs`

- [ ] **Step 1: Write failing tests**

Add focused unit tests next to the existing Claude assignment tests:

```rust
fn metric(email: &str, five: f64, seven: f64, five_reset: Option<i64>, seven_reset: Option<i64>) -> RotationCandidate {
    RotationCandidate { email: email.to_string(), five_pct: five, seven_pct: seven, five_reset, seven_reset }
}

#[test]
fn saturated_assignment_prefers_soonest_5h_reset() {
    let candidates = [
        metric("soon@x", 97.0, 96.0, Some(1_000), Some(10_000)),
        metric("late@x", 90.0, 96.0, Some(2_000), Some(10_000)),
    ];
    let clones = [clone_host("c1", Some("late@x"))];

    let got = assign_saturated_rotation(&clones, &candidates);

    assert_eq!(got[0].1, "soon@x");
}

#[test]
fn saturated_assignment_uses_lower_5h_when_resets_are_missing() {
    let candidates = [
        metric("hot@x", 98.0, 96.0, None, Some(10_000)),
        metric("cool@x", 90.0, 96.0, None, Some(10_000)),
    ];
    let clones = [clone_host("c1", Some("hot@x"))];

    let got = assign_saturated_rotation(&clones, &candidates);

    assert_eq!(got[0].1, "cool@x");
}

#[test]
fn saturated_assignment_keeps_current_within_reset_margin() {
    let candidates = [
        metric("current@x", 98.0, 96.0, Some(1_800), Some(10_000)),
        metric("best@x", 99.0, 96.0, Some(1_000), Some(10_000)),
    ];
    let clones = [clone_host("c1", Some("current@x"))];

    let got = assign_saturated_rotation(&clones, &candidates);

    assert_eq!(got[0].1, "current@x");
}
```

- [ ] **Step 2: Verify red**

Run:

```bash
cargo test -p control-server saturated_assignment
```

Expected: fail because `RotationCandidate` and `assign_saturated_rotation` do not exist.

- [ ] **Step 3: Implement minimal helper**

Add a private `RotationCandidate` type, an ISO-UTC reset parser, candidate collection, ranking, sticky margin checks, and a saturated assignment helper. Wire `rotate_pool` so it uses the existing `assign_rotation` path when any candidate is eligible and the new saturated path only when all imported members are exhausted.

- [ ] **Step 4: Verify green**

Run:

```bash
cargo test -p control-server saturated_assignment
```

Expected: all saturated Claude tests pass.

### Task 2: Codex Saturated Rotation Helper

**Files:**
- Modify: `crates/control-server/src/codex.rs`

- [ ] **Step 1: Write failing tests**

Mirror the Claude saturated tests using Codex clone account fields and the same `RotationCandidate` shape.

- [ ] **Step 2: Verify red**

Run:

```bash
cargo test -p control-server codex_saturated_assignment
```

Expected: fail until the Codex helper exists.

- [ ] **Step 3: Implement Codex mirror**

Add the same private ranking and sticky fallback behavior in `codex.rs`, using Codex provider-filtered usage and `codex_account_email`.

- [ ] **Step 4: Verify green**

Run:

```bash
cargo test -p control-server codex_saturated_assignment
```

Expected: all Codex saturated tests pass.

### Task 3: Full Verification

**Files:**
- Verify: `crates/control-server/src/claude.rs`
- Verify: `crates/control-server/src/codex.rs`

- [ ] **Step 1: Run package tests**

Run:

```bash
cargo test -p control-server
```

Expected: every control-server test passes.

- [ ] **Step 2: Inspect diff**

Run:

```bash
git diff -- crates/control-server/src/claude.rs crates/control-server/src/codex.rs docs/superpowers/plans/2026-07-07-reset-aware-auto-account-rotation.md
```

Expected: diff is limited to the reset-aware rotation algorithm, tests, and this plan.
