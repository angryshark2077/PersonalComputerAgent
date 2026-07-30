use std::{fmt, io};

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
pub enum FrameError {
    ZeroLength,
    Oversized,
    Truncated,
    InvalidUtf8,
    InvalidJson,
    Io(io::Error),
}

impl PartialEq for FrameError {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::ZeroLength, Self::ZeroLength)
                | (Self::Oversized, Self::Oversized)
                | (Self::Truncated, Self::Truncated)
                | (Self::InvalidUtf8, Self::InvalidUtf8)
                | (Self::InvalidJson, Self::InvalidJson)
                | (Self::Io(_), Self::Io(_))
        )
    }
}

impl Eq for FrameError {}

impl fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ZeroLength => "zero-length Bridge frame",
            Self::Oversized => "oversized Bridge frame",
            Self::Truncated => "truncated Bridge frame",
            Self::InvalidUtf8 => "Bridge frame is not UTF-8",
            Self::InvalidJson => "Bridge frame is not valid JSON",
            Self::Io(_) => "Bridge frame I/O failed",
        })
    }
}

impl std::error::Error for FrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

/// Reads one length-prefixed JSON frame without consuming bytes from a following frame.
///
/// # Errors
///
/// Rejects zero, oversized, truncated, non-UTF-8, and invalid JSON frames.
pub async fn read_frame<R>(reader: &mut R) -> Result<Value, FrameError>
where
    R: AsyncRead + Unpin,
{
    let payload = read_frame_bytes(reader).await?;
    serde_json::from_slice(&payload).map_err(|_| FrameError::InvalidJson)
}

/// Reads one length-prefixed UTF-8 frame without normalizing the JSON representation.
///
/// This is used by strict typed decoders so Serde can detect duplicate struct fields directly.
///
/// # Errors
///
/// Rejects zero, oversized, truncated, and non-UTF-8 frames.
pub async fn read_frame_bytes<R>(reader: &mut R) -> Result<Vec<u8>, FrameError>
where
    R: AsyncRead + Unpin,
{
    let mut prefix = [0_u8; 4];
    reader
        .read_exact(&mut prefix)
        .await
        .map_err(map_read_error)?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length == 0 {
        return Err(FrameError::ZeroLength);
    }
    if length > MAX_FRAME_BYTES {
        return Err(FrameError::Oversized);
    }

    let mut payload = vec![0_u8; length];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(map_read_error)?;
    std::str::from_utf8(&payload).map_err(|_| FrameError::InvalidUtf8)?;
    Ok(payload)
}

/// Writes one length-prefixed JSON frame.
///
/// # Errors
///
/// Rejects serialized payloads larger than one MiB and reports safe I/O/JSON errors.
pub async fn write_frame<W>(writer: &mut W, value: &Value) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin,
{
    let payload = serde_json::to_vec(value).map_err(|_| FrameError::InvalidJson)?;
    if payload.is_empty() {
        return Err(FrameError::ZeroLength);
    }
    if payload.len() > MAX_FRAME_BYTES {
        return Err(FrameError::Oversized);
    }
    let length = u32::try_from(payload.len()).map_err(|_| FrameError::Oversized)?;
    writer
        .write_all(&length.to_be_bytes())
        .await
        .map_err(FrameError::Io)?;
    writer.write_all(&payload).await.map_err(FrameError::Io)?;
    writer.flush().await.map_err(FrameError::Io)
}

fn map_read_error(error: io::Error) -> FrameError {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        FrameError::Truncated
    } else {
        FrameError::Io(error)
    }
}
