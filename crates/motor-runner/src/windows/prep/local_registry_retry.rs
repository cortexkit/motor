use std::{io, time::Duration};

const ERROR_WINHTTP_TIMEOUT: i32 = 12002;
const ERROR_WINHTTP_NAME_NOT_RESOLVED: i32 = 12007;
const ERROR_WINHTTP_CANNOT_CONNECT: i32 = 12029;
const ERROR_WINHTTP_CONNECTION_ERROR: i32 = 12030;
const RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(3)];

#[derive(Debug)]
pub(in crate::windows) enum FetchError {
    Transient(io::Error),
    Permanent(io::Error),
}

// Only the network code opts in to retries. Policy and progress errors are
// permanent regardless of their I/O error kind.
impl From<io::Error> for FetchError {
    fn from(error: io::Error) -> Self {
        Self::Permanent(error)
    }
}

impl From<FetchError> for io::Error {
    fn from(error: FetchError) -> Self {
        match error {
            FetchError::Transient(error) | FetchError::Permanent(error) => error,
        }
    }
}

pub(in crate::windows) fn winhttp_error(url: &str, phase: &str, error: io::Error) -> FetchError {
    let code = error.raw_os_error();
    if code == Some(ERROR_WINHTTP_TIMEOUT) {
        return FetchError::Transient(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("local-registry HTTPS request timed out during {phase}: {url}"),
        ));
    }
    let error = io::Error::new(
        error.kind(),
        format!("local-registry HTTPS {phase} failed for {url}: {error}"),
    );
    match code {
        Some(
            ERROR_WINHTTP_NAME_NOT_RESOLVED
            | ERROR_WINHTTP_CANNOT_CONNECT
            | ERROR_WINHTTP_CONNECTION_ERROR,
        ) => FetchError::Transient(error),
        _ => FetchError::Permanent(error),
    }
}

pub(in crate::windows) fn check_body_length(
    url: &str,
    declared: usize,
    received: usize,
) -> Result<(), FetchError> {
    use std::cmp::Ordering;

    match received.cmp(&declared) {
        Ordering::Equal => Ok(()),
        Ordering::Less => Err(FetchError::Transient(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("incomplete HTTPS response from {url}: declared {declared} bytes, received {received}"),
        ))),
        Ordering::Greater => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("HTTPS response from {url} exceeded Content-Length: declared {declared} bytes, received {received}"),
        )
        .into()),
    }
}

pub(in crate::windows) fn retry_fetch(
    mut fetch: impl FnMut() -> Result<Vec<u8>, FetchError>,
    mut sleep: impl FnMut(Duration),
) -> io::Result<Vec<u8>> {
    let mut delays = RETRY_DELAYS.into_iter();
    loop {
        match fetch() {
            Ok(bytes) => return Ok(bytes),
            Err(FetchError::Permanent(error)) => return Err(error),
            Err(FetchError::Transient(error)) => match delays.next() {
                Some(delay) => sleep(delay),
                None => {
                    let attempts = RETRY_DELAYS.len() + 1;
                    return Err(io::Error::new(
                        error.kind(),
                        format!("{error} (tried {attempts} times)"),
                    ));
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://index.crates.io/3/a/aes";

    #[test]
    fn transient_winhttp_failure_then_success_returns_body() {
        for code in [12002, 12007, 12029, 12030] {
            let mut attempts = 0;
            let mut delays = Vec::new();
            let body = retry_fetch(
                || {
                    attempts += 1;
                    if attempts == 1 {
                        Err(winhttp_error(
                            URL,
                            "send request",
                            io::Error::from_raw_os_error(code),
                        ))
                    } else {
                        Ok(b"complete response".to_vec())
                    }
                },
                |delay| delays.push(delay),
            )
            .unwrap();

            assert_eq!(body, b"complete response", "WinHTTP error {code}");
            assert_eq!(attempts, 2);
            assert_eq!(delays, [Duration::from_secs(1)]);
        }
    }

    #[test]
    fn three_transient_failures_report_last_error_and_attempt_count() {
        let mut attempts = 0;
        let mut delays = Vec::new();
        let error = retry_fetch(
            || {
                attempts += 1;
                Err(FetchError::Transient(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("fetch deadline exceeded on attempt {attempts}"),
                )))
            },
            |delay| delays.push(delay),
        )
        .unwrap_err();

        assert_eq!(attempts, 3);
        assert_eq!(delays, [Duration::from_secs(1), Duration::from_secs(3)]);
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(
            error.to_string(),
            "fetch deadline exceeded on attempt 3 (tried 3 times)"
        );
    }

    #[test]
    fn permanent_failures_do_not_retry() {
        for (kind, message) in [
            (io::ErrorKind::Other, "unexpected HTTP status 503"),
            (io::ErrorKind::PermissionDenied, "authorization refused"),
            (io::ErrorKind::InvalidInput, "response exceeds size limit"),
            (io::ErrorKind::InvalidData, "archive checksum mismatch"),
            (io::ErrorKind::TimedOut, "progress callback timed out"),
        ] {
            let mut attempts = 0;
            let error = retry_fetch(
                || {
                    attempts += 1;
                    Err(io::Error::new(kind, message).into())
                },
                |_| panic!("unexpected retry for {message}"),
            )
            .unwrap_err();

            assert_eq!(attempts, 1);
            assert_eq!(error.kind(), kind);
            assert_eq!(error.to_string(), message);
        }
    }

    #[test]
    fn winhttp_security_error_does_not_retry() {
        // ERROR_WINHTTP_SECURE_FAILURE reports a TLS certificate failure.
        let error = winhttp_error(URL, "send request", io::Error::from_raw_os_error(12175));
        assert!(matches!(error, FetchError::Permanent(_)));
    }

    #[test]
    fn only_short_bodies_are_retryable() {
        assert!(check_body_length(URL, 3, 3).is_ok());
        assert!(matches!(
            check_body_length(URL, 3, 2),
            Err(FetchError::Transient(error)) if error.kind() == io::ErrorKind::UnexpectedEof
        ));
        assert!(matches!(
            check_body_length(URL, 3, 4),
            Err(FetchError::Permanent(error)) if error.kind() == io::ErrorKind::InvalidData
        ));
    }
}
