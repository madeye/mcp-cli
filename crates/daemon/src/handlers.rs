use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

use grep_regex::RegexMatcherBuilder;
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch};
use ignore::WalkBuilder;
use memmap2::Mmap;
use protocol::{
    ChangeKind, CodeDependenciesParams, CodeDependenciesResult, CodeFindOccurrencesParams,
    CodeFindOccurrencesResult, CodeImportsParams, CodeImportsResult, CodeOccurrence,
    CodeOutlineBatchItem, CodeOutlineBatchParams, CodeOutlineBatchResult, CodeOutlineParams,
    CodeOutlineResult, CodeSymbolsBatchItem, CodeSymbolsBatchParams, CodeSymbolsBatchResult,
    CodeSymbolsParams, CodeSymbolsResult, DependencyEdge, FsApplyPatchParams, FsApplyPatchResult,
    FsChangesParams, FsChangesResult, FsReadBatchItem, FsReadBatchParams, FsReadBatchResult,
    FsReadParams, FsReadResult, FsReadSkeletonParams, FsReadSkeletonResult, FsReplaceAllParams,
    FsReplaceAllResult, FsScanParams, FsScanResult, FsSnapshotResult, GitCommit, GitDiffParams,
    GitDiffResult, GitLogParams, GitLogResult, GitStatusEntry, GitStatusParams, GitStatusResult,
    ImportEntry, MetricsGainParams, MetricsToolLatencyParams, RpcError, SearchContextLine,
    SearchGrepParams, SearchGrepResult, SearchHit, SkeletonElidedRegion, ToolGhParams,
    ToolGhResult, ToolRunParams, ToolRunResult,
};

use crate::compact;
use crate::languages::Language;
use crate::search_cache::SearchKey;
use crate::server::{resolve_within, Daemon};

/// How many directory rows we keep per status class in compact mode
/// before collapsing the rest into a synthetic `(other)` row. Picked
/// empirically — enough to spot patterns, few enough that a 5000-file
/// dirty tree still serializes to a kilobyte or two.
const GIT_STATUS_TOP_DIRS_PER_CLASS: usize = 16;
/// Same cap for `fs.scan`'s flat per-directory roll-up. `fs.scan`
/// typically sees 10-100× more entries than `git.status` (whole tree
/// vs. dirty files), so the row budget is larger.
const FS_SCAN_TOP_DIRS: usize = 32;

const FS_READ_DEFAULT_LIMIT: u64 = 256 * 1024;
const SEARCH_DEFAULT_LIMIT: usize = 200;
/// Upper bound on the per-match context window. Bigger values blow up
/// the response without buying the agent new signal — and a compromised
/// client sending `context: 1e9` otherwise makes the daemon stream the
/// whole file per match.
const SEARCH_CONTEXT_CAP: u32 = 20;
const TOOL_RUN_DEFAULT_OUTPUT_LIMIT: usize = 64 * 1024;
const TOOL_RUN_FAILURE_TAIL_LIMIT: usize = 16 * 1024;

pub fn fs_read(daemon: &Daemon, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
    let params: FsReadParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))?;
    let result = fs_read_inner(daemon, &params)?;
    Ok(serde_json::to_value(result).unwrap())
}

pub fn fs_read_batch(
    daemon: &Daemon,
    params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let params: FsReadBatchParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))?;
    // Serialize the reads (no per-file locking needed — mmap is
    // read-only, and tokio's single-threaded cooperative model means
    // parallelism here would just bounce blocking syscalls across
    // worker threads without real wins on the mmap fast path).
    // Per-request failures become `FsReadBatchItem.error` entries; the
    // batch itself only fails for malformed top-level params.
    let responses = params
        .requests
        .into_iter()
        .map(|req| {
            let path = req.path.clone();
            match fs_read_inner(daemon, &req) {
                Ok(r) => FsReadBatchItem {
                    path,
                    result: Some(r),
                    error: None,
                },
                Err(e) => FsReadBatchItem {
                    path,
                    result: None,
                    error: Some(e),
                },
            }
        })
        .collect();
    Ok(serde_json::to_value(FsReadBatchResult { responses }).unwrap())
}

/// Shared core for `fs_read` and `fs_read_batch`. Takes the parsed
/// params (no JSON round-trip per batch item) and returns the
/// structured result; callers JSON-encode at their own layer.
fn fs_read_inner(daemon: &Daemon, params: &FsReadParams) -> Result<FsReadResult, RpcError> {
    let path = resolve_within(&daemon.root, &params.path)?;

    let file = File::open(&path).map_err(|e| RpcError::new(-32010, format!("open: {e}")))?;
    let metadata = file
        .metadata()
        .map_err(|e| RpcError::new(-32011, format!("stat: {e}")))?;
    let total_size = metadata.len();
    let mtime_ns = metadata_mtime_ns(&metadata);
    let (version, _) = daemon.changelog.snapshot();

    if total_size == 0 {
        return Ok(FsReadResult {
            path: params.path.clone(),
            version,
            mtime_ns,
            bytes_read: 0,
            total_size: 0,
            content: String::new(),
            truncated: false,
            stripped_regions: Vec::new(),
        });
    }

    // Safe: we only read the mapping; another process modifying the file mid-read
    // would risk SIGBUS, but for source-tree workloads this is the standard tradeoff.
    let mmap =
        unsafe { Mmap::map(&file) }.map_err(|e| RpcError::new(-32012, format!("mmap: {e}")))?;

    let offset = params.offset.min(total_size);
    let remaining = total_size - offset;
    let limit = params
        .length
        .unwrap_or(FS_READ_DEFAULT_LIMIT)
        .min(remaining);
    let end = offset + limit;
    let slice = &mmap[offset as usize..end as usize];

    let content = String::from_utf8_lossy(slice).into_owned();
    let truncated = end < total_size;

    // Detection uses cues from the head of the file (shebang, leading
    // comments, `@generated` markers), so only run it on whole-file
    // reads starting at byte 0 — an offset>0 slice is almost certainly
    // a scroll-page, not a fresh view, and boilerplate won't be there
    // anyway.
    let (content, stripped_regions) = if params.strip_noise && params.offset == 0 {
        let stripped = compact::strip_noise::strip_noise(&content);
        (stripped.content, stripped.regions)
    } else {
        (content, Vec::new())
    };

    Ok(FsReadResult {
        path: params.path.clone(),
        version,
        mtime_ns,
        bytes_read: limit,
        total_size,
        content,
        truncated,
        stripped_regions,
    })
}

pub fn fs_snapshot(
    daemon: &Daemon,
    _params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let (version, oldest_retained) = daemon.changelog.snapshot();
    Ok(serde_json::to_value(FsSnapshotResult {
        version,
        capacity: daemon.changelog.capacity(),
        oldest_retained,
    })
    .unwrap())
}

pub fn fs_changes(
    daemon: &Daemon,
    params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let params: FsChangesParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))?;
    let (version, changes, overflowed) = daemon.changelog.changes_since(params.since);
    Ok(serde_json::to_value(FsChangesResult {
        version,
        changes,
        overflowed,
    })
    .unwrap())
}

pub fn fs_apply_patch(
    daemon: &Daemon,
    params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let params: FsApplyPatchParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))?;
    let path = resolve_within(&daemon.root, &params.path)?;
    check_write_preconditions(
        daemon,
        &path,
        params.expected_version,
        params.expected_mtime_ns,
    )?;

    let content =
        fs::read_to_string(&path).map_err(|e| RpcError::new(-32080, format!("read: {e}")))?;
    let patched = apply_unified_patch(&content, &params.patch)?;
    write_file_checked(&path, patched.as_bytes())?;
    record_daemon_write(daemon, &path);
    let metadata = fs::metadata(&path).map_err(|e| RpcError::new(-32011, format!("stat: {e}")))?;
    let (version, _) = daemon.changelog.snapshot();
    let result = FsApplyPatchResult {
        path: params.path,
        applied: true,
        version,
        mtime_ns: metadata_mtime_ns(&metadata),
    };
    Ok(serde_json::to_value(result).unwrap())
}

