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
    pub const FS_SNAPSHOT: &str = "fs.snapshot";
    pub const FS_CHANGES: &str = "fs.changes";
    pub const FS_SCAN: &str = "fs.scan";
    pub const GIT_STATUS: &str = "git.status";
    pub const SEARCH_GREP: &str = "search.grep";
    pub const CODE_OUTLINE: &str = "code.outline";
    pub const CODE_SYMBOLS: &str = "code.symbols";
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsReadResult {
    pub path: String,
    pub bytes_read: u64,
    pub total_size: u64,
    pub content: String,
    pub truncated: bool,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub path: String,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsScanResult {
    /// Version captured at the start of the walk. After processing the result,
    /// call `fs.changes(since: version)` to replay anything that landed while
    /// the scan was running.
    pub version: u64,
    /// Paths relative to the project root. Honours gitignore; excludes `.git/`.
    pub files: Vec<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeOutlineParams {
    /// Path relative to project root (or absolute inside root).
    pub path: String,
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
