//! Bound diagnostic strings to their first terminator; Windows debug-event
//! lengths can include padding that must never become logged child heap data.
pub(super) fn decode(bytes: &[u8], unicode: bool) -> String {
    if unicode {
        let words = bytes.as_chunks::<2>().0.iter().map(|pair| u16::from_le_bytes(*pair)).take_while(|word| *word != 0).collect::<Vec<_>>();
        String::from_utf16_lossy(&words)
    } else {
        let length = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[..length]).into_owned()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn debug_text_does_not_log_unicode_heap_tail() {
        assert_eq!(decode(&[b'R',0,0,0,0xad,0xba,0x0d,0xf0], true), "R");
    }
    #[test]
    fn debug_text_does_not_log_ansi_heap_tail() {
        assert_eq!(decode(b"loader\0private heap", false), "loader");
    }
    #[test]
    fn debug_text_handles_missing_terminator_and_partial_word() {
        assert_eq!(decode(&[b'R',0,0xff], true), "R");
        assert_eq!(decode(b"loader", false), "loader");
    }
}
