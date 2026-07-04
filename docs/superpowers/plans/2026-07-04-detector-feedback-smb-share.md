# Detector-feedback SMB share — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Formalize the currently hand-rolled detector-feedback SMB share by adding a code-defined `[feedback]` share to the control-server's in-container `smbd`, alongside the existing `[clones]` share.

**Architecture:** The control-server already runs its own `smbd` on port 445 (`crates/control-server/src/smb.rs`), rendering `smb.conf` from code and supervising the daemon. This plan adds a second share section, `[feedback]`, rooted at `<data_dir>/detector-feedback` (the folder `save_detector_feedback` writes). It is read-write and served as root (the directory + its records are control-server-owned). Unlike `[clones]`, it serves a plain local directory, so it needs none of the `wide links` / `/proc` traversal machinery. The folder name is single-sourced through a new constant in `files.rs` so the writer and the share path can never diverge.

**Tech Stack:** Rust (tokio, tracing), Samba `smbd`, existing `control-server` crate.

## Global Constraints

- **Scope is `detector-feedback` only** — never share the whole `<data_dir>`. It also holds `state.json` and the `claude-accounts.json` secret store; a broader share would leak credentials.
- **No new config surface** — always-on, no `config.json` toggle, no wizard/Settings UI, no CLI flag. Matches the `[clones]` share posture.
- **No new published port** — same `smbd`, same port 445, same `-p 445:445`.
- **Auth is the existing fixed credential** — SMB user `rmng`, password `rmng`, `valid users = rmng` on both shares.
- **Created files are root-owned** on the feedback share (matches the API writer); no `inherit owner`.
- **Test command:** `cargo test -p control-server` (run a single test by appending its name, e.g. `cargo test -p control-server detector_feedback_root`).
- **Reference spec:** `docs/superpowers/specs/2026-07-04-detector-feedback-smb-share-design.md`.

---

## File Structure

- `crates/control-server/src/files.rs` — **modify.** Add the `DETECTOR_FEEDBACK_DIR` constant and a pure `detector_feedback_root(data_dir) -> PathBuf` helper (mirrors `homes::hosts_root`); use them in `save_detector_feedback`. Add a unit test.
- `crates/control-server/src/smb.rs` — **modify.** Extend `render_smb_conf` to emit the `[feedback]` section; add `absolute_feedback_root`; wire `run()` to resolve + create the feedback dir and pass both roots to the renderer; update the module doc. Extend the unit tests.
- `crates/control-server/src/main.rs` — **modify.** One-line header-comment update (both shares).
- `crates/control-server/README.md` — **modify.** Update the three SMB mentions to describe both shares.
- `docs/DEPLOY.md` — **modify.** Document the `feedback` share next to `clones`.

---

## Task 1: Single-source the feedback dir name (`files.rs`)

**Files:**
- Modify: `crates/control-server/src/files.rs` (add const + helper near line 129; change `save_detector_feedback` at line 154; add test in the `mod tests` block at line 183)

**Interfaces:**
- Consumes: nothing new (`PathBuf`/`Path` already imported at `files.rs:7`).
- Produces:
  - `pub const DETECTOR_FEEDBACK_DIR: &str` — the folder name `"detector-feedback"`.
  - `pub fn detector_feedback_root(data_dir: &str) -> PathBuf` — lexical `<data_dir>/detector-feedback`. Consumed by `smb.rs` in Task 3.

- [ ] **Step 1: Write the failing test**

Add this test function inside the existing `#[cfg(test)] mod tests { ... }` block (`crates/control-server/src/files.rs:184`, which already has `use super::*;`):

```rust
    #[test]
    fn detector_feedback_root_joins_the_shared_dir_name() {
        // The on-disk folder name is wire-visible — docs and the `feedback` SMB share path
        // depend on it — so pin it: a rename must be deliberate, not accidental.
        assert_eq!(DETECTOR_FEEDBACK_DIR, "detector-feedback");
        assert_eq!(
            detector_feedback_root("data"),
            std::path::Path::new("data/detector-feedback")
        );
        assert_eq!(
            detector_feedback_root("/srv/rmng/data"),
            std::path::Path::new("/srv/rmng/data/detector-feedback")
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p control-server detector_feedback_root`
Expected: FAIL — a compile error, `cannot find value DETECTOR_FEEDBACK_DIR` / `cannot find function detector_feedback_root in this scope`.

