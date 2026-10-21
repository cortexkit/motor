use motor_protocol::{default_return_max_bytes, RunRequest, RETURN_MAX_BYTES};
use serde::{Deserialize, Serialize};

/// File-return options for the direct command interface's exec.run operation.
/// The shared RunRequest does not yet carry these fields, so they are merged
/// into its JSON object rather than nested on the wire.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReturnOptions {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub return_paths: Vec<String>,
    #[serde(
        default = "default_return_max_bytes",
        skip_serializing_if = "is_default_cap"
    )]
    pub return_max_bytes: u64,
    /// Ask the runner for chunked returned files (`Run.return_chunked`). Only
    /// the agent route sets it, so it is never read from or written to the
    /// direct interface's JSON: `ck motor run` keeps the single
    /// `return_result` behaviour.
    #[serde(skip)]
    pub return_chunked: bool,
}

fn is_default_cap(cap: &u64) -> bool {
    *cap == RETURN_MAX_BYTES
}

impl Default for ReturnOptions {
    fn default() -> Self {
        Self {
            return_paths: Vec::new(),
            return_max_bytes: RETURN_MAX_BYTES,
            return_chunked: false,
        }
    }
}

impl ReturnOptions {
    pub fn params(&self, request: &RunRequest) -> serde_json::Value {
        let mut params = serde_json::to_value(request).expect("serializable run request");
        params.as_object_mut().unwrap().extend(
            serde_json::to_value(self)
                .expect("serializable return options")
                .as_object()
                .unwrap()
                .clone(),
        );
        params
    }
}