pub fn fs_replace_all(
    daemon: &Daemon,
    params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let params: FsReplaceAllParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))?;
    if params.search.is_empty() {
        return Err(RpcError::new(-32602, "search must not be empty"));
    }
    let path = resolve_within(&daemon.root, &params.path)?;
    check_write_preconditions(
        daemon,
        &path,
        params.expected_version,
        params.expected_mtime_ns,
    )?;

    let content =
        fs::read_to_string(&path).map_err(|e| RpcError::new(-32080, format!("read: {e}")))?;
    let replacements = content.matches(&params.search).count();
    if let Some(max) = params.max_replacements {
        if replacements > max {
            return Err(RpcError::new(
                -32086,
                format!("replacement count {replacements} exceeds max_replacements {max}"),
            ));
        }
    }
    let replaced = content.replace(&params.search, &params.replacement);
    if replacements > 0 {
        write_file_checked(&path, replaced.as_bytes())?;
        record_daemon_write(daemon, &path);
    }
    let metadata = fs::metadata(&path).map_err(|e| RpcError::new(-32011, format!("stat: {e}")))?;
    let (version, _) = daemon.changelog.snapshot();
    let result = FsReplaceAllResult {
        path: params.path,
        replacements,
        version,
        mtime_ns: metadata_mtime_ns(&metadata),
    };
    Ok(serde_json::to_value(result).unwrap())
}

fn check_write_preconditions(
    daemon: &Daemon,
    path: &std::path::Path,
    expected_version: Option<u64>,
    expected_mtime_ns: Option<u64>,
) -> Result<(), RpcError> {
    if let Some(expected) = expected_version {
        let (current, _) = daemon.changelog.snapshot();
        if current != expected {
            return Err(RpcError::new(
                -32082,
                format!("stale write: expected version {expected}, current version {current}"),
            ));
        }
    }
    if let Some(expected) = expected_mtime_ns {
        let metadata =
            fs::metadata(path).map_err(|e| RpcError::new(-32011, format!("stat: {e}")))?;
        let current = metadata_mtime_ns(&metadata);
        if current != expected {
            return Err(RpcError::new(
                -32083,
                format!("stale write: expected mtime_ns {expected}, current mtime_ns {current}"),
            ));
        }
    }
    Ok(())
}

fn write_file_checked(path: &std::path::Path, bytes: &[u8]) -> Result<(), RpcError> {
    fs::write(path, bytes).map_err(|e| RpcError::new(-32081, format!("write: {e}")))
}

fn record_daemon_write(daemon: &Daemon, path: &std::path::Path) {
    let rel = path
        .strip_prefix(&daemon.root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string();
    daemon.changelog.record(rel, ChangeKind::Modified);
}

fn metadata_mtime_ns(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

fn apply_unified_patch(original: &str, patch: &str) -> Result<String, RpcError> {
    let original_lines: Vec<&str> = original.split_inclusive('\n').collect();
    let patch_lines: Vec<&str> = patch.split_inclusive('\n').collect();
    let mut output = String::with_capacity(original.len() + patch.len());
    let mut original_idx = 0usize;
    let mut patch_idx = 0usize;
    let mut saw_hunk = false;

    while patch_idx < patch_lines.len() {
        let line = patch_lines[patch_idx];
        if !line.starts_with("@@ ") {
            patch_idx += 1;
            continue;
        }
        saw_hunk = true;
        let (old_start, _) = parse_hunk_header(line)?;
        let hunk_start = old_start.saturating_sub(1);
        if hunk_start < original_idx || hunk_start > original_lines.len() {
            return Err(RpcError::new(-32084, "patch hunk starts outside file"));
        }
        for line in &original_lines[original_idx..hunk_start] {
            output.push_str(line);
        }
        original_idx = hunk_start;
        patch_idx += 1;

        while patch_idx < patch_lines.len() && !patch_lines[patch_idx].starts_with("@@ ") {
            let hunk_line = patch_lines[patch_idx];
            if hunk_line.starts_with("--- ") || hunk_line.starts_with("+++ ") {
                break;
            }
            if hunk_line.starts_with("\\ No newline at end of file") {
                patch_idx += 1;
                continue;
            }
            let (tag, content) = hunk_line.split_at(1);
            match tag {
                " " => {
                    let current = original_lines.get(original_idx).ok_or_else(|| {
                        RpcError::new(-32084, "patch context extends beyond file")
                    })?;
                    if *current != content {
                        return Err(RpcError::new(-32084, "patch context mismatch"));
                    }
                    output.push_str(current);
                    original_idx += 1;
                }
                "-" => {
                    let current = original_lines.get(original_idx).ok_or_else(|| {
                        RpcError::new(-32084, "patch removal extends beyond file")
                    })?;
                    if *current != content {
                        return Err(RpcError::new(-32084, "patch removal mismatch"));
                    }
                    original_idx += 1;
                }
                "+" => output.push_str(content),
                _ => return Err(RpcError::new(-32084, "invalid patch hunk line")),
            }
            patch_idx += 1;
        }
    }

    if !saw_hunk {
        return Err(RpcError::new(-32084, "patch contains no hunks"));
    }
    for line in &original_lines[original_idx..] {
        output.push_str(line);
    }
    Ok(output)
}

fn parse_hunk_header(header: &str) -> Result<(usize, usize), RpcError> {
    let old = header
        .split_whitespace()
        .find(|part| part.starts_with('-'))
        .ok_or_else(|| RpcError::new(-32084, "invalid patch hunk header"))?;
    let old = old.trim_start_matches('-');
    let mut parts = old.split(',');
    let start = parts
        .next()
        .and_then(|s| s.parse::<usize>().ok())
        .ok_or_else(|| RpcError::new(-32084, "invalid patch old start"))?;
    let count = parts
        .next()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1);
    Ok((start, count))
}

pub fn fs_scan(daemon: &Daemon, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
    let params: FsScanParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))?;

    let scan_root = match &params.path {
        Some(p) => resolve_within(&daemon.root, p)?,
        None => daemon.root.clone(),
    };

    // Capture the cursor before we start walking. Any event landing during the
    // walk will still be reachable via fs.changes(since: version), so the
    // client can close the race with a single follow-up call.
    let (version, _) = daemon.changelog.snapshot();

    let max = params.max_results.unwrap_or(usize::MAX);
    let mut files: Vec<String> = Vec::new();
    let mut truncated = false;

    for entry in WalkBuilder::new(&scan_root)
        .standard_filters(true)
        .hidden(true)
        .build()
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        if files.len() >= max {
            truncated = true;
            break;
        }
        let path = entry.path();
        let rel = path.strip_prefix(&daemon.root).unwrap_or(path);
        files.push(rel.to_string_lossy().to_string());
    }

    let (files, compact) = if params.compact {
        let c = compact::fs_scan_compact(&files, FS_SCAN_TOP_DIRS);
        (Vec::new(), Some(c))
    } else {
        (files, None)
    };

    Ok(serde_json::to_value(FsScanResult {
        version,
        files,
        truncated,
        compact,
    })
    .unwrap())
}

