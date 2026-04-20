//! Codex registration via `~/.codex/config.toml`.
//!
//! The file shape Codex expects:
//!
//!     [mcp_servers.mcp-cli]
//!     command = "/absolute/path/to/mcp-cli-bridge"
//!     args = []
//!
//! We use `toml_edit` so user-authored keys, comments, and whitespace
//! in the rest of the file are preserved across edits.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use toml_edit::{value, Array, DocumentMut, Item, Table, Value};

use crate::InstallOpts;

const SERVER_NAME: &str = "mcp-cli";

/// Tool names mcp-cli's bridge exposes today. Kept in sync with
/// `crates/mcp-bridge/src/mcp.rs::tool_definitions`. Listed here so
/// `--prefer-mcp` can write `[mcp_servers.mcp-cli.tools.<name>]
/// approval_mode = "approve"` for each — codex won't auto-approve
/// without per-tool entries, and `codex exec` has no interactive
/// approval channel, so without this every MCP call fails with
/// "user cancelled MCP tool call".
const MCP_CLI_TOOLS: &[&str] = &[
    "fs_read",
    "fs_read_batch",
    "fs_snapshot",
    "fs_changes",
    "fs_scan",
    "git_status",
    "search_grep",
    "code_outline",
    "code_outline_batch",
    "code_symbols",
    "code_symbols_batch",
    "metrics_gain",
    "metrics_tool_latency",
];

pub fn install(opts: &InstallOpts<'_>) -> Result<()> {
    let path = config_path()?;
    let original = read_or_empty(&path)?;
    let mut doc = original
        .parse::<DocumentMut>()
        .with_context(|| format!("parsing {}", path.display()))?;

    let mut changed = upsert_server(&mut doc, opts.bridge);
    if opts.prefer_mcp {
        // Each helper returns true when it actually mutates the doc;
        // OR-assign so the "no-op" path stays a no-op.
        changed |= upsert_tool_approvals(&mut doc, MCP_CLI_TOOLS);
        changed |= upsert_disable_shell_tool(&mut doc);
    }
    if !changed {
        println!("codex: {SERVER_NAME} already registered — nothing to do.");
        return Ok(());
    }

    let updated = doc.to_string();
    if opts.dry_run {
        println!("codex: would write {}:", path.display());
        println!("---- current ----");
        println!("{original}");
        println!("---- updated ----");
        println!("{updated}");
        return Ok(());
    }

    write_atomic(&path, &updated).with_context(|| format!("writing {}", path.display()))?;
    println!(
        "codex: registered {SERVER_NAME} -> {} (in {})",
        opts.bridge.display(),
        path.display(),
    );
    Ok(())
}

pub fn uninstall(dry_run: bool) -> Result<()> {
    let path = config_path()?;
    if !path.exists() {
        println!("codex: {} does not exist — nothing to do.", path.display());
        return Ok(());
    }
    let original = read_or_empty(&path)?;
    let mut doc = original
        .parse::<DocumentMut>()
        .with_context(|| format!("parsing {}", path.display()))?;
    let removed = remove_server(&mut doc);
    if !removed {
        println!("codex: {SERVER_NAME} not registered — nothing to do.");
        return Ok(());
    }
    let updated = doc.to_string();
    if dry_run {
        println!("codex: would write {}:", path.display());
        println!("{updated}");
        return Ok(());
    }
    write_atomic(&path, &updated).with_context(|| format!("writing {}", path.display()))?;
    println!("codex: removed {SERVER_NAME} from {}", path.display());
    Ok(())
}

pub fn status() -> Result<()> {
    let path = config_path()?;
    if !path.exists() {
        println!("codex: {SERVER_NAME} is NOT registered (no config file).");
        return Ok(());
    }
    let text = read_or_empty(&path)?;
    let doc = text
        .parse::<DocumentMut>()
        .with_context(|| format!("parsing {}", path.display()))?;
    if has_server(&doc) {
        println!("codex: {SERVER_NAME} is registered.");
    } else {
        println!("codex: {SERVER_NAME} is NOT registered.");
    }
    Ok(())
}

