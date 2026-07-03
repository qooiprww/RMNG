# Control-Server Self-Update & Restart Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add in-product buttons to the control-server's own settings page to **restart** the control-server (apply changed startup settings) and **update** it (pull the latest image from Docker Hub and swap the running container onto it), with version/update-available detection and a publish pipeline for the image.

**Architecture:** The control-server already runs as a privileged container that drives the host Docker daemon via bollard. Restart is an in-place `docker restart self`. Update pulls a new image, captures the live container run-spec via self-inspect, and launches a detached helper container (the new image run with a hidden `self-upgrade` subcommand) that outlives the swap — it stops/removes the old container and recreates it from the new image. A handoff file in the `/data` volume carries the spec + op id across the reboot; on boot the new server reconciles it and reports the result.

**Tech Stack:** Rust (edition 2024, axum 0.7, bollard 0.19, tokio, anyhow, serde), ts-rs (Rust→TS type generation), React Router 7 SPA (TypeScript, Tailwind v4, Bun), Docker.

**Reference spec:** `docs/superpowers/specs/2026-07-03-control-server-self-update-design.md`.

## Global Constraints

- **Rust edition 2024, rust-version 1.85** — matches the workspace `Cargo.toml`.
- **bollard 0.19** is the ONLY Docker client — never shell out to the `docker` CLI from the server.
- **No-env-settings invariant** — all config lives in `config.json` (read via `App::config()`); never add `-e`/env-var settings. The one exception is the reserved `RUST_LOG`.
- **ts-rs regeneration** — after ANY change to a `#[ts(export)]` type in `crates/wire/src/`, run `cargo test -p wire` to regenerate `frontend/app/lib/wire/*.ts`. Never hand-edit generated wire types.
- **Two frontend type sources** — the frontend has BOTH generated `~/lib/wire/*` types AND a hand-written `~/lib/types.ts`. `OperationProgress.tsx` consumes `~/lib/types.ts`, so an `OperationKind` change must be applied to `~/lib/types.ts` AND its `VERB` map, in addition to the regenerated wire enum.
- **DNS-label naming** — container/image names use lowercase letters, digits, hyphens.
- **Handler return convention** — axum handlers return `Result<Json<T>, (StatusCode, String)>` (no shared `AppError`); map job errors with `.map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))`.
- **Commit trailer** — end every commit message with:
  ```
  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  ```
- **Verify loop** for Rust+frontend changes: `cargo test -p wire && cargo build -p control-server && (cd frontend && bun run typecheck)`.

---

## File Structure

**Created:**
- `scripts/publish-server.sh` — build + label + tag + push `pegasis0/rmng`.
- `crates/control-server/src/update.rs` — the `self-upgrade` subcommand, handoff-file types, and boot-time `reconcile_pending`.

**Modified:**
- `Dockerfile` — `ARG GIT_SHA` / `ARG BUILD_DATE` → OCI `LABEL`s in the runtime stage.
- `crates/wire/src/config.rs` — add `docker.serverImage` field.
- `crates/wire/src/control.rs` — add `OperationKind::Update`; add `UpdateStatus` DTO.
- `crates/control-server/src/docker.rs` — version/detection primitives (`self_image_info`, `registry_digest`, `check_update`, `is_update_available`), swap primitives (`SelfSpec`, `inspect_self`, `create_and_start_from_spec`), and `restart_self`.
- `crates/control-server/src/jobs.rs` — `start_update` / `run_update`.
- `crates/control-server/src/main.rs` — `self-upgrade` argv dispatch; `reconcile_pending` before `fail_stale_ops`; register `mod update`.
- `crates/control-server/src/web.rs` — `GET /api/server/version`, `POST /api/server/update`, `POST /api/server/restart`.
- `frontend/app/lib/types.ts` — add `"update"` to `OperationKind`.
- `frontend/app/components/OperationProgress.tsx` — add `update` to the `VERB` map.
- `frontend/app/lib/api.ts` — `getUpdateStatus`, `updateServer`, `restartServer`.
- `frontend/app/components/SettingsPanel.tsx` — new "Control-server" section (version, Check, Update, Restart); replace the restart banner.
- `frontend/app/routes/_index.tsx` — thread the new API fns into `SettingsPanel`.
- `docs/DEPLOY.md` — document the buttons + the first-update-is-manual caveat.

---

## Phase A — Publish pipeline & version detection

### Task A1: Image version labels + publish script

**Files:**
- Modify: `Dockerfile` (runtime stage, after line 106)
- Create: `scripts/publish-server.sh`

**Interfaces:**
- Produces: an image whose runtime layer carries labels `org.opencontainers.image.revision`, `.created`, `.version`; the moving tag `pegasis0/rmng:latest` + a dated `:YYYYMMDD`.

- [ ] **Step 1: Add build-arg labels to the Dockerfile runtime stage**

In `Dockerfile`, in the `runtime` stage, immediately after the `apt-get install` block (after line 106, before the `COPY --from=rust-build /out/rmng-control-server ...` line at 108), add:

```dockerfile
# Version stamp for the in-product self-update UI. Passed by scripts/publish-server.sh
# (--build-arg); a plain `docker build` with no args leaves them empty → the UI shows a
# "dev build". These are the only place the running server learns its own version.
ARG GIT_SHA=""
ARG BUILD_DATE=""
LABEL org.opencontainers.image.revision="$GIT_SHA" \
      org.opencontainers.image.created="$BUILD_DATE" \
      org.opencontainers.image.version="$GIT_SHA"
```

- [ ] **Step 2: Create the publish script**

Create `scripts/publish-server.sh`:

```bash
#!/usr/bin/env bash
# Build + publish the RMNG control-server image (root Dockerfile).
#
#   scripts/publish-server.sh [SERVER_REPO]
#
# Builds with the REPO ROOT as context, stamps git-SHA + build-date labels (so the running
# server can show its version + detect updates), tags an immutable dated `:YYYYMMDD` + a
# moving `:latest`, and pushes both. Repo defaults to pegasis0/rmng; override via the
# SERVER_REPO env or the first positional arg. Rollback = repoint the update reference
# (config docker.serverImage) at a prior dated tag.
set -euo pipefail

REPO="${1:-${SERVER_REPO:-pegasis0/rmng}}"
DATE_TAG="$(date +%Y%m%d)"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GIT_SHA="$(cd "$ROOT" && git rev-parse --short HEAD)"
BUILD_DATE="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

echo ">> building $REPO:$DATE_TAG (+ :latest) from $ROOT (sha=$GIT_SHA)"
docker build \
  --build-arg "GIT_SHA=$GIT_SHA" \
  --build-arg "BUILD_DATE=$BUILD_DATE" \
  -t "$REPO:$DATE_TAG" \
  -t "$REPO:latest" \
  "$ROOT"

echo ">> pushing $REPO:$DATE_TAG"
docker push "$REPO:$DATE_TAG"
echo ">> pushing $REPO:latest"
docker push "$REPO:latest"

echo ">> published $REPO:$DATE_TAG and $REPO:latest"
```

- [ ] **Step 3: Make it executable**

Run: `chmod +x scripts/publish-server.sh`

- [ ] **Step 4: Verify the Dockerfile builds with labels (fast, no push)**