pub fn git_status(
    daemon: &Daemon,
    params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let params: GitStatusParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))?;
    let repo_root = match params.repo {
        Some(r) => resolve_within(&daemon.root, &r)?,
        None => daemon.root.clone(),
    };

    let repo = git2::Repository::discover(&repo_root)
        .map_err(|e| RpcError::new(-32020, format!("discover repo: {e}")))?;

    let head = repo.head().ok();
    let branch = head
        .as_ref()
        .and_then(|h| h.shorthand().map(|s| s.to_string()));
    let head_oid = head
        .as_ref()
        .and_then(|h| h.target().map(|o| o.to_string()));

    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true).recurse_untracked_dirs(true);
    let statuses = repo
        .statuses(Some(&mut opts))
        .map_err(|e| RpcError::new(-32021, format!("statuses: {e}")))?;

    let entries: Vec<GitStatusEntry> = statuses
        .iter()
        .filter_map(|s| {
            let path = s.path()?.to_string();
            Some(GitStatusEntry {
                path,
                status: format_status(s.status()),
            })
        })
        .collect();

    let raw_result = GitStatusResult {
        branch: branch.clone(),
        head: head_oid.clone(),
        entries: entries.clone(),
        compact: None,
    };
    let raw_bytes = serialized_size(&raw_result);

    let result = if params.compact {
        let compact = compact::git_status_compact(&entries, GIT_STATUS_TOP_DIRS_PER_CLASS);
        GitStatusResult {
            branch,
            head: head_oid,
            entries: Vec::new(),
            compact: Some(compact),
        }
    } else {
        raw_result
    };
    let value = serde_json::to_value(&result).unwrap();
    let compacted_bytes = serialized_size(&result);
    daemon
        .metrics
        .record(protocol::methods::GIT_STATUS, raw_bytes, compacted_bytes);
    Ok(value)
}

pub fn git_log(daemon: &Daemon, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
    let params: GitLogParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))?;
    let repo_root = match params.repo {
        Some(r) => resolve_within(&daemon.root, &r)?,
        None => daemon.root.clone(),
    };

    let repo = git2::Repository::discover(&repo_root)
        .map_err(|e| RpcError::new(-32020, format!("discover repo: {e}")))?;

    let mut revwalk = repo
        .revwalk()
        .map_err(|e| RpcError::new(-32040, format!("revwalk: {e}")))?;

    if let Some(rev) = &params.revision {
        let obj = repo
            .revparse_single(rev)
            .map_err(|e| RpcError::new(-32041, format!("revparse {rev}: {e}")))?;
        revwalk
            .push(obj.id())
            .map_err(|e| RpcError::new(-32042, format!("push rev: {e}")))?;
    } else {
        revwalk
            .push_head()
            .map_err(|e| RpcError::new(-32043, format!("push head: {e}")))?;
    }

    if let Some(path) = &params.path {
        // revwalk doesn't support path filtering directly in libgit2 easily without
        // manually checking blobs, but we can set the sorting and manually filter.
        // For now, simpler to just let it walk and filter if max_count is small.
        // Actually, libgit2 has simplified path filtering if we use a different approach
        // but for a "compact one-liner" log, we'll just do the walk.
        tracing::debug!(path = %path, "path filtering in git.log is not yet optimized");
    }

    let mut commits = Vec::new();
    let max = params.max_count.unwrap_or(50);

    for id in revwalk {
        let id = id.map_err(|e| RpcError::new(-32044, format!("walk: {e}")))?;
        let commit = repo
            .find_commit(id)
            .map_err(|e| RpcError::new(-32045, format!("find commit: {e}")))?;

        // If path filtering is requested, we need to check if this commit touched the path.
        if let Some(path) = &params.path {
            let mut touched = false;
            if commit.parent_count() > 0 {
                let parent = commit.parent(0).unwrap();
                let tree = commit.tree().unwrap();
                let parent_tree = parent.tree().unwrap();
                let diff = repo
                    .diff_tree_to_tree(Some(&parent_tree), Some(&tree), None)
                    .unwrap();
                for delta in diff.deltas() {
                    if delta
                        .new_file()
                        .path()
                        .is_some_and(|p| p.to_string_lossy() == *path)
                        || delta
                            .old_file()
                            .path()
                            .is_some_and(|p| p.to_string_lossy() == *path)
                    {
                        touched = true;
                        break;
                    }
                }
            } else {
                // First commit
                touched = true;
            }
            if !touched {
                continue;
            }
        }

        let author = commit.author();
        let author_name = author.name().unwrap_or("unknown").to_string();
        let time = commit.time();
        let date = chrono::DateTime::from_timestamp(time.seconds(), 0)
            .map(|d| d.to_rfc3339())
            .unwrap_or_else(|| "unknown".to_string());

        commits.push(GitCommit {
            sha: id.to_string(),
            author: author_name,
            date,
            message: commit.summary().unwrap_or("").to_string(),
        });

        if commits.len() >= max {
            break;
        }
    }

    let result = GitLogResult { commits };
    let raw_bytes = serialized_size(&result);
    let value = serde_json::to_value(&result).unwrap();
    // For git.log we don't have a separate compact mode yet,
    // it's already compact (summaries only).
    daemon
        .metrics
        .record(protocol::methods::GIT_LOG, raw_bytes, raw_bytes);
    Ok(value)
}

pub fn git_diff(daemon: &Daemon, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
    let params: GitDiffParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))?;
    let repo_root = match params.repo {
        Some(r) => resolve_within(&daemon.root, &r)?,
        None => daemon.root.clone(),
    };

    let repo = git2::Repository::discover(&repo_root)
        .map_err(|e| RpcError::new(-32020, format!("discover repo: {e}")))?;

    let mut opts = git2::DiffOptions::new();
    if let Some(path) = &params.path {
        opts.pathspec(path);
    }

    let diff = if let Some(target_rev) = &params.target {
        // Diff between two revisions
        let base_rev = params.base.as_deref().unwrap_or("HEAD");
        let base_obj = repo
            .revparse_single(base_rev)
            .map_err(|e| RpcError::new(-32041, format!("revparse {base_rev}: {e}")))?
            .peel_to_tree()
            .map_err(|e| RpcError::new(-32046, format!("peel to tree: {e}")))?;
        let target_obj = repo
            .revparse_single(target_rev)
            .map_err(|e| RpcError::new(-32041, format!("revparse {target_rev}: {e}")))?
            .peel_to_tree()
            .map_err(|e| RpcError::new(-32046, format!("peel to tree: {e}")))?;

        repo.diff_tree_to_tree(Some(&base_obj), Some(&target_obj), Some(&mut opts))
            .map_err(|e| RpcError::new(-32047, format!("diff: {e}")))?
    } else {
        // Diff base against working tree
        let base_rev = params.base.as_deref().unwrap_or("HEAD");
        let base_obj = repo
            .revparse_single(base_rev)
            .map_err(|e| RpcError::new(-32041, format!("revparse {base_rev}: {e}")))?
            .peel_to_tree()
            .map_err(|e| RpcError::new(-32046, format!("peel to tree: {e}")))?;

        repo.diff_tree_to_workdir_with_index(Some(&base_obj), Some(&mut opts))
            .map_err(|e| RpcError::new(-32047, format!("diff: {e}")))?
    };

    let mut diff_str = String::new();
    diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
        let origin = line.origin();
        match origin {
            '+' | '-' | ' ' => diff_str.push(origin),
            _ => {}
        }
        diff_str.push_str(std::str::from_utf8(line.content()).unwrap_or(""));
        true
    })
    .map_err(|e| RpcError::new(-32048, format!("print diff: {e}")))?;

    let result = GitDiffResult { diff: diff_str };
    let raw_bytes = serialized_size(&result);
    let value = serde_json::to_value(&result).unwrap();
    // For now git.diff is just raw patch, future M7 work could condense it.
    daemon
        .metrics
        .record(protocol::methods::GIT_DIFF, raw_bytes, raw_bytes);
    Ok(value)
}

/// Approximate serialized byte cost of a response. We use the JSON
/// length here because that's what the bridge actually ships over UDS;
/// `metrics.gain` is meant to reflect the agent-visible cost of a
/// response, not in-memory size.
fn serialized_size<T: serde::Serialize>(value: &T) -> u64 {
    serde_json::to_vec(value)
        .map(|v| v.len() as u64)
        .unwrap_or(0)
}

