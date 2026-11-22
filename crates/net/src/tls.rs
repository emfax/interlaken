//! Mutual TLS between replicas (ADR-0037).

use tonic::transport::{Certificate, ClientTlsConfig, Identity, ServerTlsConfig};

/// The name every replica's certificate must carry, unless configured otherwise.
pub const DEFAULT_SERVER_NAME: &str = "interlaken";

/// A replica's TLS material, all PEM-encoded.
///
/// Every replica holds a certificate and key signed by the cluster's CA,
/// naming [`TlsConfig::server_name`] among its DNS names. A replica serves
/// only peers presenting a certificate the CA signed, and talks only to
/// peers whose certificate the CA signed: unknown and expired certificates
/// are refused in both directions.
#[derive(Clone)]
pub struct TlsConfig {
    /// The cluster CA's certificate.
    pub ca: Vec<u8>,
    /// This replica's certificate.
    pub cert: Vec<u8>,
    /// This replica's private key.
    pub key: Vec<u8>,
    /// The DNS name peers' certificates are checked against.
    pub server_name: String,
}

impl std::fmt::Debug for TlsConfig {
    /// Render the configuration without the key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsConfig")
            .field("server_name", &self.server_name)
            .finish_non_exhaustive()
    }
}

impl TlsConfig {
    /// Construct the configuration from PEM files' contents, checking
    /// peers against [`DEFAULT_SERVER_NAME`].
    #[must_use]
    pub fn new(ca: Vec<u8>, cert: Vec<u8>, key: Vec<u8>) -> Self {
        Self {
            ca,
            cert,
            key,
            server_name: DEFAULT_SERVER_NAME.into(),
        }
    }

    /// Return the server side: this identity, requiring CA-signed client certificates.
    pub(crate) fn server(&self) -> ServerTlsConfig {
        ServerTlsConfig::new()
            .identity(Identity::from_pem(&self.cert, &self.key))
            .client_ca_root(Certificate::from_pem(&self.ca))
    }

    /// Return the client side: this identity, trusting only the CA.
    pub(crate) fn client(&self) -> ClientTlsConfig {
        ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(&self.ca))
            .identity(Identity::from_pem(&self.cert, &self.key))
            .domain_name(self.server_name.clone())
    }
}
