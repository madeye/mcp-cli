//! Filesystem watcher: turns notify events into ChangeLog entries.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use ignore::gitignore::Gitignore;
use notify::event::{EventKind, ModifyKind, RenameMode};
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use protocol::ChangeKind;

use crate::changelog::ChangeLog;

pub struct WatchHandle {
    _watcher: RecommendedWatcher,
}

pub fn spawn(root: PathBuf, log: Arc<ChangeLog>) -> Result<WatchHandle> {
    let (gitignore, _err) = Gitignore::new(root.join(".gitignore"));
    let filter = Filter {
        root: root.clone(),
        gitignore,
    };

    let mut watcher = RecommendedWatcher::new(
        move |res: notify::Result<Event>| match res {
            Ok(event) => handle_event(event, &filter, &log),
            Err(e) => tracing::warn!(error = %e, "watch error"),
        },
        Config::default().with_poll_interval(Duration::from_secs(2)),
    )
    .context("creating fs watcher")?;

    watcher
        .watch(&root, RecursiveMode::Recursive)
        .with_context(|| format!("watching {}", root.display()))?;

    tracing::info!(root = %root.display(), "fs watcher started");
    Ok(WatchHandle { _watcher: watcher })
}

struct Filter {
    root: PathBuf,
    gitignore: Gitignore,
}

impl Filter {
    fn relative(&self, p: &Path) -> Option<String> {
        let rel = p.strip_prefix(&self.root).ok()?;
        Some(rel.to_string_lossy().to_string())
    }

    fn keep(&self, p: &Path) -> bool {
        let Ok(rel) = p.strip_prefix(&self.root) else {
            return false;
        };
        // Always drop anything inside the .git dir.
        if rel
            .components()
            .next()
            .map(|c| c.as_os_str() == ".git")
            .unwrap_or(false)
        {
            return false;
        }
        let m = self.gitignore.matched_path_or_any_parents(rel, false);
        !m.is_ignore()
    }
}

fn handle_event(event: Event, filter: &Filter, log: &ChangeLog) {
    let kinds: Vec<(usize, ChangeKind)> = match event.kind {
        EventKind::Create(_) => event
            .paths
            .iter()
            .enumerate()
            .map(|(i, _)| (i, ChangeKind::Created))
            .collect(),
        EventKind::Remove(_) => event
            .paths
            .iter()
            .enumerate()
            .map(|(i, _)| (i, ChangeKind::Removed))
            .collect(),
        EventKind::Modify(ModifyKind::Name(mode)) => match mode {
            RenameMode::From => event
                .paths
                .iter()
                .enumerate()
                .map(|(i, _)| (i, ChangeKind::Removed))
                .collect(),
            RenameMode::To => event
                .paths
                .iter()
                .enumerate()
                .map(|(i, _)| (i, ChangeKind::Created))
                .collect(),
            RenameMode::Both => {
                // paths = [from, to]
                let mut v = Vec::new();
                if !event.paths.is_empty() {
                    v.push((0, ChangeKind::Removed));
                }
                if event.paths.len() >= 2 {
                    v.push((1, ChangeKind::Created));
                }
                v
            }
            _ => event
                .paths
                .iter()
                .enumerate()
                .map(|(i, _)| (i, ChangeKind::Modified))
                .collect(),
        },
        EventKind::Modify(_) => event
            .paths
            .iter()
            .enumerate()
            .map(|(i, _)| (i, ChangeKind::Modified))
            .collect(),
        // Access events and Other/Any are not interesting for change tracking.
        _ => return,
    };

    for (idx, kind) in kinds {
        let Some(path) = event.paths.get(idx) else {
            continue;
        };
        if !filter.keep(path) {
            continue;
        }
        let Some(rel) = filter.relative(path) else {
            continue;
        };
        if rel.is_empty() {
            continue;
        }
        log.record(rel, kind);
    }
}