fn format_status(status: git2::Status) -> String {
    let mut parts = Vec::new();
    if status.contains(git2::Status::INDEX_NEW) {
        parts.push("index_new");
    }
    if status.contains(git2::Status::INDEX_MODIFIED) {
        parts.push("index_modified");
    }
    if status.contains(git2::Status::INDEX_DELETED) {
        parts.push("index_deleted");
    }
    if status.contains(git2::Status::INDEX_RENAMED) {
        parts.push("index_renamed");
    }
    if status.contains(git2::Status::INDEX_TYPECHANGE) {
        parts.push("index_typechange");
    }
    if status.contains(git2::Status::WT_NEW) {
        parts.push("wt_new");
    }
    if status.contains(git2::Status::WT_MODIFIED) {
        parts.push("wt_modified");
    }
    if status.contains(git2::Status::WT_DELETED) {
        parts.push("wt_deleted");
    }
    if status.contains(git2::Status::WT_RENAMED) {
        parts.push("wt_renamed");
    }
    if status.contains(git2::Status::WT_TYPECHANGE) {
        parts.push("wt_typechange");
    }
    if status.contains(git2::Status::IGNORED) {
        parts.push("ignored");
    }
    if status.contains(git2::Status::CONFLICTED) {
        parts.push("conflicted");
    }
    if parts.is_empty() {
        "clean".to_string()
    } else {
        parts.join(",")
    }
}

pub fn search_grep(
    daemon: &Daemon,
    params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let params: SearchGrepParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))?;

    let search_root = match &params.path {
        Some(p) => resolve_within(&daemon.root, p)?,
        None => daemon.root.clone(),
    };

    // Snapshot the version first so the cache key is pinned to a point in
    // time. Any file change landing after this will bump the version and
    // invalidate the entry on next access.
    let (version, _) = daemon.changelog.snapshot();
    let context_lines = params.context.min(SEARCH_CONTEXT_CAP);
    let cache_key = SearchKey {
        pattern: params.pattern.clone(),
        path: params.path.clone(),
        glob: params.glob.clone(),
        max_results: params.max_results,
        case_insensitive: params.case_insensitive,
        context: context_lines,
    };
    if let Some(cached) = daemon.search_cache.get(version, &cache_key) {
        // Cache stores the raw form; compact on the way out if asked.
        return Ok(finalize_search(daemon, cached, params.compact));
    }

    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(params.case_insensitive)
        .build(&params.pattern)
        .map_err(|e| RpcError::new(-32030, format!("regex: {e}")))?;

    let max_hits = params.max_results.unwrap_or(SEARCH_DEFAULT_LIMIT);
    let mut hits: Vec<SearchHit> = Vec::new();
    let mut truncated = false;

    let mut walker = WalkBuilder::new(&search_root);
    walker.standard_filters(true).hidden(false);
    if let Some(glob) = &params.glob {
        let mut overrides = ignore::overrides::OverrideBuilder::new(&search_root);
        overrides
            .add(glob)
            .map_err(|e| RpcError::new(-32031, format!("glob: {e}")))?;
        let built = overrides
            .build()
            .map_err(|e| RpcError::new(-32032, format!("glob build: {e}")))?;
        walker.overrides(built);
    }

    for entry in walker.build() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path().to_path_buf();
        let rel_path = path
            .strip_prefix(&daemon.root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();

        let mut builder = SearcherBuilder::new();
        builder.line_number(true);
        if context_lines > 0 {
            builder.before_context(context_lines as usize);
            builder.after_context(context_lines as usize);
        }
        let mut searcher = builder.build();
        let mut sink = ContextSink::new(&rel_path, context_lines, &mut hits, max_hits);
        let result = searcher.search_path(&matcher, &path, &mut sink);
        if let Err(e) = result {
            tracing::debug!(path = %rel_path, error = %e, "search error");
        }
        if hits.len() >= max_hits {
            truncated = true;
            break;
        }
    }

    let raw = SearchGrepResult {
        hits,
        truncated,
        compact: None,
    };
    daemon.search_cache.insert(version, cache_key, raw.clone());
    Ok(finalize_search(daemon, raw, params.compact))
}

/// Apply optional compaction + record per-call gain metrics on a
/// search result that's already been computed (or just pulled from
/// cache). Always emits a JSON value ready to ship to the bridge.
fn finalize_search(
    daemon: &Daemon,
    raw: SearchGrepResult,
    compact_requested: bool,
) -> serde_json::Value {
    let raw_bytes = serialized_size(&raw);
    let result = if compact_requested {
        let compact = compact::search_grep_compact(&raw.hits);
        SearchGrepResult {
            hits: Vec::new(),
            truncated: raw.truncated,
            compact: Some(compact),
        }
    } else {
        raw
    };
    let value = serde_json::to_value(&result).unwrap();
    let compacted_bytes = serialized_size(&result);
    daemon
        .metrics
        .record(protocol::methods::SEARCH_GREP, raw_bytes, compacted_bytes);
    value
}

pub fn code_outline(
    daemon: &Daemon,
    params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let params: CodeOutlineParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))?;
    Ok(serde_json::to_value(code_outline_inner(daemon, &params)?).unwrap())
}

pub fn code_outline_batch(
    daemon: &Daemon,
    params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let params: CodeOutlineBatchParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))?;
    let responses = params
        .requests
        .into_iter()
        .map(|req| {
            let path = req.path.clone();
            match code_outline_inner(daemon, &req) {
                Ok(r) => CodeOutlineBatchItem {
                    path,
                    result: Some(r),
                    error: None,
                },
                Err(e) => CodeOutlineBatchItem {
                    path,
                    result: None,
                    error: Some(e),
                },
            }
        })
        .collect();
    Ok(serde_json::to_value(CodeOutlineBatchResult { responses }).unwrap())
}

fn code_outline_inner(
    daemon: &Daemon,
    params: &CodeOutlineParams,
) -> Result<CodeOutlineResult, RpcError> {
    let path = resolve_within(&daemon.root, &params.path)?;
    let (language, entries) = match daemon.backends.outline(&path, params.signatures_only)? {
        Some(r) => (Some(r.language.name().to_string()), r.entries),
        None => (None, Vec::new()),
    };
    Ok(CodeOutlineResult {
        path: params.path.clone(),
        language,
        entries,
    })
}

pub fn code_symbols(
    daemon: &Daemon,
    params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let params: CodeSymbolsParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))?;
    Ok(serde_json::to_value(code_symbols_inner(daemon, &params)?).unwrap())
}

pub fn code_symbols_batch(
    daemon: &Daemon,
    params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let params: CodeSymbolsBatchParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))?;
    let responses = params
        .requests
        .into_iter()
        .map(|req| {
            let path = req.path.clone();
            match code_symbols_inner(daemon, &req) {
                Ok(r) => CodeSymbolsBatchItem {
                    path,
                    result: Some(r),
                    error: None,
                },
                Err(e) => CodeSymbolsBatchItem {
                    path,
                    result: None,
                    error: Some(e),
                },
            }
        })
        .collect();
    Ok(serde_json::to_value(CodeSymbolsBatchResult { responses }).unwrap())
}

fn code_symbols_inner(
    daemon: &Daemon,
    params: &CodeSymbolsParams,
) -> Result<CodeSymbolsResult, RpcError> {
    let path = resolve_within(&daemon.root, &params.path)?;
    let (language, names) = match daemon.backends.symbols(&path)? {
        Some(r) => (Some(r.language.name().to_string()), r.names),
        None => (None, Vec::new()),
    };
    Ok(CodeSymbolsResult {
        path: params.path.clone(),
        language,
        names,
    })
}

pub fn code_imports(
    daemon: &Daemon,
    params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let params: CodeImportsParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))?;
    let result = code_imports_inner(daemon, &params.path)?;
    Ok(serde_json::to_value(result).unwrap())
}

