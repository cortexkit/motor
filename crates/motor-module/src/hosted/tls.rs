use super::{now_s, Error};
use crate::transport::{Channel, Duplex};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::SigningKey;
use motor_lease::{Claims, LeaseResponse, Prelude, Purpose, Version1};
use motor_protocol::Uuid;
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
    DigitallySignedStruct, SignatureScheme,
};
use sha2::{Digest, Sha256};
use std::{io, sync::Arc, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use x509_cert::der::{Decode, Encode};

#[derive(Debug)]
struct PinVerifier {
    pin: String,
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
}
impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let cert = x509_cert::Certificate::from_der(end.as_ref())
            .map_err(|_| rustls::Error::General("invalid host certificate".into()))?;
        let spki = cert
            .tbs_certificate
            .subject_public_key_info
            .to_der()
            .map_err(|_| rustls::Error::General("invalid host SPKI".into()))?;
        if motor_lease::base64url(&Sha256::digest(spki)) != self.pin {
            return Err(rustls::Error::General("hosted_spki_mismatch".into()));
        }
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

// The authenticated broker supplies the compact token. Parsing its claims here
// binds the challenge to that response; only the host verifies its JWS signature.
pub(super) fn claims(
    id: Uuid,
    lease: &LeaseResponse,
    key: &SigningKey,
    purpose: Purpose,
) -> Result<Claims, Error> {
    let encoded = lease.job_token.split('.').nth(1).ok_or(Error::Auth)?;
    let claims: Claims =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded).map_err(|_| Error::Auth)?)
            .map_err(|_| Error::Auth)?;
    let jwk = motor_lease::RequesterJwk::from_key(&key.verifying_key());
    let now = now_s();
    // Name the first failed binding so a refused lease is diagnosable from the
    // log. Every value logged here is public lease metadata, never the token.
    let failed = [
        ("job_id", lease.job_id != id.to_string()),
        ("jti", claims.jti != lease.job_id),
        ("placement", claims.placement != lease.placement),
        ("purpose", claims.purpose != purpose),
        ("max_s", claims.max_s != lease.max_s),
        ("iss", claims.iss != motor_lease::ISSUER),
        ("aud", claims.aud != lease.placement.host),
        ("iat", claims.iat > now.saturating_add(60)),
        ("exp", claims.exp <= now),
        (
            "token_expires_at_ms",
            lease.token_expires_at_ms != claims.exp.saturating_mul(1000)
                || lease.token_expires_at_ms <= now.saturating_mul(1000),
        ),
        ("cnf", motor_lease::bind_requester(&claims, &jwk).is_err()),
    ]
    .into_iter()
    .find_map(|(field, failed)| failed.then_some(field));
    if let Some(field) = failed {
        tracing::info!(job_id = %id, field, "hosted lease claims mismatch");
        return Err(Error::Auth);
    }
    Ok(claims)
}

pub(super) async fn connect(
    id: Uuid,
    lease: &LeaseResponse,
    key: &SigningKey,
    purpose: Purpose,
) -> Result<Channel, Error> {
    let claims = claims(id, lease, key, purpose)?;
    tokio::time::timeout(Duration::from_secs(60), async {
        let provider = rustls::crypto::ring::default_provider();
        let verifier = PinVerifier {
            pin: lease.endpoint_spki_sha256.clone(),
            algorithms: provider.signature_verification_algorithms,
        };
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .map_err(|_| Error::Connect)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        let tcp = tokio::net::TcpStream::connect(&lease.endpoint)
            .await
            .map_err(|_| Error::Connect)?;
        // SNI is only a TLS routing hint. The verifier above uses neither names
        // nor trust roots: the lease's SPKI is the sole certificate identity.
        let name = ServerName::try_from("hosted.invalid").map_err(|_| Error::Connect)?;
        let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(name, tcp)
            .await
            .map_err(|e| {
                if e.to_string().contains("hosted_spki_mismatch") {
                    Error::Spki
                } else {
                    Error::Connect
                }
            })?;
        authenticate(Box::new(tls), lease, key, &claims).await
    })
    .await
    .unwrap_or(Err(Error::Connect))
}

// Both transports authenticate the server before handing us a pipe. No token
// is written until either the TLS SPKI or the SSH host key has been pinned.
pub(super) async fn authenticate(
    mut pipe: Box<dyn Duplex>,
    lease: &LeaseResponse,
    key: &SigningKey,
    claims: &Claims,
) -> Result<Channel, Error> {
    write(
        &mut pipe,
        &Prelude::LeaseHello {
            v: Version1,
            token: lease.job_token.clone(),
            requester_jwk: motor_lease::RequesterJwk::from_key(&key.verifying_key()),
        },
    )
    .await
    .map_err(|_| Error::Connect)?;
    let challenge = match read(&mut pipe).await.map_err(|_| Error::Connect)? {
        Prelude::Challenge(c) => c,
        Prelude::LeaseRefused { reason } => return Err(Error::from_host(&reason)),
        _ => return Err(Error::Auth),
    };
    let sig =
        motor_lease::sign_challenge(&challenge, claims, key, now_s()).map_err(|_| Error::Auth)?;
    write(&mut pipe, &Prelude::ChallengeResponse { sig })
        .await
        .map_err(|_| Error::Connect)?;
    match read(&mut pipe).await.map_err(|_| Error::Connect)? {
        // A host that refuses tenant runner initialization says so before the
        // runner's hello; keep its reason instead of reporting a lost connection.
        Prelude::LeaseOk {} => Channel::from_hosted(pipe).await.map_err(|error| {
            error
                .get_ref()
                .and_then(|source| source.downcast_ref::<crate::transport::HostedHelloRefusal>())
                .map(|refusal| Error::from_host(&refusal.0))
                .unwrap_or(Error::Connect)
        }),
        Prelude::LeaseRefused { reason } => Err(Error::from_host(&reason)),
        _ => Err(Error::Auth),
    }
}
pub(super) async fn write<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &Prelude,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(frame).map_err(io::Error::other)?;
    writer
        .write_u32(bytes.len().try_into().map_err(io::Error::other)?)
        .await?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}
pub(super) async fn read<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Prelude> {
    let len = reader.read_u32().await? as usize;
    if len > 1024 * 1024 {
        return Err(io::Error::other("oversized lease prelude"));
    }
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}
