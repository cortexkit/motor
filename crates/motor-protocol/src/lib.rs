//! Shared exec-remote/v1 contract. Caller types are re-exported from
//! `cortexkit-exec-remote-types`; runner frames are JSON objects preceded by a four-byte
//! big-endian byte length; output and bundle payloads use standard padded base64.
//! Unknown object fields are deliberately ignored for additive compatibility.

mod codec;
pub mod logging;
mod types;

pub use codec::{read_frame, read_frame_async, write_frame, write_frame_async};
pub use types::*;

pub const PROTOCOL_VERSION: u32 = 1;
pub const CAPABILITY: &str = "exec-remote/v1";
