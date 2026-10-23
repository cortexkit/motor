//! AWS Signature Version 4 for the gateway's path-style S3 requests.
//!
//! Bodies are streamed through a bounded buffer and never held whole, so the
//! payload is declared `UNSIGNED-PAYLOAD` rather than hashed; TLS protects it
//! in transit to an `https` endpoint (production stores use one).
use super::super::provisioning::Credential;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";
const SERVICE: &str = "s3";

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Percent-encodes everything except the unreserved characters, keeping `/`
/// when `keep_slash` is set, as SigV4 canonical S3 paths require (S3 paths are
/// encoded once, not twice).
pub(super) fn uri_encode(value: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'~')
            || (keep_slash && byte == b'/')
        {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `YYYYMMDDTHHMMSSZ` for a time in UTC.
pub(super) fn amz_date(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    // Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
    // civil_from_days).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

/// The `Authorization` value for one request.
///
/// `headers` holds every header to sign as lowercase name and trimmed value;
/// it must include `host`, `x-amz-content-sha256` and `x-amz-date`.
/// `canonical_uri` is the already-encoded path. The gateway never sends a
/// query string, so the canonical query is always empty.
pub(super) fn authorization(
    method: &str,
    canonical_uri: &str,
    headers: &[(String, String)],
    payload_hash: &str,
    credential: &Credential,
    region: &str,
    amz_date: &str,
) -> String {
    let mut sorted: Vec<&(String, String)> = headers.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut canonical_headers = String::new();
    for (name, value) in &sorted {
        canonical_headers.push_str(name);
        canonical_headers.push(':');
        canonical_headers.push_str(value.trim());
        canonical_headers.push('\n');
    }
    let signed_headers = sorted
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_request = format!(
        "{method}\n{canonical_uri}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );
    let day = &amz_date[..8];
    let scope = format!("{day}/{region}/{SERVICE}/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex(&Sha256::digest(canonical_request.as_bytes()))
    );
    let mut key = format!("AWS4{}", credential.secret_access_key).into_bytes();
    for part in [day, region, SERVICE, "aws4_request"] {
        key = hmac(&key, part.as_bytes());
    }
    let signature = hex(&hmac(&key, string_to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
        credential.access_key_id
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn example_credential() -> Credential {
        Credential {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".to_string(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string(),
        }
    }

    fn pairs(values: &[(&str, &str)]) -> Vec<(String, String)> {
        values
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    /// The "GET Object" example of the AWS SigV4 header-signing documentation.
    #[test]
    fn matches_the_aws_get_object_example() {
        let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let header = authorization(
            "GET",
            "/test.txt",
            &pairs(&[
                ("host", "examplebucket.s3.amazonaws.com"),
                ("range", "bytes=0-9"),
                ("x-amz-content-sha256", empty),
                ("x-amz-date", "20130524T000000Z"),
            ]),
            empty,
            &example_credential(),
            "us-east-1",
            "20130524T000000Z",
        );
        assert_eq!(
            header,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    /// The "PUT Object" example of the same documentation, which also signs a
    /// storage class and needs `$` encoded in the path.
    #[test]
    fn matches_the_aws_put_object_example() {
        let payload = hex(&Sha256::digest(b"Welcome to Amazon S3."));
        let header = authorization(
            "PUT",
            &format!("/{}", uri_encode("test$file.text", true)),
            &pairs(&[
                ("date", "Fri, 24 May 2013 00:00:00 GMT"),
                ("host", "examplebucket.s3.amazonaws.com"),
                ("x-amz-content-sha256", &payload),
                ("x-amz-date", "20130524T000000Z"),
                ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
            ]),
            &payload,
            &example_credential(),
            "us-east-1",
            "20130524T000000Z",
        );
        assert!(
            header.ends_with(
                "Signature=98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"
            ),
            "{header}"
        );
    }

    #[test]
    fn formats_utc_dates() {
        assert_eq!(amz_date(UNIX_EPOCH), "19700101T000000Z");
        assert_eq!(
            amz_date(UNIX_EPOCH + Duration::from_secs(1_369_353_600)),
            "20130524T000000Z"
        );
        // 2024-02-29 23:59:59, a leap day.
        assert_eq!(
            amz_date(UNIX_EPOCH + Duration::from_secs(1_709_251_199)),
            "20240229T235959Z"
        );
    }
}
