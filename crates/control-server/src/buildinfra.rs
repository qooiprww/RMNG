//! Shared Docker build infra for the clone fleet: a pull-through Docker Hub cache
//! (`rmng-registry`) and a shared BuildKit daemon (`rmng-buildkit`), plus the reconciler
//! that migrates the mirror config + a remote buildx builder onto every running clone.
//! Mirrors `crate::ssh`'s reconciler shape. The two infra containers are ensured by
//! `DockerCtl::ensure_build_infra` (see `docker.rs`); this module owns the pure config
//! rendering and the per-clone apply loop.

use anyhow::{Context, Result};

/// Pull-through cache container name (DNS-resolvable by clones on the `rmng` bridge).
pub const REGISTRY_CONTAINER: &str = "rmng-registry";
/// Shared BuildKit daemon container name.
pub const BUILDKIT_CONTAINER: &str = "rmng-buildkit";
/// Named volume holding the pull-through cache's blobs.
pub const REGISTRY_DATA_VOL: &str = "rmng-registry-data";
/// Named volume holding the shared BuildKit layer cache.
pub const BUILDKIT_CACHE_VOL: &str = "rmng-buildkit-cache";
/// The registry address clones put in `daemon.json` (container DNS name : port).
pub const REGISTRY_ADDR: &str = "rmng-registry:5000";
/// The BuildKit GRPC endpoint the clones' remote buildx builder connects to (plaintext on
/// the trusted bridge).
pub const BUILDKIT_ENDPOINT: &str = "tcp://rmng-buildkit:1234";
/// The buildx builder name registered in each clone.
pub const BUILDER_NAME: &str = "rmng";

/// Merge the pull-through mirror settings into a clone's existing `daemon.json` content
/// (empty/whitespace ⇒ start from `{}`). Adds `registry-mirrors: ["http://rmng-registry:5000"]`
/// and `insecure-registries: ["rmng-registry:5000"]` (the HTTP mirror *requires* the insecure
/// entry). Idempotent: returns `Ok(None)` when both keys already carry our values (⇒ caller
/// writes nothing and sends no SIGHUP); otherwise `Ok(Some(pretty_json))`. All other keys are
/// preserved. Pure — unit-tested.
pub fn merge_mirror_daemon_json(existing: &str) -> Result<Option<String>> {
    use serde_json::{Map, Value};
    let mirror = format!("http://{REGISTRY_ADDR}");
    let mut root: Value = if existing.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::from_str(existing).context("parsing existing daemon.json")?
    };
    let obj = root.as_object_mut().context("daemon.json is not a JSON object")?;

    let has = |obj: &Map<String, Value>, key: &str, val: &str| {
        obj.get(key)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().any(|v| v.as_str() == Some(val)))
            .unwrap_or(false)
    };
    if has(obj, "registry-mirrors", &mirror) && has(obj, "insecure-registries", REGISTRY_ADDR) {
        return Ok(None);
    }

    merge_into_string_array(obj, "registry-mirrors", &mirror);
    merge_into_string_array(obj, "insecure-registries", REGISTRY_ADDR);
    Ok(Some(serde_json::to_string_pretty(&root)?))
}

fn merge_into_string_array(obj: &mut serde_json::Map<String, serde_json::Value>, key: &str, val: &str) {
    use serde_json::Value;
    let arr = obj.entry(key.to_string()).or_insert_with(|| Value::Array(Vec::new()));
    match arr.as_array_mut() {
        Some(a) if a.iter().any(|v| v.as_str() == Some(val)) => {}
        Some(a) => a.push(Value::String(val.to_string())),
        None => *arr = Value::Array(vec![Value::String(val.to_string())]), // malformed → replace
    }
}

/// The `buildkitd.toml` for `rmng-buildkit`: a GC policy capping the shared layer cache at
/// `gc_gb` GiB. Pure — unit-tested (a mis-scaled cap is a silent unbounded-cache bug).
///
/// NOTE (E2E): confirm `keepBytes`/`[[worker.oci.gcpolicy]]` against the pinned buildkit
/// version during Task 6; older versions use `gckeepstorage`. Adjust here if the pinned tag
/// wants the legacy key.
pub fn render_buildkitd_toml(gc_gb: u32) -> String {
    let keep_bytes = gc_gb as u64 * 1024 * 1024 * 1024;
    format!(
        "# Rendered by rmng control-server — do not edit.\n\
         root = \"/var/lib/buildkit\"\n\
         [worker.oci]\n\
         enabled = true\n\
         gc = true\n\
         [[worker.oci.gcpolicy]]\n\
         keepBytes = {keep_bytes}\n\
         all = true\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_into_empty_adds_both_keys() {
        let out = merge_mirror_daemon_json("").unwrap().expect("empty file must produce a write");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["registry-mirrors"][0], "http://rmng-registry:5000");
        assert_eq!(v["insecure-registries"][0], "rmng-registry:5000");
    }

    #[test]
    fn merge_preserves_unrelated_keys() {
        let existing = r#"{"log-driver":"json-file","registry-mirrors":["http://other:5000"]}"#;
        let out = merge_mirror_daemon_json(existing).unwrap().expect("must write");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["log-driver"], "json-file", "unrelated key preserved");
        let mirrors = v["registry-mirrors"].as_array().unwrap();
        assert!(mirrors.iter().any(|m| m == "http://other:5000"), "existing mirror kept");
        assert!(mirrors.iter().any(|m| m == "http://rmng-registry:5000"), "ours appended");
        assert_eq!(v["insecure-registries"][0], "rmng-registry:5000");
    }

    #[test]
    fn merge_is_noop_when_already_applied() {
        let existing = r#"{"registry-mirrors":["http://rmng-registry:5000"],"insecure-registries":["rmng-registry:5000"]}"#;
        assert!(
            merge_mirror_daemon_json(existing).unwrap().is_none(),
            "already-applied config must produce no write (⇒ no SIGHUP)"
        );
    }

    #[test]
    fn buildkitd_toml_has_scaled_keep_bytes() {
        // 40 GiB → 42949672960 bytes must appear verbatim; a mis-scaled cap is a silent
        // unbounded-cache bug.
        let out = render_buildkitd_toml(40);
        assert!(out.contains("42949672960"), "keepBytes for 40 GiB:\n{out}");
        assert!(out.contains("gc = true"), "GC must be enabled:\n{out}");
    }
}