fn code_imports_inner(daemon: &Daemon, path: &str) -> Result<CodeImportsResult, RpcError> {
    let abs = resolve_within(&daemon.root, path)?;
    let language = Language::detect(&abs);
    let content =
        fs::read_to_string(&abs).map_err(|e| RpcError::new(-32080, format!("read: {e}")))?;
    let base_dir = abs.parent().unwrap_or(&daemon.root);
    let imports = language
        .map(|lang| extract_imports(lang, &content, base_dir, &daemon.root))
        .unwrap_or_default();
    Ok(CodeImportsResult {
        path: path.to_string(),
        language: language.map(|l| l.name().to_string()),
        imports,
    })
}

fn extract_imports(
    lang: Language,
    content: &str,
    base_dir: &Path,
    root: &Path,
) -> Vec<ImportEntry> {
    let mut imports = Vec::new();
    let mut go_block = false;
    for (idx, raw_line) in content.lines().enumerate() {
        let line_no = idx as u32 + 1;
        let line = raw_line.trim();
        match lang {
            Language::Rust => {
                if let Some(module) = line
                    .strip_prefix("use ")
                    .and_then(|s| s.trim_end_matches(';').split_whitespace().next())
                    .or_else(|| {
                        line.strip_prefix("pub use ")
                            .and_then(|s| s.trim_end_matches(';').split_whitespace().next())
                    })
                {
                    imports.push(import_entry(module, line_no, base_dir, root, lang));
                } else if let Some(module) = line
                    .strip_prefix("mod ")
                    .and_then(|s| s.trim_end_matches(';').split_whitespace().next())
                {
                    imports.push(import_entry(module, line_no, base_dir, root, lang));
                }
            }
            Language::Python => {
                if let Some(rest) = line.strip_prefix("import ") {
                    for module in rest.split(',').filter_map(|s| s.split_whitespace().next()) {
                        imports.push(import_entry(module, line_no, base_dir, root, lang));
                    }
                } else if let Some(rest) = line.strip_prefix("from ") {
                    if let Some(module) = rest.split_whitespace().next() {
                        imports.push(import_entry(module, line_no, base_dir, root, lang));
                    }
                }
            }
            Language::TypeScript | Language::Tsx => {
                if let Some(module) = quoted_module(line) {
                    if line.starts_with("import ")
                        || line.starts_with("export ")
                        || line.contains(" require(")
                    {
                        imports.push(import_entry(&module, line_no, base_dir, root, lang));
                    }
                }
            }
            Language::Go => {
                if line == "import (" {
                    go_block = true;
                    continue;
                }
                if go_block && line == ")" {
                    go_block = false;
                    continue;
                }
                if let Some(module) = quoted_module(line.strip_prefix("import ").unwrap_or(line)) {
                    if go_block || line.starts_with("import ") {
                        imports.push(import_entry(&module, line_no, base_dir, root, lang));
                    }
                }
            }
            Language::C | Language::Cpp => {
                if let Some(module) = line.strip_prefix("#include").and_then(quoted_module) {
                    imports.push(import_entry(&module, line_no, base_dir, root, lang));
                }
            }
        }
    }
    imports
}

fn quoted_module(line: &str) -> Option<String> {
    let start = line.find(['"', '\''])?;
    let quote = line.as_bytes()[start] as char;
    let tail = &line[start + 1..];
    let end = tail.find(quote)?;
    Some(tail[..end].to_string())
}

fn import_entry(
    module: &str,
    line: u32,
    base_dir: &Path,
    root: &Path,
    lang: Language,
) -> ImportEntry {
    ImportEntry {
        module: module.to_string(),
        resolved_path: resolve_import_path(module, base_dir, root, lang),
        line,
    }
}

fn resolve_import_path(
    module: &str,
    base_dir: &Path,
    root: &Path,
    lang: Language,
) -> Option<String> {
    let candidates: Vec<PathBuf> = match lang {
        Language::TypeScript | Language::Tsx if module.starts_with('.') => {
            ["ts", "tsx", "js", "jsx"]
                .iter()
                .flat_map(|ext| {
                    [
                        base_dir.join(format!("{module}.{ext}")),
                        base_dir.join(module).join(format!("index.{ext}")),
                    ]
                })
                .collect()
        }
        Language::Python if module.starts_with('.') => {
            let trimmed = module.trim_start_matches('.').replace('.', "/");
            vec![
                base_dir.join(format!("{trimmed}.py")),
                base_dir.join(&trimmed).join("__init__.py"),
            ]
        }
        Language::Rust => {
            let rel = module.split("::").next().unwrap_or(module);
            vec![
                base_dir.join(format!("{rel}.rs")),
                base_dir.join(rel).join("mod.rs"),
            ]
        }
        _ => Vec::new(),
    };
    candidates
        .into_iter()
        .find(|p| p.exists())
        .and_then(|p| p.canonicalize().ok())
        .and_then(|p| {
            p.strip_prefix(root)
                .ok()
                .map(|r| r.to_string_lossy().to_string())
        })
}

pub fn code_dependencies(
    daemon: &Daemon,
    params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let params: CodeDependenciesParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))?;
    let focus = params.path.clone();
    let files = source_files(&daemon.root, params.max_files.unwrap_or(4096));
    let mut all_edges = Vec::new();
    for path in &files {
        if let Ok(imports) = code_imports_inner(daemon, path) {
            for import in imports.imports {
                if let Some(to) = import.resolved_path.clone() {
                    all_edges.push(DependencyEdge {
                        from: path.clone(),
                        to,
                        module: import.module,
                        line: import.line,
                    });
                }
            }
        }
    }
    let (dependencies, dependents) = if let Some(focus) = focus {
        (
            all_edges
                .iter()
                .filter(|e| e.from == focus)
                .cloned()
                .collect(),
            all_edges.into_iter().filter(|e| e.to == focus).collect(),
        )
    } else {
        (all_edges, Vec::new())
    };
    Ok(serde_json::to_value(CodeDependenciesResult {
        files_scanned: files.len(),
        dependencies,
        dependents,
    })
    .unwrap())
}

pub fn code_find_occurrences(
    daemon: &Daemon,
    params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let params: CodeFindOccurrencesParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))?;
    if params.identifier.is_empty() {
        return Err(RpcError::new(-32602, "identifier must not be empty"));
    }
    let max = params.max_results.unwrap_or(200);
    let files = match params.path {
        Some(path) => vec![path],
        None => source_files(&daemon.root, 4096),
    };
    let mut occurrences = Vec::new();
    let mut truncated = false;
    for rel in files {
        let abs = resolve_within(&daemon.root, &rel)?;
        let Some(parsed) = daemon
            .parse_cache
            .get_or_parse(&abs)
            .map_err(|e| RpcError::new(-32041, format!("parse {}: {e}", abs.display())))?
        else {
            continue;
        };
        collect_identifier_occurrences(
            parsed.tree.root_node(),
            &parsed.source,
            &rel,
            &params.identifier,
            max,
            &mut occurrences,
        );
        if occurrences.len() >= max {
            truncated = true;
            break;
        }
    }
    Ok(serde_json::to_value(CodeFindOccurrencesResult {
        occurrences,
        truncated,
    })
    .unwrap())
}

fn collect_identifier_occurrences(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    path: &str,
    needle: &str,
    max: usize,
    out: &mut Vec<CodeOccurrence>,
) {
    if out.len() >= max {
        return;
    }
    let kind = node.kind();
    if matches!(
        kind,
        "identifier" | "field_identifier" | "type_identifier" | "property_identifier"
    ) {
        if node.utf8_text(source).ok() == Some(needle) {
            let pos = node.start_position();
            out.push(CodeOccurrence {
                path: path.to_string(),
                line: pos.row as u32 + 1,
                column: pos.column as u32 + 1,
                kind: kind.to_string(),
            });
        }
        return;
    }
    if kind.contains("comment") || kind.contains("string") {
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_identifier_occurrences(child, source, path, needle, max, out);
        if out.len() >= max {
            break;
        }
    }
}

