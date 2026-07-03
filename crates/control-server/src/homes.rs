//! `data/hosts/<id>` — every managed clone's home (`/home/rmng`) surfaced in one place, so
//! all clones' files are browsable from a single directory (on the control-server box, and
//! on the Docker host itself via the `rmng-data` volume at
//! `/var/lib/docker/volumes/rmng-data/_data/hosts/…`).
//!
//! The Docker-port successor to the Proxmox-era sshfs reconciler (`mounts.rs`, deleted):
//! instead of FUSE-mounting each host's home over SSH, it maintains a plain symlink
//! `<data_dir>/hosts/<id>` → `/proc/<clone-pid>/root/home/rmng` for every RUNNING managed
//! clone. This works because the control-server container runs with `pid: "host"` (see
//! compose.yaml): it shares the Docker host's PID namespace, so `/proc/<pid>/root/...` IS
//! the clone container's root filesystem — and the very same link path resolves on the
//! host too (that's the user's access path).
//!
//! A 15s reconcile loop (same cadence as the old one) links new/running clones, repoints
//! stale links (a clone's PID changes across restarts), and removes links for
//! stopped/deleted/unmanaged hosts. Best-effort throughout: a transient daemon error just
//! retries next tick. When a clone's PID is known but `/proc/<pid>` isn't visible in our
//! namespace (operator forgot `pid: "host"`), it warns ONCE per host, then skips.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use wire::Host;

use crate::app::App;
use crate::docker::CLONE_USER;
use crate::files::is_safe_id;

const RECONCILE_INTERVAL: Duration = Duration::from_secs(15);

/// The directory holding one symlink per managed clone home (`<data_dir>/hosts`).
fn hosts_root(data_dir: &str) -> PathBuf {
    Path::new(data_dir).join("hosts")
}

/// The clone's home as seen through the shared PID namespace: with `pid: "host"`,
/// `/proc/<pid>/root` is the clone container's root fs, so the rmng user's home lives here.
fn clone_home(pid: i64) -> PathBuf {
    PathBuf::from(format!("/proc/{pid}/root/home/{CLONE_USER}"))
}

/// The `/proc/<pid>` entry whose presence proves the clone's PID is visible in our
/// namespace (i.e. the operator did add `pid: "host"`).
fn proc_dir(pid: i64) -> PathBuf {
    PathBuf::from(format!("/proc/{pid}"))
}

/// Names present under `hosts/` that no longer belong to a maintained clone and should be
/// removed (stopped, deleted, unmanaged, or a leftover from a previous run). Pure so it's
/// unit-testable: `existing` is the directory listing, `desired` the ids we linked this
/// tick.
fn entries_to_remove(existing: &[String], desired: &HashSet<String>) -> Vec<String> {
    existing.iter().filter(|n| !desired.contains(*n)).cloned().collect()
}

/// Create or repoint `link` → `target`, best-effort. A link already pointing at `target`
/// is left untouched; a stale symlink or a leftover non-symlink entry (e.g. an empty
/// sshfs-era mountpoint dir) is replaced. Failures are logged, not fatal — next tick
/// retries.
fn ensure_symlink(link: &Path, target: &Path, id: &str) {
    match std::fs::symlink_metadata(link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if std::fs::read_link(link).map(|cur| cur == target).unwrap_or(false) {
                return; // already correct
            }
            let _ = std::fs::remove_file(link); // stale symlink → replace
        }
        Ok(_) => {
            let _ = std::fs::remove_dir(link); // leftover (empty) real dir
        }
        Err(_) => {} // nothing there → just create
    }
    match std::os::unix::fs::symlink(target, link) {
        Ok(()) => tracing::info!(target: "homes", "linked {id} → {}", target.display()),
        Err(e) => tracing::warn!(target: "homes", "linking {id} → {}: {e}", target.display()),
    }
}

/// Remove `hosts/` entries not in `desired`. Only sweeps our own symlinks and empty
/// safe-named dirs (the is_safe_id guard keeps us from touching anything unexpected).
fn prune_stale(root: &Path, desired: &HashSet<String>) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    let names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    for name in entries_to_remove(&names, desired) {
        let p = root.join(&name);
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => {
                if std::fs::remove_file(&p).is_ok() {
                    tracing::info!(target: "homes", "removed stale clone-home link {name}");
                }
            }
            // sshfs-era leftover mountpoint dir — sweep it if empty + safe-named.
            Ok(m) if m.is_dir() && is_safe_id(&name) => {
                let _ = std::fs::remove_dir(&p);
            }
            _ => {}
        }
    }
}

