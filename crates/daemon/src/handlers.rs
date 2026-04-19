use std::fs::File;

use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use grep_searcher::SearcherBuilder;
use ignore::WalkBuilder;
use memmap2::Mmap;
use protocol::{
    FsReadParams, FsReadResult, GitStatusEntry, GitStatusParams, GitStatusResult, RpcError,
    SearchGrepParams, SearchGrepResult, SearchHit,
};

use crate::server::{resolve_within, Daemon};

const FS_READ_DEFAULT_LIMIT: u64 = 256 * 1024;
const SEARCH_DEFAULT_LIMIT: usize = 200;

pub fn fs_read(daemon: &Daemon, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
    let params: FsReadParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))?;
    let path = resolve_within(&daemon.root, &params.path)?;

    let file = File::open(&path).map_err(|e| RpcError::new(-32010, format!("open: {e}")))?;
    let total_size = file
        .metadata()
        .map_err(|e| RpcError::new(-32011, format!("stat: {e}")))?
        .len();

    if total_size == 0 {
        return Ok(serde_json::to_value(FsReadResult {
            path: params.path,
            bytes_read: 0,
            total_size: 0,
            content: String::new(),
            truncated: false,
        })
        .unwrap());
    }

    // Safe: we only read the mapping; another process modifying the file mid-read
    // would risk SIGBUS, but for source-tree workloads this is the standard tradeoff.
    let mmap = unsafe { Mmap::map(&file) }.map_err(|e| RpcError::new(-32012, format!("mmap: {e}")))?;

    let offset = params.offset.min(total_size);
    let remaining = total_size - offset;
    let limit = params.length.unwrap_or(FS_READ_DEFAULT_LIMIT).min(remaining);
    let end = offset + limit;
    let slice = &mmap[offset as usize..end as usize];

    let content = String::from_utf8_lossy(slice).into_owned();
    let truncated = end < total_size;
    Ok(serde_json::to_value(FsReadResult {
        path: params.path,
        bytes_read: limit,
        total_size,
        content,
        truncated,
    })
    .unwrap())
}

pub fn git_status(daemon: &Daemon, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
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
    let head_oid = head.as_ref().and_then(|h| h.target().map(|o| o.to_string()));

    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true).recurse_untracked_dirs(true);
    let statuses = repo
        .statuses(Some(&mut opts))
        .map_err(|e| RpcError::new(-32021, format!("statuses: {e}")))?;

    let entries = statuses
        .iter()
        .filter_map(|s| {
            let path = s.path()?.to_string();
            Some(GitStatusEntry { path, status: format_status(s.status()) })
        })
        .collect();

    Ok(serde_json::to_value(GitStatusResult { branch, head: head_oid, entries }).unwrap())
}

fn format_status(status: git2::Status) -> String {
    let mut parts = Vec::new();
    if status.contains(git2::Status::INDEX_NEW) { parts.push("index_new"); }
    if status.contains(git2::Status::INDEX_MODIFIED) { parts.push("index_modified"); }
    if status.contains(git2::Status::INDEX_DELETED) { parts.push("index_deleted"); }
    if status.contains(git2::Status::INDEX_RENAMED) { parts.push("index_renamed"); }
    if status.contains(git2::Status::INDEX_TYPECHANGE) { parts.push("index_typechange"); }
    if status.contains(git2::Status::WT_NEW) { parts.push("wt_new"); }
    if status.contains(git2::Status::WT_MODIFIED) { parts.push("wt_modified"); }
    if status.contains(git2::Status::WT_DELETED) { parts.push("wt_deleted"); }
    if status.contains(git2::Status::WT_RENAMED) { parts.push("wt_renamed"); }
    if status.contains(git2::Status::WT_TYPECHANGE) { parts.push("wt_typechange"); }
    if status.contains(git2::Status::IGNORED) { parts.push("ignored"); }
    if status.contains(git2::Status::CONFLICTED) { parts.push("conflicted"); }
    if parts.is_empty() { "clean".to_string() } else { parts.join(",") }
}

pub fn search_grep(daemon: &Daemon, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
    let params: SearchGrepParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid params: {e}")))?;

    let search_root = match &params.path {
        Some(p) => resolve_within(&daemon.root, p)?,
        None => daemon.root.clone(),
    };

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
        overrides.add(glob).map_err(|e| RpcError::new(-32031, format!("glob: {e}")))?;
        let built = overrides.build().map_err(|e| RpcError::new(-32032, format!("glob build: {e}")))?;
        walker.overrides(built);
    }

    'outer: for entry in walker.build() {
        let entry = match entry { Ok(e) => e, Err(_) => continue };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) { continue; }
        let path = entry.path().to_path_buf();
        let rel_path = path
            .strip_prefix(&daemon.root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();

        let mut searcher = SearcherBuilder::new().line_number(true).build();
        let local_path = rel_path.clone();
        let result = searcher.search_path(
            &matcher,
            &path,
            UTF8(|lnum, line| {
                if hits.len() >= max_hits {
                    return Ok(false);
                }
                let mut line_text = line.to_string();
                if line_text.ends_with('\n') { line_text.pop(); }
                if line_text.ends_with('\r') { line_text.pop(); }
                hits.push(SearchHit { path: local_path.clone(), line_number: lnum, line: line_text });
                Ok(true)
            }),
        );
        if let Err(e) = result {
            tracing::debug!(path = %rel_path, error = %e, "search error");
        }
        if hits.len() >= max_hits {
            truncated = true;
            break 'outer;
        }
    }

    Ok(serde_json::to_value(SearchGrepResult { hits, truncated }).unwrap())
}