Run:
```bash
docker build --build-arg GIT_SHA=testsha --build-arg BUILD_DATE=2026-07-03T00:00:00Z -t rmng:labeltest . \
  && docker inspect -f '{{ index .Config.Labels "org.opencontainers.image.revision" }}' rmng:labeltest
```
Expected: prints `testsha`. (This is a full image build; it may take several minutes. If build resources are unavailable in this environment, defer this verification to Task D1's E2E — note that in the commit message.)

- [ ] **Step 5: Commit**

```bash
git add Dockerfile scripts/publish-server.sh
git commit -m "feat(deploy): version-labeled control-server image + publish-server.sh

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task A2: `docker.serverImage` config field

**Files:**
- Modify: `crates/wire/src/config.rs:144-204` (the `DockerConfig` struct, its default fns, and `Default` impl)
- Test: `crates/wire/src/config.rs` (inline `#[cfg(test)]` if present, else a new test)

**Interfaces:**
- Produces: `DockerConfig.server_image: String` (JSON `serverImage`), default `"pegasis0/rmng:latest"`.

- [ ] **Step 1: Add the field to `DockerConfig`**

In `crates/wire/src/config.rs`, inside `struct DockerConfig` (after the `template_reference` field, before the closing brace at line 172), add:

```rust
    /// Registry reference the in-product self-update pulls the control-server image from
    /// (and digest-compares against for update-available detection). Immediate-apply (read
    /// fresh per check/update); no secret (public image over the local daemon), so it
    /// passes through the redacted view.
    #[serde(default = "default_server_image")]
    pub server_image: String,
```

- [ ] **Step 2: Add the default fn**

In `crates/wire/src/config.rs`, next to `default_template_reference` (after line 191), add:

```rust
fn default_server_image() -> String {
    "pegasis0/rmng:latest".into()
}
```

- [ ] **Step 3: Add it to the `Default` impl**

In `crates/wire/src/config.rs`, in `impl Default for DockerConfig` (lines 193-204), add the field to the constructed `Self`:

```rust
            template_reference: default_template_reference(),
            server_image: default_server_image(),
```

- [ ] **Step 4: Write a test that the default is set**

Add to the `#[cfg(test)]` module in `crates/wire/src/config.rs` (create the module if none exists):

```rust
    #[test]
    fn docker_config_default_server_image() {
        let d = DockerConfig::default();
        assert_eq!(d.server_image, "pegasis0/rmng:latest");
    }
```

- [ ] **Step 5: Run the test + regenerate TS**

Run: `cargo test -p wire docker_config_default_server_image`
Expected: PASS. Then run `cargo test -p wire` (regenerates `frontend/app/lib/wire/*`).

- [ ] **Step 6: Commit**

```bash
git add crates/wire/src/config.rs frontend/app/lib/wire/
git commit -m "feat(wire): docker.serverImage config field for self-update target

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task A3: `UpdateStatus` DTO + docker.rs detection primitives

**Files:**
- Modify: `crates/wire/src/control.rs` (add `UpdateStatus` near the other DTOs, ~line 234)
- Modify: `crates/control-server/src/docker.rs` (add primitives near the images section, ~line 735-825)
- Test: `crates/control-server/src/docker.rs` (inline test for the pure compare helper)

**Interfaces:**
- Produces (wire): `struct UpdateStatus { current_revision: Option<String>, current_created: Option<String>, current_digest: Option<String>, remote_digest: Option<String>, available: bool, reference: String, error: Option<String> }`.
- Produces (docker.rs): `DockerCtl::self_image_info(&self, self_id: &str) -> Result<ServerImageInfo>`, `DockerCtl::registry_digest(&self, reference: &str) -> Result<String>`, `DockerCtl::check_update(&self, reference: &str) -> UpdateStatus`, and the pure `is_update_available(current_digest: Option<&str>, remote_digest: &str) -> bool`.

- [ ] **Step 1: Add the `UpdateStatus` DTO to wire**

In `crates/wire/src/control.rs`, after the `ContainerStats` struct (after line 234), add:

```rust
/// Version + update-available status for the control-server itself, served by
/// `GET /api/server/version`. `current_*` come from the running image's OCI labels /
/// RepoDigest; `remote_digest` from a registry manifest query (no pull). `available` is
/// true when a remote digest was fetched and differs from the running one. `error` carries
/// a non-fatal detail (e.g. registry unreachable) so the UI can show "couldn't check".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export, export_to = "../../../frontend/app/lib/wire/")]
pub struct UpdateStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_created: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_digest: Option<String>,
    pub available: bool,
    pub reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
```

- [ ] **Step 2: Regenerate TS + export the new type**

Run: `cargo test -p wire`
Expected: PASS, and `frontend/app/lib/wire/UpdateStatus.ts` now exists.

- [ ] **Step 3: Add `UpdateStatus` to the docker.rs `wire` import**

In `crates/control-server/src/docker.rs`, extend the `use wire::{...}` line (line 50) to include `UpdateStatus`:

```rust
use wire::{ContainerStats, DockerConfig, EnvCheckRow, ImageInfo, SetupEnv, UpdateStatus};
```

- [ ] **Step 4: Write the failing test for the pure compare helper**

In the `#[cfg(test)]` module at the bottom of `crates/control-server/src/docker.rs`, add:

```rust
    #[test]
    fn is_update_available_compares_digests() {
        // No local digest known → treat as available (can't prove up-to-date).
        assert!(super::is_update_available(None, "sha256:bbb"));
        // Same digest → up to date.
        assert!(!super::is_update_available(Some("sha256:aaa"), "sha256:aaa"));
        // Different digest → update available.
        assert!(super::is_update_available(Some("sha256:aaa"), "sha256:bbb"));
    }
```

- [ ] **Step 5: Run it to verify it fails**

Run: `cargo test -p control-server is_update_available_compares_digests`
Expected: FAIL — `cannot find function is_update_available`.

- [ ] **Step 6: Implement the primitives**

In `crates/control-server/src/docker.rs`, add the `ServerImageInfo` struct near the top-level structs (after `ManagedContainer`, ~line 272):

```rust
/// The running control-server image's identity, read from its own container → image.
/// `repo_digest` is the `repo@sha256:…` for the configured repo (the update-check
/// baseline); `revision`/`created` are the OCI labels stamped by scripts/publish-server.sh.
#[derive(Debug, Clone, Default)]
pub struct ServerImageInfo {
    pub image_id: String,
    pub repo_digest: Option<String>,
    pub revision: Option<String>,
    pub created: Option<String>,
}
```

Add the methods inside `impl DockerCtl` (after `image_labels`, ~line 808):

```rust
    /// The running control-server image's identity: inspect our own container to get its
    /// image id, then inspect that image for the RepoDigest matching `repo` + the OCI
    /// version labels. `repo` is the reference-without-tag of `docker.serverImage`.
    pub async fn self_image_info(&self, self_id: &str, repo: &str) -> Result<ServerImageInfo> {
        let docker = self.daemon()?;
        let ctr = docker
            .inspect_container(self_id, None::<bollard::query_parameters::InspectContainerOptions>)
            .await
            .context("inspecting self container for image info")?;
        let image_id = ctr.image.clone().ok_or_else(|| anyhow!("self container has no image id"))?;
        let img = docker.inspect_image(&image_id).await.context("inspecting self image")?;
        let labels = img.config.as_ref().and_then(|c| c.labels.clone()).unwrap_or_default();
        // RepoDigest for our repo, e.g. "pegasis0/rmng@sha256:…". Match on the repo prefix.
        let repo_digest = img
            .repo_digests
            .unwrap_or_default()
            .into_iter()
            .find(|rd| rd.starts_with(&format!("{repo}@")));
        Ok(ServerImageInfo {
            image_id,
            repo_digest,
            revision: labels.get("org.opencontainers.image.revision").cloned().filter(|s| !s.is_empty()),
            created: labels.get("org.opencontainers.image.created").cloned().filter(|s| !s.is_empty()),
        })
    }

    /// The remote manifest digest of `reference` from the registry, WITHOUT pulling
    /// (Docker's `/distribution/{name}/json`). Returns the descriptor digest string
    /// (`sha256:…`). Surfaces registry errors verbatim (auth / rate-limit / not-found).
    pub async fn registry_digest(&self, reference: &str) -> Result<String> {
        let info = self
            .daemon()?
            .inspect_registry_image(reference, None)
            .await
            .with_context(|| format!("querying the registry for {reference}"))?;
        // bollard's DistributionInspect carries an OCI descriptor with the manifest digest.
        Ok(info.descriptor.digest)
    }

    /// Compute the full update status for the UI: current identity + remote digest +
    /// available flag. Never bails — registry / daemon failures land in `status.error`
    /// with `available = false`, so the UI can always render something.
    pub async fn check_update(&self, reference: &str, self_id: Option<&str>) -> UpdateStatus {
        let (repo, _tag) = split_reference(reference);
        let mut status = UpdateStatus {
            current_revision: None,
            current_created: None,
            current_digest: None,
            remote_digest: None,
            available: false,
            reference: reference.to_string(),
            error: None,
        };
        // Current identity (dev mode / no self container → leave current_* None).
        if let Some(id) = self_id {
            match self.self_image_info(id, &repo).await {
                Ok(info) => {
                    status.current_revision = info.revision;
                    status.current_created = info.created;
                    status.current_digest = info.repo_digest.map(|rd| {
                        // Keep just the sha256:… part for a clean compare/display.
                        rd.split_once('@').map(|(_, d)| d.to_string()).unwrap_or(rd)
                    });
                }
                Err(e) => status.error = Some(format!("reading current image: {e}")),
            }
        }
        // Remote digest.
        match self.registry_digest(reference).await {
            Ok(remote) => {
                status.available = is_update_available(status.current_digest.as_deref(), &remote);
                status.remote_digest = Some(remote);
            }
            Err(e) => {
                // Don't overwrite a current-image error with the remote one; append.
                let msg = format!("checking registry: {e}");
                status.error = Some(match status.error.take() {
                    Some(prev) => format!("{prev}; {msg}"),
                    None => msg,
                });
            }
        }
        status
    }
```

Add the pure helper at the bottom of the file, near the other `Pure helpers` (after `short_id`, ~line 1585):

```rust
/// Whether a remote digest represents an update over the running one. Unknown local digest
/// (dev build / no RepoDigest) → true (can't prove up-to-date, so offer the update).
fn is_update_available(current_digest: Option<&str>, remote_digest: &str) -> bool {
    match current_digest {
        Some(cur) => cur != remote_digest,
        None => true,
    }
}
```

- [ ] **Step 7: Run the test to verify it passes**

Run: `cargo test -p control-server is_update_available_compares_digests`
Expected: PASS.

- [ ] **Step 8: Build to confirm the daemon-driven methods compile**

Run: `cargo build -p control-server`
Expected: compiles. If `inspect_registry_image` or `info.descriptor.digest` names differ in bollard 0.19, fix per the compiler error (the method exists; the return type is `bollard::models::DistributionInspect` — adjust the field access to reach the manifest digest, e.g. `info.descriptor.digest` may be `info.descriptor.digest` or nested — follow the type).

- [ ] **Step 9: Commit**

```bash
git add crates/wire/src/control.rs crates/control-server/src/docker.rs frontend/app/lib/wire/
git commit -m "feat(docker): control-server version + update-available detection

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task A4: `GET /api/server/version` endpoint + api.ts client

**Files:**
- Modify: `crates/control-server/src/web.rs` (router ~line 71, new handler near `setup_env`)
- Modify: `frontend/app/lib/api.ts`

**Interfaces:**
- Consumes: `DockerCtl::check_update`, `EnvReport.self_container` (via `app.docker.env().await.self_container`).
- Produces: `GET /api/server/version` → `Json<UpdateStatus>`; `getUpdateStatus(): Promise<UpdateStatus>` in api.ts.

- [ ] **Step 1: Add the route**

In `crates/control-server/src/web.rs`, in `router()`, after the `.route("/api/setup/env", get(setup_env))` line (line 60), add:

```rust
        .route("/api/server/version", get(server_version))
```

- [ ] **Step 2: Add the handler**

In `crates/control-server/src/web.rs`, near `setup_env`, add (this handler never fails — it always returns a status, with errors carried inside `UpdateStatus.error`):

```rust
/// `GET /api/server/version` — the control-server's own version + whether Hub has a newer
/// image (registry digest compare, no pull). Never 500s: registry/daemon failures land in
/// `UpdateStatus.error` so the UI always renders.
async fn server_version(State(app): State<App>) -> Json<wire::UpdateStatus> {
    let reference = app.config().docker.server_image;
    let self_id = app.docker.env().await.self_container;
    Json(app.docker.check_update(&reference, self_id.as_deref()).await)
}
```

- [ ] **Step 3: Add the api.ts client wrapper**

In `frontend/app/lib/api.ts`, add the import at the top (with the other wire imports):

```ts
import type { UpdateStatus } from "~/lib/wire/UpdateStatus";
```

and the wrapper (near `pullTemplate`):

```ts
/** The control-server's own version + whether Hub has a newer image (no pull). */
export const getUpdateStatus = () => getJson("/api/server/version") as Promise<UpdateStatus>;
```

- [ ] **Step 4: Verify backend compiles + frontend typechecks**

Run: `cargo build -p control-server && (cd frontend && bun run typecheck)`
Expected: both succeed.

- [ ] **Step 5: Commit**

```bash
git add crates/control-server/src/web.rs frontend/app/lib/api.ts
git commit -m "feat(web): GET /api/server/version endpoint + api client

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task A5: "Control-server" settings section (version + Check)

**Files:**
- Modify: `frontend/app/components/SettingsPanel.tsx` (props interface ~line 98-119, destructure ~121-133, new Section in the render body)
- Modify: `frontend/app/routes/_index.tsx` (import + thread the prop)

**Interfaces:**
- Consumes: `getUpdateStatus` from api.ts.
- Produces: a `getUpdateStatus` prop on `SettingsPanelProps`; a rendered "Control-server" section showing version + an "Up to date"/"Update available" badge + a **Check** button.

- [ ] **Step 1: Add the prop to `SettingsPanelProps`**

In `frontend/app/components/SettingsPanel.tsx`, add the import near the top:

```ts
import type { UpdateStatus } from "~/lib/wire/UpdateStatus";
```

and add to the `SettingsPanelProps` interface (after `applyMonitors`, ~line 111):

```ts
  /** Read the control-server's own version + update-available status. */
  getUpdateStatus: () => Promise<UpdateStatus>;
```

and add `getUpdateStatus` to the destructured params (after `applyMonitors,` ~line 127):

```ts
  getUpdateStatus,
```

- [ ] **Step 2: Add local state + the check handler**

In `frontend/app/components/SettingsPanel.tsx`, near the other `useState` declarations (~line 140-144), add:

```ts
  const [serverStatus, setServerStatus] = useState<UpdateStatus | null>(null);
  const [serverMsg, setServerMsg] = useState<string | null>(null);

  useEffect(() => {
    getUpdateStatus().then(setServerStatus).catch((e) => setServerMsg(`✗ ${(e as Error).message}`));
  }, [getUpdateStatus]);

  async function checkUpdate() {
    setServerMsg("checking…");
    try {
      const s = await getUpdateStatus();
      setServerStatus(s);
      setServerMsg(s.error ? `⚠ ${s.error}` : s.available ? "update available" : "up to date");
    } catch (e) {
      setServerMsg(`✗ ${(e as Error).message}`);
    }
  }
```

(If `useEffect` is not already imported in this file, add it to the `react` import.)

- [ ] **Step 3: Render the section**

In `frontend/app/components/SettingsPanel.tsx`, add this `Section` in the render body alongside the other sections (place it near the "Docker / Clones" section, ~line 510). Use the file's existing `Section`/button classNames:

```tsx
            <Section title="Control-server" effect="restart" hint="Update to the latest published image, or restart to apply changed startup settings.">
              <div className="space-y-2">
                <div className="text-xs text-slate-500 dark:text-slate-400">
                  {serverStatus?.currentRevision ? (
                    <>Version <code>{serverStatus.currentRevision}</code>{serverStatus.currentCreated ? ` · ${serverStatus.currentCreated}` : ""}</>
                  ) : (
                    "dev build (unversioned image)"
                  )}
                </div>
                <div className="flex items-center gap-2">
                  <button
                    type="button"
                    onClick={checkUpdate}
                    className="rounded border border-slate-300 dark:border-slate-600 px-2.5 py-1.5 text-xs text-slate-600 dark:text-slate-300 hover:bg-slate-50 dark:hover:bg-slate-800"
                  >
                    Check for updates
                  </button>
                  {serverStatus ? (
                    <span className={`rounded px-1.5 py-0.5 text-[10px] font-semibold ${serverStatus.available ? "bg-amber-100 dark:bg-amber-900/40 text-amber-700 dark:text-amber-400" : "bg-emerald-100 dark:bg-emerald-900/40 text-emerald-700 dark:text-emerald-400"}`}>
                      {serverStatus.available ? "update available" : "up to date"}
                    </span>
                  ) : null}
                  {serverMsg ? <p className="text-xs text-slate-500 dark:text-slate-400">{serverMsg}</p> : null}
                </div>
              </div>
            </Section>
```

- [ ] **Step 4: Thread the prop from the route**

In `frontend/app/routes/_index.tsx`, add `getUpdateStatus` to the `~/lib/api` import block (lines 11-26):

```ts
  getUpdateStatus,
```

and pass it to the `<SettingsPanel ...>` mount (~line 363-381), after `applyMonitors={applyMonitors}`:

```tsx
          getUpdateStatus={getUpdateStatus}
```

- [ ] **Step 5: Typecheck**

Run: `(cd frontend && bun run typecheck)`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add frontend/app/components/SettingsPanel.tsx frontend/app/routes/_index.tsx
git commit -m "feat(frontend): control-server version + check-for-updates in settings

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase B — Self-update swap

### Task B1: `SelfSpec` capture + recreate primitives

**Files:**
- Modify: `crates/control-server/src/docker.rs` (add `SelfSpec` + methods near the container section, ~line 915-1043)
- Test: `crates/control-server/src/docker.rs` (inline test with a JSON fixture)

**Interfaces:**
- Produces: `struct SelfSpec { container_name, new_image_ref, old_image_id, config: ContainerConfig, host_config: HostConfig, networks: HashMap<String, EndpointSettings> }` (serde), `SelfSpec::from_inspect(&ContainerInspectResponse, new_image_ref: &str) -> Result<SelfSpec>`, `DockerCtl::inspect_self(&self, self_id) -> Result<ContainerInspectResponse>`, `DockerCtl::create_and_start_from_spec(&self, spec: &SelfSpec) -> Result<String>`.

- [ ] **Step 1: Extend the bollard model imports**

In `crates/control-server/src/docker.rs`, extend the `use bollard::models::{...}` block (lines 35-40) to include `ContainerInspectResponse`:

```rust
    ContainerConfig, ContainerCreateBody, ContainerInspectResponse, EndpointSettings, HostConfig,
    Ipam, IpamConfig, Mount, MountBindOptions, MountBindOptionsPropagationEnum, MountPointTypeEnum,
    MountTypeEnum, NetworkConnectRequest, NetworkCreateRequest, NetworkingConfig, RestartPolicy,
    RestartPolicyNameEnum, VolumeCreateOptions,
```

Add `use serde::{Deserialize, Serialize};` to the imports if not present.

- [ ] **Step 2: Write the failing test for `from_inspect`**

In the `#[cfg(test)]` module of `crates/control-server/src/docker.rs`, add:

```rust
    #[test]
    fn self_spec_from_inspect_projects_fields() {
        // Minimal ContainerInspectResponse JSON (bollard models are Deserialize).
        let json = r#"{
            "Id": "abc123",
            "Name": "/rmng",
            "Image": "sha256:oldimageid",
            "Config": { "Hostname": "rmng", "Env": ["RUST_LOG=info"], "Image": "rmng:latest" },
            "HostConfig": { "Privileged": true, "PidMode": "host" },
            "NetworkSettings": { "Networks": { "rmng": { "Aliases": ["rmng-control"] } } }
        }"#;
        let resp: super::ContainerInspectResponse = serde_json::from_str(json).unwrap();
        let spec = super::SelfSpec::from_inspect(&resp, "pegasis0/rmng:latest").unwrap();
        assert_eq!(spec.container_name, "rmng"); // leading slash stripped
        assert_eq!(spec.old_image_id, "sha256:oldimageid");
        assert_eq!(spec.new_image_ref, "pegasis0/rmng:latest");
        assert_eq!(spec.host_config.privileged, Some(true));
        assert_eq!(spec.host_config.pid_mode.as_deref(), Some("host"));
        assert!(spec.networks.contains_key("rmng"));
    }
```

- [ ] **Step 3: Run it to verify it fails**

Run: `cargo test -p control-server self_spec_from_inspect_projects_fields`
Expected: FAIL — `cannot find type SelfSpec`.

- [ ] **Step 4: Implement `SelfSpec` + `from_inspect`**

In `crates/control-server/src/docker.rs`, add near `CreateSpec` (after line 260):

```rust
/// A captured control-server run-spec: everything `create_container` needs to recreate our
/// own container, projected from a live self-inspect. Serialized into the update handoff
/// file so the `self-upgrade` helper (a fresh process from the new image) can recreate us.
/// The only field NOT copied from the running container is the image — overridden to
/// `new_image_ref`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelfSpec {
    /// The container name (== how the deployment named us), leading `/` stripped.
    pub container_name: String,
    /// The image the recreated container runs (the newly pulled ref).
    pub new_image_ref: String,
    /// The image id we were running before the swap (for the create-error fallback).
    pub old_image_id: String,
    /// Captured `Config` (hostname/env/labels/exposed_ports/stop_signal/stop_timeout/…).
    pub config: ContainerConfig,
    /// Captured `HostConfig` (privileged/pid_mode/init/mounts/port_bindings/restart_policy/…).
    pub host_config: HostConfig,
    /// Captured network attachments + aliases (preserves the rmng-control alias).
    pub networks: HashMap<String, EndpointSettings>,
}

impl SelfSpec {
    /// Project a container inspect into a `SelfSpec`, overriding the image to `new_image_ref`.
    /// Pure (no I/O) so it's unit-testable against a fixture inspect.
    pub fn from_inspect(resp: &ContainerInspectResponse, new_image_ref: &str) -> Result<SelfSpec> {
        let container_name =
            resp.name.clone().unwrap_or_default().trim_start_matches('/').to_string();
        let old_image_id = resp.image.clone().ok_or_else(|| anyhow!("inspect has no image id"))?;
        let config = resp.config.clone().ok_or_else(|| anyhow!("inspect has no config"))?;
        let host_config =
            resp.host_config.clone().ok_or_else(|| anyhow!("inspect has no host_config"))?;
        let networks = resp
            .network_settings
            .as_ref()
            .and_then(|n| n.networks.clone())
            .unwrap_or_default();
        Ok(SelfSpec {
            container_name,
            new_image_ref: new_image_ref.to_string(),
            old_image_id,
            config,
            host_config,
            networks,
        })
    }
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p control-server self_spec_from_inspect_projects_fields`
Expected: PASS.

- [ ] **Step 6: Add `inspect_self` + `create_and_start_from_spec`**

In `crates/control-server/src/docker.rs`, inside `impl DockerCtl` (near `create_clone_container`, after line 1043), add:

```rust
    /// Full inspect of our own container (Config + HostConfig + NetworkSettings), the input
    /// to [`SelfSpec::from_inspect`].
    pub async fn inspect_self(&self, self_id: &str) -> Result<ContainerInspectResponse> {
        self.daemon()?
            .inspect_container(self_id, None::<bollard::query_parameters::InspectContainerOptions>)
            .await
            .with_context(|| format!("inspecting self container {self_id}"))
    }

    /// Create + start a container from a captured [`SelfSpec`], reusing the container name.
    /// The caller must have already removed any container holding that name (the swap does
    /// stop→remove→create). The image is `spec.new_image_ref` (the fallback path passes a
    /// spec whose `new_image_ref` was set to the old image id). Returns the new container id.
    pub async fn create_and_start_from_spec(&self, spec: &SelfSpec) -> Result<String> {
        let c = &spec.config;
        let body = ContainerCreateBody {
            hostname: c.hostname.clone(),
            env: c.env.clone(),
            labels: c.labels.clone(),
            exposed_ports: c.exposed_ports.clone(),
            entrypoint: c.entrypoint.clone(),
            cmd: c.cmd.clone(),
            stop_signal: c.stop_signal.clone(),
            stop_timeout: c.stop_timeout,
            image: Some(spec.new_image_ref.clone()),
            host_config: Some(spec.host_config.clone()),
            networking_config: Some(NetworkingConfig {
                endpoints_config: Some(spec.networks.clone()),
            }),
            ..Default::default()
        };
        let opts = CreateContainerOptionsBuilder::new().name(&spec.container_name).build();
        let docker = self.daemon()?;
        let id = docker
            .create_container(Some(opts), body)
            .await
            .with_context(|| format!("recreating container {}", spec.container_name))?
            .id;
        docker
            .start_container(&id, None::<bollard::query_parameters::StartContainerOptions>)
            .await
            .with_context(|| format!("starting recreated container {}", spec.container_name))?;
        Ok(id)
    }
```

- [ ] **Step 7: Build**

Run: `cargo build -p control-server`
Expected: compiles. (If `ContainerCreateBody` lacks `exposed_ports`/`entrypoint`/`cmd` in bollard 0.19, drop those lines — they are optional; the mandatory ones are `image`, `host_config`, `networking_config`, `env`, `labels`, `hostname`, `stop_signal`, `stop_timeout`.)

- [ ] **Step 8: Commit**

```bash
git add crates/control-server/src/docker.rs
git commit -m "feat(docker): SelfSpec capture + recreate-from-spec primitives

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task B2: `update.rs` — handoff file, `self-upgrade` subcommand, reconcile

**Files:**
- Create: `crates/control-server/src/update.rs`
- Modify: `crates/control-server/src/main.rs` (register `mod update`, argv dispatch, reconcile wiring)
- Test: `crates/control-server/src/update.rs` (handoff round-trip)

**Interfaces:**
- Consumes: `DockerCtl::{inspect_self, create_and_start_from_spec, stop_container, remove_container, self_image_info}`, `SelfSpec`, `App`, `config::load`.
- Produces: `update::HANDOFF_PATH: &str`, `struct Handoff { spec: SelfSpec, op_id: String, target_digest: Option<String> }`, `update::write_handoff`, `update::self_upgrade_main() -> !` (the subcommand entry), `update::reconcile_pending(app: &App)` (boot).

- [ ] **Step 1: Write the failing handoff round-trip test**

Create `crates/control-server/src/update.rs` with just the type + test first:

```rust
//! Control-server self-update: the `self-upgrade` helper subcommand + boot reconcile.
//!
//! A container can't `docker rm -f` and recreate itself atomically. So `jobs::run_update`
//! pulls the new image, captures our run-spec, writes a [`Handoff`] into the `/data` volume,
//! and launches a DETACHED helper container from the NEW image running `rmng-control-server
//! self-upgrade <handoff>`. That helper ([`self_upgrade_main`]) stops+removes the old
//! container and recreates it from the new image. On the next boot the fresh server runs
//! [`reconcile_pending`] to resolve the surviving update Operation.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::docker::SelfSpec;

/// The handoff file, in the `/data` volume so it survives the container swap.
pub const HANDOFF_PATH: &str = "/data/update-handoff.json";

/// Everything the `self-upgrade` helper + the post-reboot reconcile need.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handoff {
    pub spec: SelfSpec,
    /// The `Operation` id to resolve on the next boot.
    pub op_id: String,
    /// The remote digest we pulled — reconcile compares the new running image against it.
    #[serde(default)]
    pub target_digest: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_round_trips() {
        let json = r#"{
            "Id": "abc", "Name": "/rmng", "Image": "sha256:old",
            "Config": {}, "HostConfig": {}, "NetworkSettings": { "Networks": {} }
        }"#;
        let resp: crate::docker::ContainerInspectResponse = serde_json::from_str(json).unwrap();
        let spec = SelfSpec::from_inspect(&resp, "pegasis0/rmng:latest").unwrap();
        let h = Handoff { spec, op_id: "op_1".into(), target_digest: Some("sha256:new".into()) };
        let s = serde_json::to_string(&h).unwrap();
        let back: Handoff = serde_json::from_str(&s).unwrap();
        assert_eq!(back.op_id, "op_1");
        assert_eq!(back.target_digest.as_deref(), Some("sha256:new"));
        assert_eq!(back.spec.new_image_ref, "pegasis0/rmng:latest");
    }
}
```

Note: this requires `ContainerInspectResponse` to be reachable as `crate::docker::ContainerInspectResponse`. Add a `pub use bollard::models::ContainerInspectResponse;` re-export at the top of `docker.rs` if it isn't already public, OR change the test to build the `SelfSpec` directly. Simplest: add to `docker.rs` (top level): `pub use bollard::models::ContainerInspectResponse;`.

- [ ] **Step 2: Register the module + run the failing test**

In `crates/control-server/src/main.rs`, add `mod update;` to the module list (after `mod state;`, line 22).

Run: `cargo test -p control-server handoff_round_trips`
Expected: PASS (once the `pub use` re-export is added). If it fails to find the type, add `pub use bollard::models::ContainerInspectResponse;` near the top of `docker.rs`.

- [ ] **Step 3: Add `write_handoff` + the helper subcommand**

Append to `crates/control-server/src/update.rs` (before the `#[cfg(test)]` module):