- [ ] **Step 3: Add the constant and the helper**

Insert immediately below the `// --- detector feedback ---` comment (`crates/control-server/src/files.rs:129`), above `pub struct DetectorFeedback`:

```rust
/// The detector-feedback records directory name (`<data_dir>/detector-feedback`): where
/// `save_detector_feedback` writes and the `feedback` SMB share (`smb.rs`) reads. Single-sourced
/// here so the writer and the share path can never diverge — mirrors `homes::hosts_root`.
pub const DETECTOR_FEEDBACK_DIR: &str = "detector-feedback";

/// Lexical root of the detector-feedback records under `data_dir`. Pure (no symlink resolution),
/// so it's unit-testable; `smb.rs` wraps it with `std::path::absolute` for the share `path`.
pub fn detector_feedback_root(data_dir: &str) -> PathBuf {
    Path::new(data_dir).join(DETECTOR_FEEDBACK_DIR)
}
```

- [ ] **Step 4: Use the helper in `save_detector_feedback`**

In `crates/control-server/src/files.rs:154`, replace the inline join:

```rust
    let dir = Path::new(data_dir).join("detector-feedback");
```

with:

```rust
    let dir = detector_feedback_root(data_dir);
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p control-server detector_feedback_root`
Expected: PASS (1 test). Also run the whole crate to confirm no regression: `cargo test -p control-server` → all pass.

- [ ] **Step 6: Commit**

```bash
git add crates/control-server/src/files.rs
git commit -m "refactor(files): single-source the detector-feedback dir name"
```

---

## Task 2: Render the `[feedback]` share section (`smb.rs`)

**Files:**
- Modify: `crates/control-server/src/smb.rs` (`render_smb_conf` at lines 47-73; the two existing tests at lines 235-264; add one new test)

**Interfaces:**
- Consumes: nothing from Task 1 yet (this task is pure string rendering).
- Produces: `pub fn render_smb_conf(hosts_root: &Path, feedback_root: &Path) -> String` — **signature changes** from one arg to two. Consumed by `run()` in Task 3.

- [ ] **Step 1: Write the failing tests**

Update the two existing tests and add a third, in `crates/control-server/src/smb.rs`.

Replace the existing `render_smb_conf_has_load_bearing_lines` test (lines 235-255) with the two-arg call plus feedback needles:

```rust
    #[test]
    fn render_smb_conf_has_load_bearing_lines() {
        let out = render_smb_conf(
            Path::new("/data/data/hosts"),
            Path::new("/data/data/detector-feedback"),
        );
        for needle in [
            "[global]",
            "server min protocol = SMB2",
            "unix extensions = no",
            "smb ports = 445",
            "[clones]",
            "path = /data/data/hosts",
            "wide links = yes",
            "follow symlinks = yes",
            "inherit owner = unix only",
            "[feedback]",
            "path = /data/data/detector-feedback",
            "read only = no",
            "force user = root",
            "force group = rmng",
            "valid users = rmng",
        ] {
            assert!(out.contains(needle), "smb.conf missing `{needle}`:\n{out}");
        }
    }
```

Replace the existing `render_smb_conf_interpolates_the_share_path` test (lines 257-264) with the two-arg call:

```rust
    #[test]
    fn render_smb_conf_interpolates_the_share_path() {
        // The clones share root must be exactly where the reconciler links, else the share is
        // silently empty — so `path` tracks the argument, not a hardcoded default.
        let out = render_smb_conf(
            Path::new("/srv/rmng/data/hosts"),
            Path::new("/srv/rmng/data/detector-feedback"),
        );
        assert!(out.contains("path = /srv/rmng/data/hosts"), "{out}");
        assert!(!out.contains("/data/data/hosts"), "{out}");
    }
```

Add this new test (locks the design decision that feedback is a plain dir, not a `/proc` share):

```rust
    #[test]
    fn render_smb_conf_feedback_is_a_plain_share() {
        let out = render_smb_conf(
            Path::new("/data/data/hosts"),
            Path::new("/srv/rmng/data/detector-feedback"),
        );
        assert!(out.contains("[feedback]"), "{out}");
        assert!(out.contains("path = /srv/rmng/data/detector-feedback"), "{out}");
        // The feedback section must NOT carry the /proc-traversal options — those are clones-only.
        let feedback = &out[out.find("[feedback]").expect("feedback section")..];
        assert!(!feedback.contains("wide links"), "feedback must not enable wide links:\n{out}");
        assert!(!feedback.contains("follow symlinks"), "{out}");
        assert!(!feedback.contains("inherit owner"), "{out}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p control-server render_smb_conf`
