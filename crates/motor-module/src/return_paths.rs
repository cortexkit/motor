use motor_protocol::{default_return_max_bytes, RunRequest, RETURN_MAX_BYTES};
use serde::{Deserialize, Serialize};

/// File-return options for the direct command interface's exec.run operation.
/// Unlike agent requests, direct requests name absolute workspace paths. The
/// options share the RunRequest JSON object rather than nesting on the wire.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReturnOptions {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub return_paths: Vec<String>,
    #[serde(
        default = "default_return_max_bytes",
        skip_serializing_if = "is_default_cap"
    )]
    pub return_max_bytes: u64,
    /// Prefer chunked returned files on the direct interface. The module
    /// negotiates with the runner and caps legacy returns at 256 KiB.
    /// Agent requests are refused if the runner lacks chunked-return support;
    /// they are not downgraded to legacy returns.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
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