pub fn fs_read_skeleton(
    daemon: &Daemon,
    params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let params: FsReadSkeletonParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))?;
    let outline = code_outline_inner(
        daemon,
        &CodeOutlineParams {
            path: params.path.clone(),
            signatures_only: true,
        },
    )?;
    let abs = resolve_within(&daemon.root, &params.path)?;
    let content =
        fs::read_to_string(&abs).map_err(|e| RpcError::new(-32080, format!("read: {e}")))?;
    let mut lines: Vec<String> = content.lines().map(|l| format!("{l}\n")).collect();
    if !content.ends_with('\n') {
        if let Some(last) = lines.last_mut() {
            last.pop();
        }
    }
    let mut elided = Vec::new();
    for entry in outline.entries.iter().rev() {
        let keep = params.target_symbol.as_deref() == Some(entry.name.as_str())
            || params
                .target_line
                .is_some_and(|line| line >= entry.start_line && line <= entry.end_line);
        if keep || entry.end_line <= entry.start_line + 2 {
            continue;
        }
        let start = entry.start_line as usize;
        let end = entry.end_line as usize;
        if start >= end || end > lines.len() {
            continue;
        }
        let removed = end - start;
        lines.splice(
            start..end,
            [format!(
                "// ... {removed} lines elided from {} ...\n",
                entry.name
            )],
        );
        elided.push(SkeletonElidedRegion {
            symbol: entry.name.clone(),
            start_line: entry.start_line + 1,
            end_line: entry.end_line,
            lines: removed as u32,
        });
    }
    elided.reverse();
    Ok(serde_json::to_value(FsReadSkeletonResult {
        path: params.path,
        language: outline.language,
        content: lines.concat(),
        elided_regions: elided,
    })
    .unwrap())
}

fn source_files(root: &Path, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    for entry in WalkBuilder::new(root)
        .standard_filters(true)
        .hidden(true)
        .build()
    {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if Language::detect(path).is_none() {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        out.push(rel);
        if out.len() >= max {
            break;
        }
    }
    out
}

pub fn tool_run(daemon: &Daemon, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
    let params: ToolRunParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))?;
    if params.command.trim().is_empty() {
        return Err(RpcError::new(-32602, "command must not be empty"));
    }
    let result = tool_run_inner(daemon, &params)?;
    Ok(serde_json::to_value(result).unwrap())
}

fn tool_run_inner(daemon: &Daemon, params: &ToolRunParams) -> Result<ToolRunResult, RpcError> {
    let cwd = match &params.cwd {
        Some(cwd) => resolve_within(&daemon.root, cwd)?,
        None => daemon.root.clone(),
    };
    let cwd_display = cwd
        .strip_prefix(&daemon.root)
        .unwrap_or(&cwd)
        .to_string_lossy()
        .to_string();
    let (version, _) = daemon.changelog.snapshot();
    let cache_key = if params.cache {
        Some(tool_run_cache_key(version, &cwd_display, params))
    } else {
        None
    };
    if let Some(key) = &cache_key {
        if let Some(mut cached) = daemon.tool_run_cache.lock().get(key).cloned() {
            cached.cached = true;
            return Ok(cached);
        }
    }

    let output = Command::new(&params.command)
        .args(&params.args)
        .current_dir(&cwd)
        .envs(params.env.iter().map(|e| (&e.name, &e.value)))
        .output()
        .map_err(|e| RpcError::new(-32070, format!("spawn {}: {e}", params.command)))?;

    let limit = params
        .max_output_bytes
        .unwrap_or(TOOL_RUN_DEFAULT_OUTPUT_LIMIT)
        .max(1);
    let (stdout, stdout_truncated) = truncate_bytes(&output.stdout, limit);
    let (stderr, stderr_truncated) = truncate_bytes(&output.stderr, limit);
    let success = output.status.success();
    let failure_output = if success {
        String::new()
    } else {
        failure_tail(&output.stdout, &output.stderr)
    };
    let result = ToolRunResult {
        command: params.command.clone(),
        args: params.args.clone(),
        cwd: cwd_display,
        exit_code: output.status.code(),
        success,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
        failure_output,
        cached: false,
    };

    let raw_bytes = serialized_size(&result);
    daemon
        .metrics
        .record(protocol::methods::TOOL_RUN, raw_bytes, raw_bytes);

    if success {
        if let Some(key) = cache_key {
            daemon.tool_run_cache.lock().insert(key, result.clone());
        }
    }
    Ok(result)
}

fn tool_run_cache_key(version: u64, cwd: &str, params: &ToolRunParams) -> String {
    serde_json::json!({
        "version": version,
        "cwd": cwd,
        "command": params.command,
        "args": params.args,
        "env": params.env,
        "max_output_bytes": params.max_output_bytes,
    })
    .to_string()
}

fn truncate_bytes(bytes: &[u8], max: usize) -> (String, bool) {
    if bytes.len() <= max {
        return (String::from_utf8_lossy(bytes).into_owned(), false);
    }
    let start = bytes.len().saturating_sub(max);
    (
        format!(
            "[[mcp-cli: output truncated to last {max} bytes]]\n{}",
            String::from_utf8_lossy(&bytes[start..])
        ),
        true,
    )
}

fn failure_tail(stdout: &[u8], stderr: &[u8]) -> String {
    let mut combined = Vec::with_capacity(stdout.len() + stderr.len() + 16);
    if !stdout.is_empty() {
        combined.extend_from_slice(b"$ stdout\n");
        combined.extend_from_slice(stdout);
    }
    if !stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with(b"\n") {
            combined.push(b'\n');
        }
        combined.extend_from_slice(b"$ stderr\n");
        combined.extend_from_slice(stderr);
    }
    truncate_bytes(&combined, TOOL_RUN_FAILURE_TAIL_LIMIT).0
}

pub fn tool_gh(daemon: &Daemon, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
    let params: ToolGhParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))?;
    let kind = params.kind.as_str();
    if kind != "pr" && kind != "issue" {
        return Err(RpcError::new(-32602, "kind must be `pr` or `issue`"));
    }

    let default_fields = match kind {
        "pr" => "number,title,state,isDraft,author,headRefName,baseRefName,url,statusCheckRollup",
        "issue" => "number,title,state,author,assignees,labels,url,comments",
        _ => unreachable!(),
    };
    let fields = if params.fields.is_empty() {
        default_fields.to_string()
    } else {
        params.fields.join(",")
    };

    let mut cmd = Command::new("gh");
    cmd.current_dir(&daemon.root).arg(kind).arg("view");
    if let Some(selector) = &params.selector {
        cmd.arg(selector);
    }
    if let Some(repo) = &params.repo {
        cmd.arg("--repo").arg(repo);
    }
    cmd.arg("--json").arg(&fields);

    let output = cmd
        .output()
        .map_err(|e| RpcError::new(-32071, format!("spawn gh: {e}")))?;
    let (stdout, _) = truncate_bytes(&output.stdout, TOOL_RUN_DEFAULT_OUTPUT_LIMIT);
    let (stderr, _) = truncate_bytes(&output.stderr, TOOL_RUN_DEFAULT_OUTPUT_LIMIT);
    let value = if output.status.success() {
        serde_json::from_slice(&output.stdout).ok()
    } else {
        None
    };
    let result = ToolGhResult {
        kind: params.kind,
        selector: params.selector,
        exit_code: output.status.code(),
        success: output.status.success(),
        value,
        stdout,
        stderr,
    };
    let raw_bytes = serialized_size(&result);
    daemon
        .metrics
        .record(protocol::methods::TOOL_GH, raw_bytes, raw_bytes);
    Ok(serde_json::to_value(result).unwrap())
}

