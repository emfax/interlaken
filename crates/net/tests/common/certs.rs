//! Test certificates: a cluster CA and node certificates it signs.

#![allow(dead_code)]

use interlaken_net::{DEFAULT_SERVER_NAME, TlsConfig, TlsMaterial};
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, SanType, date_time_ymd};

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

    /// Return this CA's certificate, PEM-encoded.
    pub fn ca(&self) -> String {
        self.ca_pem.clone()
    }

    /// Return material holding a certificate for URI `uri` (and the
    /// default DNS name), trusting `trusted` CAs' certificates. With
    /// `expired_days` set, the certificate expired that many days ago.
    pub fn member(&self, uri: &str, expired_days: Option<i64>, trusted: &str) -> TlsMaterial {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec![DEFAULT_SERVER_NAME.to_owned()]).unwrap();
        params
            .subject_alt_names
            .push(SanType::URI(uri.try_into().unwrap()));
        if let Some(days) = expired_days {
            let now = time::OffsetDateTime::now_utc();
            params.not_before = now - time::Duration::days(days + 30);
            params.not_after = now - time::Duration::days(days);
        }
        let cert = params.signed_by(&key, &self.issuer).unwrap();
        TlsMaterial {
            ca: trusted.as_bytes().to_vec(),
            cert: cert.pem().into_bytes(),
            key: key.serialize_pem().into_bytes(),
        }
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
