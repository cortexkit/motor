use crate::Frame;
use std::io::{self, Read, Write};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

fn json_bytes(frame: &Frame) -> io::Result<(Vec<u8>, [u8; 4])> {
    let bytes = serde_json::to_vec(frame).map_err(invalid_data)?;
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

/// EOF at a frame boundary is a clean close. EOF anywhere else is a truncated
/// upload, never a clean close. No advertised-length-sized allocation is made
/// before bytes actually arrive.
pub fn read_frame(reader: &mut impl Read) -> io::Result<Option<Frame>> {
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
    let length = u32::from_be_bytes(length) as u64;
    let mut bytes = Vec::new();
    reader.take(length).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != length {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated JSON frame",
        ));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(invalid_data)
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
    let mut length = [0; 4];
    if reader.read(&mut length[..1]).await? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut length[1..]).await?;
    let length = u32::from_be_bytes(length) as u64;
    let mut bytes = Vec::new();
    reader.take(length).read_to_end(&mut bytes).await?;
    if bytes.len() as u64 != length {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated JSON frame",
        ));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(invalid_data)
}