pub fn metrics_gain(
    daemon: &Daemon,
    params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    // Params struct is empty today; reject anything unexpected so a
    // future schema change can pin the parser without breaking clients.
    let _params: MetricsGainParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))?;
    Ok(serde_json::to_value(daemon.metrics.snapshot()).unwrap())
}

pub fn metrics_tool_latency(
    daemon: &Daemon,
    params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let _params: MetricsToolLatencyParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))?;
    Ok(serde_json::to_value(daemon.metrics.snapshot_latency()).unwrap())
}

/// Custom `grep-searcher` Sink that emits matches *with their
/// surrounding context lines already attached*. Collapses the common
/// "grep then fs_read around the hit" two-call pattern into a single
/// `search_grep` round-trip.
///
/// Lifetime soup: we borrow `&mut Vec<SearchHit>` across files so the
/// top-level handler can cap total hits — each new file gets a fresh
/// sink but appends to the same hit vec.
struct ContextSink<'a> {
    path: &'a str,
    hits: &'a mut Vec<SearchHit>,
    max_hits: usize,
    /// Index into `hits[]` of the most recent match this sink emitted
    /// for the current file. `None` at the start of each file (the
    /// handler constructs one sink per file). Used to attach After /
    /// Other context lines and any leftover pending on `finish`.
    last_hit_idx: Option<usize>,
    /// Before-context lines buffered ahead of the next match.
    /// `grep-searcher` emits context in file order with the kind
    /// (Before / After / Other) marked, so classification is
    /// explicit — no line-number arithmetic required.
    pending_before: Vec<SearchContextLine>,
}

impl<'a> ContextSink<'a> {
    fn new(
        path: &'a str,
        _context_lines: u32,
        hits: &'a mut Vec<SearchHit>,
        max_hits: usize,
    ) -> Self {
        Self {
            path,
            hits,
            max_hits,
            last_hit_idx: None,
            pending_before: Vec::new(),
        }
    }

    fn strip_crlf(bytes: &[u8]) -> String {
        let mut s = String::from_utf8_lossy(bytes).into_owned();
        if s.ends_with('\n') {
            s.pop();
        }
        if s.ends_with('\r') {
            s.pop();
        }
        s
    }
}

impl Sink for ContextSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        if self.hits.len() >= self.max_hits {
            return Ok(false);
        }
        let line_number = mat.line_number().unwrap_or(0);
        let line = Self::strip_crlf(mat.bytes());
        let context = std::mem::take(&mut self.pending_before);
        self.last_hit_idx = Some(self.hits.len());
        self.hits.push(SearchHit {
            path: self.path.to_string(),
            line_number,
            line,
            context,
        });
        // Continue unless we just hit the cap on the *match* count
        // (which will stop the per-file search immediately).
        Ok(self.hits.len() < self.max_hits)
    }

    fn context(&mut self, _: &Searcher, ctx: &SinkContext<'_>) -> Result<bool, Self::Error> {
        // After the match cap is hit we stop appending context too —
        // a trailing After block from an earlier match is harmless to
        // drop since the handler already decided to truncate.
        if self.hits.len() >= self.max_hits {
            return Ok(false);
        }
        let entry = SearchContextLine {
            line_number: ctx.line_number().unwrap_or(0),
            line: Self::strip_crlf(ctx.bytes()),
        };
        match ctx.kind() {
            SinkContextKind::Before => self.pending_before.push(entry),
            SinkContextKind::After | SinkContextKind::Other => {
                if let Some(idx) = self.last_hit_idx {
                    self.hits[idx].context.push(entry);
                }
                // Otherwise we saw context without a preceding match
                // in this file (shouldn't happen for a well-formed
                // Before stream; defensive drop).
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Repository, Signature};
    use std::fs;
    use tempfile::tempdir;

    fn create_test_repo(path: &std::path::Path) -> Repository {
        let repo = Repository::init(path).unwrap();
        let sig = Signature::now("Test User", "test@example.com").unwrap();

        {
            let mut index = repo.index().unwrap();
            // Commit 1
            fs::write(path.join("file1.txt"), "hello").unwrap();
            index.add_path(std::path::Path::new("file1.txt")).unwrap();
            index.write().unwrap();
            let tree_id = index.write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "First commit", &tree, &[])
                .unwrap();
        }

        {
            let mut index = repo.index().unwrap();
            // Commit 2
            fs::write(path.join("file2.txt"), "world").unwrap();
            index.add_path(std::path::Path::new("file2.txt")).unwrap();
            index.write().unwrap();
            let tree_id = index.write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            let parent = repo.head().unwrap().peel_to_commit().unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "Second commit", &tree, &[&parent])
                .unwrap();
        }

        repo
    }

    fn test_daemon(root: &std::path::Path) -> Daemon {
        let parse_cache = std::sync::Arc::new(crate::parse_cache::ParseCache::new(10));
        Daemon {
            root: root.canonicalize().unwrap(),
            changelog: std::sync::Arc::new(crate::changelog::ChangeLog::with_capacity(10)),
            search_cache: std::sync::Arc::new(crate::search_cache::SearchCache::new(10)),
            tool_run_cache: std::sync::Arc::new(parking_lot::Mutex::new(
                std::collections::HashMap::new(),
            )),
            parse_cache: parse_cache.clone(),
            backends: {
                let mut reg = crate::backends::BackendRegistry::new();
                reg.register(std::sync::Arc::new(
                    crate::backends::TreeSitterBackend::new(parse_cache),
                ));
                reg
            },
            frame_pool: std::sync::Arc::new(crate::buffer_pool::BufferPool::new(1, 1024)),
            metrics: std::sync::Arc::new(crate::metrics::ToolMetrics::new()),
        }
    }

    #[test]
    fn test_git_log_basic() {
        let tmp = tempdir().unwrap();
        let repo_path = tmp.path();
        let _repo = create_test_repo(repo_path);

        let daemon = test_daemon(repo_path);

        let params = serde_json::json!({});
        let result = git_log(&daemon, params).unwrap();
        let log: GitLogResult = serde_json::from_value(result).unwrap();

        assert_eq!(log.commits.len(), 2);
        assert_eq!(log.commits[0].message, "Second commit");
        assert_eq!(log.commits[1].message, "First commit");
        assert_eq!(log.commits[0].author, "Test User");
    }

    #[test]
    fn test_git_log_max_count() {
        let tmp = tempdir().unwrap();
        let repo_path = tmp.path();
        let _repo = create_test_repo(repo_path);

        let daemon = test_daemon(repo_path);

        let params = serde_json::json!({"max_count": 1});
        let result = git_log(&daemon, params).unwrap();
        let log: GitLogResult = serde_json::from_value(result).unwrap();

        assert_eq!(log.commits.len(), 1);
        assert_eq!(log.commits[0].message, "Second commit");
    }

    #[test]
    fn test_git_log_path_filter() {
        let tmp = tempdir().unwrap();
        let repo_path = tmp.path();
        let _repo = create_test_repo(repo_path);

        let daemon = test_daemon(repo_path);

        let params = serde_json::json!({"path": "file1.txt"});
        let result = git_log(&daemon, params).unwrap();
        let log: GitLogResult = serde_json::from_value(result).unwrap();

        // libgit2 path filtering implementation I wrote checks if the path was touched.
        // file1.txt was touched in the first commit.
        assert_eq!(log.commits.len(), 1);
        assert_eq!(log.commits[0].message, "First commit");
    }

    #[test]
    fn test_git_diff_basic() {
        let tmp = tempdir().unwrap();
        let repo_path = tmp.path();
        let _repo = create_test_repo(repo_path);

        let daemon = test_daemon(repo_path);

        // Diff between HEAD^ and HEAD
        let params = serde_json::json!({"base": "HEAD^", "target": "HEAD"});
        let result = git_diff(&daemon, params).unwrap();
        let diff_res: GitDiffResult = serde_json::from_value(result).unwrap();

        assert!(diff_res.diff.contains("+++ b/file2.txt"));
        assert!(diff_res.diff.contains("+world"));
    }

    #[test]
    fn test_tool_run_truncates_and_reports_failure_tail() {
        let tmp = tempdir().unwrap();
        let daemon = test_daemon(tmp.path());
        let params = serde_json::json!({
            "command": "sh",
            "args": ["-c", "printf abcdefghij; printf errormsg >&2; exit 7"],
            "max_output_bytes": 4
        });
        let result = tool_run(&daemon, params).unwrap();
        let run: ToolRunResult = serde_json::from_value(result).unwrap();

        assert_eq!(run.exit_code, Some(7));
        assert!(!run.success);
        assert!(run.stdout_truncated);
        assert!(run.stderr_truncated);
        assert!(run.stdout.ends_with("ghij"));
        assert!(run.stderr.ends_with("rmsg"));
        assert!(run.failure_output.contains("$ stdout"));
        assert!(run.failure_output.contains("$ stderr"));
    }

    #[test]
    fn test_tool_run_cache_reuses_successful_result_at_same_version() {
        let tmp = tempdir().unwrap();
        let daemon = test_daemon(tmp.path());
        let params = serde_json::json!({
            "command": "sh",
            "args": ["-c", "printf cached"],
            "cache": true
        });

        let first: ToolRunResult =
            serde_json::from_value(tool_run(&daemon, params.clone()).unwrap()).unwrap();
        let second: ToolRunResult =
            serde_json::from_value(tool_run(&daemon, params).unwrap()).unwrap();

        assert_eq!(first.stdout, "cached");
        assert!(!first.cached);
        assert!(second.cached);
        assert_eq!(second.stdout, "cached");
    }

    #[test]
    fn test_fs_read_exposes_occ_tokens() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "one\n").unwrap();
        let daemon = test_daemon(tmp.path());

        let result = fs_read(&daemon, serde_json::json!({"path": "a.txt"})).unwrap();
        let read: FsReadResult = serde_json::from_value(result).unwrap();

        assert_eq!(read.version, 0);
        assert!(read.mtime_ns > 0);
        assert_eq!(read.content, "one\n");
    }

    #[test]
    fn test_fs_replace_all_rejects_stale_version() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "one one\n").unwrap();
        let daemon = test_daemon(tmp.path());
        daemon
            .changelog
            .record("other.txt".to_string(), ChangeKind::Modified);

        let err = fs_replace_all(
            &daemon,
            serde_json::json!({
                "path": "a.txt",
                "search": "one",
                "replacement": "two",
                "expected_version": 0
            }),
        )
        .expect_err("stale version should be rejected");

        assert_eq!(err.code, -32082);
        assert_eq!(
            fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "one one\n"
        );
    }

    #[test]
    fn test_fs_replace_all_replaces_and_advances_changelog() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "one one\n").unwrap();
        let daemon = test_daemon(tmp.path());

        let result = fs_replace_all(
            &daemon,
            serde_json::json!({
                "path": "a.txt",
                "search": "one",
                "replacement": "two",
                "expected_version": 0,
                "max_replacements": 2
            }),
        )
        .unwrap();
        let replaced: FsReplaceAllResult = serde_json::from_value(result).unwrap();

        assert_eq!(replaced.replacements, 2);
        assert_eq!(replaced.version, 1);
        assert_eq!(
            fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "two two\n"
        );
    }

    #[test]
    fn test_fs_apply_patch_applies_clean_hunk() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let daemon = test_daemon(tmp.path());
        let patch = "\
--- a/a.txt
+++ b/a.txt
@@ -1,3 +1,3 @@
 one
