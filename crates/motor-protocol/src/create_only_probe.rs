use serde::{Deserialize, Serialize};

pub const CREATE_ONLY_PROBE_CASES: [&str; 4] = [
    "concurrent_creates",
    "existing_object",
    "interrupted_body",
    "fill_race",
];

/// Fixed diagnostics prevent store error bodies or signing material from
/// crossing the host-only probe's reply boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CreateOnlyProbeDetail {
    Passed,
    UnexpectedStatusOrBytes,
    TimedOut,
    RemoteCacheUnavailable,
    RemoteCacheAccountMismatch,
    RemoteCacheWriteKeyMissing,
    RemoteCacheWriteBlocked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateOnlyProbeCase {
    pub name: String,
    pub ok: bool,
    pub detail: CreateOnlyProbeDetail,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateOnlyProbeReport {
    pub ok: bool,
    pub exit_code: u8,
    pub cases: Vec<CreateOnlyProbeCase>,
}

impl CreateOnlyProbeReport {
    pub fn new(cases: Vec<CreateOnlyProbeCase>) -> Self {
        let ok = cases.len() == CREATE_ONLY_PROBE_CASES.len()
            && cases
                .iter()
                .zip(CREATE_ONLY_PROBE_CASES)
                .all(|(case, name)| {
                    case.name == name && case.ok && case.detail == CreateOnlyProbeDetail::Passed
                });
        Self {
            ok,
            exit_code: u8::from(!ok),
            cases,
        }
    }

    pub fn refused(detail: CreateOnlyProbeDetail) -> Self {
        Self::new(
            CREATE_ONLY_PROBE_CASES
                .iter()
                .map(|name| CreateOnlyProbeCase {
                    name: (*name).into(),
                    ok: false,
                    detail,
                })
                .collect(),
        )
    }

    pub fn valid(&self) -> bool {
        self.cases.len() == CREATE_ONLY_PROBE_CASES.len()
            && self
                .cases
                .iter()
                .zip(CREATE_ONLY_PROBE_CASES)
                .all(|(case, name)| {
                    case.name == name && case.ok == (case.detail == CreateOnlyProbeDetail::Passed)
                })
            && self == &Self::new(self.cases.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_are_closed_and_nonzero_on_any_failure() {
        let failed = CreateOnlyProbeReport::refused(CreateOnlyProbeDetail::RemoteCacheWriteBlocked);
        assert!(failed.valid());
        assert!(!failed.ok);
        assert_eq!(failed.exit_code, 1);
        let mut passed = failed.clone();
        for case in &mut passed.cases {
            case.ok = true;
            case.detail = CreateOnlyProbeDetail::Passed;
        }
        let passed = CreateOnlyProbeReport::new(passed.cases);
        assert!(passed.valid());
        assert!(passed.ok);
        assert_eq!(passed.exit_code, 0);
        let mut value = serde_json::to_value(&passed).unwrap();
        value["cases"][0]["detail"] = "store response containing a secret".into();
        assert!(serde_json::from_value::<CreateOnlyProbeReport>(value).is_err());
        let mut invalid = passed;
        invalid.cases[0].name = "unrecognized".into();
        assert!(!invalid.valid());
    }
}