Expected: FAIL — compile error, `this function takes 1 argument but 2 arguments were supplied` (the tests call the new two-arg signature; the function still takes one).

- [ ] **Step 3: Update `render_smb_conf` to take the feedback root and emit the section**

Replace the whole function body (`crates/control-server/src/smb.rs:47-73`). Update the doc line above it too (it currently says "Only `path` varies"):

```rust
/// Render `smb.conf` for the `clones` share (rooted at `hosts_root`) and the `feedback` share
/// (rooted at `feedback_root`). Pure (no I/O) so it's unit-testable. Only the two `path` lines
/// vary; every other line is literal per the design. The `feedback` share is a plain local
/// directory (the detector-feedback records) — no `wide links`/`follow symlinks`/`inherit owner`,
/// which exist on `[clones]` only for `/proc/<pid>/root` traversal.
pub fn render_smb_conf(hosts_root: &Path, feedback_root: &Path) -> String {
    format!(
        "[global]
   server min protocol = SMB2
   unix extensions = no
   allow insecure wide links = yes
   security = user
   smb ports = 445
   load printers = no
   printing = bsd
   disable spoolss = yes
   vfs objects = catia fruit streams_xattr
   log level = 1

[clones]
   path = {}
   read only = no
   wide links = yes
   follow symlinks = yes
   force user = root
   force group = rmng
   valid users = rmng
   inherit owner = unix only

[feedback]
   path = {}
   read only = no
   force user = root
   force group = rmng
   valid users = rmng
",
        hosts_root.display(),
        feedback_root.display()
    )
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p control-server render_smb_conf`
Expected: PASS (3 tests: `render_smb_conf_has_load_bearing_lines`, `render_smb_conf_interpolates_the_share_path`, `render_smb_conf_feedback_is_a_plain_share`).

- [ ] **Step 5: Commit**

```bash
git add crates/control-server/src/smb.rs
git commit -m "feat(smb): render the [feedback] share section"
```

---

## Task 3: Wire `run()` to serve the feedback dir (`smb.rs`)

**Files:**
- Modify: `crates/control-server/src/smb.rs` (module doc lines 1-22; add `absolute_feedback_root` near `absolute_hosts_root` at lines 78-81; `run()` at lines 217-229)

**Interfaces:**
- Consumes: `crate::files::detector_feedback_root` (Task 1); `render_smb_conf(hosts_root, feedback_root)` (Task 2).
- Produces: no new public API. `run()` now renders `smb.conf` with both shares and ensures the feedback dir exists.

