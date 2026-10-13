use crate::{types::FrameTag, Frame};
use serde::Deserialize;
use std::io::{self, Read, Write};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Maximum JSON body size, excluding the four-byte prefix. The largest routine
/// payload is a 64 KiB bundle chunk (87,384 base64 bytes); 1 MiB leaves room for
/// its envelope and large control metadata without allowing unbounded frames.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Future records carry their cursor without being delivered as known frames.
#[derive(Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum DecodedFrame {
    Frame(Frame),
    Unknown(Option<u64>),
}

fn check_length(length: usize) -> io::Result<()> {
    if length > MAX_FRAME_BYTES {
        return Err(invalid_data("JSON frame exceeds the 1 MiB limit"));
    }
    Ok(())
}

/// Decode known frames directly. Only failures inspect a minimal envelope to
/// distinguish a future tag from a malformed payload of a supported type.
pub fn decode_frame(bytes: &[u8]) -> io::Result<DecodedFrame> {
    check_length(bytes.len())?;
    match serde_json::from_slice(bytes) {
        Ok(frame) => Ok(DecodedFrame::Frame(frame)),
        Err(error) => {
            #[derive(Deserialize)]
            struct Envelope {
                #[serde(rename = "type")]
                tag: FrameTag,
                seq: Option<serde_json::Value>,
            }
            match serde_json::from_slice::<Envelope>(bytes) {
                Ok(envelope) if matches!(envelope.tag, FrameTag::Unknown) => Ok(
                    DecodedFrame::Unknown(envelope.seq.and_then(|seq| seq.as_u64())),
                ),
                _ => Err(invalid_data(error)),
            }
        }
    }
}

fn json_bytes(frame: &Frame) -> io::Result<(Vec<u8>, [u8; 4])> {
    let bytes = serde_json::to_vec(frame).map_err(invalid_data)?;
    check_length(bytes.len())?;
    let length = u32::try_from(bytes.len())
        .map_err(|_| invalid_data("JSON frame exceeds the four-byte length field"))?;
    Ok((bytes, length.to_be_bytes()))
}

fn invalid_data(error: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

/// Writes exactly one length-prefixed object. The caller owns flushing and
/// serialisation between concurrent producers on a channel.
pub fn write_frame(writer: &mut impl Write, frame: &Frame) -> io::Result<()> {
    let (bytes, length) = json_bytes(frame)?;
    writer.write_all(&length)?;
    writer.write_all(&bytes)
}

/// Read a known frame, skipping future records. Use [`read_record`] when their
/// sequence cursors are needed.
pub fn read_frame(reader: &mut impl Read) -> io::Result<Option<Frame>> {
    loop {
        match read_record(reader)? {
            Some(DecodedFrame::Frame(frame)) => return Ok(Some(frame)),
            Some(DecodedFrame::Unknown(_)) => continue,
            None => return Ok(None),
        }
    }
}

/// EOF at a frame boundary is a clean close. EOF anywhere else is a truncated
/// upload. Oversized prefixes are rejected before any body allocation or read;
/// accepted bodies allocate only as bytes arrive.
pub fn read_record(reader: &mut impl Read) -> io::Result<Option<DecodedFrame>> {
    let mut length = [0; 4];
    loop {
        match reader.read(&mut length[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    reader.read_exact(&mut length[1..])?;
    let length = u32::from_be_bytes(length) as usize;
    check_length(length)?;
    let mut bytes = Vec::new();
    reader.take(length as u64).read_to_end(&mut bytes)?;
    if bytes.len() != length {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated JSON frame",
        ));
    }
    decode_frame(&bytes).map(Some)
}

pub async fn write_frame_async<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &Frame,
) -> io::Result<()> {
    let (bytes, length) = json_bytes(frame)?;
    writer.write_all(&length).await?;
    writer.write_all(&bytes).await
}

/// Like read_frame, with asynchronous IO. A caller must not cancel and retry a
/// partially polled read on the same stream: cancellation detaches the channel.
pub async fn read_frame_async<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<Frame>> {
    loop {
        match read_record_async(reader).await? {
            Some(DecodedFrame::Frame(frame)) => return Ok(Some(frame)),
            Some(DecodedFrame::Unknown(_)) => continue,
            None => return Ok(None),
        }
    }
}

/// Asynchronous [`read_record`]. Cancellation must detach the stream, not retry
/// a partially consumed frame.
pub async fn read_record_async<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> io::Result<Option<DecodedFrame>> {
    let mut length = [0; 4];
    if reader.read(&mut length[..1]).await? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut length[1..]).await?;
    let length = u32::from_be_bytes(length) as usize;
    check_length(length)?;
    let mut bytes = Vec::new();
    reader.take(length as u64).read_to_end(&mut bytes).await?;
    if bytes.len() != length {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated JSON frame",
        ));
    }
    decode_frame(&bytes).map(Some)
}
