//! Host key generation and the enrollment facts printed for the broker.
use super::scratch;
use motor_host::enroll::{self, Enrollment, KeyPaths};
use std::os::unix::fs::{MetadataExt, PermissionsExt};

/// RFC 8032 section 7.1 test 1: this seed's Ed25519 public key is
/// d75a9801...511a, so the expected hex below is independent of this code.
const SEED: &str = "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
const PUBLIC_HEX: &str = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a";

/// A self-signed Ed25519 certificate made with OpenSSL 3.5.5
/// (`openssl req -new -x509 -key k.pem -subj /CN=test-host -days 36500`). Its pin
/// was computed by OpenSSL, not by this crate:
/// `openssl x509 -pubkey -noout | openssl pkey -pubin -outform DER |
/// openssl dgst -sha256 -binary | base64 | tr '+/' '-_' | tr -d '='`.
const CERTIFICATE: &str = "-----BEGIN CERTIFICATE-----
MIIBPjCB8aADAgECAhRjcfNBIP5WzLJAzrkPfHrhwwK7azAFBgMrZXAwFDESMBAG
A1UEAwwJdGVzdC1ob3N0MCAXDTI2MTAyMDAzMjg0NloYDzIxMjYwOTI2MDMyODQ2
WjAUMRIwEAYDVQQDDAl0ZXN0LWhvc3QwKjAFBgMrZXADIQDwssw57F84jkpTo7r9
xDOwdsjJrU12WtDFW2/0NegNjqNTMFEwHQYDVR0OBBYEFAKqN4er3G2082gYTbFF
8ef1rNpiMB8GA1UdIwQYMBaAFAKqN4er3G2082gYTbFF8ef1rNpiMA8GA1UdEwEB
/wQFMAMBAf8wBQYDK2VwA0EAVUc3/+E8UXV8eoU7tyHu/VMV5VR197rFbWSaBLK7
YkycTINaraPkgvK1//JGTcQ4LF4R5AQlTbqhi2JG0O/yAQ==
-----END CERTIFICATE-----
";
const PIN: &str = "KKquaEQRFSx7abONCrOIewsTGBuYrrM6QG6MF3QBUpk";

fn seed() -> [u8; 32] {
    let bytes: Vec<u8> = (0..64)
        .step_by(2)
        .map(|i| u8::from_str_radix(&SEED[i..i + 2], 16).unwrap())
        .collect();
    bytes.try_into().unwrap()
}

fn certificate_der() -> Vec<u8> {
    let certs = rustls_pemfile::certs(&mut CERTIFICATE.as_bytes())
        .collect::<std::io::Result<Vec<_>>>()
        .unwrap();
    certs[0].to_vec()
}

#[test]
fn enrollment_from_fixed_seed_and_certificate_prints_register_host_facts() {
    assert_eq!(enroll::spki_sha256(&certificate_der()).unwrap(), PIN);
    let endpoint = enroll::parse_endpoint("1.2.3.4:8443").unwrap();
    let enrollment = Enrollment::new("test-host", &seed(), &certificate_der(), endpoint).unwrap();
    assert_eq!(
        enrollment.render(),
        format!(
            "host_id: test-host\npublic_key_hex: {PUBLIC_HEX}\nendpoint: 1.2.3.4:8443\n\
             endpoint_spki_sha256: {PIN}\nplatform: linux-x64\n\
             register-host test-host {PUBLIC_HEX} 1.2.3.4:8443 {PIN} linux-x64\n"
        )
    );
}

#[test]
fn enrollment_refuses_values_outside_the_broker_grammar() {
    let endpoint = enroll::parse_endpoint("1.2.3.4:8443").unwrap();
    for host_id in ["", "test host", "test/host", &"a".repeat(65)] {
        assert!(Enrollment::new(host_id, &seed(), &certificate_der(), endpoint).is_err());
    }
    assert!(Enrollment::new(&"a".repeat(64), &seed(), &certificate_der(), endpoint).is_ok());
    assert!(Enrollment::new("test-host", &seed(), b"not a certificate", endpoint).is_err());
    for text in [
        "1.2.3.4",
        "1.2.3.4:0",
        "1.2.3.4:65536",
        "https://1.2.3.4:8443",
        "1.2.3.4:8443/",
        "01.2.3.4:8443",
        "[2001:4860::1]:8443",
        "host.example:8443",
        "0.0.0.0:8443",
        "127.0.0.1:8443",
        "10.0.0.5:8443",
        "172.16.0.1:8443",
        "192.168.1.1:8443",
        "169.254.1.1:8443",
        "100.64.0.1:8443",
        "198.18.0.1:8443",
        "203.0.113.7:8443",
        "224.0.0.1:8443",
        "255.255.255.255:8443",
    ] {
        assert!(enroll::parse_endpoint(text).is_err(), "{text} was accepted");
    }
    assert_eq!(
        enroll::parse_endpoint("5.6.7.8:8443").unwrap().to_string(),
        "5.6.7.8:8443"
    );
}

