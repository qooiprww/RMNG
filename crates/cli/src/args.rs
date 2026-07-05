//! The clap command tree. Fleet management only — driving the agents *inside*
//! clones is the desktop MCP's job (computer use), and code moves via git.

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "rmng",
    version,
    about = "Fleet management for the RMNG control-server",
    long_about = "Fleet management for the RMNG control-server.\n\n\
                  Server resolution: --server, else $RMNG_CONTROL_URL (preset in every \
                  clone), else http://localhost:9000."
)]
pub struct Cli {
    /// Control-server web-API origin (e.g. http://rmng-control:9000)
    #[arg(long, global = true, value_name = "URL")]
    pub server: Option<String>,
    /// Emit the raw wire JSON instead of a table
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List hosts with their fleet status (agent report, accounts, note)
    Ps,
    /// Point the operator's viewer at a host (`none` clears the selection)
    Select {
        /// Host id, or `none`
        host: String,
    },
    /// Create a clone under an exact hostname
    Clone {
        /// Clone-source image reference (see `rmng image ls`)
        #[arg(long)]
        image: String,
        /// Exact hostname for the new clone (DNS label)
        #[arg(long)]
        hostname: String,
        /// Claude account selection: email, `auto`, `group:<name>`, or `none`
        #[arg(long)]
        claude: Option<String>,
        /// Codex account selection: email, `auto`, `group:<name>`, or `none`
        #[arg(long)]
        codex: Option<String>,
        /// Env preset name (optional; fleet workers usually need none)
        #[arg(long)]
        preset: Option<String>,
        #[command(flatten)]
        wait: WaitArgs,
    },
    /// Destroy a clone (asks for confirmation unless --yes)
    Rm {
        /// Host id
        host: String,
        /// Skip the confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
        #[command(flatten)]
        wait: WaitArgs,
    },
    /// Clone-source image operations
    #[command(subcommand)]
    Image(ImageCmd),
    /// Claude / Codex account operations
    #[command(subcommand)]
    Account(AccountCmd),
    /// List operations (clone / delete / pull / commit / update)
    Ops,
    /// Wait for an operation to reach a terminal state
    Wait {
        /// Operation id (as printed by clone/rm/image commands)
        op_id: String,
        /// Give up after this many seconds
        #[arg(long, default_value_t = 600)]
        timeout: u64,
    },
    /// Print the ready-to-paste `ssh` command for a clone (jump via the control-server bastion)
    Ssh {
        /// Host id of the clone
        host: String,
    },
}

/// `--wait [--timeout N]` shared by the operation-starting commands.
#[derive(Args, Debug)]
pub struct WaitArgs {
    /// Block until the operation finishes (rides the /events SSE stream)
    #[arg(long)]
    pub wait: bool,
    /// Seconds to wait before giving up (with --wait)
    #[arg(long, default_value_t = 600)]
    pub timeout: u64,
}

#[derive(Subcommand, Debug)]
pub enum ImageCmd {
    /// List clone-source images
    Ls,
    /// Pull the clone template from a registry (default: the configured reference)
    Pull {
        /// Registry reference (e.g. pegasis0/rmng-template:latest)
        reference: Option<String>,
        #[command(flatten)]
        wait: WaitArgs,
    },
    /// Commit a running clone to a new clone-source image `<name>:latest`
    Commit {
        /// Host id of the clone to commit
        host: String,
        /// Image name (DNS label; becomes the repo of `<name>:latest`)
        name: String,
        #[command(flatten)]
        wait: WaitArgs,
    },
    /// Remove a clone-source image (fails while clones use it)
    Rm {
        /// Image reference or id
        reference: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum AccountCmd {
    /// List imported accounts with usage windows (both providers by default)
    Ls {
        /// Only Claude accounts
        #[arg(long, conflicts_with = "codex")]
        claude: bool,
        /// Only Codex accounts
        #[arg(long)]
        codex: bool,
    },
    /// Hot-swap a clone's account (Claude by default; --codex for Codex)
    Swap {
        /// Host id
        host: String,
        /// email, `auto`, `group:<name>`, or `none`
        account: String,
        /// Swap the Codex account instead of Claude
        #[arg(long)]
        codex: bool,
    },
}

/// `--server` > `$RMNG_CONTROL_URL` > localhost default.
pub fn resolve_server(flag: Option<String>, env: Option<String>) -> String {
    flag.filter(|s| !s.trim().is_empty())
        .or(env.filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(|| "http://localhost:9000".to_string())
        .trim_end_matches('/')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_ps() {
        let cli = Cli::parse_from(["rmng", "ps"]);
        assert!(matches!(cli.cmd, Cmd::Ps));
        assert!(!cli.json);
    }

    #[test]
    fn parses_clone_with_accounts_and_wait() {
        let cli = Cli::parse_from([
            "rmng",
            "clone",
            "--image",
            "hyperhost-worker:latest",
            "--hostname",
            "w-cp-claude",
            "--claude",
            "auto",
            "--codex",
            "none",
            "--wait",
            "--timeout",
            "120",
        ]);
        match cli.cmd {
            Cmd::Clone {
                image,
                hostname,
                claude,
                codex,
                preset,
                wait,
            } => {
                assert_eq!(image, "hyperhost-worker:latest");
                assert_eq!(hostname, "w-cp-claude");
                assert_eq!(claude.as_deref(), Some("auto"));
                assert_eq!(codex.as_deref(), Some("none"));
                assert_eq!(preset, None);
                assert!(wait.wait);
                assert_eq!(wait.timeout, 120);
            }
            other => panic!("wrong cmd: {other:?}"),
        }
    }

    #[test]
    fn global_flags_work_after_subcommand() {
        let cli = Cli::parse_from(["rmng", "ps", "--json", "--server", "http://x:9000"]);
        assert!(cli.json);
        assert_eq!(cli.server.as_deref(), Some("http://x:9000"));
    }

    #[test]
    fn account_ls_provider_flags_conflict() {
        assert!(Cli::try_parse_from(["rmng", "account", "ls", "--claude", "--codex"]).is_err());
    }

    #[test]
    fn rm_requires_host() {
        assert!(Cli::try_parse_from(["rmng", "rm"]).is_err());
    }

    #[test]
    fn server_resolution_precedence() {
        assert_eq!(
            resolve_server(Some("http://flag:1/".into()), Some("http://env:2".into())),
            "http://flag:1"
        );
        assert_eq!(
            resolve_server(None, Some("http://env:2".into())),
            "http://env:2"
        );
        assert_eq!(resolve_server(None, None), "http://localhost:9000");
        // Blank values fall through rather than producing an empty base URL.
        assert_eq!(
            resolve_server(Some("  ".into()), None),
            "http://localhost:9000"
        );
    }
}
