//! Codex (OpenAI/ChatGPT) accounts — the sibling of `claude.rs`. Same server-owned
//! single-token model: the server holds each account's OAuth pair in the 0600 store
//! `codex-accounts.json`, refreshes access tokens itself (expiry decoded from the
//! access-token JWT — the Codex OAuth response has no `expires_in`), injects only the
//! short-lived access + id token + account_id into a clone's `~/.codex/auth.json` with an
//! empty refresh token, and re-pushes on every rotation. Importing harvests the OAuth
//! triple from a clone already signed in to Codex via ChatGPT, then clears the clone's
//! auth.json so its CLI can never rotate the refresh token the server now owns.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use wire::{ClaudeSpend, ClaudeUsage, ClaudeUsageWindow, CloneGroup, Host};

use crate::app::App;
// TODO(task-8/9): consumed by usage + scoring
#[allow(unused_imports)]
use crate::clone_ops::{extract_json, jwt_claims, now_ms, rand_u64, run_clone_op, shuffle, snippet};

// TODO(task-8/9): consumed by usage + scoring
#[allow(dead_code)]
const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
#[allow(dead_code)]
const OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
#[allow(dead_code)]
const OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Refresh an access token this far before its expiry (must exceed the worst-case
/// poll gap). Matches claude's lead.
#[allow(dead_code)]
const REFRESH_LEAD_MS: i64 = 60 * 60 * 1000;
#[allow(dead_code)]
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
#[allow(dead_code)]
const STAGGER: Duration = Duration::from_millis(400);

// scoring knobs — copied from claude.rs (identical semantics).
#[allow(dead_code)]
const SESSION_HEADROOM_PCT: f64 = 40.0;
#[allow(dead_code)]
const SEVEN_DAY_CAP_PCT: f64 = 95.0;
#[allow(dead_code)]
const ROTATE_MAX_FIVE_HOUR_PCT: f64 = 90.0;
#[allow(dead_code)]
const ROTATE_SECS: u64 = 600;

const IMPORT_SCRIPT: &str = include_str!("../scripts/codex-import.sh");

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredCodexAccount {
    /// `codex:<account_id>`.
    pub id: String,
    pub email: String,
    pub account_id: String,
    #[serde(default)]
    pub plan: String,
    #[serde(default)]
    pub active: bool,
    pub access_token: String,
    #[serde(default)]
    pub id_token: String,
    pub refresh_token: String,
    #[serde(default)]
    pub expires_at: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct AccountsFile {
    #[serde(default)]
    accounts: Vec<StoredCodexAccount>,
}

pub struct CodexStore {
    accounts: Mutex<Vec<StoredCodexAccount>>,
    last_good: Mutex<HashMap<String, ClaudeUsage>>,
    path: PathBuf,
    polling: Mutex<bool>,
    refresh_gate: tokio::sync::Mutex<()>,
    pushed: Mutex<HashMap<String, String>>,
}

impl CodexStore {
    pub fn load(data_dir: &str) -> Self {
        let path = std::env::var("RMNG_CODEX_ACCOUNTS_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| Path::new(data_dir).join("codex-accounts.json"));
        let accounts = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<AccountsFile>(&s).ok())
            .map(|f| f.accounts)
            .unwrap_or_default();
        Self {
            accounts: Mutex::new(accounts),
            last_good: Mutex::new(HashMap::new()),
            path,
            polling: Mutex::new(false),
            refresh_gate: tokio::sync::Mutex::new(()),
            pushed: Mutex::new(HashMap::new()),
        }
    }

    fn save(&self, accounts: &[StoredCodexAccount]) -> Result<()> {
        if let Some(d) = self.path.parent() {
            std::fs::create_dir_all(d).ok();
        }
        let tmp = self.path.with_extension(format!("tmp.{}", std::process::id()));
        let body =
            serde_json::to_string_pretty(&AccountsFile { accounts: accounts.to_vec() })? + "\n";
        std::fs::write(&tmp, body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).ok();
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    fn snapshot(&self) -> Vec<StoredCodexAccount> {
        self.accounts.lock().unwrap().clone()
    }

    fn get_by_email(&self, email: &str) -> Option<StoredCodexAccount> {
        self.accounts.lock().unwrap().iter().find(|a| a.email == email).cloned()
    }

    fn emails(&self) -> Vec<String> {
        self.accounts.lock().unwrap().iter().map(|a| a.email.clone()).collect()
    }

    fn update_account(&self, acct: &StoredCodexAccount) -> Result<()> {
        let mut accounts = self.accounts.lock().unwrap();
        match accounts.iter_mut().find(|a| a.id == acct.id) {
            Some(existing) => *existing = acct.clone(),
            None => accounts.push(acct.clone()),
        }
        self.save(&accounts)
    }

    pub fn forget_pushed(&self, host_id: &str) {
        self.pushed.lock().unwrap().remove(host_id);
    }
}

// --- import from a signed-in clone ----------------------------------------

/// The on-disk `~/.codex/auth.json` shape.
#[derive(Deserialize)]
struct CodexAuthFile {
    #[serde(rename = "OPENAI_API_KEY", default)]
    openai_api_key: Option<String>,
    #[serde(default)]
    tokens: Option<CodexTokens>,
}

#[derive(Deserialize)]
struct CodexTokens {
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
}

/// The identity + tokens harvested from a signed-in clone.
pub struct CodexAuth {
    pub email: String,
    pub plan: String,
    pub account_id: String,
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
}

pub struct ImportResult {
    pub email: String,
    pub cleared: bool,
}

/// Parse + validate a `~/.codex/auth.json` body into a [`CodexAuth`]. Requires a
/// ChatGPT login (`OPENAI_API_KEY` null/absent) with a full `tokens` block, and decodes
/// the id_token JWT for email / plan / account_id (account_id falls back to the JWT claim
/// when absent from `tokens`).
fn parse_codex_auth(raw: &str) -> Result<CodexAuth> {
    let file: CodexAuthFile = serde_json::from_str(extract_json(raw))
        .map_err(|_| anyhow::anyhow!("couldn't parse ~/.codex/auth.json"))?;
    if file.openai_api_key.as_deref().is_some_and(|k| !k.is_empty()) {
        bail!("this clone is signed in to Codex with an API key, not a ChatGPT subscription");
    }
    let tokens = file.tokens.context("~/.codex/auth.json has no tokens block (not signed in?)")?;
    let (Some(id_token), Some(access_token), Some(refresh_token)) =
        (tokens.id_token, tokens.access_token, tokens.refresh_token)
    else {
        bail!("~/.codex/auth.json is missing its id/access/refresh tokens");
    };
    let claims = jwt_claims(&id_token).context("codex id_token is not a decodable JWT")?;
    let auth_ns = claims.get("https://api.openai.com/auth");
    let email = claims
        .get("email")
        .and_then(|v| v.as_str())
        .context("codex id_token has no email claim")?
        .to_string();
    let plan = auth_ns
        .and_then(|a| a.get("chatgpt_plan_type"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let account_id = tokens
        .account_id
        .filter(|s| !s.is_empty())
        .or_else(|| {
            auth_ns
                .and_then(|a| a.get("chatgpt_account_id"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .context("codex auth has no account_id (in tokens or id_token claim)")?;
    Ok(CodexAuth { email, plan, account_id, id_token, access_token, refresh_token })
}

/// Confirm clone `host` is signed in to Codex via ChatGPT and return its identity +
/// tokens. Reads `~/.codex/auth.json` (codex has no clean JSON `login status`).
pub async fn check_clone_auth(app: &App, host: &Host) -> Result<CodexAuth> {
    if !host.managed {
        bail!("host '{}' is not a managed clone; only clones can be imported", host.id);
    }
    let raw = run_clone_op(app, &host.id, IMPORT_SCRIPT, "status", &[]).await?;
    parse_codex_auth(&raw).map_err(|e| {
        anyhow::anyhow!("{e} — is codex installed and signed in on '{}'?", host.id)
    })
}

/// Import a Codex account from a signed-in clone: harvest the OAuth triple, upsert into
/// the 0600 store (by id), then delete the clone's auth.json so its CLI can't rotate the
/// refresh token the server now owns.
pub async fn import_clone_account(app: &App, host: &Host) -> Result<ImportResult> {
    if !host.managed {
        bail!("host '{}' is not a managed clone; only clones can be imported", host.id);
    }
    let auth = check_clone_auth(app, host).await?;
    let stored = StoredCodexAccount {
        id: format!("codex:{}", auth.account_id),
        email: auth.email.clone(),
        account_id: auth.account_id,
        plan: auth.plan,
        active: false,
        access_token: auth.access_token,
        id_token: auth.id_token,
        refresh_token: auth.refresh_token,
        expires_at: 0,
    };
    {
        let mut accts = app.codex.accounts.lock().unwrap();
        let mut by_id: HashMap<String, StoredCodexAccount> =
            accts.drain(..).map(|a| (a.id.clone(), a)).collect();
        by_id.insert(stored.id.clone(), stored);
        let mut next: Vec<_> = by_id.into_values().collect();
        next.sort_by(|a, b| a.email.cmp(&b.email));
        app.codex.save(&next)?;
        *accts = next;
    }
    let cleared = match run_clone_op(app, &host.id, IMPORT_SCRIPT, "clear", &[]).await {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("codex import: clearing '{}' auth.json failed: {e}", host.id);
            false
        }
    };
    app.codex.forget_pushed(&host.id);
    tracing::info!("imported Codex account {} from '{}' (cleared={cleared})", auth.email, host.id);
    Ok(ImportResult { email: auth.email, cleared })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt_with(payload: &str) -> String {
        let b64 = crate::provision::b64_encode(payload.as_bytes());
        let url = b64.trim_end_matches('=').replace('+', "-").replace('/', "_");
        format!("eyJhbGciOiJub25lIn0.{url}.sig")
    }

    fn sample_account() -> StoredCodexAccount {
        StoredCodexAccount {
            id: "codex:acc-1".into(),
            email: "z@openai.com".into(),
            account_id: "acc-1".into(),
            plan: "plus".into(),
            active: false,
            access_token: "eyJaccess".into(),
            id_token: "eyJid".into(),
            refresh_token: "rt-1".into(),
            expires_at: 0,
        }
    }

    #[test]
    fn parses_codex_auth_identity() {
        let id_token = jwt_with(
            r#"{"email":"z@openai.com","exp":2000000000,"https://api.openai.com/auth":{"chatgpt_plan_type":"plus","chatgpt_account_id":"acc-1"}}"#,
        );
        let file = format!(
            r#"{{"OPENAI_API_KEY":null,"tokens":{{"id_token":"{id_token}","access_token":"eyJaccess","refresh_token":"rt-1","account_id":"acc-1"}},"last_refresh":"2026-07-01T00:00:00Z"}}"#
        );
        let auth = parse_codex_auth(&file).unwrap();
        assert_eq!(auth.email, "z@openai.com");
        assert_eq!(auth.plan, "plus");
        assert_eq!(auth.account_id, "acc-1");
        assert_eq!(auth.refresh_token, "rt-1");
        assert_eq!(auth.access_token, "eyJaccess");
    }

    #[test]
    fn rejects_api_key_login() {
        // A codex CLI signed in with an API key has OPENAI_API_KEY set — not importable.
        let file = r#"{"OPENAI_API_KEY":"sk-proj-xxx","tokens":null}"#;
        assert!(parse_codex_auth(file).is_err());
        // Missing tokens block is also an error.
        assert!(parse_codex_auth(r#"{"OPENAI_API_KEY":null}"#).is_err());
    }

    #[test]
    fn store_upsert_roundtrip() {
        let dir = std::env::temp_dir().join(format!("rmng-codex-store-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = CodexStore::load(dir.to_str().unwrap());
        store.update_account(&sample_account()).unwrap();
        // Second store loading the same file sees the account.
        let reloaded = CodexStore::load(dir.to_str().unwrap());
        assert_eq!(reloaded.emails(), vec!["z@openai.com".to_string()]);
        assert_eq!(reloaded.get_by_email("z@openai.com").unwrap().account_id, "acc-1");
        std::fs::remove_dir_all(&dir).ok();
    }
}
