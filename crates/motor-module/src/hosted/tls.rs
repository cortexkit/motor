use super::{now_s, Error};
use crate::transport::Channel;
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
    if lease.job_id != id.to_string()
        || claims.jti != lease.job_id
        || claims.placement != lease.placement
        || claims.purpose != purpose
        || claims.max_s != lease.max_s
        || claims.iss != motor_lease::ISSUER
        || claims.aud != lease.placement.host
        || claims.iat > now_s().saturating_add(60)
        || claims.exp <= now_s()
        || motor_lease::bind_requester(&claims, &jwk).is_err()
    {
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
        let mut tls = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect(name, tcp)
            .await
            .map_err(|e| {
                if e.to_string().contains("hosted_spki_mismatch") {
                    Error::Spki
                } else {
                    Error::Connect
                }
            })?;
        // This is the first write of the token, strictly after certificate pinning
        // and TLS proof of possession of the certificate's private key.
        write(
            &mut tls,
            &Prelude::LeaseHello {
                v: Version1,
                token: lease.job_token.clone(),
                requester_jwk: motor_lease::RequesterJwk::from_key(&key.verifying_key()),
            },
        )
        .await
        .map_err(|_| Error::Connect)?;
        let challenge = match read(&mut tls).await.map_err(|_| Error::Connect)? {
            Prelude::Challenge(c) => c,
            Prelude::LeaseRefused { reason } => return Err(Error::from_host(&reason)),
            _ => return Err(Error::Auth),
        };
        let sig = motor_lease::sign_challenge(&challenge, &claims, key, now_s())
            .map_err(|_| Error::Auth)?;
        write(&mut tls, &Prelude::ChallengeResponse { sig })
            .await
            .map_err(|_| Error::Connect)?;
        match read(&mut tls).await.map_err(|_| Error::Connect)? {
            Prelude::LeaseOk {} => Channel::from_hosted(Box::new(tls))
                .await
                .map_err(|_| Error::Connect),
            Prelude::LeaseRefused { reason } => Err(Error::from_host(&reason)),
            _ => Err(Error::Auth),
        }
    })
    .await
    .unwrap_or(Err(Error::Connect))
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
