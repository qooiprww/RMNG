//! `binswap.rs` — the automatic hash-based binary hot-swap engine.
//!
//! Clone containers run two RMNG binaries under `systemd --user`
//! (`rmng-clone-daemon` + `agent-wrapper`, installed under `/opt/rmng/bin`). This engine
//! keeps them current WITHOUT a manual per-host "redeploy": it hashes the payloads the
//! control-server ships (once, at startup) and compares them against the on-disk binaries
//! in each running clone; a mismatch triggers [`crate::provision::redeploy_clone`], which
//! bounces only the user units — never the container, never the desktop session.
//!
//! ## Shape
//! - [`SwapState`] hangs off [`App`]. It holds an unbounded channel of clone-ids to check,
//!   the expected sha256 hashes (warmed eagerly at [`spawn`]), and a per-host guard map
//!   (`failures` / `next_swap_allowed` / `pending`).
//! - A single **worker** task drains the channel and runs [`check_host`] — the ONE guarded
//!   path. Serial by construction: only the worker ever touches the guard map, so the map's
//!   `Mutex` never contends (it exists for `Sync` + interior mutability, and is never held
//!   across an `.await`).
//! - A **sweep** loop enqueues every managed container 60 s after boot, then every 5 min —
//!   this catches a clone whose *stale* daemon is so broken it can't even connect.
//! - [`SwapState::request_check`] is a sync enqueue used later by the mediaplane on a
//!   daemon `Hello` (a fresh registration is exactly when a just-swapped — or freshly
//!   started stale — daemon should be re-checked). It is also the sweep's enqueue path.
//!
//! ## Enqueue → check → swap → verify
//! The check is cheap and always allowed; only the *swap* is rate-limited. After a swap the
//! daemon restarts and re-`Hello`s, which enqueues another check — that re-check is the
//! success verification (hashes now match ⇒ the guard resets). See [`check_host`] for the
//! full state machine.
//!
//! ## Surfacing
//! Failures are log-only (debug for no-ops; WARN with operator guidance for real trouble).
//! Deliberately NO `state_note`/`unread` writes: those belong to `monitor.rs` + the agent,
//! and a swap-engine scribble there would fight them.
//!
//! ## Known deferral
//! `agent-wrapper` is swapped immediately, even mid-session (decided). Deferring it while
//! the clone's `monitor_state == Working` would be a small change here: gate the
//! `agent-wrapper` entry in [`check_host`]'s swap set on the host's monitor state.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::app::App;
use crate::provision::{REDEPLOY_UNITS, RedeployUnit};

/// Where clones install the swappable binaries (set up by `template/setup/30-user.sh` at
/// template build); the units exec from
/// here and [`crate::provision::redeploy_clone`] uploads here.
const BIN_DIR: &str = "/opt/rmng/bin";

/// First sweep pass this long after boot (let clones finish their first-boot registration
/// before we start hashing them).
const FIRST_SWEEP_DELAY: Duration = Duration::from_secs(60);
/// Steady-state sweep cadence.
const SWEEP_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Swap backoff base (the 1st failed attempt gates the next swap for this long).
const BACKOFF_BASE_SECS: u64 = 30;
/// Swap backoff ceiling.
const BACKOFF_MAX_SECS: u64 = 30 * 60;

/// One expected-up-to-date unit: its swap-plan row + the sha256 hex of the payload the
/// control-server ships. Warmed once at [`spawn`]; a unit whose payload is absent in a dev
/// checkout is simply omitted (its check is then skipped — nothing to compare against).
struct ExpectedUnit {
    unit: &'static RedeployUnit,
    /// Lowercase sha256 hex of the shipped payload bytes.
    sha256: String,
}

