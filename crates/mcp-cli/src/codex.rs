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

pub fn install(opts: &InstallOpts<'_>) -> Result<()> {
    let path = config_path()?;
    let original = read_or_empty(&path)?;
    let mut doc = original
        .parse::<DocumentMut>()
        .with_context(|| format!("parsing {}", path.display()))?;

    let changed = upsert_server(&mut doc, opts.bridge);
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
