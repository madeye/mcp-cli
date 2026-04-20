mod claude_code;
mod codex;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "mcp-cli",
    about = "Installer for the mcp-cli sidecar daemon + MCP stdio bridge",
    version
)]
struct Args {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Register the MCP bridge with agent(s) so new sessions pick it up.
    Install {
        #[arg(long, value_enum, default_value_t = Target::All)]
        target: Target,

        /// Override the bridge binary path. Defaults to the
        /// `mcp-cli-bridge` binary next to this installer.
        #[arg(long)]
        bridge_path: Option<PathBuf>,

        /// Skip writing; just show what would change.
        #[arg(long, default_value_t = false)]
        dry_run: bool,

        /// Bias the agent toward the mcp-cli MCP tools by also
        /// disabling the agent's built-in shell tool, so cat / grep /
        /// git / etc. invocations have to go through MCP. Currently
        /// only honoured by the `codex` target — sets
        /// `[features] shell_tool = false` in `~/.codex/config.toml`.
        /// Without this, codex will mount mcp-cli but still prefer
        /// Bash for everything (which is what the M5 benchmark
        /// surfaced).
        #[arg(long, default_value_t = false)]
        prefer_mcp: bool,
    },
    /// Remove the mcp-cli registration from agent(s).
    Uninstall {
        #[arg(long, value_enum, default_value_t = Target::All)]
        target: Target,

        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Print whether each target currently has mcp-cli registered.
    Status {
        #[arg(long, value_enum, default_value_t = Target::All)]
        target: Target,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum Target {
    ClaudeCode,
    Codex,
    All,
}

impl Target {
    pub fn iter(self) -> impl Iterator<Item = Target> {
        match self {
            Target::All => vec![Target::ClaudeCode, Target::Codex].into_iter(),
            t => vec![t].into_iter(),
        }
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();
    match args.cmd {
        Command::Install {
            target,
            bridge_path,
            dry_run,
            prefer_mcp,
        } => {
            let bridge = resolve_bridge_path(bridge_path)?;
            let opts = InstallOpts {
                bridge: &bridge,
                dry_run,
                prefer_mcp,
            };
            run_each(target, |t| match t {
                Target::ClaudeCode => claude_code::install(&opts),
                Target::Codex => codex::install(&opts),
                Target::All => unreachable!(),
            })
        }
        Command::Uninstall { target, dry_run } => run_each(target, |t| match t {
            Target::ClaudeCode => claude_code::uninstall(dry_run),
            Target::Codex => codex::uninstall(dry_run),
            Target::All => unreachable!(),
        }),
        Command::Status { target } => run_each(target, |t| match t {
            Target::ClaudeCode => claude_code::status(),
            Target::Codex => codex::status(),
            Target::All => unreachable!(),
        }),
    }
}

pub struct InstallOpts<'a> {
    pub bridge: &'a std::path::Path,
    pub dry_run: bool,
    /// When true, install also configures the target agent to prefer
    /// the mcp-cli MCP tools over its own built-in shell. See the
    /// `--prefer-mcp` flag documentation on `Install` for details.
    pub prefer_mcp: bool,
}

fn run_each<F>(target: Target, mut f: F) -> Result<()>
where
    F: FnMut(Target) -> Result<()>,
{
    let mut first_err: Option<anyhow::Error> = None;
    for t in target.iter() {
        if let Err(e) = f(t) {
            eprintln!("{}: {:#}", target_name(t), e);
            if first_err.is_none() {
                first_err = Some(e);
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

pub fn target_name(t: Target) -> &'static str {
    match t {
        Target::ClaudeCode => "claude-code",
        Target::Codex => "codex",
        Target::All => "all",
    }
}

/// Resolve the bridge binary: explicit flag wins; otherwise look next to
/// the installer itself; otherwise fall back to the PATH.
fn resolve_bridge_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        let canon = p
            .canonicalize()
            .with_context(|| format!("canonicalize {}", p.display()))?;
        return Ok(canon);
    }
    let self_exe = std::env::current_exe().context("resolving current_exe")?;
    if let Some(parent) = self_exe.parent() {
        let candidate = parent.join(bin_name("mcp-cli-bridge"));
        if candidate.exists() {
            return candidate
                .canonicalize()
                .with_context(|| format!("canonicalize {}", candidate.display()));
        }
    }
    // Last resort: hope it's on PATH. Agents don't need a pre-resolved
    // path for Claude Code's `claude mcp add`, but Codex's config takes
    // a command string verbatim — so warn the user.
    eprintln!(
        "warning: mcp-cli-bridge not found next to mcp-cli; falling back to bare name. \
         Agents must find it on PATH or you should pass --bridge-path."
    );
    Ok(PathBuf::from(bin_name("mcp-cli-bridge")))
}

fn bin_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_string()
    }
}