```rust
/// Serialize + write the handoff atomically (temp + rename) into the `/data` volume.
pub fn write_handoff(h: &Handoff) -> Result<()> {
    let tmp = format!("{HANDOFF_PATH}.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(h)?).context("writing handoff temp")?;
    std::fs::rename(&tmp, HANDOFF_PATH).context("renaming handoff into place")?;
    Ok(())
}

/// Remove the handoff file if present (idempotent).
pub fn clear_handoff() {
    let _ = std::fs::remove_file(HANDOFF_PATH);
}

/// The `self-upgrade` subcommand entry — a fresh process from the NEW image, launched
/// detached by `jobs::run_update`. Reads the handoff, stops+removes the old container, and
/// recreates it from the new image. On a create/start failure it recreates the OLD image so
/// the host is never left with nothing running. Runs its own tokio runtime and never returns
/// to the normal server boot (the process exits when done).
pub fn self_upgrade_main(handoff_path: &str) -> ! {
    // A tiny multi-thread runtime is enough for a handful of daemon calls.
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime for self-upgrade");
    let code = rt.block_on(async move { run_helper(handoff_path).await });
    std::process::exit(code);
}

async fn run_helper(handoff_path: &str) -> i32 {
    tracing::info!(target: "update", "self-upgrade helper starting ({handoff_path})");
    let raw = match std::fs::read(handoff_path) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(target: "update", "reading handoff {handoff_path}: {e}");
            return 1;
        }
    };
    let handoff: Handoff = match serde_json::from_slice(&raw) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(target: "update", "parsing handoff: {e}");
            return 1;
        }
    };
    // Build a bollard client from config (respects a custom docker.socket). config.json is in
    // the /data volume, which is mounted into the helper too, so config::load() works here.
    let cfg = match crate::config::load() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(target: "update", "loading config in helper: {e}");
            return 1;
        }
    };
    let docker = crate::docker::DockerCtl::connect(&cfg.docker);
    let spec = &handoff.spec;

    // Stop + remove the old container (frees the name + published ports).
    if let Err(e) = docker.stop_container(&spec.container_name).await {
        tracing::warn!(target: "update", "stopping old container: {e}");
    }
    if let Err(e) = docker.remove_container(&spec.container_name).await {
        tracing::error!(target: "update", "removing old container: {e}");
        return 1;
    }

    // Recreate from the NEW image.
    match docker.create_and_start_from_spec(spec).await {
        Ok(id) => {
            tracing::info!(target: "update", "recreated {} on new image ({})", spec.container_name, id);
            0
        }
        Err(e) => {
            tracing::error!(target: "update", "recreate on new image failed: {e} — rolling back to old image");
            // Create-error fallback: recreate from the OLD image so the host isn't bricked.
            let mut fallback = spec.clone();
            fallback.new_image_ref = spec.old_image_id.clone();
            match docker.create_and_start_from_spec(&fallback).await {
                Ok(_) => tracing::warn!(target: "update", "rolled back to old image"),
                Err(e2) => tracing::error!(target: "update", "rollback ALSO failed: {e2}"),
            }
            1
        }
    }
}
```

