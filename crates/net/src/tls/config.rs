//! A member's TLS configuration (ADR-0037, ADR-0041).

use std::sync::Arc;
use std::time::Duration;

use super::verify::{DnsName, PeerVerifier};

/// The name every replica's certificate must carry, unless configured otherwise.
pub const DEFAULT_SERVER_NAME: &str = "interlaken";

/// A member's TLS material, all PEM-encoded: what [`TlsConfig`] starts with
/// and [`Cluster::rotate_tls`](crate::Cluster::rotate_tls) replaces.
#[derive(Clone)]
pub struct TlsMaterial {
    /// The trusted CAs' certificates. Several may be given, one after
    /// another, to trust an old and a new CA while certificates move from
    /// one to the other.
    pub ca: Vec<u8>,
    /// This member's certificate, optionally followed by intermediates.
    pub cert: Vec<u8>,
    /// This member's private key.
    pub key: Vec<u8>,
}

impl std::fmt::Debug for TlsMaterial {
    /// Render the material without the key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsMaterial").finish_non_exhaustive()
    }
}

/// How a member authenticates its peers with mutual TLS.
///
/// Every member holds a certificate and key signed by a trusted CA. A
/// member accepts a peer only if the peer's certificate chains to a trusted
/// CA, both when the peer connects and when this member connects to it,
/// and the `verifier` field's verifier accepts it: by default, if it
/// names [`TlsConfig::server_name`] among its DNS names and hasn't expired.
#[derive(Clone)]
pub struct TlsConfig {
    /// The trusted CAs' certificates; several may follow one another.
    pub ca: Vec<u8>,
    /// This member's certificate.
    pub cert: Vec<u8>,
    /// This member's private key.
    pub key: Vec<u8>,
    /// The DNS name this member asks for when it connects, and the name the
    /// default verifier checks peers' certificates for.
    pub server_name: String,
    /// Decides which peers with a valid chain to accept. `None` is
    /// [`DnsName`] of [`server_name`](Self::server_name).
    pub verifier: Option<Arc<dyn PeerVerifier>>,
    /// How long after expiry a peer's certificate is still offered to the
    /// verifier, provided its CAs are valid now. Zero, the default, refuses
    /// expired certificates outright.
    pub expired_grace: Duration,
}

impl std::fmt::Debug for TlsConfig {
    /// Render the configuration without the key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsConfig")
            .field("server_name", &self.server_name)
            .field("expired_grace", &self.expired_grace)
            .finish_non_exhaustive()
    }
}

impl TlsConfig {
    /// Construct the configuration from PEM files' contents, checking
    /// peers against [`DEFAULT_SERVER_NAME`] with no grace period.
    #[must_use]
    pub fn new(ca: Vec<u8>, cert: Vec<u8>, key: Vec<u8>) -> Self {
        Self {
            ca,
            cert,
            key,
            server_name: DEFAULT_SERVER_NAME.into(),
            verifier: None,
            expired_grace: Duration::ZERO,
        }
    }

    /// Return the configuration with `verifier` deciding which peers to accept.
    #[must_use]
    pub fn with_verifier(mut self, verifier: Arc<dyn PeerVerifier>) -> Self {
        self.verifier = Some(verifier);
        self
    }

    /// Return the configuration offering certificates expired by up to
    /// `grace` to the verifier.
    #[must_use]
    pub fn with_expired_grace(mut self, grace: Duration) -> Self {
        self.expired_grace = grace;
        self
    }

    /// Return the material: CAs, certificate and key.
    pub(crate) fn material(&self) -> TlsMaterial {
        TlsMaterial {
            ca: self.ca.clone(),
            cert: self.cert.clone(),
            key: self.key.clone(),
        }
    }

    /// Return the verifier, or the default.
    pub(crate) fn verifier(&self) -> Arc<dyn PeerVerifier> {
        self.verifier
            .clone()
            .unwrap_or_else(|| Arc::new(DnsName(self.server_name.clone())))
    }
}