> No new unit test here: the tested seam is `render_smb_conf` (Task 2). `run()` performs I/O and, like the existing code, is proven by the manual integration gate (Task 5), plus `cargo build`/`cargo test`/`cargo clippy` staying green. `absolute_feedback_root` wraps the already-tested pure `detector_feedback_root` with `std::path::absolute` (CWD-dependent, so not unit-tested — same reason `absolute_hosts_root` isn't).

- [ ] **Step 1: Add `absolute_feedback_root`**

Insert immediately after `absolute_hosts_root` (`crates/control-server/src/smb.rs:81`):

```rust
/// The feedback share root as an absolute path, sourced from `files::detector_feedback_root` so
/// the share and the writer never diverge. Lexical `std::path::absolute` only (no symlink
/// resolution), mirroring [`absolute_hosts_root`].
fn absolute_feedback_root(data_dir: &str) -> PathBuf {
    let root = crate::files::detector_feedback_root(data_dir);
    std::path::absolute(&root).unwrap_or(root)
}
```

- [ ] **Step 2: Wire `run()` to resolve, create, and render both roots**

Replace the body of `run()` (`crates/control-server/src/smb.rs:217-229`) with:

```rust
pub async fn run(app: App) {
    let cfg = app.config();
    let root = absolute_hosts_root(&cfg.data_dir);
    let feedback_root = absolute_feedback_root(&cfg.data_dir);
    let _ = std::fs::create_dir_all(&root); // harmless if homes already made it
    let _ = std::fs::create_dir_all(&feedback_root); // may not exist until the first report

    match std::fs::write(SMB_CONF, render_smb_conf(&root, &feedback_root)) {
        Ok(()) => tracing::info!(
            target: "smb",
            "wrote {SMB_CONF} (clones {}, feedback {})",
            root.display(),
            feedback_root.display()
        ),
        Err(e) => tracing::error!(target: "smb", "writing {SMB_CONF}: {e}"),
    }

    provision_account().await;
    supervise().await; // never returns
}
```

- [ ] **Step 3: Update the module doc to describe both shares**

At the top of `crates/control-server/src/smb.rs`, replace the first paragraph (lines 1-5, ending "...every clone's home side by side.") — change the opening sentence to name two shares, keeping the rest of the `clones` explanation:

Replace:

```rust
//! `smbd` supervisor + the `clones` SMB share. The control-server runs its own smbd
//! (port 445) exporting one read-write share whose root is `data/hosts` — the symlink
```

with:

```rust
//! `smbd` supervisor + two SMB shares (`clones`, `feedback`). The control-server runs its own
//! smbd (port 445) exporting two read-write shares. `clones`'s root is `data/hosts` — the symlink
```

Then, immediately after the `clones` paragraph (after the `accepted risk for this trusted, credential-gated share.)` line, currently line 16), insert a new paragraph:

```rust
//!
//! The `feedback` share is far simpler: `data/detector-feedback` is a plain control-server-owned
//! directory (the records `save_detector_feedback` writes), not a `/proc` symlink, so it needs
//! none of the `wide links` traversal machinery. `force user = root` there is only so the
//! authenticated `rmng` client can read/write the root-owned records; files it creates land
//! root-owned, matching the API writer. It works regardless of `pid: "host"`.
```

- [ ] **Step 4: Verify the crate builds, lints, and all tests pass**

Run: `cargo test -p control-server`
Expected: PASS — all tests, including Task 1's `detector_feedback_root...` and Task 2's three `render_smb_conf...` tests.

Run: `cargo clippy -p control-server --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/control-server/src/smb.rs
git commit -m "feat(smb): serve the feedback dir as the [feedback] share"
```

---

## Task 4: Documentation (`main.rs`, README, DEPLOY)

**Files:**
- Modify: `crates/control-server/src/main.rs:5` (header comment)
- Modify: `crates/control-server/README.md` (lines 5, 22, 35-36)
- Modify: `docs/DEPLOY.md` (line 76 and the "Browsing clone homes" section, lines 267-273)

**Interfaces:** none (documentation only).

- [ ] **Step 1: Update the `main.rs` header comment**

In `crates/control-server/src/main.rs:5`, replace:

```rust
//! (9005) — plus an smbd `clones` share on 445 that surfaces every clone's home. All live.
```

with:

```rust
//! (9005) — plus an smbd serving `clones` (every clone's home) and `feedback` (detector-feedback
//! records) shares on 445. All live.
```

- [ ] **Step 2: Update `crates/control-server/README.md`**

Line 5 — replace `plus an SMB clone-home share on 445` with `plus SMB shares (clones + feedback) on 445`.

Line 22 (the `SMB` table row) — replace:

```
| **SMB** | 445 | SMB (smbd) | the `clones` share — browse every running clone's `/home/rmng` from `smb://<host>/clones` (fixed cred `rmng`/`rmng`) |
```

with:

```
| **SMB** | 445 | SMB (smbd) | two shares (fixed cred `rmng`/`rmng`): `clones` — browse every running clone's `/home/rmng` from `smb://<host>/clones`; `feedback` — the detector-feedback records (`data/detector-feedback`) at `smb://<host>/feedback` |
```

Lines 35-36 — replace:

```
(host poller) · `homes` (clone-home symlinks under `data/hosts/`) · `smb` (smbd supervisor +
read-write `clones` share over `data/hosts`) · `files`
```

with:

```
(host poller) · `homes` (clone-home symlinks under `data/hosts/`) · `smb` (smbd supervisor +
read-write `clones` share over `data/hosts` and `feedback` share over `data/detector-feedback`) · `files`
```

- [ ] **Step 3: Update `docs/DEPLOY.md`**

Line 76 (the `-p 445:445` table row) — replace:

```
| `-p 445:445` | the SMB clone-home share (`clones`) — browse every running clone's `/home/rmng` from `smb://<host>/clones` (below) |
```

with:

```
| `-p 445:445` | the SMB shares — `clones` (browse every running clone's `/home/rmng`) and `feedback` (the detector-feedback records) — from `smb://<host>/clones` and `smb://<host>/feedback` (below) |
```

In the "Browsing clone homes" section, after the SMB bullet that ends `SMB land owned by the clone's own rmng user (uid 1000).` (line 273), add a new bullet:

```markdown
- **The `feedback` share** — the same `smbd` also serves the control-server's detector-feedback
  records (`data/detector-feedback`) as `smb://<host>/feedback`, read-write, same `rmng`/`rmng`
  credential. Browse or prune the JSON records + screenshots while tuning the detector. Scoped to
  that folder only (not the whole `data_dir`, which holds the `claude-accounts.json` secret store).
  Unlike `clones`, it does not need `--pid host`.
```

- [ ] **Step 4: Verify the crate still builds (the `main.rs` doc-comment change)**

Run: `cargo build -p control-server`
Expected: builds clean.

- [ ] **Step 5: Commit**

```bash
git add crates/control-server/src/main.rs crates/control-server/README.md docs/DEPLOY.md
git commit -m "docs: document the feedback SMB share (clones + feedback)"
```

---

## Task 5: Manual integration verification (CT 106)

**Files:** none — this is the real pass/fail gate the unit tests cannot fake. Requires a running build on the W6800 build box / staging (see the `deploy-staging-ct106` memory). Run it after Tasks 1-4 land.

**Interfaces:** none.

- [ ] **Step 1: Build and run the control-server image with the feedback dir published**

On the target host, rebuild and start the control-server (per `docs/DEPLOY.md` — `docker compose up --build`, ensuring `-p 445:445` is published and host port 445 is free).
Expected: the `smb` tracing target logs `wrote /data/smb.conf (clones …, feedback …)` and `smbd` starts (no restart-backoff loop).

- [ ] **Step 2: Ensure at least one feedback record exists**

Either trigger a real detector report, or drop a test file into the records dir inside the container:

Run: `docker exec <control-server-container> sh -c 'mkdir -p /data/data/detector-feedback && echo test > /data/data/detector-feedback/probe.txt'`
Expected: the file exists at `/data/data/detector-feedback/probe.txt`.

- [ ] **Step 3: List the share from a client**

Run (Linux): `smbclient //<host>/feedback -U rmng%rmng -c 'ls'`
Expected: the share connects and lists `probe.txt` (and any real `<id>.json` / `<id>.jpg` records).

- [ ] **Step 4: Prove read-write (create + delete over SMB)**

Run:

```bash
smbclient //<host>/feedback -U rmng%rmng -c 'put /etc/hostname smb-probe.txt; ls; del smb-probe.txt; ls'
```

Expected: `put` succeeds, `smb-probe.txt` appears in the first `ls`, `del` succeeds, and it is gone from the second `ls`. Confirms read-write.

- [ ] **Step 5: Confirm the `clones` share still works (no regression)**

Run: `smbclient //<host>/clones -U rmng%rmng -c 'ls'`
Expected: still lists running clone ids as before — the second share did not break the first.

- [ ] **Step 6: Clean up the probe file**

Run: `docker exec <control-server-container> rm -f /data/data/detector-feedback/probe.txt`
Expected: removed. (Real records, if any, are left in place.)

---

## Self-Review notes

- **Spec coverage:** `[feedback]` section (Task 2), root single-sourcing via a shared const (Task 1), `run()` wiring + create-dir + module doc (Task 3), always-on / no `pid: host` dependency (Task 3 doc + Task 5 step 3), read-write + root ownership (Task 2 config + Task 5 step 4), scope-to-folder-not-`data_dir` (Global Constraints + Task 4 doc), unit tests for the render + root helper (Tasks 1-2), manual integration gate (Task 5), docs in DEPLOY/README/main.rs (Task 4). All spec sections map to a task.
- **Placeholder scan:** none — every code/step shows exact content; `<host>` / `<control-server-container>` in Task 5 are runtime host/container identifiers the operator substitutes, not plan placeholders.
- **Type consistency:** `DETECTOR_FEEDBACK_DIR` / `detector_feedback_root` (Task 1) are referenced verbatim by `absolute_feedback_root` (Task 3); `render_smb_conf(&Path, &Path)` (Task 2) is called with exactly two `&Path` args in `run()` (Task 3) and all three tests.