/// Per-host swap guard. Only the worker mutates it, so no lost-update races; the `Mutex`
/// on the containing map is for `Sync`, never held across an `.await`.
#[derive(Default)]
struct HostGuard {
    /// Consecutive failed/unverified swap attempts — drives the backoff. Reset to 0 the
    /// moment a check finds the clone up to date.
    failures: u32,
    /// Earliest instant the next *swap* is allowed (checks are never gated). `None` = now.
    next_swap_allowed: Option<Instant>,
    /// Bin names of units a prior swap TOUCHED but couldn't verify (upload-ok/start-failed:
    /// their on-disk hash now matches yet the unit is down). Forces those units back into
    /// the stale set on the next check so the swap is retried until it takes.
    pending: HashSet<&'static str>,
}

/// The engine's shared state (a field on [`App`]). All fields start empty; [`spawn`] warms
/// `tx` + `expected`.
#[derive(Default)]
pub struct SwapState {
    /// Enqueue channel to the worker. `OnceLock` so [`request_check`](Self::request_check)
    /// is a sync, lock-free send from any thread (e.g. the mediaplane on `Hello`).
    tx: OnceLock<mpsc::UnboundedSender<String>>,
    /// Expected sha256 per unit, warmed at worker start. Empty in a dev checkout with no
    /// staged binaries (the engine is then idle). Hashes are cached, not the bytes
    /// (`agent-wrapper` is ~90 MB); bytes are re-read from disk only when a swap actually
    /// fires.
    expected: OnceLock<Vec<ExpectedUnit>>,
    /// Per-host swap guards.
    hosts: Mutex<HashMap<String, HostGuard>>,
}

impl SwapState {
    /// Enqueue a hash check for a clone. Sync + non-blocking, callable from any thread
    /// (the mediaplane's `Hello` handler + the sweep loop). A no-op — with a debug line —
    /// before the engine has started or after the worker has stopped.
    pub fn request_check(&self, clone_id: &str) {
        match self.tx.get() {
            Some(tx) => {
                if tx.send(clone_id.to_string()).is_err() {
                    tracing::debug!(target: "binswap", "request_check({clone_id}): worker gone; dropped");
                }
            }
            None => tracing::debug!(target: "binswap", "request_check({clone_id}): engine not started; dropped"),
        }
    }
}

/// Lowercase sha256 hex of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// Backoff (seconds) before the next swap after `failures` consecutive unverified attempts:
/// `min(30·2^failures, 1800)`. Saturating, so a runaway `failures` can't overflow the shift
/// — it just pins at the 30-min ceiling.
fn backoff_secs(failures: u32) -> u64 {
    let factor = 1u64.checked_shl(failures).unwrap_or(u64::MAX);
    BACKOFF_BASE_SECS.saturating_mul(factor).min(BACKOFF_MAX_SECS)
}

/// Parse `sha256sum <paths…>` output into a `path → lowercase-hex-digest` map. Tolerates
/// interleaved error lines from the merged stdout+stderr the exec returns (e.g.
/// `sha256sum: /opt/rmng/bin/agent-wrapper: No such file or directory`) and both coreutils
/// separators — `"<hash>  <path>"` (text mode, two spaces) and `"<hash> *<path>"` (binary
/// mode). A line whose first 64 bytes aren't hex, or that lacks the two-char separator, is
/// ignored. Pure — unit-tested.
fn parse_sha256sum(out: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in out.lines() {
        // Need 64 hex + 2 separator bytes + at least one path byte. Work in bytes and
        // validate the fixed ASCII prefix BEFORE slicing the &str, so a merged-stderr log
        // line with a multibyte char at offset 64 can't panic a str slice (the worker must
        // not panic on hostile exec output).
        let bytes = line.as_bytes();
        if bytes.len() < 67 || !bytes[..64].iter().all(u8::is_ascii_hexdigit) {
            continue;
        }
        // coreutils: a space, then a space (text mode) or '*' (binary mode), then the path.
        if bytes[64] != b' ' || (bytes[65] != b' ' && bytes[65] != b'*') {
            continue;
        }
        // Bytes 0..66 are all ASCII (verified above), so 64 and 66 are char boundaries.
        map.insert(line[66..].to_string(), line[..64].to_ascii_lowercase());
    }
    map
}

