//! Runtime payload assets. The Docker image ships everything the control-server
//! distributes (and the frontend it serves) on the filesystem under
//! [`INSTALL_DIR`] — the `clone-daemon` and `agent-wrapper` binaries, the patched
//! `gnome-shell.deb` (shell-01 hide screen-sharing indicator + shell-03 enable
//! `org.gnome.Shell.Eval`) and `static/` (the built frontend). Everything is stored
//! PLAIN (no gzip — registry pushes compress layers anyway) and nothing is compiled
//! into the binary (rust-embed is gone): a payload is looked up at use time with a
//! two-entry search path — the image install dir first, then the repo dev dir — so
//! `cargo run -p control-server` from a checkout picks up locally staged payloads
//! and the dev frontend build without any configuration.

use std::path::{Path, PathBuf};

/// Where the Docker image installs the payloads + frontend (Dockerfile runtime stage).
pub const INSTALL_DIR: &str = "/usr/local/share/rmng";

/// Dev payload dir inside the repo (gitignored; stage plain `clone-daemon` /
/// `agent-wrapper` / `gnome-shell.deb` here by hand or via a local build). Compile-time
/// absolute so it resolves regardless of CWD; in the image the baked build path simply
/// doesn't exist and the search falls through.
const DEV_PAYLOAD_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/embedded-bin");

/// Dev frontend build output (`bun run build` in `frontend/`).
const DEV_STATIC_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../frontend/build/client");

/// Read the payload file `name` (a plain filename, e.g. `clone-daemon` or
/// `gnome-shell.deb`), if present + non-empty. Missing payloads are tolerated by
/// design: callers warn and fall back (e.g. no `gnome-shell.deb` → clones run the
/// stock shell).
pub fn payload(name: &str) -> Option<Vec<u8>> {
    let path = [Path::new(INSTALL_DIR), Path::new(DEV_PAYLOAD_DIR)]
        .iter()
        .map(|d| d.join(name))
        .find(|p| p.is_file())?;
    let bytes = std::fs::read(&path).ok()?;
    (!bytes.is_empty()).then_some(bytes)
}

/// Resolve the frontend dir: `<INSTALL_DIR>/static` in the image, else the repo dev
/// build. `None` when neither has an `index.html` (the web layer serves a 404 hint;
/// the API stays up).
pub fn static_dir() -> Option<PathBuf> {
    [Path::new(INSTALL_DIR).join("static"), PathBuf::from(DEV_STATIC_DIR)]
        .into_iter()
        .find(|p| p.join("index.html").is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// When the patched gnome-shell deb is staged (image build or a dev copy), the
    /// payload must be a real Debian package — guards the plain filename
    /// (`gnome-shell.deb`) staying in sync with the Dockerfile + provisioning push. On
    /// a clean checkout the deb isn't staged, so absence is acceptable (skips).
    #[test]
    fn gnome_shell_deb_is_valid_when_present() {
        match payload("gnome-shell.deb") {
            // `.deb` is an `ar` archive — first member is "debian-binary".
            Some(bytes) => assert!(
                bytes.starts_with(b"!<arch>\ndebian-binary"),
                "gnome-shell.deb payload is not a valid .deb (got {} bytes, head {:?})",
                bytes.len(),
                &bytes[..bytes.len().min(16)]
            ),
            None => eprintln!("gnome-shell.deb payload not staged — skipping"),
        }
    }
}
