//! Output compaction primitives — the M7 building blocks.
//!
//! Inspired by [`rtk`](https://github.com/rtk-ai/rtk). Tool handlers
//! that opt into a compact mode compose these primitives instead of
//! reinventing the same group/truncate/dedupe logic per tool.

use std::collections::BTreeMap;
use std::path::Path;

use protocol::{
    GitStatusClassBucket, GitStatusCompact, GitStatusDirCount, GitStatusEntry, SearchFileBucket,
    SearchGrepCompact, SearchHit,
};

/// Bucket `git.status` entries by their primary status class and, within
/// each class, by parent directory. The result is the rtk-style "60 files
/// changed across these directories" view rather than the per-file dump.
///
/// `top_dirs_per_class` caps how many directory rows per class show up in
/// the response — the remainder is summed into a synthetic `(other)` row
/// so totals always reconcile.
pub fn git_status_compact(
    entries: &[GitStatusEntry],
    top_dirs_per_class: usize,
) -> GitStatusCompact {
    let mut by_class: BTreeMap<&str, BTreeMap<String, usize>> = BTreeMap::new();
    let mut total = 0usize;

    for entry in entries {
        let class = primary_status_class(&entry.status);
        // Skip cleanly-tracked entries — they're noise in a status view.
        if class == "clean" {
            continue;
        }
        total += 1;
        let dir = parent_dir_str(&entry.path);
        *by_class.entry(class).or_default().entry(dir).or_default() += 1;
    }

    let mut buckets: Vec<GitStatusClassBucket> = by_class
        .into_iter()
        .map(|(class, dirs)| {
            // Sort dirs by count desc so the heaviest hitter is first;
            // ties broken by directory name for deterministic output.
            let mut sorted: Vec<(String, usize)> = dirs.into_iter().collect();
            sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

            let count: usize = sorted.iter().map(|(_, c)| c).sum();
            let (head, tail) = if sorted.len() > top_dirs_per_class {
                sorted.split_at(top_dirs_per_class)
            } else {
                (&sorted[..], &[][..])
            };

            let mut by_dir: Vec<GitStatusDirCount> = head
                .iter()
                .map(|(d, c)| GitStatusDirCount {
                    dir: d.clone(),
                    count: *c,
                })
                .collect();
            if !tail.is_empty() {
                let other_count: usize = tail.iter().map(|(_, c)| c).sum();
                by_dir.push(GitStatusDirCount {
                    dir: "(other)".to_string(),
                    count: other_count,
                });
            }

            GitStatusClassBucket {
                class: class.to_string(),
                count,
                by_dir,
            }
        })
        .collect();

    // Stable order: heaviest class first, then alphabetical for ties.
    buckets.sort_by(|a, b| b.count.cmp(&a.count).then(a.class.cmp(&b.class)));

    GitStatusCompact {
        by_class: buckets,
        total,
    }
}

/// Bucket `search.grep` hits by file. One bucket per file with a match
/// count plus first / last line numbers — enough for the agent to know
/// which files matter and roughly where, without paying for every hit.
pub fn search_grep_compact(hits: &[SearchHit]) -> SearchGrepCompact {
    // BTreeMap so the output is stable; iteration order matters for tests
    // and for diffability of cached responses.
    let mut by_path: BTreeMap<&str, (usize, u64, u64)> = BTreeMap::new();
    for hit in hits {
        let entry =
            by_path
                .entry(hit.path.as_str())
                .or_insert((0, hit.line_number, hit.line_number));
        entry.0 += 1;
        entry.1 = entry.1.min(hit.line_number);
        entry.2 = entry.2.max(hit.line_number);
    }

    let buckets: Vec<SearchFileBucket> = by_path
        .into_iter()
        .map(|(path, (matches, first, last))| SearchFileBucket {
            path: path.to_string(),
            matches,
            first_line: first,
            last_line: last,
        })
        .collect();

    let total_matches = buckets.iter().map(|b| b.matches).sum();
    SearchGrepCompact {
        buckets,
        total_matches,
    }
}

/// Map a libgit2 status string (e.g. `index_modified,wt_modified`) to a
/// single human-friendly class label. Picks the most actionable one
/// when multiple flags are set — `conflicted` > `deleted` > `renamed` >
/// `typechange` > `modified` > `untracked` > `ignored` > `clean`.
fn primary_status_class(status: &str) -> &'static str {
    if status.contains("conflicted") {
        return "conflicted";
    }
    if status.contains("deleted") {
        return "deleted";
    }
    if status.contains("renamed") {
        return "renamed";
    }
    if status.contains("typechange") {
        return "typechange";
    }
    if status.contains("modified") {
        return "modified";
    }
    if status.contains("wt_new") || status.contains("index_new") {
        return "untracked";
    }
    if status.contains("ignored") {
        return "ignored";
    }
    "clean"
}