/// Hash every staged clone payload once. A payload absent in a dev checkout (`None`) is
/// omitted — that unit is then never flagged stale (nothing to compare).
fn warm_expected() -> Vec<ExpectedUnit> {
    REDEPLOY_UNITS
        .iter()
        .filter_map(|u| {
            let bytes = crate::assets::payload(u.payload)?;
            Some(ExpectedUnit { unit: u, sha256: sha256_hex(&bytes) })
        })
        .collect()
}

/// Start the engine: warm the expected hashes, publish the enqueue channel, and spawn the
/// worker + sweep tasks. Call once from `main`, beside the other background loops and
/// before `mediaplane::spawn` (so the channel is live before the first `Hello` could fire
/// a `request_check`).
pub fn spawn(app: App) {
    let (tx, rx) = mpsc::unbounded_channel::<String>();
    if app.swap.tx.set(tx).is_err() {
        tracing::error!("binswap: already started; refusing to spawn a second worker");
        return;
    }

    let expected = warm_expected();
    if expected.is_empty() {
        tracing::warn!(
            "binswap: no clone binaries staged (dev checkout without embedded-bin?) — the \
             hot-swap engine is idle"
        );
    } else {
        let summary = expected
            .iter()
            .map(|e| format!("{}={}…", e.unit.bin, &e.sha256[..12]))
            .collect::<Vec<_>>()
            .join(", ");
        tracing::info!("binswap: expecting clones at {summary}");
    }
    // Only ever set here (guarded by the `tx` set above), so the result is uninteresting.
    let _ = app.swap.expected.set(expected);

    tokio::spawn(worker(app.clone(), rx));
    tokio::spawn(sweep(app));
}

/// Drain the enqueue channel, checking one host at a time. Serial by design (one worker):
/// no two `check_host` runs overlap, so the guard map is uncontended. Panic containment
/// matches the codebase's other spawned loops (`monitor::run`, `homes::run`): a panic ends
/// the task; `check_host` is written not to panic (no unwrap that can fail, no slice past a
/// fixed-length digest).
async fn worker(app: App, mut rx: mpsc::UnboundedReceiver<String>) {
    tracing::info!("binswap: worker started");
    while let Some(id) = rx.recv().await {
        check_host(&app, &id).await;
    }
    tracing::warn!("binswap: enqueue channel closed; hot-swap engine stopped");
}

/// Sweep loop: first pass [`FIRST_SWEEP_DELAY`] after boot, then every [`SWEEP_INTERVAL`].
/// Enqueues every managed container; [`check_host`] is the sole gate on what's actually
/// eligible (a build worker / stopped clone is enqueued but rejected there).
async fn sweep(app: App) {
    tokio::time::sleep(FIRST_SWEEP_DELAY).await;
    loop {
        sweep_once(&app).await;
        tokio::time::sleep(SWEEP_INTERVAL).await;
    }
}

async fn sweep_once(app: &App) {
    match app.docker.list_managed_containers().await {
        Ok(list) => {
            for c in &list {
                app.swap.request_check(&c.name);
            }
            tracing::debug!(target: "binswap", "sweep enqueued {} managed container(s)", list.len());
        }
        Err(e) => tracing::warn!(
            "binswap: sweep could not list managed containers ({e:#}); skipping this pass — \
             retrying in {}s",
            SWEEP_INTERVAL.as_secs()
        ),
    }
}