/// One reconcile pass. `warned` tracks host ids we've already logged a missing-`/proc`
/// warning for, so the "add `pid: host`" hint fires once, not every tick.
async fn reconcile(app: &App, warned: &mut HashSet<String>) {
    let cfg = app.config();
    let root = hosts_root(&cfg.data_dir);
    let _ = std::fs::create_dir_all(&root);

    // Only managed clones (container name == host id) with a path-safe id are candidates.
    let hosts: Vec<Host> = app
        .store
        .get()
        .hosts
        .into_iter()
        .filter(|h| h.managed && is_safe_id(&h.id))
        .collect();

    // Ids we maintain a link for this tick; everything else under hosts/ gets pruned.
    let mut desired: HashSet<String> = HashSet::new();

    for h in &hosts {
        let pid = match app.docker.container_pid(&h.id).await {
            Ok(Some(p)) => p,
            Ok(None) => continue, // stopped / gone → no link (prune removes any stale one)
            Err(e) => {
                // Daemon down / dev mode → quiet, retry next tick. Keep any existing link
                // so a transient blip doesn't thrash it.
                tracing::debug!(target: "homes", "pid probe for {} failed: {e:#}", h.id);
                if root.join(&h.id).exists() {
                    desired.insert(h.id.clone());
                }
                continue;
            }
        };

        // `pid: "host"` missing → the clone's PID isn't in our namespace. Warn once, skip.
        if !proc_dir(pid).exists() {
            if warned.insert(h.id.clone()) {
                tracing::warn!(
                    target: "homes",
                    "clone {} pid {pid} not visible in /proc — add `pid: \"host\"` to the \
                     control-server service (compose.yaml) to browse clone homes under data/hosts",
                    h.id
                );
            }
            continue;
        }
        warned.remove(&h.id); // resolved → allow a fresh warning if it ever recurs

        ensure_symlink(&root.join(&h.id), &clone_home(pid), &h.id);
        desired.insert(h.id.clone());
    }

    prune_stale(&root, &desired);

    // Keep the once-warned set bounded to hosts that still exist + are managed.
    let managed: HashSet<String> = hosts.iter().map(|h| h.id.clone()).collect();
    warned.retain(|id| managed.contains(id));
}

/// Background reconcile loop; spawned once at startup (matches `monitor::run`).
pub async fn run(app: App) {
    tracing::info!("clone-home reconciler started (data/hosts, every {}s)", RECONCILE_INTERVAL.as_secs());
    let mut warned: HashSet<String> = HashSet::new();
    loop {
        reconcile(&app, &mut warned).await;
        tokio::time::sleep(RECONCILE_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_root_joins_hosts() {
        assert_eq!(hosts_root("data"), Path::new("data/hosts"));
        assert_eq!(hosts_root("/srv/rmng/data"), Path::new("/srv/rmng/data/hosts"));
    }

    #[test]
    fn clone_home_targets_proc_root_home() {
        // With pid:host, /proc/<pid>/root is the clone's fs; home is the rmng user's.
        assert_eq!(clone_home(4321), PathBuf::from("/proc/4321/root/home/rmng"));
    }

    #[test]
    fn proc_dir_shape() {
        assert_eq!(proc_dir(17), PathBuf::from("/proc/17"));
    }

    #[test]
    fn entries_to_remove_keeps_desired_drops_the_rest() {
        let existing = vec!["a".to_string(), "b".to_string(), "gone".to_string()];
        let desired: HashSet<String> = ["a".to_string(), "b".to_string()].into_iter().collect();
        assert_eq!(entries_to_remove(&existing, &desired), vec!["gone".to_string()]);
        // No managed clones (empty desired) → everything on disk is stale.
        assert_eq!(entries_to_remove(&existing, &HashSet::new()), existing);
        // Nothing on disk → nothing to remove.
        assert!(entries_to_remove(&[], &desired).is_empty());
    }
}
