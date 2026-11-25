//! The rustls side of Interlaken's TLS: verifiers and the certificate
//! resolver, all reading the current [`TlsState`] (ADR-0041).

use std::sync::Arc;

use interlaken_replication::types::NodeId;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, Error, OtherError, ServerConfig,
    SignatureScheme,
};

use super::state::TlsState;
use super::verify::{Direction, PeerRefused};

/// Return the handshake error for a refused peer.
fn refusal(refused: PeerRefused) -> Error {
    Error::InvalidCertificate(CertificateError::Other(OtherError(Arc::new(refused))))
}

/// Checks the certificates of peers connecting to this member.
#[derive(Debug)]
struct Incoming {
    /// The material and verifier.
    state: Arc<TlsState>,
    /// The CAs' subjects when the verifier was made, offered as hints.
    subjects: Vec<DistinguishedName>,
}

impl ClientCertVerifier for Incoming {
    /// Return the trusted CAs' subjects.
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &self.subjects
    }

    /// Check the peer's chain and identity.
    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        self.state
            .check(end_entity, intermediates, now, Direction::Incoming, None)
            .map(|()| ClientCertVerified::assertion())
            .map_err(refusal)
    }

    /// Verify a TLS 1.2 handshake signature.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        let algorithms = &self.state.provider.signature_verification_algorithms;
        verify_tls12_signature(message, cert, dss, algorithms)
    }

    /// Verify a TLS 1.3 handshake signature.
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        let algorithms = &self.state.provider.signature_verification_algorithms;
        verify_tls13_signature(message, cert, dss, algorithms)
    }

    /// Return the signature schemes the provider verifies.
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.state
            .provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Checks the certificate of the peer this member connects to.
#[derive(Debug)]
struct Outgoing {
    /// The material and verifier.
    state: Arc<TlsState>,
    /// The node dialled.
    target: NodeId,
}

impl ServerCertVerifier for Outgoing {
    /// Check the peer's chain and identity; the verifier checks names.
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        self.state
            .check(
                end_entity,
                intermediates,
                now,
                Direction::Outgoing,
                Some(self.target),
            )
            .map(|()| ServerCertVerified::assertion())
            .map_err(refusal)
    }

    /// Verify a TLS 1.2 handshake signature.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        let algorithms = &self.state.provider.signature_verification_algorithms;
        verify_tls12_signature(message, cert, dss, algorithms)
    }

    /// Verify a TLS 1.3 handshake signature.
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        let algorithms = &self.state.provider.signature_verification_algorithms;
        verify_tls13_signature(message, cert, dss, algorithms)
    }

    /// Return the signature schemes the provider verifies.
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.state
            .provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Presents this member's current certificate to connecting peers.
#[derive(Debug)]
struct Resolver(Arc<TlsState>);

impl ResolvesServerCert for Resolver {
    /// Return the current certificate and key.
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(Arc::clone(&self.0.material().certified))
    }
}

impl TlsState {
    /// Return the server's configuration: client certificates required and
    /// checked against the current material, the current certificate
    /// presented, HTTP/2 offered.
    ///
    /// # Errors
    ///
    /// Returns rustls's error if the provider supports no protocol version.
    pub(crate) fn server_config(self: &Arc<Self>) -> Result<Arc<ServerConfig>, Error> {
        let verifier = Arc::new(Incoming {
            state: Arc::clone(self),
            subjects: self.material().subjects.clone(),
        });
        let mut config = ServerConfig::builder_with_provider(Arc::clone(&self.provider))
            .with_safe_default_protocol_versions()?
            .with_client_cert_verifier(verifier)
            .with_cert_resolver(Arc::new(Resolver(Arc::clone(self))));
        config.alpn_protocols = vec![b"h2".to_vec()];
        Ok(Arc::new(config))
    }

    /// Return the verifier for connections to node `target`.
    pub(crate) fn client_verifier(self: &Arc<Self>, target: NodeId) -> Arc<dyn ServerCertVerifier> {
        Arc::new(Outgoing {
            state: Arc::clone(self),
            target,
        })
    }
}
