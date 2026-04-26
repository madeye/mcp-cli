use serde::{Deserialize, Serialize};

pub mod paths;

pub const PROTOCOL_VERSION: &str = "0.1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

impl RpcError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

pub mod methods {
    pub const PING: &str = "ping";
    pub const FS_READ: &str = "fs.read";
    pub const FS_READ_BATCH: &str = "fs.read_batch";
    pub const FS_SNAPSHOT: &str = "fs.snapshot";
    pub const FS_CHANGES: &str = "fs.changes";
    pub const FS_SCAN: &str = "fs.scan";
    pub const GIT_STATUS: &str = "git.status";
    pub const GIT_LOG: &str = "git.log";
    pub const GIT_DIFF: &str = "git.diff";
    pub const SEARCH_GREP: &str = "search.grep";
    pub const CODE_OUTLINE: &str = "code.outline";
    pub const CODE_OUTLINE_BATCH: &str = "code.outline_batch";
    pub const CODE_SYMBOLS: &str = "code.symbols";
    pub const CODE_SYMBOLS_BATCH: &str = "code.symbols_batch";
    pub const TOOL_RUN: &str = "tool.run";
    pub const TOOL_GH: &str = "tool.gh";
    pub const METRICS_GAIN: &str = "metrics.gain";
    pub const METRICS_TOOL_LATENCY: &str = "metrics.tool_latency";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsReadParams {
    pub path: String,
    #[serde(default)]
    pub offset: u64,
    #[serde(default)]
    pub length: Option<u64>,
    /// When true, the daemon replaces boilerplate regions in `content`
    /// with single-line `[[mcp-cli: stripped …]]` markers. Currently
    /// recognized: leading license-header comments, long base64 blobs,
    /// and the body of files tagged `@generated` / `DO NOT EDIT`. See
    /// `FsReadResult::stripped_regions` for per-region detail. Only
    /// meaningful when reading from the start of the file.
    #[serde(default)]
    pub strip_noise: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsReadResult {
    pub path: String,
    pub bytes_read: u64,
    pub total_size: u64,
    pub content: String,
    pub truncated: bool,
    /// Populated when `FsReadParams::strip_noise` is true and the
    /// daemon elided boilerplate sections from `content`. Omitted
    /// (empty) otherwise. Line numbers refer to the ORIGINAL file
    /// content so callers can request them specifically if needed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stripped_regions: Vec<StrippedRegion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrippedRegion {
    /// `"license"`, `"base64"`, or `"generated"`.
    pub kind: String,
    /// 1-based inclusive line range in the original file content.
    pub start_line: u32,
    pub end_line: u32,
    /// `end_line - start_line + 1` — the number of lines collapsed
    /// into a single marker.
    pub lines: u32,
}

// ---- fs.read_batch ------------------------------------------------------

/// Batch multiple `fs.read` calls into a single RPC round-trip.
///
/// The M5 benchmark showed codex scroll-paging through large files
/// (six reads of `mcp_connection_manager.rs` at different offsets,
/// five reads of `exec.rs`, …) — each read was its own MCP turn, each
/// turn paid model-reasoning latency. Batching folds those N turns
/// into one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsReadBatchParams {
    /// Read requests, processed in order. Each maps 1:1 onto a
    /// `FsReadResult` entry in the response (or an `error` when that
    /// specific request fails). Per-request failures do not abort the
    /// batch.
    pub requests: Vec<FsReadParams>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsReadBatchResult {
    pub responses: Vec<FsReadBatchItem>,
}

/// One entry per input request. Exactly one of `result` / `error` is
/// set — mirroring `Result<FsReadResult, RpcError>` with the verbose
/// serde shape collapsed into two optional fields so the JSON stays
/// small.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsReadBatchItem {
    /// Echoes the request's `path` so the caller can correlate
    /// responses back to inputs even if they were re-ordered before
    /// serialization (today we preserve order, but clients shouldn't
    /// have to rely on positional matching).
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<FsReadResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitStatusParams {
    #[serde(default)]
    pub repo: Option<String>,
    /// When true, the response carries `compact` and omits `entries`.
    /// The compact form groups entries by status class with
    /// per-directory counts — usually 5–10× smaller than the full
    /// list for a non-trivial dirty tree.
    #[serde(default)]
    pub compact: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitStatusEntry {
    pub path: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitStatusResult {
    pub branch: Option<String>,
    pub head: Option<String>,
    /// Per-file detail. Omitted in compact mode (see `compact`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<GitStatusEntry>,
    /// Roll-up summary requested via `params.compact = true`. Omitted
    /// in raw mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact: Option<GitStatusCompact>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GitStatusCompact {
    /// One entry per status class actually observed (`modified`,
    /// `untracked`, `deleted`, `renamed`, `typechange`, `conflicted`).
    pub by_class: Vec<GitStatusClassBucket>,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GitStatusClassBucket {
    pub class: String,
    pub count: usize,
    /// Top directories under this class with their per-directory counts,
    /// ordered by count descending.
    pub by_dir: Vec<GitStatusDirCount>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GitStatusDirCount {
    pub dir: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GitLogParams {
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub max_count: Option<usize>,
    /// Optional branch, tag, or commit SHA to start from. Defaults to HEAD.
    #[serde(default)]
    pub revision: Option<String>,
    /// Optional path to filter commits by.
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitLogResult {
    pub commits: Vec<GitCommit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitCommit {
    pub sha: String,
    pub author: String,
    pub date: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GitDiffParams {
    #[serde(default)]
    pub repo: Option<String>,
    /// Base revision to compare from.
    #[serde(default)]
    pub base: Option<String>,
    /// Target revision to compare to. If omitted, compares base against working tree.
    #[serde(default)]
    pub target: Option<String>,
    /// Optional path filter.
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitDiffResult {
    /// Unified diff format.
    pub diff: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchGrepParams {
    pub pattern: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub glob: Option<String>,
    #[serde(default)]
    pub max_results: Option<usize>,
    #[serde(default)]
    pub case_insensitive: bool,
    /// When true, the response carries `compact` and omits `hits` —
    /// one bucket per matching file with match count + first / last
    /// line numbers. Use when the agent only needs to know which files
    /// match, not every line.
    #[serde(default)]
    pub compact: bool,
    /// Return this many lines of context before and after each match,
    /// attached as `hit.context`. Default 0 (matches only). Capped at
    /// 20 by the daemon to keep responses bounded.
    #[serde(default)]
    pub context: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub path: String,
    pub line_number: u64,
    pub line: String,
    /// Surrounding context requested via `SearchGrepParams::context`.
    /// Each entry carries its own 1-based `line_number` and the line
    /// text (trailing `\r`/`\n` stripped). Empty (and omitted in
    /// serialization) when context wasn't requested.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context: Vec<SearchContextLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchContextLine {
    pub line_number: u64,
    pub line: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchGrepResult {
    /// Per-line detail. Omitted in compact mode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hits: Vec<SearchHit>,
    pub truncated: bool,
    /// Roll-up requested via `params.compact = true`. Omitted in raw mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact: Option<SearchGrepCompact>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SearchGrepCompact {
    pub buckets: Vec<SearchFileBucket>,
    pub total_matches: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SearchFileBucket {
    pub path: String,
    pub matches: usize,
    pub first_line: u64,
    pub last_line: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsSnapshotResult {
    /// Current monotonic version cursor for the watched tree.
    pub version: u64,
    /// Capacity of the in-memory change ring; older changes are dropped.
    pub capacity: usize,
    /// Oldest version still queryable via `fs.changes`. Anything older
    /// requires a full re-scan.
    pub oldest_retained: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsChangesParams {
    /// Return all events with version > `since`.
    pub since: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Created,
    Modified,
    Removed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeEntry {
    pub path: String,
    pub kind: ChangeKind,
    pub version: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsChangesResult {
    pub version: u64,
    pub changes: Vec<ChangeEntry>,
    /// True if `since` was older than the oldest retained version, meaning
    /// the client missed events and should do a fresh full scan.
    pub overflowed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FsScanParams {
    /// Optional subdirectory (relative to the project root) to enumerate.
    /// Defaults to the entire project tree.
    #[serde(default)]
    pub path: Option<String>,
    /// Optional cap on number of entries returned. If the walker yields more
    /// than this, `truncated` is set and the tail is dropped.
    #[serde(default)]
    pub max_results: Option<usize>,
    /// When true, the response carries `compact` and omits `files` — a
    /// roll-up by immediate parent directory, usually 10–100× smaller
    /// than the flat path list for a non-trivial tree.
    #[serde(default)]
    pub compact: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsScanResult {
    /// Version captured at the start of the walk. After processing the result,
    /// call `fs.changes(since: version)` to replay anything that landed while
    /// the scan was running.
    pub version: u64,
    /// Paths relative to the project root. Honours gitignore; excludes `.git/`.
    /// Omitted in compact mode (see `compact`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    pub truncated: bool,
    /// Roll-up requested via `params.compact = true`. Omitted in raw mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact: Option<FsScanCompact>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FsScanCompact {
    /// One entry per immediate parent directory actually observed,
    /// ordered by `count` descending (alphabetical tie-break). Top-
    /// level files use `"."`. When the bucket count exceeds the
    /// configured cap, the tail is summed into a synthetic `(other)`
    /// row so `total` always reconciles.
    pub by_dir: Vec<FsScanDirBucket>,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FsScanDirBucket {
    pub dir: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeOutlineParams {
    /// Path relative to project root (or absolute inside root).
    pub path: String,
    /// When true, each entry carries a `signature` field containing the
    /// declaration header up to (but not including) the body — e.g.
    /// `fn foo(x: u32) -> bool` instead of the full function. Entries
    /// without a recognizable body (constants, type aliases, unit
    /// structs) fall back to the first line of the declaration.
    /// Cheaper than fetching the full file when the agent only needs
    /// signatures.
    #[serde(default)]
    pub signatures_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeOutlineEntry {
    /// One of: `function`, `method`, `struct`, `enum`, `class`, `trait`,
    /// `interface`, `module`, `namespace`, `type`, `constant`, `variable`,
    /// `macro`, `impl`, `union`, `field`.
    pub kind: String,
    pub name: String,
    /// Byte offset of the start of the declaration node.
    pub start_byte: u32,
    /// Byte offset (exclusive) of the end of the declaration node.
    pub end_byte: u32,
    /// 1-based line of the start of the declaration.
    pub start_line: u32,
    /// 1-based line of the end of the declaration (inclusive).
    pub end_line: u32,
    /// Declaration header up to the body, with interior whitespace
    /// collapsed to single spaces. Populated only when
    /// `CodeOutlineParams::signatures_only` is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeOutlineResult {
    pub path: String,
    /// Detected language name (e.g. `rust`, `python`). `None` for
    /// extensions without a registered grammar — `entries` is empty.
    pub language: Option<String>,
    pub entries: Vec<CodeOutlineEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeSymbolsParams {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeSymbolsResult {
    pub path: String,
    pub language: Option<String>,
    /// Flat, de-duplicated list of top-level symbol names (function names,
    /// type names, etc.). For a full structural view, use `code.outline`.
    pub names: Vec<String>,
}

// ---- code.outline_batch / code.symbols_batch ----------------------------

/// Batch many `code.outline` calls into one round-trip. Same shape as
/// `fs.read_batch`: per-entry `result` or `error`; per-request failures
/// don't abort the batch. Closes the agent loop when the model needs
/// the structural view of N files at once (the M5 bench's "list
/// symbols across these 6 files" pattern was 6 turns; this folds it
/// into 1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeOutlineBatchParams {
    pub requests: Vec<CodeOutlineParams>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeOutlineBatchResult {
    pub responses: Vec<CodeOutlineBatchItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeOutlineBatchItem {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<CodeOutlineResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeSymbolsBatchParams {
    pub requests: Vec<CodeSymbolsParams>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeSymbolsBatchResult {
    pub responses: Vec<CodeSymbolsBatchItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeSymbolsBatchItem {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<CodeSymbolsResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

// ---- metrics.gain --------------------------------------------------------

/// `metrics.gain` takes no params today; reserved struct for forward-compat.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MetricsGainParams {}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MetricsGainResult {
    pub per_tool: Vec<ToolGainEntry>,
    pub total_raw_bytes: u64,
    pub total_compacted_bytes: u64,
    /// `1.0 - compacted/raw`, clamped to `[0.0, 1.0]`. `0.0` if no
    /// calls have been recorded yet.
    pub savings_ratio: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolGainEntry {
    /// Method name (e.g. `git.status`, `search.grep`, `fs.read`).
    pub tool: String,
    pub calls: u64,
    pub raw_bytes: u64,
    pub compacted_bytes: u64,
}

// ---- metrics.tool_latency ------------------------------------------------

/// `metrics.tool_latency` takes no params today; reserved struct for
/// forward-compat (we may add filtering or a since-cursor later).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MetricsToolLatencyParams {}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MetricsToolLatencyResult {
    pub per_tool: Vec<ToolLatencyEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolLatencyEntry {
    /// Method name (e.g. `git.status`, `search.grep`, `fs.read`).
    pub tool: String,
    /// Number of dispatched calls observed for this method.
    pub calls: u64,
    /// Sum of per-call elapsed time in microseconds. We keep the sum
    /// so multi-process callers can aggregate without losing precision;
    /// `mean_us` is the precomputed convenience value.
    pub latency_sum_us: u64,
    pub mean_us: u64,
    pub max_us: u64,
}

// ---- tool.run ------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolRunParams {
    /// Executable name or absolute path. Shell features are intentionally
    /// not interpreted here; callers that need a shell can pass
    /// `command: "sh", args: ["-lc", "..."]`.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional working directory, relative to the project root or
    /// absolute within it. Defaults to the daemon root.
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Vec<ToolRunEnv>,
    /// Per-stream output cap in bytes. Defaults to 64 KiB.
    #[serde(default)]
    pub max_output_bytes: Option<usize>,
    /// Cache successful results while the watched tree version is
    /// unchanged. Defaults to false because commands may read external
    /// state.
    #[serde(default)]
    pub cache: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolRunEnv {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRunResult {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub exit_code: Option<i32>,
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    /// Combined stdout/stderr tail on failure, useful when callers only
    /// need the actionable error context. Empty on success.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub failure_output: String,
    #[serde(default)]
    pub cached: bool,
}

// ---- tool.gh -------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolGhParams {
    /// `pr` or `issue`.
    pub kind: String,
    /// Number, URL, or branch accepted by `gh pr view` / `gh issue view`.
    /// Omitted value lets gh choose its contextual default where supported.
    #[serde(default)]
    pub selector: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    /// JSON fields to request. Defaults to a compact set for each kind.
    #[serde(default)]
    pub fields: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolGhResult {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    pub exit_code: Option<i32>,
    pub success: bool,
    /// Parsed JSON when gh returned valid JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    /// Raw stdout if JSON parsing failed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stdout: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stderr: String,
}
