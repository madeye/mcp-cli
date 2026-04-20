//! Claude Code registration via the `claude mcp` CLI.
//!
//! We prefer shelling out to the official CLI rather than editing
//! Claude Code's JSON config directly — the CLI owns the on-disk schema
//! and will keep working across versions that shuffle config fields.

use std::process::Command;

use anyhow::{Context, Result};

use crate::InstallOpts;

const SERVER_NAME: &str = "mcp-cli";

pub fn install(opts: &InstallOpts<'_>) -> Result<()> {
    let claude = match find_claude_cli()? {
        Some(path) => path,
        None => {
            eprintln!("claude-code: `claude` CLI not found on PATH. Skipping.");
            eprintln!(
                "  To register manually, run: claude mcp add {SERVER_NAME} {}",
                opts.bridge.display()
            );
            return Ok(());
        }
    };

    if is_registered(&claude)? {
        println!("claude-code: {SERVER_NAME} already registered — nothing to do.");
        return Ok(());
    }

    if opts.dry_run {
        println!(
            "claude-code: would run `{} mcp add {SERVER_NAME} {}`",
            claude,
            opts.bridge.display()
        );
        return Ok(());
    }

    let status = Command::new(&claude)
        .args(["mcp", "add", SERVER_NAME])
        .arg(opts.bridge)
        .status()
        .with_context(|| format!("running {claude} mcp add"))?;
    if !status.success() {
        anyhow::bail!("`claude mcp add` exited with {status}");
    }
    println!(
        "claude-code: registered {SERVER_NAME} -> {}",
        opts.bridge.display()
    );
    Ok(())
}

pub fn uninstall(dry_run: bool) -> Result<()> {
    let claude = match find_claude_cli()? {
        Some(path) => path,
        None => {
            eprintln!("claude-code: `claude` CLI not found on PATH. Skipping.");
            return Ok(());
        }
    };
    if !is_registered(&claude)? {
        println!("claude-code: {SERVER_NAME} not registered — nothing to do.");
        return Ok(());
    }
    if dry_run {
        println!("claude-code: would run `{claude} mcp remove {SERVER_NAME}`");
        return Ok(());
    }
    let status = Command::new(&claude)
        .args(["mcp", "remove", SERVER_NAME])
        .status()
        .with_context(|| format!("running {claude} mcp remove"))?;
    if !status.success() {
        anyhow::bail!("`claude mcp remove` exited with {status}");
    }
    println!("claude-code: removed {SERVER_NAME}");
    Ok(())
}

pub fn status() -> Result<()> {
    let Some(claude) = find_claude_cli()? else {
        println!("claude-code: `claude` CLI not found on PATH.");
        return Ok(());
    };
    if is_registered(&claude)? {
        println!("claude-code: {SERVER_NAME} is registered.");
    } else {
        println!("claude-code: {SERVER_NAME} is NOT registered.");
    }
    Ok(())
}

fn find_claude_cli() -> Result<Option<String>> {
    // PATH lookup without pulling in `which` crate — iterate PATH manually.
    let Some(path_var) = std::env::var_os("PATH") else {
        return Ok(None);
    };
    let target = if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    };
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(target);
        if candidate.is_file() {
            return Ok(Some(candidate.to_string_lossy().into_owned()));
        }
    }
    Ok(None)
}

fn is_registered(claude: &str) -> Result<bool> {
    let out = Command::new(claude)
        .args(["mcp", "list"])
        .output()
        .with_context(|| format!("running {claude} mcp list"))?;
    // `claude mcp list` returns non-zero when no servers exist in some
    // versions. Treat non-zero as "probably empty" rather than an error.
    if !out.status.success() {
        return Ok(false);
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Be liberal in what we accept: any line mentioning `mcp-cli` counts.
    // The CLI's output format is "<name>: <command>" — we match on the
    // server name at a word boundary so we don't false-positive on a
    // command path that happens to contain `mcp-cli`.
    Ok(stdout.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with(&format!("{SERVER_NAME}:"))
            || trimmed.starts_with(&format!("{SERVER_NAME} "))
    }))
}