- [ ] **Step 4: Add `reconcile_pending`**

Append to `crates/control-server/src/update.rs` (before `#[cfg(test)]`):

```rust
/// Boot-time resolution of a surviving update Operation. Runs BEFORE `jobs::fail_stale_ops`
/// so it isn't clobbered as "interrupted". Reads the handoff, compares our new running image
/// digest to the target, marks the op Done/Error, and clears the handoff. Best-effort: a
/// missing handoff (normal boot) is a no-op; a daemon-unreachable boot completes optimistically.
pub async fn reconcile_pending(app: &App) {
    if !Path::new(HANDOFF_PATH).exists() {
        return;
    }
    let raw = match std::fs::read(HANDOFF_PATH) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: "update", "reading handoff at boot: {e}");
            clear_handoff();
            return;
        }
    };
    let handoff: Handoff = match serde_json::from_slice(&raw) {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(target: "update", "parsing handoff at boot: {e}");
            clear_handoff();
            return;
        }
    };
    let op_id = handoff.op_id.clone();
    let self_id = app.docker.env().await.self_container;
    let (repo, _tag) = crate::docker::split_reference_pub(&handoff.spec.new_image_ref);

    // Determine the running image digest (best-effort).
    let now_digest = match self_id.as_deref() {
        Some(id) => app.docker.self_image_info(id, &repo).await.ok().and_then(|i| {
            i.repo_digest.map(|rd| rd.split_once('@').map(|(_, d)| d.to_string()).unwrap_or(rd))
        }),
        None => None,
    };
    let (done, msg) = match (&handoff.target_digest, &now_digest) {
        (Some(target), Some(now)) if now == target => (true, "update complete".to_string()),
        (Some(target), Some(now)) => (
            false,
            format!("update did not take effect (running {now}, expected {target})"),
        ),
        // Couldn't verify (daemon down / no repo digest): optimistic, since this new binary
        // is running at all.
        _ => (true, "update complete (digest unverified)".to_string()),
    };

    app.store.mutate(|s| {
        if let Some(op) = s.operations.iter_mut().find(|o| o.id == op_id) {
            op.status = if done { wire::OperationStatus::Done } else { wire::OperationStatus::Error };
            op.step = "done".into();
            op.pct = 100.0;
            op.message = msg.clone();
            op.log.push(msg.clone());
            op.finished_at = Some(chrono_now_ms());
        }
    });
    tracing::info!(target: "update", "reconciled update op {op_id}: {msg}");
    clear_handoff();
    // Best-effort: remove the leftover helper container.
    let _ = app.docker.remove_container("rmng-self-upgrade").await;
}

/// Milliseconds since epoch (matches jobs.rs `now_ms`).
fn chrono_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
```

