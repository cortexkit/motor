use sha2::{Digest, Sha256};
use std::io::{self, BufRead};

/// Hash one JSONL frame without retaining its payload, including a torn final
/// frame that can be arbitrarily long before recovery truncates it.
pub(super) fn frame(reader: &mut impl BufRead) -> io::Result<Option<(u64, bool)>> {
    let mut header = [0; 65];
    let mut header_len = 0;
    let mut length = 0;
    let mut digest = Sha256::new();
    loop {
        let bytes = reader.fill_buf()?;
        if bytes.is_empty() {
            return Ok((length != 0).then_some((length, false)));
        }
        let newline = bytes.iter().position(|b| *b == b'\n');
        let n = newline.unwrap_or(bytes.len());
        let prefix = n.min(header.len() - header_len);
        header[header_len..header_len + prefix].copy_from_slice(&bytes[..prefix]);
        header_len += prefix;
        digest.update(&bytes[prefix..n]);
        let consumed = n + usize::from(newline.is_some());
        length += consumed as u64;
        reader.consume(consumed);
        if newline.is_some() {
            let valid = header_len == header.len()
                && header[64] == b'\t'
                && format!("{:x}", digest.finalize()).as_bytes() == &header[..64];
            return Ok(Some((length, valid)));
        }
    }
}
