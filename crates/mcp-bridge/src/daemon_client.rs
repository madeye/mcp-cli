//! Bridge → daemon RPC client over UDS.
//!
//! Owns the connection plus enough config to re-establish it. If a call
//! fails because the underlying stream is dead (daemon crashed,
//! idle-exited, was kill -9'd), `call` drops the stream, reconnects via
//! the same auto-spawn path used at startup, and retries once. Persistent
//! errors (bad regex, file not found, …) still surface — the retry only
//! covers transport-layer failures.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, Context, Result};
use protocol::{Request, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use crate::spawn::{self, SpawnArgs};

const MAX_FRAME: u32 = 16 * 1024 * 1024;

/// Everything the client needs to (re-)establish a connection. Cloned
/// when a reconnect actually fires; cheap because the vec is tiny.
#[derive(Clone)]
pub struct ConnectConfig {
    pub socket: PathBuf,
    pub root: PathBuf,
    pub daemon_bin: Option<PathBuf>,
    pub autospawn: bool,
    pub daemon_extra_args: Vec<String>,
}

pub struct DaemonClient {
    // `None` means the previous stream died mid-call; the next `call`
    // will see it and reconnect before doing any I/O.
    stream: Mutex<Option<UnixStream>>,
    next_id: AtomicU64,
    config: ConnectConfig,
}

impl DaemonClient {
    /// Open the initial connection (auto-spawning the daemon if needed)
    /// and wrap it in a client that can reconnect on transport failure.
    pub async fn connect(config: ConnectConfig) -> Result<Self> {
        let stream = connect_with_autospawn(&config).await?;
        Ok(Self {
            stream: Mutex::new(Some(stream)),
            next_id: AtomicU64::new(1),
            config,
        })
    }

    pub async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        match self.try_call(method, &params).await {
            Ok(v) => Ok(v),
            Err(e) if looks_like_disconnect(&e) => {
                tracing::warn!(
                    error = %e,
                    socket = %self.config.socket.display(),
                    "daemon connection lost, reconnecting and retrying once",
                );
                // Drop the dead stream so the next try_call goes through
                // reconnect_locked and gets a fresh one.
                self.stream.lock().await.take();
                // Single retry. If the second call also fails, surface
                // that error — we don't want an infinite reconnect loop
                // when the daemon is broken in a way reconnect can't fix.
                self.try_call(method, &params).await
            }
            Err(e) => Err(e),
        }
    }

    async fn try_call(
        &self,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = Request {
            id,
            method: method.to_string(),
            params: params.clone(),
        };
        let payload = serde_json::to_vec(&req)?;
        let len = u32::try_from(payload.len()).map_err(|_| anyhow!("request too large"))?;

        let mut guard = self.stream.lock().await;
        if guard.is_none() {
            *guard = Some(connect_with_autospawn(&self.config).await?);
        }
        let stream = guard.as_mut().expect("just populated");

        // Write request frame. On any error here we leave `guard` holding
        // the (now likely dead) stream — `call` will Take() it on the
        // next pass and reconnect.
        stream.write_all(&len.to_be_bytes()).await?;
        stream.write_all(&payload).await?;
        stream.flush().await?;

        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let resp_len = u32::from_be_bytes(len_buf);
        if resp_len > MAX_FRAME {
            return Err(anyhow!("response too large: {resp_len}"));
        }
        let mut buf = vec![0u8; resp_len as usize];
        stream.read_exact(&mut buf).await?;
        let resp: Response = serde_json::from_slice(&buf)?;
        if let Some(err) = resp.error {
            return Err(anyhow!("daemon error {}: {}", err.code, err.message));
        }
        Ok(resp.result.unwrap_or(serde_json::Value::Null))
    }
}

/// Open a stream to the daemon, auto-spawning if the socket is missing
/// or refusing connections. Used for both the initial connect and for
/// post-failure reconnects.
async fn connect_with_autospawn(cfg: &ConnectConfig) -> Result<UnixStream> {
    match UnixStream::connect(&cfg.socket).await {
        Ok(s) => Ok(s),
        Err(e) if cfg.autospawn && spawn::is_connect_retryable(&e) => {
            tracing::info!(
                socket = %cfg.socket.display(),
                reason = %e,
                "daemon unreachable, auto-spawning",
            );
            spawn::spawn_and_connect(SpawnArgs {
                socket: &cfg.socket,
                root: &cfg.root,
                daemon_bin: cfg.daemon_bin.as_deref(),
                extra_args: &cfg.daemon_extra_args,
            })
            .await
        }
        Err(e) => Err(e).with_context(|| format!("connect {}", cfg.socket.display())),
    }
}

/// Heuristic: does this anyhow error chain look like a transport-layer
/// disconnect we can recover by reconnecting? We're deliberately broad
/// — false positives waste at most one extra connect attempt; false
/// negatives leak a reconnect-able failure as a hard error to the user.
fn looks_like_disconnect(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            return matches!(
                io.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::NotConnected
                    | std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::ConnectionRefused
            );
        }
        false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnect_detector_recognises_broken_pipe() {
        let io = std::io::Error::from(std::io::ErrorKind::BrokenPipe);
        let err = anyhow::Error::from(io).context("write failed");
        assert!(looks_like_disconnect(&err));
    }

    #[test]
    fn disconnect_detector_recognises_unexpected_eof() {
        let io = std::io::Error::from(std::io::ErrorKind::UnexpectedEof);
        assert!(looks_like_disconnect(&anyhow::Error::from(io)));
    }

    #[test]
    fn disconnect_detector_recognises_connection_refused() {
        // Hits the auto-spawn fallback path: a daemon that idle-exited
        // and unlinked its socket would surface as NotFound on the next
        // connect attempt; if it just dropped the listening socket it'd
        // be ConnectionRefused.
        let io = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert!(looks_like_disconnect(&anyhow::Error::from(io)));
        let io = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(looks_like_disconnect(&anyhow::Error::from(io)));
    }

    #[test]
    fn disconnect_detector_ignores_unrelated_errors() {
        let err = anyhow!("daemon error -32601: unknown method: foo");
        assert!(!looks_like_disconnect(&err));
        let err = anyhow!("response too large: 999999999");
        assert!(!looks_like_disconnect(&err));
    }
}