Note: `reconcile_pending` calls `crate::docker::split_reference_pub`. Add a public wrapper in `docker.rs` next to the private `split_reference` (line 1477):

```rust
/// Public wrapper over [`split_reference`] for callers outside this module (update.rs).
pub fn split_reference_pub(reference: &str) -> (String, String) {
    split_reference(reference)
}
```

- [ ] **Step 5: Wire argv dispatch + reconcile into main.rs**

In `crates/control-server/src/main.rs`, at the very top of `main()` (after the tracing init block, before `let cfg = config::load()?;` at line 40), add:

```rust
    // Self-upgrade helper mode: a detached container launched by `jobs::run_update` from the
    // NEW image runs `rmng-control-server self-upgrade <handoff>`. It swaps the container and
    // exits — it never becomes the normal server.
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("self-upgrade") {
        let handoff = argv.get(2).cloned().unwrap_or_else(|| update::HANDOFF_PATH.to_string());
        update::self_upgrade_main(&handoff); // diverges
    }
```

Then, inside the `#[tokio::main] async fn main`, add the reconcile call IMMEDIATELY BEFORE `jobs::fail_stale_ops(&app);` (line 50):

```rust
    // Resolve a surviving self-update Operation before fail_stale_ops would clobber it as
    // "interrupted". Best-effort; safe when the handoff is absent (normal boot).
    update::reconcile_pending(&app).await;
```

- [ ] **Step 6: Build + run the module tests**

Run: `cargo test -p control-server update:: && cargo build -p control-server`
Expected: tests PASS, build compiles.

- [ ] **Step 7: Commit**

```bash
git add crates/control-server/src/update.rs crates/control-server/src/main.rs crates/control-server/src/docker.rs
git commit -m "feat(update): self-upgrade helper subcommand + boot reconcile

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task B3: `OperationKind::Update` + `jobs::start_update`/`run_update`

**Files:**
- Modify: `crates/wire/src/control.rs:170-180` (`OperationKind`)
- Modify: `frontend/app/lib/types.ts:91` (`OperationKind` union)
- Modify: `frontend/app/components/OperationProgress.tsx:11-16` (`VERB` map)
- Modify: `crates/control-server/src/jobs.rs` (add `start_update`/`run_update`; extend `make_op`)
- Test: `crates/control-server/src/jobs.rs` (guard test)

**Interfaces:**
- Consumes: `DockerCtl::{pull_image, inspect_self, env}`, `SelfSpec::from_inspect`, `update::{Handoff, write_handoff, clear_handoff}`, `pull_op_progress`.
- Produces: `jobs::start_update(app: &App, reference: &str) -> Result<Operation, JobError>`.

- [ ] **Step 1: Add the `Update` variant to the wire enum**

In `crates/wire/src/control.rs`, in `enum OperationKind` (lines 170-180), add after `Commit,`:

```rust
    /// Self-update the control-server: pull a new image + swap the running container.
    Update,
```

- [ ] **Step 2: Regenerate TS + update the hand-written types + VERB map**

Run: `cargo test -p wire` (regenerates `frontend/app/lib/wire/OperationKind.ts`).

In `frontend/app/lib/types.ts` (line 91), extend the union:

```ts
export type OperationKind = "clone" | "delete" | "pull" | "commit" | "update";
```

In `frontend/app/components/OperationProgress.tsx`, add to the `VERB` map (lines 11-16):

```ts
  update: "Updating",
```

- [ ] **Step 3: Write the failing guard test**

In the `#[cfg(test)] mod tests` of `crates/control-server/src/jobs.rs`, add:

```rust
    #[tokio::test]
    async fn start_update_rejects_when_an_op_is_running() {
        let app = test_app();
        app.store.mutate(|s| s.operations.push(running_op("op_x", "some-clone")));
        let err = start_update(&app, "pegasis0/rmng:latest").unwrap_err();
        assert!(err.0.contains("in flight") || err.0.contains("already"), "got: {}", err.0);
    }
```

- [ ] **Step 4: Run it to verify it fails**

Run: `cargo test -p control-server start_update_rejects_when_an_op_is_running`
Expected: FAIL — `cannot find function start_update`.

- [ ] **Step 5: Extend `make_op` for the Update kind**

In `crates/control-server/src/jobs.rs`, in `make_op` (lines 74-82), add an arm to the `match kind`:

```rust
        OperationKind::Update => "queued control-server update".to_string(),
```

- [ ] **Step 6: Implement `start_update` + `run_update`**

In `crates/control-server/src/jobs.rs`, add (near `start_pull`):

