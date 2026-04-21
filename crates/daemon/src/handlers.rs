use std::fs::File;

use grep_regex::RegexMatcherBuilder;
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch};
use ignore::WalkBuilder;
use memmap2::Mmap;
use protocol::{
    CodeOutlineBatchItem, CodeOutlineBatchParams, CodeOutlineBatchResult, CodeOutlineParams,
    CodeOutlineResult, CodeSymbolsBatchItem, CodeSymbolsBatchParams, CodeSymbolsBatchResult,
    CodeSymbolsParams, CodeSymbolsResult, FsChangesParams, FsChangesResult, FsReadBatchItem,
    FsReadBatchParams, FsReadBatchResult, FsReadParams, FsReadResult, FsScanParams, FsScanResult,
    FsSnapshotResult, GitStatusEntry, GitStatusParams, GitStatusResult, MetricsGainParams,
    MetricsToolLatencyParams, RpcError, SearchContextLine, SearchGrepParams, SearchGrepResult,
    SearchHit,
};

use crate::compact;
use crate::search_cache::SearchKey;
use crate::server::{resolve_within, Daemon};

/// How many directory rows we keep per status class in compact mode
/// before collapsing the rest into a synthetic `(other)` row. Picked
/// empirically — enough to spot patterns, few enough that a 5000-file
/// dirty tree still serializes to a kilobyte or two.
const GIT_STATUS_TOP_DIRS_PER_CLASS: usize = 16;

const FS_READ_DEFAULT_LIMIT: u64 = 256 * 1024;
const SEARCH_DEFAULT_LIMIT: usize = 200;
/// Upper bound on the per-match context window. Bigger values blow up
/// the response without buying the agent new signal — and a compromised
/// client sending `context: 1e9` otherwise makes the daemon stream the
/// whole file per match.
const SEARCH_CONTEXT_CAP: u32 = 20;

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
    let total_size = file
        .metadata()
        .map_err(|e| RpcError::new(-32011, format!("stat: {e}")))?
        .len();

    if total_size == 0 {
        return Ok(FsReadResult {
            path: params.path.clone(),
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
        .hidden(false)
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

    Ok(serde_json::to_value(FsScanResult {
        version,
        files,
        truncated,
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
