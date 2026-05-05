//! `pipe` RPC handler — compose multiple daemon RPCs into one
//! round-trip with reference forwarding (`{"$ref": "$N.<jsonpath>"}`)
//! and bounded-concurrency `for_each` fan-out.
//!
//! The point: an MCP client paying model-reasoning latency per
//! `tools/call` doesn't want to spend that latency on glue calls like
//! "scan, then read the matching files". `pipe` lets the agent send a
//! shape like `[{search.grep …}, {fs.read for_each $0.hits[*].path}]`
//! in one turn; the daemon resolves references and dispatches each
//! step locally.
//!
//! The reference dialect is RFC 9535 JSONPath: an expression starts
//! with either `$N` (an integer step index) or `$item` (the current
//! `for_each` element), optionally followed by a JSONPath subpath
//! (`.entries[0].path`, `[*].sha`). Plain `$ref` substitutions must
//! resolve to exactly one node — multi-cardinality lookups belong in
//! `for_each`. Nested pipe steps are rejected outright.

use std::sync::Arc;

use protocol::{PipeItemResult, PipeParams, PipeResult, PipeStep, PipeStepResult, RpcError};
use serde_json::Value;
use serde_json_path::JsonPath;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::server::{dispatch_method, Daemon};

const FOR_EACH_DEFAULT_CONCURRENCY: usize = 8;
/// Hard ceiling — bigger than this is almost always a mistake (one
/// daemon dispatching 256 sync handlers in parallel will saturate the
/// blocking pool and starve other connections). Keep the door open for
/// agents that genuinely need more by clamping rather than erroring.
const FOR_EACH_MAX_CONCURRENCY: usize = 64;

pub async fn pipe(daemon: Arc<Daemon>, params: Value) -> Result<Value, RpcError> {
    let pipe_params: PipeParams = serde_json::from_value(params)
        .map_err(|e| RpcError::new(-32602, format!("invalid pipe params: {e}")))?;
    if pipe_params.steps.is_empty() {
        return Err(RpcError::new(-32602, "pipe requires at least one step"));
    }

    // Per-step result slots, indexed by step number. `None` means the
    // step errored or hasn't run yet — later `$ref` lookups against a
    // None slot fail with a clear diagnostic instead of pretending the
    // upstream succeeded.
    let mut step_results: Vec<Option<Value>> = Vec::with_capacity(pipe_params.steps.len());
    let mut response_steps: Vec<PipeStepResult> = Vec::with_capacity(pipe_params.steps.len());

    for step in &pipe_params.steps {
        if step.method == protocol::methods::PIPE {
            return Err(RpcError::new(
                -32602,
                "nested pipe not supported; flatten composition at the caller",
            ));
        }
        let mut step_response = PipeStepResult {
            method: step.method.clone(),
            ..Default::default()
        };

        if let Some(for_each_expr) = &step.for_each {
            match resolve_ref(for_each_expr, &step_results, None, /*single*/ false) {
                Ok(Value::Array(items)) => {
                    let item_results = run_for_each(Arc::clone(&daemon), step, items).await;
                    let any_err = item_results.iter().any(|r| r.error.is_some());
                    let aggregate: Vec<Value> = item_results
                        .iter()
                        .filter_map(|r| r.result.clone())
                        .collect();
                    step_response.items = Some(item_results);
                    response_steps.push(step_response);
                    if any_err && !step.continue_on_error {
                        step_results.push(None);
                        return finalize(response_steps);
                    }
                    step_results.push(Some(Value::Array(aggregate)));
                }
                Ok(_) => unreachable!("resolve_ref(single=false) always returns Array"),
                Err(e) => {
                    step_response.error = Some(e);
                    response_steps.push(step_response);
                    step_results.push(None);
                    if !step.continue_on_error {
                        return finalize(response_steps);
                    }
                }
            }
        } else {
            let resolved = match substitute(&step.params, &step_results, None) {
                Ok(v) => v,
                Err(e) => {
                    step_response.error = Some(e);
                    response_steps.push(step_response);
                    step_results.push(None);
                    if !step.continue_on_error {
                        return finalize(response_steps);
                    }
                    continue;
                }
            };
            match dispatch_method(&daemon, &step.method, resolved) {
                Ok(v) => {
                    step_response.result = Some(v.clone());
                    response_steps.push(step_response);
                    step_results.push(Some(v));
                }
                Err(e) => {
                    step_response.error = Some(e);
                    response_steps.push(step_response);
                    step_results.push(None);
                    if !step.continue_on_error {
                        return finalize(response_steps);
                    }
                }
            }
        }
    }
    finalize(response_steps)
}