```rust
/// Validate + register a control-server self-update op, then drive it in the background.
/// Guard: reject if ANY operation is running — the swap kills the server, which would abort
/// every in-flight clone/pull/commit. `reference` is `config.docker.serverImage`.
pub fn start_update(app: &App, reference: &str) -> Result<Operation, JobError> {
    let st = app.store.get();
    if st.operations.iter().any(|o| o.status == OperationStatus::Running) {
        return Err(JobError(
            "another operation is in flight; wait for it to finish before updating".into(),
        ));
    }
    let op = make_op(OperationKind::Update, "control-server", None);
    let (ret, op_id) = (op.clone(), op.id.clone());
    app.store.mutate(|s| s.operations.push(op));
    let (app2, reference) = (app.clone(), reference.to_string());
    tokio::spawn(async move { run_update(app2, op_id, reference).await });
    Ok(ret)
}

async fn run_update(app: App, op_id: String, reference: String) {
    // 1. Determine our own container id (can't self-update in dev mode).
    let self_id = match app.docker.env().await.self_container {
        Some(id) => id,
        None => {
            return fail_op(&app, &op_id, "not running as a container (dev mode) — nothing to update".into());
        }
    };

    // 2. Pull the new image (2–80% of the bar). patch_op writes each tick into the op; the
    //    pull callback borrows (app_cb, op_cb) and calls patch_op directly — no separate
    //    progress closure to fight the borrow checker.
    patch_op(&app, &op_id, |op| {
        op.step = "pull".into();
        op.message = format!("pulling {reference}");
    });
    {
        let (app_cb, op_cb) = (app.clone(), op_id.clone());
        let pull = app
            .docker
            .pull_image(&reference, |ev| match ev {
                crate::docker::PullEvent::Status { layer, status } => {
                    patch_op(&app_cb, &op_cb, |op| {
                        op.log.push(format!("pull: {layer}: {status}"));
                        if op.log.len() > 200 {
                            let d = op.log.len() - 200;
                            op.log.drain(0..d);
                        }
                    });
                }
                crate::docker::PullEvent::Bytes { frac } => {
                    patch_op(&app_cb, &op_cb, |op| {
                        op.pct = op.pct.max(2.0 + frac * 78.0);
                        op.message = format!("pulling {reference}: {}%", (frac * 100.0) as i64);
                    });
                }
            })
            .await;
        if let Err(e) = pull {
            return fail_op(&app, &op_id, format!("pull failed: {e:#}"));
        }
    }

    // 3. Capture our run-spec.
    patch_op(&app, &op_id, |op| {
        op.step = "capture".into();
        op.message = "capturing run-spec".into();
    });
    let resp = match app.docker.inspect_self(&self_id).await {
        Ok(r) => r,
        Err(e) => return fail_op(&app, &op_id, format!("inspecting self: {e:#}")),
    };
    let spec = match crate::docker::SelfSpec::from_inspect(&resp, &reference) {
        Ok(s) => s,
        Err(e) => return fail_op(&app, &op_id, format!("capturing run-spec: {e:#}")),
    };

    // 4. Resolve the target digest (for boot reconcile). Best-effort.
    let target_digest = app.docker.registry_digest(&reference).await.ok();

    // 5. Write the handoff + launch the detached helper from the NEW image.
    patch_op(&app, &op_id, |op| {
        op.step = "handoff".into();
        op.message = "handing off to the updater".into();
    });
    let handoff = crate::update::Handoff { spec, op_id: op_id.clone(), target_digest };
    if let Err(e) = crate::update::write_handoff(&handoff) {
        return fail_op(&app, &op_id, format!("writing handoff: {e:#}"));
    }
    let socket = app.config().docker.socket;
    if let Err(e) = app.docker.launch_upgrade_helper(&reference, &self_id, &socket).await {
        crate::update::clear_handoff();
        return fail_op(&app, &op_id, format!("launching updater: {e:#}"));
    }
    // The helper now stops us; this task dies with the container. Leave the op Running at 85%
    // — the rebooted server's reconcile_pending finalizes it.
    patch_op(&app, &op_id, |op| {
        op.pct = op.pct.max(85.0);
        op.message = "updater launched — the server will restart on the new image".into();
    });
}
```

- [ ] **Step 7: Add `launch_upgrade_helper` to docker.rs**

In `crates/control-server/src/docker.rs`, inside `impl DockerCtl` (near `create_and_start_from_spec`), add:

```rust
    /// Launch the detached `self-upgrade` helper container from `new_image` (already pulled).
    /// It mounts the docker socket + the /data volume (so it can read the handoff + config)
    /// and runs `rmng-control-server self-upgrade`. Named `rmng-self-upgrade`, NOT
    /// `rmng.managed`-labeled (ephemeral infra, kept out of managed sweeps), `network: none`,
    /// pre-cleaned. The helper outlives the old container's removal. `socket` is
    /// `config.docker.socket` — the docker.sock is bound directly (respects a custom path)
    /// rather than discovered, because Compose stores it under Mounts, not HostConfig.Binds.
    pub async fn launch_upgrade_helper(&self, new_image: &str, self_id: &str, socket: &str) -> Result<()> {
        const HELPER_NAME: &str = "rmng-self-upgrade";
        // Reclaim a leftover helper from a crashed earlier run (idempotent, 404-ok).
        let _ = self.remove_container(HELPER_NAME).await;

        // Discover our /data source (named volume or bind) so the helper reads the same
        // handoff + config. `mounts` covers BOTH compose (long-syntax → Mounts) and the
        // one-liner. docker.sock is bound directly from `socket` below.
        let me = self.inspect_self(self_id).await?;
        let mut mounts: Vec<Mount> = Vec::new();
        if let Some(ms) = me.mounts.clone() {
            for m in ms {
                if m.destination.as_deref() == Some("/data") {
                    let is_vol = m.name.is_some();
                    mounts.push(Mount {
                        target: Some("/data".to_string()),
                        source: m.name.clone().or(m.source.clone()),
                        typ: Some(if is_vol { MountTypeEnum::VOLUME } else { MountTypeEnum::BIND }),
                        ..Default::default()
                    });
                }
            }
        }

        let host_config = HostConfig {
            // docker.sock as a bind (source == target == the configured socket path).
            binds: Some(vec![format!("{socket}:{socket}")]),
            mounts: if mounts.is_empty() { None } else { Some(mounts) },
            network_mode: Some("none".to_string()),
            auto_remove: Some(false),
            ..Default::default()
        };
        let body = ContainerCreateBody {
            image: Some(new_image.to_string()),
            entrypoint: Some(vec![
                "/usr/local/bin/rmng-control-server".to_string(),
                "self-upgrade".to_string(),
                "/data/update-handoff.json".to_string(),
            ]),
            cmd: Some(Vec::new()),
            host_config: Some(host_config),
            ..Default::default()
        };
        let opts = CreateContainerOptionsBuilder::new().name(HELPER_NAME).build();
        let docker = self.daemon()?;
        let id = docker.create_container(Some(opts), body).await.context("creating self-upgrade helper")?.id;
        docker
            .start_container(&id, None::<bollard::query_parameters::StartContainerOptions>)
            .await
            .context("starting self-upgrade helper")?;
        Ok(())
    }
```

- [ ] **Step 8: Run the guard test + build + typecheck**

Run: `cargo test -p control-server start_update_rejects_when_an_op_is_running`
Expected: PASS.
Run: `cargo build -p control-server && (cd frontend && bun run typecheck)`
Expected: both succeed.

- [ ] **Step 9: Commit**

```bash
git add crates/wire/src/control.rs crates/control-server/src/jobs.rs crates/control-server/src/docker.rs frontend/app/lib/wire/ frontend/app/lib/types.ts frontend/app/components/OperationProgress.tsx
git commit -m "feat(update): OperationKind::Update + start_update job + helper launch

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task B4: `POST /api/server/update` + Update button

**Files:**
- Modify: `crates/control-server/src/web.rs` (route + handler)
- Modify: `frontend/app/lib/api.ts` (`updateServer`)
- Modify: `frontend/app/components/SettingsPanel.tsx` (Update button in the Control-server section)

**Interfaces:**
- Consumes: `jobs::start_update`, `getUpdateStatus` (for enabling).
- Produces: `POST /api/server/update` → `Json<Operation>`; `updateServer(): Promise<Operation>`.

- [ ] **Step 1: Add the route**

In `crates/control-server/src/web.rs`, `router()`, after the `/api/server/version` route (from Task A4), add:

```rust
        .route("/api/server/update", post(server_update))
```

- [ ] **Step 2: Add the handler**

In `crates/control-server/src/web.rs`, near `server_version`, add (async-Operation template, mirrors `images_pull`):

```rust
/// `POST /api/server/update` — pull `config.docker.serverImage` and swap the running
/// control-server container onto it. Returns the driving Operation (kind `update`); the
/// server restarts mid-op, and the rebooted server's reconcile finalizes it.
async fn server_update(State(app): State<App>) -> Result<Json<Operation>, (StatusCode, String)> {
    let reference = app.config().docker.server_image;
    jobs::start_update(&app, &reference)
        .map(Json)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))
}
```

- [ ] **Step 3: Add the api.ts wrapper**

In `frontend/app/lib/api.ts`, add the `Operation` import if not present (`import type { Operation } from "~/lib/wire/Operation";`) and:

```ts
/** Pull the latest control-server image and swap the running container onto it. Returns the
 *  driving Operation (kind `update`); the server restarts mid-op. */
