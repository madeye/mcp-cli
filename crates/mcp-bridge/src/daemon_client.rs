use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, Result};
use protocol::{Request, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

const MAX_FRAME: u32 = 16 * 1024 * 1024;

pub struct DaemonClient {
    stream: Mutex<UnixStream>,
    next_id: AtomicU64,
}

impl DaemonClient {
    pub async fn connect(socket: &Path) -> Result<Self> {
        let stream = UnixStream::connect(socket).await?;
        Ok(Self { stream: Mutex::new(stream), next_id: AtomicU64::new(1) })
    }

    pub async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = Request { id, method: method.to_string(), params };
        let payload = serde_json::to_vec(&req)?;
        let len = u32::try_from(payload.len()).map_err(|_| anyhow!("request too large"))?;

        let mut guard = self.stream.lock().await;
        guard.write_all(&len.to_be_bytes()).await?;
        guard.write_all(&payload).await?;
        guard.flush().await?;

        let mut len_buf = [0u8; 4];
        guard.read_exact(&mut len_buf).await?;
        let resp_len = u32::from_be_bytes(len_buf);
        if resp_len > MAX_FRAME {
            return Err(anyhow!("response too large: {resp_len}"));
        }
        let mut buf = vec![0u8; resp_len as usize];
        guard.read_exact(&mut buf).await?;
        let resp: Response = serde_json::from_slice(&buf)?;
        if let Some(err) = resp.error {
            return Err(anyhow!("daemon error {}: {}", err.code, err.message));
        }
        Ok(resp.result.unwrap_or(serde_json::Value::Null))
    }
}
