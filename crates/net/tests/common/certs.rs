//! Test certificates: a cluster CA and node certificates it signs.

#![allow(dead_code)]

use interlaken_net::{DEFAULT_SERVER_NAME, TlsConfig};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, date_time_ymd};

/// A certificate authority that issues node certificates.
pub struct Pki {
    /// The CA's certificate, PEM-encoded.
    ca_pem: String,
    /// The CA, for signing.
    issuer: Issuer<'static, KeyPair>,
}

impl Pki {
    /// Construct a new CA.
    pub fn new() -> Self {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let cert = params.self_signed(&key).unwrap();
        Self {
            ca_pem: cert.pem(),
            issuer: Issuer::new(params, key),
        }
    }

    /// Return a node's TLS configuration, its certificate valid now.
    pub fn node(&self) -> TlsConfig {
        self.issue(false)
    }

    /// Return a node's TLS configuration whose certificate expired in 2021.
    pub fn expired_node(&self) -> TlsConfig {
        self.issue(true)
    }

    /// Return a configuration trusting this CA but holding a certificate
    /// from `other`: an unknown peer, as this cluster sees it.
    pub fn node_signed_by(&self, other: &Pki) -> TlsConfig {
        let mut tls = other.node();
        tls.ca = self.ca_pem.clone().into_bytes();
        tls
    }

    /// Issue a node certificate, valid now or, if `expired`, only in 2020.
    fn issue(&self, expired: bool) -> TlsConfig {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec![DEFAULT_SERVER_NAME.to_owned()]).unwrap();
        if expired {
            params.not_before = date_time_ymd(2020, 1, 1);
            params.not_after = date_time_ymd(2021, 1, 1);
        }
        let cert = params.signed_by(&key, &self.issuer).unwrap();
        TlsConfig::new(
            self.ca_pem.clone().into_bytes(),
            cert.pem().into_bytes(),
            key.serialize_pem().into_bytes(),
        )
    }
}