/// Parent directory of a project-relative path, normalised to `"."` for
/// top-level files. Backslashes from a Windows-formatted entry are
/// folded to `/` so the bucket key is stable across platforms.
fn parent_dir_str(path: &str) -> String {
    let normalised = path.replace('\\', "/");
    let p = Path::new(&normalised);
    match p.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_string_lossy().into_owned(),
        _ => ".".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, status: &str) -> GitStatusEntry {
        GitStatusEntry {
            path: path.to_string(),
            status: status.to_string(),
        }
    }

    fn hit(path: &str, line: u64) -> SearchHit {
        SearchHit {
            path: path.to_string(),
            line_number: line,
            line: format!("L{}", line),
            context: Vec::new(),
        }
    }

    #[test]
    fn primary_class_picks_most_actionable_flag() {
        assert_eq!(primary_status_class("conflicted,wt_modified"), "conflicted");
        assert_eq!(primary_status_class("wt_deleted,index_deleted"), "deleted");
        assert_eq!(primary_status_class("wt_renamed,wt_modified"), "renamed",);
        assert_eq!(primary_status_class("wt_typechange"), "typechange");
        assert_eq!(
            primary_status_class("wt_modified,index_modified"),
            "modified"
        );
        assert_eq!(primary_status_class("wt_new"), "untracked");
        assert_eq!(primary_status_class("ignored"), "ignored");
        assert_eq!(primary_status_class("clean"), "clean");
    }

    #[test]
    fn git_compact_drops_clean_and_groups_by_dir() {
        let entries = vec![
            entry("src/a.rs", "wt_modified"),
            entry("src/b.rs", "wt_modified"),
            entry("src/c.rs", "wt_new"),
            entry("docs/x.md", "wt_modified"),
            entry("noise.txt", "clean"),
        ];
        let c = git_status_compact(&entries, 16);
        assert_eq!(c.total, 4);
        // Two classes: modified (3) and untracked (1). Modified comes first.
        assert_eq!(c.by_class.len(), 2);
        let m = &c.by_class[0];
        assert_eq!(m.class, "modified");
        assert_eq!(m.count, 3);
        // src/ has 2, docs/ has 1.
        assert_eq!(m.by_dir.len(), 2);
        assert_eq!(m.by_dir[0].dir, "src");
        assert_eq!(m.by_dir[0].count, 2);
        let u = &c.by_class[1];
        assert_eq!(u.class, "untracked");
        assert_eq!(u.count, 1);
    }

    #[test]
    fn git_compact_collapses_overflow_dirs_into_other() {
        // Generate 5 modified files across 5 distinct directories,
        // request top-2 → expect 2 dirs + (other) row totalling the rest.
        let entries: Vec<_> = (0..5)
            .map(|i| entry(&format!("dir{i}/file.rs"), "wt_modified"))
            .collect();
        let c = git_status_compact(&entries, 2);
        let m = &c.by_class[0];
        assert_eq!(m.count, 5);
        assert_eq!(m.by_dir.len(), 3); // 2 named + 1 (other)
        let other = m.by_dir.iter().find(|d| d.dir == "(other)").unwrap();
        assert_eq!(other.count, 3);
    }

    #[test]
    fn git_compact_uses_dot_for_top_level_files() {
        let entries = vec![entry("README.md", "wt_modified")];
        let c = git_status_compact(&entries, 16);
        assert_eq!(c.by_class[0].by_dir[0].dir, ".");
    }

    #[test]
    fn search_compact_buckets_per_file_with_line_range() {
        let hits = vec![
            hit("src/a.rs", 10),
            hit("src/a.rs", 99),
            hit("src/a.rs", 50),
            hit("src/b.rs", 7),
        ];
        let c = search_grep_compact(&hits);
        assert_eq!(c.total_matches, 4);
        assert_eq!(c.buckets.len(), 2);
        let a = c.buckets.iter().find(|b| b.path == "src/a.rs").unwrap();
        assert_eq!(a.matches, 3);
        assert_eq!(a.first_line, 10);
        assert_eq!(a.last_line, 99);
    }

    #[test]
    fn search_compact_handles_empty_input() {
        let c = search_grep_compact(&[]);
        assert!(c.buckets.is_empty());
        assert_eq!(c.total_matches, 0);
    }
}
