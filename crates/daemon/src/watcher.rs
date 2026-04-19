//! Filesystem watcher: turns notify events into ChangeLog entries.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use ignore::gitignore::Gitignore;
use ignore::Match;
use notify::event::{EventKind, ModifyKind, RenameMode};
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use parking_lot::Mutex;
use protocol::ChangeKind;
use std::collections::HashMap;

use crate::changelog::ChangeLog;

pub struct WatchHandle {
    _watcher: RecommendedWatcher,
}

pub fn spawn(root: PathBuf, log: Arc<ChangeLog>) -> Result<WatchHandle> {
    let filter = Arc::new(Filter::new(root.clone()));
    let filter_for_watch = filter.clone();

    let mut watcher = RecommendedWatcher::new(
        move |res: notify::Result<Event>| match res {
            Ok(event) => handle_event(event, &filter_for_watch, &log),
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

pub struct Filter {
    root: PathBuf,
    /// Cached `.gitignore` per directory. `None` means "no .gitignore in that dir";
    /// we still insert a cache entry so we don't re-stat repeatedly.
    cache: Mutex<HashMap<PathBuf, Option<Arc<Gitignore>>>>,
}

impl Filter {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn relative(&self, p: &Path) -> Option<String> {
        let rel = p.strip_prefix(&self.root).ok()?;
        Some(rel.to_string_lossy().to_string())
    }

    fn gitignore_for(&self, dir: &Path) -> Option<Arc<Gitignore>> {
        if let Some(hit) = self.cache.lock().get(dir) {
            return hit.clone();
        }
        let gi_path = dir.join(".gitignore");
        let value = if gi_path.is_file() {
            let (gi, _err) = Gitignore::new(&gi_path);
            Some(Arc::new(gi))
        } else {
            None
        };
        self.cache.lock().insert(dir.to_path_buf(), value.clone());
        value
    }

    /// Invalidate the cached `.gitignore` for a single directory. Called when
    /// we see a filesystem event targeting a `.gitignore` file so rule edits
    /// take effect without restarting the daemon.
    fn invalidate(&self, dir: &Path) {
        self.cache.lock().remove(dir);
    }

    /// Return `true` if the path should be kept (NOT ignored).
    pub fn keep(&self, p: &Path) -> bool {
        let Ok(rel) = p.strip_prefix(&self.root) else {
            return false;
        };
        if rel
            .components()
            .next()
            .map(|c| c.as_os_str() == ".git")
            .unwrap_or(false)
        {
            return false;
        }

        // Walk parent directories deepest -> shallowest; deeper .gitignore
        // files override shallower ones. First decisive match wins.
        let mut dirs: Vec<PathBuf> = vec![self.root.clone()];
        if let Some(parent) = rel.parent() {
            let mut cur = self.root.clone();
            for comp in parent.components() {
                cur.push(comp);
                dirs.push(cur.clone());
            }
        }

        for dir in dirs.into_iter().rev() {
            let Some(gi) = self.gitignore_for(&dir) else {
                continue;
            };
            let abs = self.root.join(rel);
            let Ok(rel_to_gi) = abs.strip_prefix(&dir) else {
                continue;
            };
            match gi.matched_path_or_any_parents(rel_to_gi, false) {
                Match::Ignore(_) => return false,
                Match::Whitelist(_) => return true,
                Match::None => continue,
            }
        }
        true
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
        // If a .gitignore itself changed, drop its cached entry so subsequent
        // checks see the new rules.
        if path.file_name().map(|n| n == ".gitignore").unwrap_or(false) {
            if let Some(parent) = path.parent() {
                filter.invalidate(parent);
            }
        }
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