-two
+TWO
 three
";

        let result = fs_apply_patch(
            &daemon,
            serde_json::json!({
                "path": "a.txt",
                "patch": patch,
                "expected_version": 0
            }),
        )
        .unwrap();
        let applied: FsApplyPatchResult = serde_json::from_value(result).unwrap();

        assert!(applied.applied);
        assert_eq!(applied.version, 1);
        assert_eq!(
            fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "one\nTWO\nthree\n"
        );
    }

    #[test]
    fn test_fs_apply_patch_rejects_context_mismatch() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let daemon = test_daemon(tmp.path());
        let patch = "\
@@ -1,3 +1,3 @@
 one
-missing
+TWO
 three
";

        let err = fs_apply_patch(
            &daemon,
            serde_json::json!({
                "path": "a.txt",
                "patch": patch
            }),
        )
        .expect_err("mismatched patch should fail");

        assert_eq!(err.code, -32084);
        assert_eq!(
            fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "one\ntwo\nthree\n"
        );
    }

    #[test]
    fn test_code_imports_resolves_relative_typescript_import() {
        let tmp = tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(
            tmp.path().join("src/auth.ts"),
            "export function login() {}\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("src/app.ts"),
            "import { login } from './auth';\nlogin();\n",
        )
        .unwrap();
        let daemon = test_daemon(tmp.path());

        let result = code_imports(&daemon, serde_json::json!({"path": "src/app.ts"})).unwrap();
        let imports: CodeImportsResult = serde_json::from_value(result).unwrap();

        assert_eq!(imports.imports.len(), 1);
        assert_eq!(imports.imports[0].module, "./auth");
        assert_eq!(
            imports.imports[0].resolved_path.as_deref(),
            Some("src/auth.ts")
        );
    }

    #[test]
    fn test_code_dependencies_reports_reverse_dependents() {
        let tmp = tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/auth.ts"), "export const token = 1;\n").unwrap();
        fs::write(
            tmp.path().join("src/app.ts"),
            "import { token } from './auth';\nconsole.log(token);\n",
        )
        .unwrap();
        let daemon = test_daemon(tmp.path());

        let result =
            code_dependencies(&daemon, serde_json::json!({"path": "src/auth.ts"})).unwrap();
        let deps: CodeDependenciesResult = serde_json::from_value(result).unwrap();

        assert!(deps.dependencies.is_empty());
        assert_eq!(deps.dependents.len(), 1);
        assert_eq!(deps.dependents[0].from, "src/app.ts");
    }

    #[test]
    fn test_code_find_occurrences_ignores_strings_and_comments() {
        let tmp = tempdir().unwrap();
        fs::write(
            tmp.path().join("a.rs"),
            "fn main() {\n let target = 1;\n println!(\"target\");\n // target\n target;\n}\n",
        )
        .unwrap();
        let daemon = test_daemon(tmp.path());

        let result = code_find_occurrences(
            &daemon,
            serde_json::json!({"path": "a.rs", "identifier": "target"}),
        )
        .unwrap();
        let found: CodeFindOccurrencesResult = serde_json::from_value(result).unwrap();

        let lines: Vec<u32> = found.occurrences.iter().map(|o| o.line).collect();
        assert_eq!(lines, vec![2, 5]);
    }

    #[test]
    fn test_fs_read_skeleton_elides_non_target_bodies() {
        let tmp = tempdir().unwrap();
        fs::write(
            tmp.path().join("a.rs"),
            "fn keep() {\n let a = 1;\n let b = 2;\n}\n\nfn fold() {\n let c = 3;\n let d = 4;\n}\n",
        )
        .unwrap();
        let daemon = test_daemon(tmp.path());

        let result = fs_read_skeleton(
            &daemon,
            serde_json::json!({"path": "a.rs", "target_symbol": "keep"}),
        )
        .unwrap();
        let skeleton: FsReadSkeletonResult = serde_json::from_value(result).unwrap();

        assert!(skeleton.content.contains("let a = 1"));
        assert!(skeleton.content.contains("lines elided from fold"));
        assert!(!skeleton.content.contains("let c = 3"));
    }
}