export const updateServer = () => postJson("/api/server/update", {}) as Promise<Operation>;
```

- [ ] **Step 4: Add the Update button to the Control-server section**

In `frontend/app/components/SettingsPanel.tsx`, add `updateServer` as a prop:
- In `SettingsPanelProps` (after `getUpdateStatus`): `updateServer: () => Promise<unknown>;`
- In the destructure: `updateServer,`

Add the handler near `checkUpdate`:

```tsx
  async function doUpdate() {
    if (!confirm("Update the control-server now?\n\nIt will pull the latest image and restart itself. The UI will briefly disconnect and reconnect; running clones are unaffected.")) return;
    setServerMsg("updating… the server will restart shortly");
    try {
      await updateServer();
    } catch (e) {
      setServerMsg(`✗ ${(e as Error).message}`);
    }
  }
```

Add the button inside the Control-server section's button row (after the Check button), enabled only when an update is available:

```tsx
                  <button
                    type="button"
                    onClick={doUpdate}
                    disabled={!serverStatus?.available}
                    className="rounded bg-slate-900 px-3 py-1.5 text-xs font-medium text-white hover:bg-slate-700 disabled:opacity-50 dark:bg-slate-100 dark:text-slate-900 dark:hover:bg-white"
                  >
                    Update
                  </button>
```

- [ ] **Step 5: Thread `updateServer` from the route**

In `frontend/app/routes/_index.tsx`, add `updateServer` to the `~/lib/api` import block and pass it into `<SettingsPanel ...>`:

```tsx
          updateServer={updateServer}
```

- [ ] **Step 6: Build + typecheck**

Run: `cargo build -p control-server && (cd frontend && bun run typecheck)`
Expected: both succeed.

- [ ] **Step 7: Commit**

```bash
git add crates/control-server/src/web.rs frontend/app/lib/api.ts frontend/app/components/SettingsPanel.tsx frontend/app/routes/_index.tsx
git commit -m "feat(web): POST /api/server/update + Update button

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase C — Restart

### Task C1: `restart_self` + `POST /api/server/restart` + api client

**Files:**
- Modify: `crates/control-server/src/docker.rs` (`restart_self`)
- Modify: `crates/control-server/src/web.rs` (route + handler)
- Modify: `frontend/app/lib/api.ts` (`restartServer`)

**Interfaces:**
- Produces: `DockerCtl::restart_self(&self, self_id: &str) -> Result<()>`; `POST /api/server/restart` → `Json<serde_json::Value>`; `restartServer(): Promise<{ok: boolean}>`.

- [ ] **Step 1: Add `restart_self` to docker.rs**

In `crates/control-server/src/docker.rs`, inside `impl DockerCtl` (near `restart`/`stop_container`, ~line 1067), add:

```rust
    /// Restart our own container in place (the programmatic twin of `docker restart rmng`) —
    /// the daemon stops+starts the same container, which re-reads config.json on boot. Used
    /// to apply restart-required settings. The `--restart unless-stopped` policy is a backstop
    /// if the daemon's restart is interrupted. Uses the systemd stop timeout.
    pub async fn restart_self(&self, self_id: &str) -> Result<()> {
        let opts = bollard::query_parameters::RestartContainerOptionsBuilder::new()
            .t(STOP_TIMEOUT_SECS)
            .build();
        self.daemon()?
            .restart_container(self_id, Some(opts))
            .await
            .with_context(|| format!("restarting self container {self_id}"))?;
        Ok(())
    }
```

Note: if `RestartContainerOptionsBuilder` isn't the exact bollard 0.19 name, use `restart_container(self_id, None::<bollard::query_parameters::RestartContainerOptions>)` (no timeout) — the restart still works, just with the default timeout.

- [ ] **Step 2: Add the route**

In `crates/control-server/src/web.rs`, `router()`, after the `/api/server/update` route, add:

```rust
        .route("/api/server/restart", post(server_restart))
```

- [ ] **Step 3: Add the handler**

In `crates/control-server/src/web.rs`, near `server_update`, add:

```rust
/// `POST /api/server/restart` — restart the control-server in place to apply restart-required
/// settings (ports / sockets / static dir / chroma), re-read from config.json on boot. The
/// response is sent before the daemon tears us down; the UI reconnects when we're back.
async fn server_restart(State(app): State<App>) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let self_id = app
        .docker
        .env()
        .await
        .self_container
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "not running as a container (dev mode) — restart manually".to_string()))?;
    let docker = app.docker.clone();
    // Spawn the restart so the HTTP response flushes to the client BEFORE the daemon stops us
    // (otherwise the browser sees a dropped connection instead of {ok:true}).
    tokio::spawn(async move {
        // Small delay to let the response return.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        if let Err(e) = docker.restart_self(&self_id).await {
            tracing::error!(target: "update", "self-restart failed: {e:#}");
        }
    });
    Ok(Json(serde_json::json!({ "ok": true })))
}
```

- [ ] **Step 4: Add the api.ts wrapper**

In `frontend/app/lib/api.ts`, add:

```ts
/** Restart the control-server in place to apply changed startup settings. The UI briefly
 *  disconnects and reconnects. */
export const restartServer = () => postJson("/api/server/restart", {}) as Promise<{ ok: boolean }>;
```

- [ ] **Step 5: Build + typecheck**

Run: `cargo build -p control-server && (cd frontend && bun run typecheck)`
Expected: both succeed.

- [ ] **Step 6: Commit**

```bash
git add crates/control-server/src/docker.rs crates/control-server/src/web.rs frontend/app/lib/api.ts
git commit -m "feat(web): POST /api/server/restart (in-place docker restart self)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task C2: Replace the restart banner with a Restart button + docs

**Files:**
- Modify: `frontend/app/components/SettingsPanel.tsx` (banner region ~line 387-397; Control-server section)
- Modify: `frontend/app/routes/_index.tsx` (thread `restartServer`)
- Modify: `docs/DEPLOY.md`

**Interfaces:**
- Consumes: `restartServer` from api.ts.
- Produces: an inline **Restart control-server** button; the `docker restart rmng` text is gone.

- [ ] **Step 1: Add `restartServer` as a prop + handler**

In `frontend/app/components/SettingsPanel.tsx`:
- Add to `SettingsPanelProps`: `restartServer: () => Promise<{ ok: boolean }>;`
- Add to destructure: `restartServer,`
- Add a handler near `doUpdate`:

```tsx
  async function doRestart() {
    if (!confirm("Restart the control-server now to apply the changed settings?\n\nThe UI will briefly disconnect and reconnect; running clones are unaffected.")) return;
    setServerMsg("restarting… reconnecting shortly");
    try {
      await restartServer();
    } catch (e) {
      setServerMsg(`✗ ${(e as Error).message}`);
    }
  }
```

- [ ] **Step 2: Replace the banner with a Restart button**

In `frontend/app/components/SettingsPanel.tsx`, replace the `restartRequired` banner block (lines 393-397) with a button-carrying banner:

```tsx
        {restartRequired ? (
          <div className="mb-3 flex items-center gap-3 rounded border border-amber-300 dark:border-amber-900 bg-amber-50 dark:bg-amber-950/40 px-3 py-2 text-xs text-amber-800 dark:text-amber-400">
            <span>Changed port/socket/video settings need a restart to apply.</span>
            <button
              type="button"
              onClick={doRestart}
              className="rounded border border-amber-400 dark:border-amber-700 px-2 py-1 text-xs font-medium text-amber-800 dark:text-amber-300 hover:bg-amber-100 dark:hover:bg-amber-900/40"
            >
              Restart control-server
            </button>
          </div>
        ) : null}
```

Also add a **Restart** button to the Control-server section's button row (after Update), so restart is reachable even without a pending change:

```tsx
                  <button
                    type="button"
                    onClick={doRestart}
                    className="rounded border border-slate-300 dark:border-slate-600 px-2.5 py-1.5 text-xs text-slate-600 dark:text-slate-300 hover:bg-slate-50 dark:hover:bg-slate-800"
                  >
                    Restart
                  </button>
```

- [ ] **Step 3: Thread `restartServer` from the route**

In `frontend/app/routes/_index.tsx`, add `restartServer` to the `~/lib/api` import block and pass it to `<SettingsPanel ...>`:

```tsx
          restartServer={restartServer}
```

- [ ] **Step 4: Update DEPLOY.md**

In `docs/DEPLOY.md`, in the "Upgrades" section (~line 194), add a note:

```markdown
### In-product restart & update (Docker deployment)

Once the control-server is running a build that includes the self-update feature, its
Settings page has **Restart control-server** and **Update** buttons:

- **Restart** does an in-place `docker restart` of the control-server container (applies
  changed port/socket/static-dir/chroma settings, re-read from config.json on boot). It does
  NOT change the container's host-published port mapping — a `listen` port moved outside the
  published `9000-9003` range still needs a host-level recreate.
- **Update** pulls `docker.serverImage` (default `pegasis0/rmng:latest`) and swaps the
  running container onto it via a detached helper. Running clones and the data volumes
  survive.

**First update is manual.** A server that predates this feature has no update code path, so
the first hop onto a feature-bearing image is still the manual `docker pull … && docker rm -f
… && docker run …` above. Every update after that is in-product.

