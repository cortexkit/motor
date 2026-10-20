//! Host key generation and the enrollment facts the broker registers.
//!
//! The broker's operator script registers a host with
//! `register-host <host_id> <public_key_hex> <host:port> <endpoint_spki_sha256> <platform>`.
//! Every value printed here is checked against the grammar in
//! `motor-lease/contracts/cloud/runners-contract.md` first, so a malformed fact
//! is refused on the host instead of being pasted into the broker registry.

use anyhow::{anyhow, bail, ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use x509_cert::der::{Decode, Encode};

/// The only platform this host agent serves: tenant VMs are x86-64 Linux.
pub const PLATFORM: &str = "linux-x64";

/// Reads the raw 32-byte Ed25519 host signing seed. The file must be a regular
/// file (not a symlink), owned by the current user and unreadable by group and
/// others, because it authenticates every heartbeat and usage report.
pub fn read_signing_seed(path: &Path) -> Result<[u8; 32]> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("open host signing key {}", path.display()))?;
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no arguments or preconditions.
    ensure!(
        metadata.is_file()
            && metadata.permissions().mode() & 0o077 == 0
            && metadata.uid() == unsafe { libc::geteuid() },
        "host signing key must be a private, owned regular file"
    );
    let mut bytes = Vec::new();
    (&mut file).take(33).read_to_end(&mut bytes)?;
    bytes
        .try_into()
        .map_err(|_| anyhow!("host_signing_key must contain a 32-byte Ed25519 seed"))
}

/// Unpadded base64url SHA-256 of the certificate's DER SubjectPublicKeyInfo,
/// computed exactly as the hosted client's pin verifier computes it.
pub fn spki_sha256(certificate_der: &[u8]) -> Result<String> {
    let certificate = x509_cert::Certificate::from_der(certificate_der)
        .map_err(|e| anyhow!("invalid TLS certificate: {e}"))?;
    let spki = certificate
        .tbs_certificate
        .subject_public_key_info
        .to_der()
        .map_err(|e| anyhow!("invalid TLS certificate public key: {e}"))?;
    Ok(motor_lease::base64url(&Sha256::digest(spki)))
}

/// Reads the single PEM certificate the TLS listener serves and returns its DER.
pub fn read_certificate(path: &Path) -> Result<Vec<u8>> {
    let file =
        File::open(path).with_context(|| format!("open TLS certificate {}", path.display()))?;
    let certificates = rustls_pemfile::certs(&mut std::io::BufReader::new(file))
        .collect::<std::io::Result<Vec<_>>>()?;
    match certificates.as_slice() {
        [only] => Ok(only.to_vec()),
        _ => bail!("TLS certificate file must contain exactly one certificate"),
    }
}

/// Parses the endpoint the broker hands clients: a public IPv4 address and an
/// explicit nonzero port. Private, loopback and other non-routable addresses
/// are refused because clients could never reach them.
pub fn parse_endpoint(text: &str) -> Result<SocketAddrV4> {
    let endpoint: SocketAddrV4 = text
        .parse()
        .map_err(|_| anyhow!("endpoint must be <public IPv4>:<port>, got {text:?}"))?;
    ensure!(endpoint.port() != 0, "endpoint port must be nonzero");
    ensure!(
        endpoint.to_string() == text,
        "endpoint must be written canonically as {endpoint}"
    );
    ensure!(
        public_ipv4(*endpoint.ip()),
        "endpoint address {} is not a public IPv4 address",
        endpoint.ip()
    );
    Ok(endpoint)
}

fn public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || a == 0
        || a >= 240
        // 100.64.0.0/10 carrier-grade NAT and 198.18.0.0/15 benchmarking.
        || (a == 100 && (64..128).contains(&b))
        || (a == 198 && (b == 18 || b == 19)))
}

fn valid_host_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The five values the broker registers for a host, already validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enrollment {
    pub host_id: String,
    pub public_key_hex: String,
    pub endpoint: SocketAddrV4,
    pub endpoint_spki_sha256: String,
    pub platform: &'static str,
}

