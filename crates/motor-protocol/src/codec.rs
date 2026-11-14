use crate::{types::FrameTag, Frame, TerminalRecord};
use serde::{Deserialize, Serialize};
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

fn json_bytes(frame: &impl Serialize) -> io::Result<(Vec<u8>, [u8; 4])> {
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

/// Guest-to-host accounting, separate from the shared caller terminal record.
/// Unknown measurements stay absent; overage is present only for burst leases.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalMemory {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_peak_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_working_peak_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_over_mib_ms: Option<u64>,
}

/// Add accounting only on the guest-to-host stream. Ordinary Frame decoders
/// ignore these fields and still decode the unchanged terminal record.
pub async fn write_terminal_with_memory_async<W: AsyncWrite + Unpin>(
    writer: &mut W,
    terminal: &TerminalRecord,
    memory: &TerminalMemory,
) -> io::Result<()> {
    #[derive(Serialize)]
    struct Envelope<'a> {
        #[serde(rename = "type")]
        kind: &'static str,
        #[serde(flatten)]
        terminal: &'a TerminalRecord,
        #[serde(flatten)]
        memory: &'a TerminalMemory,
    }
    let (bytes, length) = json_bytes(&Envelope {
        kind: "terminal",
        terminal,
        memory,
    })?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn terminal_accounting_is_additive_and_absent_by_default() {
        let terminal = TerminalRecord::new(
            crate::Uuid::now_v7(),
            crate::Outcome::Exit { code: 0 },
            100,
            0,
            0,
        );
        let frame = Frame::Terminal(terminal.clone());
        let mut ordinary = Vec::new();
        write_frame_async(&mut ordinary, &frame).await.unwrap();
        for memory in [
            TerminalMemory::default(),
            TerminalMemory {
                mem_peak_mib: Some(12),
                mem_working_peak_mib: Some(10),
                mem_over_mib_ms: Some(100),
            },
        ] {
            let mut bytes = Vec::new();
            write_terminal_with_memory_async(&mut bytes, &terminal, &memory)
                .await
                .unwrap();
            assert_eq!(
                u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize,
                bytes.len() - 4
            );
            assert_eq!(
                decode_frame(&bytes[4..]).unwrap(),
                DecodedFrame::Frame(frame.clone())
            );
            let value: serde_json::Value = serde_json::from_slice(&bytes[4..]).unwrap();
            for (name, expected) in [
                ("mem_peak_mib", memory.mem_peak_mib),
                ("mem_working_peak_mib", memory.mem_working_peak_mib),
                ("mem_over_mib_ms", memory.mem_over_mib_ms),
            ] {
                assert_eq!(value.get(name).and_then(|v| v.as_u64()), expected);
                assert_eq!(value.get(name).is_some(), expected.is_some());
            }
            if memory == TerminalMemory::default() {
                assert_eq!(bytes, ordinary);
            }
        }
    }
}
