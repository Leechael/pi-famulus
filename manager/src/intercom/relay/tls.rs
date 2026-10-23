use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
    DigitallySignedStruct, SignatureScheme,
};
use std::sync::{Arc, OnceLock};

/// The platform trust store, loaded once per process: on macOS the Keychain
/// walk costs ~200ms and would otherwise block a runtime worker per dial.
pub(super) fn native_roots() -> Arc<rustls::RootCertStore> {
    static ROOTS: OnceLock<Arc<rustls::RootCertStore>> = OnceLock::new();
    ROOTS
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            for certificate in rustls_native_certs::load_native_certs().certs {
                let _ = roots.add(certificate);
            }
            Arc::new(roots)
        })
        .clone()
}

pub(super) fn connector(insecure: bool) -> Result<tokio_tungstenite::Connector, String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let roots = native_roots();
    let mut config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    if insecure {
        config
            .dangerous()
            .set_certificate_verifier(Arc::new(InsecureCertificateVerifier));
    }
    Ok(tokio_tungstenite::Connector::Rustls(Arc::new(config)))
}
#[derive(Debug)]
struct InsecureCertificateVerifier;
impl ServerCertVerifier for InsecureCertificateVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            certificate,
            signature,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            certificate,
            signature,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