impl Enrollment {
    pub fn new(
        host_id: &str,
        seed: &[u8; 32],
        certificate_der: &[u8],
        endpoint: SocketAddrV4,
    ) -> Result<Self> {
        ensure!(
            valid_host_id(host_id),
            "host_id must match [A-Za-z0-9._-]{{1,64}}"
        );
        ensure!(
            public_ipv4(*endpoint.ip()) && endpoint.port() != 0,
            "endpoint must be a public IPv4 address with a nonzero port"
        );
        let public = ed25519_dalek::SigningKey::from_bytes(seed).verifying_key();
        let public_key_hex: String = public
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let endpoint_spki_sha256 = spki_sha256(certificate_der)?;
        // Both encodings are fixed by construction; the checks keep a future
        // change from printing a fact the broker's grammar would refuse.
        ensure!(
            public_key_hex.len() == 64
                && public_key_hex
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        );
        ensure!(
            endpoint_spki_sha256.len() == 43
                && endpoint_spki_sha256
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        Ok(Self {
            host_id: host_id.into(),
            public_key_hex,
            endpoint,
            endpoint_spki_sha256,
            platform: PLATFORM,
        })
    }

    /// One labelled line per value, in `register-host` argument order, then the
    /// filled-in `register-host` command line.
    pub fn render(&self) -> String {
        format!(
            "host_id: {}\npublic_key_hex: {}\nendpoint: {}\nendpoint_spki_sha256: {}\nplatform: {}\n\
             register-host {} {} {} {} {}\n",
            self.host_id,
            self.public_key_hex,
            self.endpoint,
            self.endpoint_spki_sha256,
            self.platform,
            self.host_id,
            self.public_key_hex,
            self.endpoint,
            self.endpoint_spki_sha256,
            self.platform,
        )
    }
}

/// The key files written by `keygen`.
pub struct KeyPaths {
    pub signing_key: PathBuf,
    pub tls_certificate: PathBuf,
    pub tls_key: PathBuf,
}

/// Creates the host signing seed and a self-signed Ed25519 TLS certificate and
/// key, each mode 0600 and owned by the calling user, and returns the enrollment
/// facts for them. Refuses before writing anything if any of the files exists,
/// so a registered host identity is never replaced by accident.
pub fn keygen(paths: &KeyPaths, host_id: &str, endpoint: SocketAddrV4) -> Result<Enrollment> {
    ensure!(
        valid_host_id(host_id),
        "host_id must match [A-Za-z0-9._-]{{1,64}}"
    );
    for path in [&paths.signing_key, &paths.tls_certificate, &paths.tls_key] {
        ensure!(
            std::fs::symlink_metadata(path).is_err(),
            "refusing to overwrite existing {}",
            path.display()
        );
    }
    let mut seed = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut seed);
    let tls_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519)?;
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new())?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, host_id);
    let certificate = params.self_signed(&tls_key)?;
    let enrollment = Enrollment::new(host_id, &seed, certificate.der(), endpoint)?;
    create_private(&paths.signing_key, &seed)?;
    create_private(&paths.tls_key, tls_key.serialize_pem().as_bytes())?;
    create_private(&paths.tls_certificate, certificate.pem().as_bytes())?;
    // Read back through the same checks the host agent applies at startup.
    ensure!(read_signing_seed(&paths.signing_key)? == seed);
    ensure!(read_certificate(&paths.tls_certificate)? == certificate.der().to_vec());
    Ok(enrollment)
}

fn create_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// Recomputes the enrollment facts from a host agent's existing key files.
/// The endpoint's port must be the port the agent listens on.
pub fn show(
    host_id: &str,
    signing_key: &Path,
    certificate: &Path,
    tls_key: &Path,
    listen: SocketAddr,
    endpoint: SocketAddrV4,
) -> Result<Enrollment> {
    ensure!(
        listen.port() == endpoint.port(),
        "endpoint port {} differs from the listen port {}",
        endpoint.port(),
        listen.port()
    );
    // Clients connect to the endpoint address, so the agent must listen on it
    // or on every IPv4 address.
    ensure!(
        listen.ip() == std::net::IpAddr::V4(Ipv4Addr::UNSPECIFIED)
            || listen.ip() == std::net::IpAddr::V4(*endpoint.ip()),
        "listen address {} does not accept connections to {}",
        listen.ip(),
        endpoint.ip()
    );
    let seed = read_signing_seed(signing_key)?;
    let certificate = read_certificate(certificate)?;
    ensure_key_matches(&certificate, tls_key)?;
    Enrollment::new(host_id, &seed, &certificate, endpoint)
}

/// A pin computed from a certificate whose private key is some other key would
/// enroll an endpoint that can never complete a TLS handshake.
fn ensure_key_matches(certificate_der: &[u8], tls_key: &Path) -> Result<()> {
    let pem = std::fs::read_to_string(tls_key)
        .with_context(|| format!("read TLS private key {}", tls_key.display()))?;
    let key = rcgen::KeyPair::from_pem(&pem).context("parse TLS private key")?;
    let certificate = x509_cert::Certificate::from_der(certificate_der)
        .map_err(|e| anyhow!("invalid TLS certificate: {e}"))?;
    let public = certificate
        .tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .raw_bytes();
    ensure!(
        key.public_key_raw() == public,
        "TLS private key does not match the certificate"
    );
    Ok(())
}