fn finalize(steps: Vec<PipeStepResult>) -> Result<Value, RpcError> {
    serde_json::to_value(PipeResult { steps })
        .map_err(|e| RpcError::new(-32603, format!("serialize pipe result: {e}")))
}

/// Run one `for_each` step. Per-item dispatch goes through the blocking
/// pool because the inner handlers are sync and may hit IO/locks; a
/// semaphore keeps the global blocking-task count bounded.
async fn run_for_each(
    daemon: Arc<Daemon>,
    step: &PipeStep,
    items: Vec<Value>,
) -> Vec<PipeItemResult> {
    let n = items.len();
    let concurrency = step
        .concurrency
        .unwrap_or(FOR_EACH_DEFAULT_CONCURRENCY)
        .clamp(1, FOR_EACH_MAX_CONCURRENCY);
    let sem = Arc::new(Semaphore::new(concurrency));
    let mut set: JoinSet<(usize, Result<Value, RpcError>)> = JoinSet::new();

    // We need to know which step results each iteration's $ref calls
    // could see; for_each runs after the prior steps so cloning is fine.
    // The clone happens once and is shared via Arc-free closure capture.
    // Sharing across blocking tasks would need Arc; substitution runs
    // on the async side first to produce a per-iteration params Value.
    for (idx, item) in items.into_iter().enumerate() {
        let resolved = substitute(&step.params, &[], Some(&item));
        let permit = match Arc::clone(&sem).acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                // Semaphore should never be closed (we own it), but if
                // it somehow is, fall through with a synthetic error.
                set.spawn(async move {
                    (
                        idx,
                        Err(RpcError::new(
                            -32603,
                            "for_each semaphore closed unexpectedly",
                        )),
                    )
                });
                continue;
            }
        };
        let daemon = Arc::clone(&daemon);
        let method = step.method.clone();
        match resolved {
            Ok(params) => {
                set.spawn_blocking(move || {
                    let _permit = permit;
                    (idx, dispatch_method(&daemon, &method, params))
                });
            }
            Err(e) => {
                drop(permit);
                set.spawn(async move { (idx, Err(e)) });
            }
        }
    }

    let mut slots: Vec<Option<PipeItemResult>> = (0..n).map(|_| None).collect();
    while let Some(joined) = set.join_next().await {
        let (idx, result) = match joined {
            Ok(t) => t,
            Err(e) if e.is_panic() => {
                // A panicking handler shouldn't take the whole pipe with
                // it; turn it into a per-item error. We don't know the
                // item index when the join task itself panicked, so
                // fill the next empty slot — best-effort.
                let payload = format!("for_each task panicked: {e}");
                if let Some(slot) = slots.iter_mut().find(|s| s.is_none()) {
                    *slot = Some(PipeItemResult {
                        result: None,
                        error: Some(RpcError::new(-32603, payload)),
                    });
                }
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, "for_each task join error");
                continue;
            }
        };
        slots[idx] = Some(match result {
            Ok(v) => PipeItemResult {
                result: Some(v),
                error: None,
            },
            Err(e) => PipeItemResult {
                result: None,
                error: Some(e),
            },
        });
    }
    slots
        .into_iter()
        .map(|s| {
            s.unwrap_or_else(|| PipeItemResult {
                result: None,
                error: Some(RpcError::new(
                    -32603,
                    "for_each item dropped without result",
                )),
            })
        })
        .collect()
}