Publish a new control-server image with `scripts/publish-server.sh` (tags `:YYYYMMDD` +
`:latest`, stamps the version labels the UI reads).
```

- [ ] **Step 5: Typecheck**

Run: `(cd frontend && bun run typecheck)`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add frontend/app/components/SettingsPanel.tsx frontend/app/routes/_index.tsx docs/DEPLOY.md
git commit -m "feat(frontend): restart-control-server button replaces docker-restart banner

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Phase D — End-to-end verification

### Task D1: Full E2E on a fresh LXC CT (executed by the assistant)

This task is executed by the assistant directly (not a context-less engineer), per the user's instruction: create a fresh LXC container with nested Docker, deploy the built image, and verify restart + update end-to-end. Node-specific values (template, storage, bridge, a free CTID) are discovered live.

**Preconditions:** Proxmox host reachable at `root@10.0.0.100` (`pct`), and local Docker logged into `pegasis0` on Docker Hub.

- [ ] **Step 1: Confirm access + pick a free CTID**

Run:
```bash
ssh root@10.0.0.100 'pct list; echo ---; pveam list local 2>/dev/null | tail; echo ---; pvesm status'
```
Expected: lists CTs (pick an unused id, e.g. 130), an Ubuntu/Debian template path, and a storage (e.g. `local-lvm`). Note the bridge from an existing CT (`pct config <existing>` → `net0`).

- [ ] **Step 2: Create a privileged, nesting+keyctl CT**

Run (substitute discovered template / storage / bridge / CTID):
```bash
ssh root@10.0.0.100 'pct create 130 <local:vztmpl/ubuntu-24.04-...tar.zst> \
  --hostname rmng-upd-test --cores 4 --memory 4096 --swap 2048 \
  --rootfs <local-lvm>:20 --net0 name=eth0,bridge=<vmbr0>,ip=dhcp \
  --features nesting=1,keyctl=1 --unprivileged 0 --onboot 0 && pct start 130'
```
Expected: CT created + started. (Privileged + `nesting=1,keyctl=1` are what nested Docker + its keyring need — see the docker-port E2E notes.)

- [ ] **Step 3: Install Docker inside the CT**

Run:
```bash
ssh root@10.0.0.100 'pct exec 130 -- bash -lc "apt-get update && apt-get install -y ca-certificates curl && install -m0755 -d /etc/apt/keyrings && curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc && echo deb [arch=amd64 signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu \$(. /etc/os-release; echo \$VERSION_CODENAME) stable > /etc/apt/sources.list.d/docker.list && apt-get update && apt-get install -y docker-ce docker-ce-cli containerd.io && docker run --rm hello-world"'
```
Expected: `hello-world` runs — confirms nested Docker works. (If it fails on keyring, raise host sysctls: `ssh root@10.0.0.100 'sysctl -w kernel.keys.maxkeys=2000 kernel.keys.maxbytes=2000000'` and retry.)

- [ ] **Step 4: Build v1 (feature) + push to a test tag**

On the local machine, build the current branch and push a dedicated E2E tag (NOT `:latest`, to avoid clobbering the real published image):
```bash
cd /home/pegasis/Projects/RMNG
SERVER_REPO=pegasis0/rmng scripts/publish-server.sh 2>/dev/null || true   # sanity: script runs
docker build --build-arg GIT_SHA=e2e-v1 --build-arg BUILD_DATE=2026-07-03T00:00:00Z -t pegasis0/rmng:e2e .
docker push pegasis0/rmng:e2e
```
Expected: `pegasis0/rmng:e2e` pushed (v1).

- [ ] **Step 5: Run the control-server (v1) in the CT with serverImage pinned to the e2e tag**

Run the container via the documented one-liner, then set `docker.serverImage` to `pegasis0/rmng:e2e` (via config.json or the wizard). Since the no-env invariant means config lives in the `rmng-data` volume, write it after first boot:
```bash
ssh root@10.0.0.100 'pct exec 130 -- bash -lc "docker pull pegasis0/rmng:e2e && docker run -d --name rmng --privileged --init --pid host --restart unless-stopped -v /var/run/docker.sock:/var/run/docker.sock -v rmng-data:/data -v rmng-sock:/srv/rmng-sock -p 9000-9003:9000-9003 pegasis0/rmng:e2e && sleep 5 && curl -s -X PUT localhost:9000/api/config -H content-type:application/json -d \"{\\\"docker\\\":{\\\"serverImage\\\":\\\"pegasis0/rmng:e2e\\\"}}\""'
```
Expected: container up; `GET localhost:9000/api/server/version` returns JSON with `currentRevision: "e2e-v1"`.

Verify version endpoint:
```bash
ssh root@10.0.0.100 'pct exec 130 -- curl -s localhost:9000/api/server/version'
```
Expected: `{"currentRevision":"e2e-v1",..., "available":false, "reference":"pegasis0/rmng:e2e", ...}`.

- [ ] **Step 6: Test RESTART**

Change a restart-required setting and restart in place:
```bash
ssh root@10.0.0.100 'pct exec 130 -- bash -lc "
  BEFORE=\$(docker inspect -f {{.Id}} rmng)
  curl -s -X PUT localhost:9000/api/config -H content-type:application/json -d \"{\\\"chroma\\\":\\\"cpu\\\"}\" >/dev/null
  curl -s -X POST localhost:9000/api/server/restart
  sleep 8
  AFTER=\$(docker inspect -f {{.Id}} rmng)
  echo container-id-before=\$BEFORE after=\$AFTER
  curl -s localhost:9000/api/config | grep -o chroma..[a-z]*"'
```
Expected: `{"ok":true}` returned; the container id is UNCHANGED (in-place restart keeps the same container); the server is back up and reports the changed `chroma`. (Adjust the config key to a valid restart-required value for the current schema.)

- [ ] **Step 7: Build v2 + push over the e2e tag**

```bash
cd /home/pegasis/Projects/RMNG
docker build --build-arg GIT_SHA=e2e-v2 --build-arg BUILD_DATE=2026-07-04T00:00:00Z -t pegasis0/rmng:e2e .
docker push pegasis0/rmng:e2e
```
Expected: `pegasis0/rmng:e2e` now points at v2 (different digest).

- [ ] **Step 8: Verify detection flips to "update available"**

```bash
ssh root@10.0.0.100 'pct exec 130 -- curl -s localhost:9000/api/server/version'
```
Expected: `available: true`, `remoteDigest` != the running digest.

- [ ] **Step 9: Test UPDATE end-to-end**

```bash
ssh root@10.0.0.100 'pct exec 130 -- bash -lc "
  BEFORE=\$(docker inspect -f {{.Id}} rmng)
  curl -s -X POST localhost:9000/api/server/update
  echo waiting for swap...
  for i in \$(seq 1 60); do sleep 3; NEW=\$(docker inspect -f {{.Id}} rmng 2>/dev/null); [ -n \"\$NEW\" ] && [ \"\$NEW\" != \"\$BEFORE\" ] && break; done
  echo before=\$BEFORE after=\$(docker inspect -f {{.Id}} rmng)
  sleep 5
  curl -s localhost:9000/api/server/version | grep -o currentRevision..[a-z0-9-]*
  docker ps -a | grep rmng-self-upgrade || echo helper-cleaned-up
  curl -s localhost:9000/events -m 2 | head -c 400"'
```
Expected: the container id CHANGES (recreated on v2); after reconnect, `currentRevision` is `e2e-v2`; the `rmng-self-upgrade` helper is gone; and the surviving update Operation is resolved (its `status` is `done` with message "update complete" in the `/events` snapshot).

- [ ] **Step 10: (Optional) verify clone survival**

If a clone was created before the update (via `POST /api/clone` with a pulled template), confirm the clone container is still `running` after the swap (`docker ps | grep <clone-id>`), demonstrating the fleet is unaffected.

- [ ] **Step 11: Tear down**

```bash
ssh root@10.0.0.100 'pct stop 130 && pct destroy 130'
docker rmi pegasis0/rmng:e2e 2>/dev/null || true
```
Expected: CT destroyed; the throwaway e2e tag can be deleted from Docker Hub via the web UI if desired.

- [ ] **Step 12: Record results**

Summarize the E2E outcome (restart kept the container id + applied the setting; update swapped the container id + flipped the version + resolved the op + left clones running) back to the user. If anything failed, capture the container logs (`docker logs rmng`, `docker logs rmng-self-upgrade`) before teardown and report.

---

## Self-Review Notes

- **Spec coverage:** publish pipeline (A1), version detection + `serverImage` (A2/A3/A4/A5), self-update swap via detached `self-upgrade` helper (B1/B2/B3/B4), create-error fallback (B2 `run_helper`), boot reconcile before `fail_stale_ops` (B2), guard-all-ops (B3), restart-in-place + banner replacement (C1/C2), first-update-manual + published-port caveat docs (C2), E2E on fresh LXC CT (D1). All spec sections map to a task.
- **Marker mechanism refinement vs spec:** the spec described a `pending_update` marker in `state.json`; this plan uses the `/data/update-handoff.json` file as the single marker (it already must persist the spec + op id), avoiding a `ControlState` wire change. Functionally equivalent and simpler.
- **Type consistency:** `UpdateStatus` fields (camelCase in TS: `currentRevision`, `currentCreated`, `available`, `remoteDigest`) are used consistently across A3/A4/A5. `SelfSpec` / `Handoff` names match across B1/B2/B3. `OperationKind::Update` → TS `"update"` → `VERB.update` are all added in B3.
- **bollard API caveats flagged inline:** `inspect_registry_image` return-field access (A3 step 8), `ContainerCreateBody` optional fields (B1 step 7), `RestartContainerOptionsBuilder` name (C1 step 1) — each has a documented fallback.