fn paths(dir: &std::path::Path) -> KeyPaths {
    KeyPaths {
        signing_key: dir.join("host-signing.key"),
        tls_certificate: dir.join("tls.crt"),
        tls_key: dir.join("tls.key"),
    }
}

#[test]
fn keygen_writes_private_owned_files_that_show_enrollment_reproduces() {
    let dir = scratch();
    let paths = paths(dir.path());
    let endpoint = enroll::parse_endpoint("1.2.3.4:8443").unwrap();
    let made = enroll::keygen(&paths, "test-host", endpoint).unwrap();
    // SAFETY: geteuid has no arguments or preconditions.
    let uid = unsafe { libc::geteuid() };
    for path in [&paths.signing_key, &paths.tls_certificate, &paths.tls_key] {
        let metadata = std::fs::symlink_metadata(path).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600, "{path:?}");
        assert_eq!(metadata.uid(), uid);
    }
    assert_eq!(std::fs::read(&paths.signing_key).unwrap().len(), 32);
    // The host agent's own TLS loader must accept what keygen wrote.
    let certs = rustls_pemfile::certs(&mut std::io::BufReader::new(
        std::fs::File::open(&paths.tls_certificate).unwrap(),
    ))
    .collect::<std::io::Result<Vec<_>>>()
    .unwrap();
    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(
        std::fs::File::open(&paths.tls_key).unwrap(),
    ))
    .unwrap()
    .unwrap();
    rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .unwrap();
    let listen = "0.0.0.0:8443".parse().unwrap();
    let shown = enroll::show(
        "test-host",
        &paths.signing_key,
        &paths.tls_certificate,
        &paths.tls_key,
        listen,
        endpoint,
    )
    .unwrap();
    assert_eq!(shown, made);
    let seed: [u8; 32] = std::fs::read(&paths.signing_key)
        .unwrap()
        .try_into()
        .unwrap();
    let public = ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key();
    let hex: String = public
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(made.public_key_hex, hex);
}

#[test]
fn keygen_refuses_to_overwrite_any_existing_key_file() {
    let endpoint = enroll::parse_endpoint("1.2.3.4:8443").unwrap();
    for existing in 0..3 {
        let dir = scratch();
        let paths = paths(dir.path());
        let target = [&paths.signing_key, &paths.tls_certificate, &paths.tls_key][existing];
        std::fs::write(target, b"keep").unwrap();
        assert!(enroll::keygen(&paths, "test-host", endpoint).is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"keep");
        // Nothing else was created either.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
    let dir = scratch();
    let paths = paths(dir.path());
    std::os::unix::fs::symlink(dir.path().join("elsewhere"), &paths.tls_key).unwrap();
    assert!(enroll::keygen(&paths, "test-host", endpoint).is_err());
    assert!(!dir.path().join("elsewhere").exists());
}

#[test]
fn show_enrollment_refuses_mismatched_listener_key_or_seed_mode() {
    let dir = scratch();
    let paths = paths(dir.path());
    let endpoint = enroll::parse_endpoint("1.2.3.4:8443").unwrap();
    enroll::keygen(&paths, "test-host", endpoint).unwrap();
    let show = |listen: &str, tls_key: &std::path::Path| {
        enroll::show(
            "test-host",
            &paths.signing_key,
            &paths.tls_certificate,
            tls_key,
            listen.parse().unwrap(),
            endpoint,
        )
    };
    assert!(show("0.0.0.0:8443", &paths.tls_key).is_ok());
    assert!(show("1.2.3.4:8443", &paths.tls_key).is_ok());
    assert!(show("0.0.0.0:9443", &paths.tls_key).is_err());
    assert!(show("127.0.0.1:8443", &paths.tls_key).is_err());
    assert!(show("[::]:8443", &paths.tls_key).is_err());
    let other = scratch();
    let other_paths = self::paths(other.path());
    enroll::keygen(&other_paths, "test-host", endpoint).unwrap();
    assert!(show("0.0.0.0:8443", &other_paths.tls_key).is_err());
    std::fs::set_permissions(&paths.signing_key, std::fs::Permissions::from_mode(0o640)).unwrap();
    assert!(show("0.0.0.0:8443", &paths.tls_key).is_err());
}
