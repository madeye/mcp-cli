use anyhow::{anyhow, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_FRAME: u32 = 16 * 1024 * 1024;

/// Convenience wrapper around `read_frame_into` for callers that don't
/// recycle buffers. Production paths use `read_frame_into` directly with
/// a pooled `Vec<u8>`; this is mainly here for tests.
#[cfg(test)]
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    match read_frame_into(reader, &mut buf).await? {
        Some(()) => Ok(Some(buf)),
        None => Ok(None),
    }
}

/// Length-prefixed frame read into a caller-owned buffer. The buffer
/// is `clear`ed and resized to the frame length; callers can pass a
/// recycled `Vec<u8>` from `BufferPool` to avoid per-request allocation.
/// Returns `Ok(None)` on a clean EOF before the length prefix.
pub async fn read_frame_into<R: AsyncRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> Result<Option<()>> {
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
    buf.clear();
    buf.resize(len as usize, 0);
    reader.read_exact(buf.as_mut_slice()).await?;
    Ok(Some(()))
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

    #[tokio::test]
    async fn read_frame_into_reuses_buffer_storage() {
        // Fill a buffer with one frame, then read a smaller frame into
        // the same buffer; the second read must not allocate fresh
        // storage if the existing capacity already covers it.
        let mut bytes: Vec<u8> = Vec::new();
        write_frame(&mut bytes, &vec![0xCDu8; 1024]).await.unwrap();
        write_frame(&mut bytes, b"tiny").await.unwrap();
        let mut cursor = Cursor::new(bytes);

        let mut buf: Vec<u8> = Vec::new();
        assert!(read_frame_into(&mut cursor, &mut buf)
            .await
            .unwrap()
            .is_some());
        let cap_after_first = buf.capacity();
        assert!(cap_after_first >= 1024);

        assert!(read_frame_into(&mut cursor, &mut buf)
            .await
            .unwrap()
            .is_some());
        assert_eq!(&buf[..], b"tiny");
        // The second read must not have shrunk capacity — the whole
        // point of the API is to reuse the existing allocation.
        assert_eq!(buf.capacity(), cap_after_first);
    }

    #[tokio::test]
    async fn read_frame_into_eof_clean_yields_none() {
        let mut cursor = Cursor::new(Vec::<u8>::new());
        let mut buf: Vec<u8> = Vec::new();
        assert!(read_frame_into(&mut cursor, &mut buf)
            .await
            .unwrap()
            .is_none());
    }
}
