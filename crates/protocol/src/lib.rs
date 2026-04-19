use serde::{Deserialize, Serialize};

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
    pub entries: Vec<GitStatusEntry>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub path: String,
    pub line_number: u64,
    pub line: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchGrepResult {
    pub hits: Vec<SearchHit>,
    pub truncated: bool,
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
