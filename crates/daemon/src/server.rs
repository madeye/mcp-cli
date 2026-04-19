use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use protocol::{Request, Response, RpcError};
use tokio::net::{UnixListener, UnixStream};

use crate::changelog::ChangeLog;
use crate::framing::{read_frame, write_frame};
use crate::handlers;
use crate::watcher::{self, WatchHandle};

pub struct Daemon {
    pub root: PathBuf,
    pub changelog: Arc<ChangeLog>,
}

pub async fn serve(socket: PathBuf, root: PathBuf, changelog_capacity: usize) -> Result<()> {
    if socket.exists() {
        std::fs::remove_file(&socket)
            .with_context(|| format!("removing stale socket {}", socket.display()))?;
    }
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;

    let changelog = Arc::new(ChangeLog::with_capacity(changelog_capacity));
    let _watch: WatchHandle = watcher::spawn(root.clone(), changelog.clone())?;
    let daemon = Arc::new(Daemon { root, changelog });

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            res = listener.accept() => {
                let (stream, _) = res?;
                let daemon = daemon.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, daemon).await {
                        tracing::warn!(error = %e, "connection ended with error");
                    }
                });
            }
            _ = &mut shutdown => {
                tracing::info!("shutting down");
                break;
            }
        }
    }
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

async fn handle_conn(mut stream: UnixStream, daemon: Arc<Daemon>) -> Result<()> {
    let (read_half, mut write_half) = stream.split();
    let mut reader = tokio::io::BufReader::new(read_half);

    while let Some(frame) = read_frame(&mut reader).await? {
        let req: Request = match serde_json::from_slice(&frame) {
            Ok(r) => r,
            Err(e) => {
                let resp = Response {
                    id: 0,
                    result: None,
                    error: Some(RpcError::new(-32700, e.to_string())),
                };
                let payload = serde_json::to_vec(&resp)?;
                write_frame(&mut write_half, &payload).await?;
                continue;
            }
        };

        let resp = dispatch(&daemon, req).await;
        let payload = serde_json::to_vec(&resp)?;
        write_frame(&mut write_half, &payload).await?;
    }
    Ok(())
}

async fn dispatch(daemon: &Daemon, req: Request) -> Response {
    let id = req.id;
    let result = match req.method.as_str() {
        protocol::methods::PING => {
            Ok(serde_json::json!({"ok": true, "version": protocol::PROTOCOL_VERSION}))
        }
        protocol::methods::FS_READ => handlers::fs_read(daemon, req.params),
        protocol::methods::FS_SNAPSHOT => handlers::fs_snapshot(daemon, req.params),
        protocol::methods::FS_CHANGES => handlers::fs_changes(daemon, req.params),
        protocol::methods::FS_SCAN => handlers::fs_scan(daemon, req.params),
        protocol::methods::GIT_STATUS => handlers::git_status(daemon, req.params),
        protocol::methods::SEARCH_GREP => handlers::search_grep(daemon, req.params),
        other => Err(RpcError::new(-32601, format!("unknown method: {other}"))),
    };
    match result {
        Ok(value) => Response {
            id,
            result: Some(value),
            error: None,
        },
        Err(err) => Response {
            id,
            result: None,
            error: Some(err),
        },
    }
}

pub fn resolve_within(root: &Path, candidate: &str) -> std::result::Result<PathBuf, RpcError> {
    let path = Path::new(candidate);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let canon = joined
        .canonicalize()
        .map_err(|e| RpcError::new(-32001, format!("canonicalize {candidate}: {e}")))?;
    if !canon.starts_with(root) {
        return Err(RpcError::new(
            -32002,
            format!("path escapes root: {}", canon.display()),
        ));
    }
    Ok(canon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        (tmp, root)
    }

    #[test]
    fn accepts_relative_file_inside_root() {
        let (_tmp, root) = setup();
        fs::write(root.join("a.txt"), b"hello").unwrap();
        let resolved = resolve_within(&root, "a.txt").expect("inside root");
        assert_eq!(resolved, root.join("a.txt"));
    }

    #[test]
    fn accepts_absolute_path_inside_root() {
        let (_tmp, root) = setup();
        fs::write(root.join("a.txt"), b"hello").unwrap();
        let abs = root.join("a.txt");
        let resolved = resolve_within(&root, abs.to_str().unwrap()).expect("inside root");
        assert_eq!(resolved, abs);
    }

    #[test]
    fn rejects_parent_traversal() {
        let (_tmp, root) = setup();
        let sub = root.join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(root.parent().unwrap().join("outside.txt"), b"x").ok();

        // Build a path that canonicalizes to a sibling of root.
        let err = resolve_within(&sub, "../../outside.txt").expect_err("should be rejected");
        assert_eq!(err.code, -32002);
    }

    #[test]
    fn rejects_absolute_path_outside_root() {
        let (_tmp, root) = setup();
        // Point at a real path that is guaranteed to exist but is not under root.
        let err = resolve_within(&root, "/tmp").expect_err("should be rejected");
        // Either canonicalize succeeded (wrong-tree) or failed; both are errors.
        assert!(err.code == -32001 || err.code == -32002);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escaping_root() {
        use std::os::unix::fs::symlink;
        let (_tmp, root) = setup();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("secret.txt");
        fs::write(&outside_file, b"nope").unwrap();
        symlink(&outside_file, root.join("link")).unwrap();

        let err =
            resolve_within(&root, "link").expect_err("symlink escaping root must be rejected");
        assert_eq!(err.code, -32002);
    }

    #[test]
    fn nonexistent_path_is_error() {
        let (_tmp, root) = setup();
        let err = resolve_within(&root, "does-not-exist").expect_err("nonexistent");
        assert_eq!(err.code, -32001);
    }
}
