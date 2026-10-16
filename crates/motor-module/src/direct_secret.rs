use sha2::{Digest, Sha256};
use std::io;
use subtle::ConstantTimeEq;

// This value is never Debug or Serialize: only the submitting route receives it.
pub(crate) struct DirectSecret(String);

impl DirectSecret {
    pub fn mint() -> io::Result<Self> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)
            .map_err(|_| io::Error::other("direct secret generation failed"))?;
        Ok(Self(
            bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
        ))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn hash(&self) -> [u8; 32] {
        digest(&self.0)
    }
}

fn digest(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

pub(crate) fn matches(stored: Option<[u8; 32]>, presented: Option<&str>) -> bool {
    let secret = presented.unwrap_or_default();
    let hash = digest(secret);
    // Missing jobs and missing credentials take the same digest comparison as
    // a wrong credential. No runner lookup occurs until this check succeeds.
    let equal = hash.ct_eq(&stored.unwrap_or([0; 32]));
    let valid = secret.len() == 64
        && secret
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    bool::from(equal) & stored.is_some() & valid
}
