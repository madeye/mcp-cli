use anyhow::{anyhow, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const MAX_FRAME: u32 = 16 * 1024 * 1024;

pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME {
        return Err(anyhow!("frame too large: {len}"));
    }
    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, payload: &[u8]) -> Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| anyhow!("frame too large to encode"))?;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn roundtrip_single_frame() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, b"hello").await.unwrap();
        let mut cursor = Cursor::new(buf);
        let frame = read_frame(&mut cursor).await.unwrap();
        assert_eq!(frame.as_deref(), Some(&b"hello"[..]));
    }

    #[tokio::test]
    async fn roundtrip_many_frames() {
        let mut buf: Vec<u8> = Vec::new();
        for payload in [&b""[..], b"a", b"bb", b"ccc"] {
            write_frame(&mut buf, payload).await.unwrap();
        }
        let mut cursor = Cursor::new(buf);
        for expected in [&b""[..], b"a", b"bb", b"ccc"] {
            let frame = read_frame(&mut cursor).await.unwrap();
            assert_eq!(frame.as_deref(), Some(expected));
        }
        let frame = read_frame(&mut cursor).await.unwrap();
        assert!(frame.is_none(), "clean EOF yields None");
    }

    #[tokio::test]
    async fn max_frame_accepted() {
        // Writing a frame exactly at MAX_FRAME should round-trip.
        let payload = vec![0x61u8; MAX_FRAME as usize];
        let mut buf: Vec<u8> = Vec::with_capacity(payload.len() + 4);
        write_frame(&mut buf, &payload).await.unwrap();
        let mut cursor = Cursor::new(buf);
        let frame = read_frame(&mut cursor).await.unwrap();
        assert_eq!(frame.unwrap().len(), MAX_FRAME as usize);
    }

    #[tokio::test]
    async fn oversize_length_rejected() {
        // Length prefix above MAX_FRAME is refused before reading the body.
        let too_big = (MAX_FRAME + 1).to_be_bytes();
        let mut cursor = Cursor::new(too_big.to_vec());
        let err = read_frame(&mut cursor).await.expect_err("should error");
        let msg = err.to_string();
        assert!(
            msg.contains("frame too large"),
            "unexpected error message: {msg}",
        );
    }

    #[tokio::test]
    async fn eof_mid_frame_is_error() {
        // Advertise 10 bytes but only provide 3.
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(&10u32.to_be_bytes());
        bytes.extend_from_slice(b"abc");
        let mut cursor = Cursor::new(bytes);
        let err = read_frame(&mut cursor).await.expect_err("should error");
        // std::io::ErrorKind::UnexpectedEof surfaces as "early eof" in tokio.
        let msg = err.to_string().to_lowercase();
        assert!(msg.contains("eof") || msg.contains("unexpected"));
    }

    #[tokio::test]
    async fn empty_stream_yields_none() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        let frame = read_frame(&mut cursor).await.unwrap();
        assert!(frame.is_none());
    }
}