/// Recursively walk `params`, replacing every `{"$ref": "<expr>"}`
/// object with the resolved value. Non-object/array nodes pass through
/// untouched.
fn substitute(
    params: &Value,
    step_results: &[Option<Value>],
    item: Option<&Value>,
) -> Result<Value, RpcError> {
    match params {
        Value::Object(obj) => {
            if obj.len() == 1 {
                if let Some(Value::String(expr)) = obj.get("$ref") {
                    return resolve_ref(expr, step_results, item, /*single*/ true);
                }
            }
            let mut out = serde_json::Map::with_capacity(obj.len());
            for (k, v) in obj {
                out.insert(k.clone(), substitute(v, step_results, item)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for v in arr {
                out.push(substitute(v, step_results, item)?);
            }
            Ok(Value::Array(out))
        }
        other => Ok(other.clone()),
    }
}

enum RefRoot {
    Step(usize),
    Item,
}

fn parse_prefix(expr: &str) -> Result<(RefRoot, String), RpcError> {
    let raw = expr.trim();
    let after = raw
        .strip_prefix('$')
        .ok_or_else(|| RpcError::new(-32602, format!("ref must start with '$': {raw:?}")))?;
    if let Some(rest) = after.strip_prefix("item") {
        return Ok((RefRoot::Item, jsonpath_suffix(raw, rest)?));
    }
    let digit_end = after.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digit_end == 0 {
        return Err(RpcError::new(
            -32602,
            format!("ref must be $N or $item: {raw:?}"),
        ));
    }
    let idx: usize = after[..digit_end]
        .parse()
        .map_err(|_| RpcError::new(-32602, format!("ref index parse failed: {raw:?}")))?;
    Ok((
        RefRoot::Step(idx),
        jsonpath_suffix(raw, &after[digit_end..])?,
    ))
}

fn jsonpath_suffix(full: &str, rest: &str) -> Result<String, RpcError> {
    if rest.is_empty() {
        Ok(String::new())
    } else if let Some(s) = rest.strip_prefix('.') {
        Ok(format!(".{s}"))
    } else if rest.starts_with('[') {
        Ok(rest.to_string())
    } else {
        Err(RpcError::new(
            -32602,
            format!("ref suffix must start with '.' or '[': {full:?}"),
        ))
    }
}

/// Evaluate a `$ref` expression. With `single = true`, requires exactly
/// one matched node and returns it. With `single = false` (used for
/// `for_each` resolution), returns all matches as a JSON array — zero
/// matches yields an empty array (zero iterations, not an error).
fn resolve_ref(
    expr: &str,
    step_results: &[Option<Value>],
    item: Option<&Value>,
    single: bool,
) -> Result<Value, RpcError> {
    let (root, suffix) = parse_prefix(expr)?;
    let target: &Value = match root {
        RefRoot::Step(idx) => step_results
            .get(idx)
            .and_then(|o| o.as_ref())
            .ok_or_else(|| {
                RpcError::new(
                    -32602,
                    format!("$ref {expr:?}: step {idx} has no result (out of range or errored)"),
                )
            })?,
        RefRoot::Item => item.ok_or_else(|| {
            RpcError::new(-32602, format!("$item used outside for_each: {expr:?}"))
        })?,
    };
    let nodes: Vec<Value> = if suffix.is_empty() {
        vec![target.clone()]
    } else {
        let path_str = format!("${suffix}");
        let path = JsonPath::parse(&path_str)
            .map_err(|e| RpcError::new(-32602, format!("invalid jsonpath {path_str:?}: {e}")))?;
        path.query(target).all().into_iter().cloned().collect()
    };
    if single {
        match nodes.len() {
            1 => Ok(nodes.into_iter().next().expect("len=1")),
            0 => Err(RpcError::new(
                -32602,
                format!("$ref {expr:?} resolved to zero nodes"),
            )),
            n => Err(RpcError::new(
                -32602,
                format!("$ref {expr:?} resolved to {n} nodes; use for_each for multi-cardinality"),
            )),
        }
    } else {
        Ok(Value::Array(nodes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn step(value: Value) -> Option<Value> {
        Some(value)
    }

    #[test]
    fn substitute_passes_through_literals() {
        let v = json!({"a": 1, "b": "x", "c": [1, 2, 3]});
        assert_eq!(substitute(&v, &[], None).unwrap(), v);
    }

    #[test]
    fn substitute_resolves_step_ref() {
        let steps = vec![step(json!({"path": "src/lib.rs", "version": 7}))];
        let params = json!({"path": {"$ref": "$0.path"}});
        let out = substitute(&params, &steps, None).unwrap();
        assert_eq!(out, json!({"path": "src/lib.rs"}));
    }

    #[test]
    fn substitute_resolves_whole_step() {
        let steps = vec![step(json!({"a": 1}))];
        let params = json!({"$ref": "$0"});
        assert_eq!(substitute(&params, &steps, None).unwrap(), json!({"a": 1}));
    }

    #[test]
    fn substitute_resolves_item_ref() {
        let item = json!({"path": "x.rs", "line": 12});
        let params = json!({"path": {"$ref": "$item.path"}, "line": {"$ref": "$item.line"}});
        let out = substitute(&params, &[], Some(&item)).unwrap();
        assert_eq!(out, json!({"path": "x.rs", "line": 12}));
    }

    #[test]
    fn substitute_rejects_zero_node_ref() {
        let steps = vec![step(json!({"a": 1}))];
        let params = json!({"$ref": "$0.missing"});
        let err = substitute(&params, &steps, None).unwrap_err();
        assert!(err.message.contains("zero nodes"));
    }

    #[test]
    fn substitute_rejects_multi_node_single_ref() {
        let steps = vec![step(json!({"items": [{"p": "a"}, {"p": "b"}]}))];
        let params = json!({"$ref": "$0.items[*].p"});
        let err = substitute(&params, &steps, None).unwrap_err();
        assert!(err.message.contains("for_each"));
    }

    #[test]
    fn substitute_errors_on_missing_step() {
        let params = json!({"$ref": "$3.foo"});
        let err = substitute(&params, &[], None).unwrap_err();
        assert!(err.message.contains("step 3"));
    }

    #[test]
    fn substitute_errors_on_item_outside_for_each() {
        let params = json!({"$ref": "$item.x"});
        let err = substitute(&params, &[], None).unwrap_err();
        assert!(err.message.contains("$item"));
    }

    #[test]
    fn resolve_ref_array_returns_all_matches() {
        let steps = vec![step(json!({"items": [{"p": "a"}, {"p": "b"}, {"p": "c"}]}))];
        let v = resolve_ref("$0.items[*].p", &steps, None, false).unwrap();
        assert_eq!(v, json!(["a", "b", "c"]));
    }

    #[test]
    fn resolve_ref_array_returns_empty_for_no_matches() {
        let steps = vec![step(json!({"items": []}))];
        let v = resolve_ref("$0.items[*].p", &steps, None, false).unwrap();
        assert_eq!(v, json!([]));
    }

    #[test]
    fn parse_prefix_handles_index_only() {
        let (root, suffix) = parse_prefix("$2").unwrap();
        assert!(matches!(root, RefRoot::Step(2)));
        assert_eq!(suffix, "");
    }

    #[test]
    fn parse_prefix_handles_bracket_index() {
        let (root, suffix) = parse_prefix("$0[0]").unwrap();
        assert!(matches!(root, RefRoot::Step(0)));
        assert_eq!(suffix, "[0]");
    }

    #[test]
    fn parse_prefix_rejects_garbage() {
        assert!(parse_prefix("foo").is_err());
        assert!(parse_prefix("$").is_err());
        assert!(parse_prefix("$0foo").is_err());
    }

    // ---- end-to-end: pipe through dispatch_method ------------------------

    fn test_daemon(root: &std::path::Path) -> Arc<Daemon> {
        let parse_cache = Arc::new(crate::parse_cache::ParseCache::new(10));
        Arc::new(Daemon {
            root: root.canonicalize().unwrap(),
            changelog: Arc::new(crate::changelog::ChangeLog::with_capacity(10)),
            search_cache: Arc::new(crate::search_cache::SearchCache::new(10)),
            tool_run_cache: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
            parse_cache: parse_cache.clone(),
            backends: {
                let mut reg = crate::backends::BackendRegistry::new();
                reg.register(Arc::new(crate::backends::TreeSitterBackend::new(
                    parse_cache,
                )));
                reg
            },
            frame_pool: Arc::new(crate::buffer_pool::BufferPool::new(1, 1024)),
            arena_pool: Arc::new(parking_lot::Mutex::new(Vec::new())),
            metrics: Arc::new(crate::metrics::ToolMetrics::new()),
            jobs: Arc::new(crate::server::JobTable {
                next_id: std::sync::atomic::AtomicU64::new(1),
                jobs: parking_lot::Mutex::new(std::collections::HashMap::new()),
            }),
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pipe_fs_scan_then_fs_read_for_each() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.txt"), b"alpha\n").unwrap();
        std::fs::write(root.join("b.txt"), b"beta\n").unwrap();
        let daemon = test_daemon(root);

        let params = json!({
            "steps": [
                {
                    "method": "fs.scan",
                    "params": {}
                },
                {
                    "method": "fs.read",
                    "params": {"path": {"$ref": "$item"}},
                    "for_each": "$0.files[*]"
                }
            ]
        });

        let value = pipe(Arc::clone(&daemon), params).await.expect("pipe ok");
        let result: protocol::PipeResult = serde_json::from_value(value).unwrap();
        assert_eq!(result.steps.len(), 2);
        assert!(result.steps[0].result.is_some());
        let items = result.steps[1].items.as_ref().expect("items present");
        assert_eq!(items.len(), 2);
        let mut contents: Vec<String> = items
            .iter()
            .map(|i| {
                let r: protocol::FsReadResult =
                    serde_json::from_value(i.result.clone().expect("ok")).unwrap();
                r.content
            })
            .collect();
        contents.sort();
        assert_eq!(contents, vec!["alpha\n".to_string(), "beta\n".to_string()]);
    }

    #[tokio::test]
    async fn pipe_aborts_on_step_error_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = test_daemon(tmp.path());

        // Step 0 errors (path escapes root); step 1 should not run.
        let params = json!({
            "steps": [
                {"method": "fs.read", "params": {"path": "../../../etc/hosts"}},
                {"method": "fs.scan", "params": {}}
            ]
        });
        let value = pipe(Arc::clone(&daemon), params).await.unwrap();
        let result: protocol::PipeResult = serde_json::from_value(value).unwrap();
        assert_eq!(result.steps.len(), 1);
        assert!(result.steps[0].error.is_some());
    }

    #[tokio::test]
    async fn pipe_continue_on_error_keeps_going() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = test_daemon(tmp.path());

        let params = json!({
            "steps": [
                {
                    "method": "fs.read",
                    "params": {"path": "does-not-exist.txt"},
                    "continue_on_error": true
                },
                {"method": "fs.scan", "params": {}}
            ]
        });
        let value = pipe(Arc::clone(&daemon), params).await.unwrap();
        let result: protocol::PipeResult = serde_json::from_value(value).unwrap();
        assert_eq!(result.steps.len(), 2);
        assert!(result.steps[0].error.is_some());
        assert!(result.steps[1].result.is_some());
    }

    #[tokio::test]
    async fn pipe_rejects_nested_pipe() {
        let tmp = tempfile::tempdir().unwrap();
        let daemon = test_daemon(tmp.path());

        let params = json!({
            "steps": [{"method": "pipe", "params": {"steps": []}}]
        });
        let err = pipe(daemon, params).await.unwrap_err();
        assert!(err.message.contains("nested pipe"));
    }
}