/// The one guarded path: hash a clone's on-disk binaries and swap the stale ones.
///
/// State machine (all logging; no state-store writes):
/// 1. **Eligibility** — not a managed clone (state store), not running, or no expected
///    hashes warmed ⇒ debug + return.
/// 2. **Hash** — `sha256sum /opt/rmng/bin/<bins>` (one exec; a missing file only makes the
///    exec non-zero, still parsed). `stale = (missing ∪ mismatched) ∪ guard.pending`.
/// 3. **Clean** — `stale` empty ⇒ reset the guard (this is the post-swap re-Hello success
///    verification) + return.
/// 4. **Backoff** — stale but `now < next_swap_allowed` ⇒ debug + return (checks aren't
///    gated, swaps are).
/// 5. **Drift guard** — re-read each stale unit's payload; **refuse to upload bytes whose
///    hash differs from the one cached at startup** (the dev payload was replaced under a
///    running server ⇒ WARN "restart the control-server", skip that unit — this makes swap
///    loops impossible). If nothing survives, WARN + return without touching the guard.
/// 6. **Swap** — [`crate::provision::redeploy_clone`] the survivors; bump `failures` + set
///    `next_swap_allowed = now + backoff(failures)`. On `Ok` clear `pending` (the re-Hello
///    check verifies + resets); on `Err` set `pending = the attempted units` (covers
///    upload-ok/start-failed).
async fn check_host(app: &App, id: &str) {
    // 1a. A clone we manage? (state store = ownership authority; rejects build workers.)
    if !app.store.get().hosts.iter().any(|h| h.id == id && h.managed) {
        tracing::debug!(target: "binswap", "{id}: not a managed clone; skip");
        return;
    }
    // 1b. Running? (rejects stopped clones swept in.)
    match app.docker.is_running(id).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::debug!(target: "binswap", "{id}: container not running; skip");
            return;
        }
        Err(e) => {
            tracing::debug!(target: "binswap", "{id}: is_running failed ({e:#}); skip");
            return;
        }
    }
    // 1c. Expected hashes warmed + non-empty?
    let Some(expected) = app.swap.expected.get() else {
        tracing::debug!(target: "binswap", "{id}: expected hashes not warmed yet; skip");
        return;
    };
    if expected.is_empty() {
        tracing::debug!(target: "binswap", "{id}: no staged clone binaries; nothing to check");
        return;
    }

    // 2. Hash the on-disk binaries (one exec). sha256sum echoes each path verbatim, so its
    // output keys match the args we pass; a missing file only sets a non-zero exit (ignored)
    // and prints an error line parse_sha256sum drops.
    let paths: Vec<String> = expected.iter().map(|e| format!("{BIN_DIR}/{}", e.unit.bin)).collect();
    let mut cmd: Vec<&str> = Vec::with_capacity(1 + paths.len());
    cmd.push("sha256sum");
    cmd.extend(paths.iter().map(String::as_str));
    let out = match app.docker.exec_capture(id, &cmd).await {
        Ok((_code, out)) => out,
        Err(e) => {
            tracing::debug!(target: "binswap", "{id}: sha256sum exec failed ({e:#}); skip");
            return;
        }
    };
    let on_disk = parse_sha256sum(&out);

    // Snapshot `pending` under the lock, then drop it (never held across the awaits above
    // or the swap below).
    let pending: HashSet<&'static str> =
        app.swap.hosts.lock().unwrap().get(id).map(|g| g.pending.clone()).unwrap_or_default();

    // stale = (missing ∪ mismatched) ∪ pending.
    let stale: Vec<&'static RedeployUnit> = expected
        .iter()
        .filter(|e| {
            let hash_ok = on_disk.get(&format!("{BIN_DIR}/{}", e.unit.bin)) == Some(&e.sha256);
            !hash_ok || pending.contains(e.unit.bin)
        })
        .map(|e| e.unit)
        .collect();

    // 3. Clean ⇒ reset the guard (post-swap success verification).
    if stale.is_empty() {
        if app.swap.hosts.lock().unwrap().remove(id).is_some() {
            tracing::info!(target: "binswap", "{id}: clone binaries up to date; swap guard cleared");
        } else {
            tracing::debug!(target: "binswap", "{id}: clone binaries up to date");
        }
        return;
    }

    // 4. Backoff gates SWAPS, not checks.
    let now = Instant::now();
    let (failures, gated) = {
        let hosts = app.swap.hosts.lock().unwrap();
        match hosts.get(id) {
            Some(g) => (g.failures, g.next_swap_allowed.is_some_and(|t| now < t)),
            None => (0, false),
        }
    };
    if gated {
        tracing::debug!(
            target: "binswap",
            "{id}: {} unit(s) stale but in swap backoff; will retry on the next sweep/Hello",
            stale.len()
        );
        return;
    }

    // 5. Drift guard: only upload bytes whose hash still matches the startup-cached expected.
    let mut to_swap: Vec<(&'static RedeployUnit, Vec<u8>)> = Vec::new();
    for e in expected.iter().filter(|e| stale.iter().any(|u| u.bin == e.unit.bin)) {
        let Some(bytes) = crate::assets::payload(e.unit.payload) else {
            tracing::warn!(
                target: "binswap",
                "{id}: {} is stale but its payload vanished from disk — stage it and RESTART \
                 the control-server; skipping",
                e.unit.payload
            );
            continue;
        };
        let on_disk_hash = sha256_hex(&bytes);
        if on_disk_hash != e.sha256 {
            tracing::warn!(
                target: "binswap",
                "{id}: {} payload on disk (sha {}…) no longer matches the hash cached at \
                 startup (sha {}…) — the control-server's payload was replaced under it. \
                 RESTART the control-server to adopt the new binary; skipping to avoid a swap loop",
                e.unit.payload,
                &on_disk_hash[..12],
                &e.sha256[..12]
            );
            continue;
        }
        to_swap.push((e.unit, bytes));
    }
    if to_swap.is_empty() {
        tracing::warn!(
            target: "binswap",
            "{id}: {} unit(s) stale but none are swappable (payload drift/absence) — not swapping",
            stale.len()
        );
        return;
    }

    // 6. Swap.
    tracing::info!(
        target: "binswap",
        "{id}: swapping {} stale unit(s): {}",
        to_swap.len(),
        to_swap.iter().map(|(u, _)| u.bin).collect::<Vec<_>>().join(", ")
    );
    let swap_result = crate::provision::redeploy_clone(app, id, &to_swap, |step, msg| {
        tracing::debug!(target: "binswap", "{id} swap {step}: {msg}");
    })
    .await;

    // Record the attempt: bump backoff (from a fresh `now`, so a slow upload doesn't eat the
    // window); on Err remember the attempted units as pending so they're retried.
    let boff = backoff_secs(failures);
    let mut hosts = app.swap.hosts.lock().unwrap();
    let guard = hosts.entry(id.to_string()).or_default();
    guard.failures = failures.saturating_add(1);
    guard.next_swap_allowed = Some(Instant::now() + Duration::from_secs(boff));
    match &swap_result {
        Ok(()) => {
            guard.pending.clear();
            tracing::info!(
                target: "binswap",
                "{id}: swap issued OK; awaiting the daemon's re-Hello to verify (next swap gated {boff}s)"
            );
        }
        Err(e) => {
            guard.pending = to_swap.iter().map(|(u, _)| u.bin).collect();
            tracing::warn!(
                target: "binswap",
                "{id}: swap FAILED ({e:#}); retrying after {boff}s. If it persists, inspect the \
                 clone's `systemctl --user` units (rmng-clone-daemon / agent-wrapper)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn parse_sha256sum_normal() {
        let out = format!(
            "{HASH_A}  /opt/rmng/bin/rmng-clone-daemon\n{HASH_B}  /opt/rmng/bin/agent-wrapper\n"
        );
        let m = parse_sha256sum(&out);
        assert_eq!(m.get("/opt/rmng/bin/rmng-clone-daemon"), Some(&HASH_A.to_string()));
        assert_eq!(m.get("/opt/rmng/bin/agent-wrapper"), Some(&HASH_B.to_string()));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn parse_sha256sum_binary_mode_and_path_with_spaces() {
        // Binary-mode separator (`*`) and a path containing spaces.
        let out = format!("{HASH_A} */opt/rmng/bin/some bin\n");
        let m = parse_sha256sum(&out);
        assert_eq!(m.get("/opt/rmng/bin/some bin"), Some(&HASH_A.to_string()));
    }

    #[test]
    fn parse_sha256sum_tolerates_interleaved_stderr_noise() {
        // Merged stdout+stderr: a real hash line, a coreutils error line, and junk.
        let out = format!(
            "sha256sum: /opt/rmng/bin/agent-wrapper: No such file or directory\n\
             {HASH_A}  /opt/rmng/bin/rmng-clone-daemon\n\
             some unrelated log line that is not a hash\n"
        );
        let m = parse_sha256sum(&out);
        // Only the valid line parsed; noise dropped.
        assert_eq!(m.len(), 1);
        assert_eq!(m.get("/opt/rmng/bin/rmng-clone-daemon"), Some(&HASH_A.to_string()));
    }

    #[test]
    fn parse_sha256sum_missing_file_line_is_dropped() {
        // A missing file produces ONLY an error line (no hash) — that path is absent from
        // the map, so check_host treats the unit as stale (missing).
        let out = format!(
            "{HASH_A}  /opt/rmng/bin/rmng-clone-daemon\n\
             sha256sum: /opt/rmng/bin/agent-wrapper: No such file or directory\n"
        );
        let m = parse_sha256sum(&out);
        assert!(m.contains_key("/opt/rmng/bin/rmng-clone-daemon"));
        assert!(!m.contains_key("/opt/rmng/bin/agent-wrapper"));
    }

    #[test]
    fn parse_sha256sum_uppercase_is_normalized() {
        let upper = HASH_A.to_ascii_uppercase();
        let out = format!("{upper}  /opt/rmng/bin/x\n");
        let m = parse_sha256sum(&out);
        // Stored lowercase so it compares equal to a lowercase expected hash.
        assert_eq!(m.get("/opt/rmng/bin/x"), Some(&HASH_A.to_string()));
    }

    #[test]
    fn parse_sha256sum_does_not_panic_on_multibyte_noise() {
        // A non-hash log line whose byte 64 lands inside a multibyte char would panic a
        // naive `split_at(64)` — assert we tolerate it and still parse the real line.
        let out = format!(
            "sha256sum warning: skipping a café directory with a long unicode ☕ tail here\n\
             {HASH_A}  /opt/rmng/bin/rmng-clone-daemon\n"
        );
        let m = parse_sha256sum(&out);
        assert_eq!(m.len(), 1);
        assert_eq!(m.get("/opt/rmng/bin/rmng-clone-daemon"), Some(&HASH_A.to_string()));
    }

    #[test]
    fn parse_sha256sum_rejects_short_and_non_hex_lines() {
        // A too-short hash, a non-hex 64-char prefix, and a hash with no separator/path.
        let non_hex = "g".repeat(64);
        let out = format!(
            "abc  /too/short\n{non_hex}  /not/hex\n{HASH_A}\n{HASH_A} x/bad-separator\n"
        );
        let m = parse_sha256sum(&out);
        assert!(m.is_empty(), "expected no valid lines, got {m:?}");
    }

    #[test]
    fn backoff_progression_and_cap() {
        assert_eq!(backoff_secs(0), 30);
        assert_eq!(backoff_secs(1), 60);
        assert_eq!(backoff_secs(2), 120);
        assert_eq!(backoff_secs(3), 240);
        assert_eq!(backoff_secs(5), 960);
        // 30·2^6 = 1920 > 1800 ⇒ capped.
        assert_eq!(backoff_secs(6), BACKOFF_MAX_SECS);
        assert_eq!(backoff_secs(7), BACKOFF_MAX_SECS);
        // A runaway count saturates the shift instead of overflowing, still capped.
        assert_eq!(backoff_secs(64), BACKOFF_MAX_SECS);
        assert_eq!(backoff_secs(u32::MAX), BACKOFF_MAX_SECS);
    }

    #[test]
    fn sha256_hex_is_lowercase_64() {
        let h = sha256_hex(b"hello");
        assert_eq!(h.len(), 64);
        assert!(h.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        // Known vector for "hello".
        assert_eq!(h, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
    }
}