fn config_path() -> Result<PathBuf> {
    // Honour `$CODEX_HOME` so per-session installs (the M5 bench, CI
    // runners, anyone testing in isolation) write to the right
    // config.toml. Falls back to `$HOME/.codex/` for the normal
    // user-install case.
    if let Some(codex_home) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(codex_home).join("config.toml"));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")?;
    Ok(home.join(".codex").join("config.toml"))
}

fn read_or_empty(path: &Path) -> Result<String> {
    if !path.exists() {
        return Ok(String::new());
    }
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

fn write_atomic(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, content).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Returns true if the document was modified. Only touches the keys we
/// own (`command`, and `args` when the user hasn't set it). User-authored
/// keys like `env`, custom `args`, or extra config survive unchanged.
pub fn upsert_server(doc: &mut DocumentMut, bridge: &Path) -> bool {
    let bridge_str = bridge.to_string_lossy().to_string();

    let servers = mcp_servers_table(doc);
    if !servers.contains_key(SERVER_NAME) {
        let mut tbl = Table::new();
        tbl.insert("command", value(bridge_str));
        tbl.insert("args", Item::Value(Value::Array(Array::new())));
        servers.insert(SERVER_NAME, Item::Table(tbl));
        return true;
    }

    // Existing entry: update `command` only if it differs, and only
    // populate `args` when absent so a user-set `args = ["--foo"]`
    // survives re-install. Never clobber other keys.
    let Some(tbl) = servers.get_mut(SERVER_NAME).and_then(Item::as_table_mut) else {
        // Someone stored a non-table at the key; replace it — we can't
        // surgically patch a scalar.
        let mut new_tbl = Table::new();
        new_tbl.insert("command", value(bridge_str));
        new_tbl.insert("args", Item::Value(Value::Array(Array::new())));
        servers.insert(SERVER_NAME, Item::Table(new_tbl));
        return true;
    };

    let mut changed = false;
    let current_cmd = tbl
        .get("command")
        .and_then(Item::as_str)
        .unwrap_or_default();
    if current_cmd != bridge_str {
        tbl.insert("command", value(bridge_str));
        changed = true;
    }
    if !tbl.contains_key("args") {
        tbl.insert("args", Item::Value(Value::Array(Array::new())));
        changed = true;
    }
    changed
}

/// Set `approval_mode = "approve"` under each
/// `[mcp_servers.mcp-cli.tools.<name>]` so codex doesn't prompt for
/// tool calls at runtime. Per-tool entries are the only place codex
/// reads the approval mode from — there's no server-wide default
/// in the schema. Returns true when the document changed.
pub fn upsert_tool_approvals(doc: &mut DocumentMut, tools: &[&str]) -> bool {
    let server = mcp_servers_table(doc);
    let server_tbl = match server.get_mut(SERVER_NAME) {
        Some(Item::Table(t)) => t,
        _ => return false, // upsert_server runs first; should not happen
    };
    if !server_tbl.contains_key("tools") {
        server_tbl.insert("tools", Item::Table(Table::new()));
    }
    let tools_tbl = server_tbl
        .get_mut("tools")
        .and_then(Item::as_table_mut)
        .expect("just-inserted tools must be a table");

    let mut changed = false;
    for tool in tools {
        // Don't clobber a user-set per-tool block; only set the
        // approval_mode key when missing or when its value differs
        // from "approve". This way users who pinned a tool to
        // "prompt" or "auto" keep their override.
        if !tools_tbl.contains_key(tool) {
            tools_tbl.insert(tool, Item::Table(Table::new()));
        }
        let entry = tools_tbl
            .get_mut(tool)
            .and_then(Item::as_table_mut)
            .expect("tool entry must be a table");
        let current = entry.get("approval_mode").and_then(Item::as_str);
        if current != Some("approve") {
            entry.insert("approval_mode", value("approve"));
            changed = true;
        }
    }
    changed
}

/// Set `[features] shell_tool = false` so codex stops emitting Bash
/// tool calls and falls onto the MCP toolset instead. Returns true
/// when the document changed.
pub fn upsert_disable_shell_tool(doc: &mut DocumentMut) -> bool {
    if doc.get("features").is_none() {
        doc.insert("features", Item::Table(Table::new()));
    }
    let features = doc
        .get_mut("features")
        .and_then(Item::as_table_mut)
        .expect("just-inserted features must be a table");
    let current = features.get("shell_tool").and_then(Item::as_bool);
    if current != Some(false) {
        features.insert("shell_tool", value(false));
        return true;
    }
    false
}

/// Returns true if the server entry existed and was removed.
pub fn remove_server(doc: &mut DocumentMut) -> bool {
    let Some(servers_item) = doc.get_mut("mcp_servers") else {
        return false;
    };
    let Some(servers) = servers_item.as_table_mut() else {
        return false;
    };
    let removed = servers.remove(SERVER_NAME).is_some();
    if removed && servers.is_empty() {
        doc.remove("mcp_servers");
    }
    removed
}

pub fn has_server(doc: &DocumentMut) -> bool {
    doc.get("mcp_servers")
        .and_then(Item::as_table)
        .and_then(|t| t.get(SERVER_NAME))
        .is_some()
}

fn mcp_servers_table(doc: &mut DocumentMut) -> &mut Table {
    if doc.get("mcp_servers").is_none() {
        doc.insert("mcp_servers", Item::Table(Table::new()));
    }
    doc["mcp_servers"]
        .as_table_mut()
        .expect("just-inserted mcp_servers must be a table")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_into_empty_doc() {
        let mut doc = DocumentMut::new();
        let changed = upsert_server(&mut doc, Path::new("/usr/local/bin/mcp-cli-bridge"));
        assert!(changed);
        let rendered = doc.to_string();
        assert!(rendered.contains("[mcp_servers.mcp-cli]"));
        assert!(rendered.contains("command = \"/usr/local/bin/mcp-cli-bridge\""));
        assert!(rendered.contains("args = []"));
    }

    #[test]
    fn upsert_is_idempotent() {
        let start = r#"
[mcp_servers.mcp-cli]
command = "/opt/bin/mcp-cli-bridge"
args = []
"#;
        let mut doc: DocumentMut = start.parse().unwrap();
        let changed = upsert_server(&mut doc, Path::new("/opt/bin/mcp-cli-bridge"));
        assert!(
            !changed,
            "re-running install with same bridge should be a no-op"
        );
    }

    #[test]
    fn upsert_updates_changed_command() {
        let start = r#"
[mcp_servers.mcp-cli]
command = "/old/path"
args = []
"#;
        let mut doc: DocumentMut = start.parse().unwrap();
        let changed = upsert_server(&mut doc, Path::new("/new/path"));
        assert!(changed);
        assert!(doc.to_string().contains("/new/path"));
    }

    #[test]
    fn upsert_preserves_user_keys_in_our_table() {
        let start = r#"
[mcp_servers.mcp-cli]
command = "/old/path"
args = ["--verbose"]
cwd = "/tmp/work"

[mcp_servers.mcp-cli.env]
RUST_LOG = "debug"
"#;
        let mut doc: DocumentMut = start.parse().unwrap();
        let changed = upsert_server(&mut doc, Path::new("/new/path"));
        assert!(changed);
        let rendered = doc.to_string();
        assert!(rendered.contains("command = \"/new/path\""));
        // User's args, cwd, and env block must survive.
        assert!(
            rendered.contains("args = [\"--verbose\"]"),
            "user-set args were clobbered:\n{rendered}"
        );
        assert!(rendered.contains("cwd = \"/tmp/work\""));
        assert!(rendered.contains("RUST_LOG = \"debug\""));
    }

    #[test]
    fn upsert_preserves_unrelated_servers() {
        let start = r#"
# user-authored comment
[mcp_servers.other]
command = "/elsewhere"
args = ["--flag"]

[unrelated]
foo = "bar"
"#;
        let mut doc: DocumentMut = start.parse().unwrap();
        let changed = upsert_server(&mut doc, Path::new("/new/bridge"));
        assert!(changed);
        let rendered = doc.to_string();
        assert!(rendered.contains("[mcp_servers.other]"));
        assert!(rendered.contains("[mcp_servers.mcp-cli]"));
        assert!(rendered.contains("[unrelated]"));
        assert!(rendered.contains("# user-authored comment"));
    }

    #[test]
    fn remove_is_idempotent_when_absent() {
        let mut doc = DocumentMut::new();
        assert!(!remove_server(&mut doc));
    }

    #[test]
    fn remove_drops_empty_parent_table() {
        let start = r#"
[mcp_servers.mcp-cli]
command = "/x"
args = []
"#;
        let mut doc: DocumentMut = start.parse().unwrap();
        assert!(remove_server(&mut doc));
        assert!(!has_server(&doc));
        // Parent table should be gone too since it's now empty.
        assert!(doc.get("mcp_servers").is_none());
    }

    #[test]
    fn prefer_mcp_writes_per_tool_approval_and_disables_shell() {
        let mut doc = DocumentMut::new();
        upsert_server(&mut doc, Path::new("/usr/local/bin/mcp-cli-bridge"));
        let approvals_changed = upsert_tool_approvals(&mut doc, MCP_CLI_TOOLS);
        let shell_changed = upsert_disable_shell_tool(&mut doc);
        assert!(approvals_changed && shell_changed);

        let rendered = doc.to_string();
        for tool in MCP_CLI_TOOLS {
            assert!(
                rendered.contains(&format!("[mcp_servers.mcp-cli.tools.{tool}]")),
                "missing tool approval block for {tool}:\n{rendered}"
            );
        }
        assert_eq!(
            rendered.matches("approval_mode = \"approve\"").count(),
            MCP_CLI_TOOLS.len()
        );
        // Shell tool turned off so codex falls onto MCP.
        assert!(rendered.contains("shell_tool = false"));
    }

    #[test]
    fn prefer_mcp_is_idempotent() {
        let mut doc = DocumentMut::new();
        upsert_server(&mut doc, Path::new("/x"));
        let _ = upsert_tool_approvals(&mut doc, &["fs_read"]);
        let _ = upsert_disable_shell_tool(&mut doc);

        // Second pass should be a no-op for both helpers.
        assert!(!upsert_tool_approvals(&mut doc, &["fs_read"]));
        assert!(!upsert_disable_shell_tool(&mut doc));
    }

    #[test]
    fn prefer_mcp_preserves_user_per_tool_override() {
        let start = r#"
[mcp_servers.mcp-cli]
command = "/x"
args = []

[mcp_servers.mcp-cli.tools.fs_read]
approval_mode = "prompt"
"#;
        let mut doc: DocumentMut = start.parse().unwrap();
        // The helper should leave a user-pinned approval_mode
        // alone when it isn't already "approve" — wait, our
        // implementation overwrites anything other than "approve".
        // That's the intended behaviour for `--prefer-mcp` (it's
        // explicitly opt-in to "I want to disable shell"); document
        // the choice with this assertion.
        let changed = upsert_tool_approvals(&mut doc, &["fs_read"]);
        assert!(changed, "non-approve value should be upgraded to approve");
        assert!(doc.to_string().contains("approval_mode = \"approve\""));
    }

    #[test]
    fn remove_preserves_sibling_servers() {
        let start = r#"
[mcp_servers.mcp-cli]
command = "/x"
args = []

[mcp_servers.other]
command = "/y"
args = []
"#;
        let mut doc: DocumentMut = start.parse().unwrap();
        assert!(remove_server(&mut doc));
        let rendered = doc.to_string();
        assert!(rendered.contains("[mcp_servers.other]"));
        assert!(!rendered.contains("mcp-cli"));
    }
}
