//! The TLS material in use and the checks every handshake runs (ADR-0041).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, SystemTime};

use interlaken_replication::types::NodeId;
use rustls::DistinguishedName;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, TrustAnchor, UnixTime};
use rustls::sign::CertifiedKey;
use webpki::{EndEntityCert, KeyUsage};

use super::config::{TlsConfig, TlsMaterial};
use super::der::validity;
use super::verify::{
    Direction, MembershipView, PeerCertificate, PeerContext, PeerRefused, PeerVerifier,
};
use crate::telemetry;

/// One trusted CA.
#[derive(Debug)]
struct Ca {
    /// When it is valid.
    validity: Option<(SystemTime, SystemTime)>,
}

/// Parsed, checked material.
pub(crate) struct Material {
    /// This member's chain and key, for both directions.
    pub(crate) certified: Arc<CertifiedKey>,
    /// The trust anchors made from the CAs.
    anchors: Vec<TrustAnchor<'static>>,
    /// The CAs, in the order of `anchors`.
    cas: Vec<Ca>,
    /// The CAs' subjects, which the server offers as hints.
    pub(crate) subjects: Vec<DistinguishedName>,
    /// The PEM material, for tonic's client identity.
    pub(crate) pem: TlsMaterial,
    /// When this member's certificate expires.
    pub(crate) not_after: Option<SystemTime>,
}

/// Return the error for unusable material.
fn invalid(what: &str, detail: &dyn std::fmt::Display) -> String {
    format!("{what}: {detail}")
}

/// Return `time` as webpki's time.
fn unix(time: SystemTime) -> UnixTime {
    UnixTime::since_unix_epoch(
        time.duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default(),
    )
}

/// Return webpki's time as a system time.
fn system(time: UnixTime) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(time.as_secs())
}

impl Material {
    /// Parse and check `pem`: the key must match the certificate, and, if
    /// `chained`, the certificate must chain to one of the CAs (as of its
    /// own validity, so an expired one can still be loaded). Opening a
    /// cluster doesn't require the chain, so a misconfigured member starts
    /// and is refused by its peers; a rotation does.
    fn load(pem: &TlsMaterial, provider: &CryptoProvider, chained: bool) -> Result<Self, String> {
        let ca_ders: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&pem.ca)
            .collect::<Result<_, _>>()
            .map_err(|e| invalid("CA certificates", &e))?;
        if ca_ders.is_empty() {
            return Err("no CA certificate".into());
        }
        let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&pem.cert)
            .collect::<Result<_, _>>()
            .map_err(|e| invalid("certificate", &e))?;
        let Some(leaf) = chain.first().cloned() else {
            return Err("no certificate".into());
        };
        let key = PrivateKeyDer::from_pem_slice(&pem.key).map_err(|e| invalid("key", &e))?;
        let certified = CertifiedKey::from_der(chain.clone(), key, provider)
            .map_err(|e| invalid("certificate and key", &e))?;
        let mut anchors = Vec::new();
        let mut cas = Vec::new();
        for der in &ca_ders {
            let anchor = webpki::anchor_from_trusted_cert(der)
                .map_err(|e| invalid("CA certificate", &e))?
                .to_owned();
            cas.push(Ca {
                validity: validity(der),
            });
            anchors.push(anchor);
        }
        let subjects = anchors
            .iter()
            .map(|a| DistinguishedName::from(a.subject.to_vec()))
            .collect();
        let not_after = validity(&leaf).map(|(_, not_after)| not_after);
        let material = Self {
            certified: Arc::new(certified),
            anchors,
            cas,
            subjects,
            pem: pem.clone(),
            not_after,
        };
        if !chained {
            return Ok(material);
        }
        let at = not_after.map_or_else(SystemTime::now, |end| {
            SystemTime::now().min(end - Duration::from_secs(1))
        });
        let cert = EndEntityCert::try_from(&leaf).map_err(|e| invalid("certificate", &e))?;
        cert.verify_for_usage(
            webpki::ALL_VERIFICATION_ALGS,
            &material.anchors,
            &chain[1..],
            unix(at),
            KeyUsage::server_auth(),
            None,
            None,
        )
        .map_err(|e| invalid("certificate doesn't chain to a CA", &e))?;
        Ok(material)
    }

    /// Check that `anchor`'s CA and the chain's `intermediates` are valid
    /// at `now`: webpki doesn't check trust anchors' dates, and checks the
    /// intermediates at the time a chain is verified at, which for an
    /// expired certificate in the grace period is before it expired.
    ///
    /// # Errors
    ///
    /// Returns which isn't valid.
    fn issuers_valid(
        &self,
        anchor: &TrustAnchor<'_>,
        intermediates: &[&[u8]],
        now: SystemTime,
    ) -> Result<(), String> {
        let valid = |window: Option<(SystemTime, SystemTime)>| {
            window.is_some_and(|(from, to)| from <= now && now <= to)
        };
        let index = self.anchors.iter().position(|a| {
            a.subject == anchor.subject
                && a.subject_public_key_info == anchor.subject_public_key_info
        });
        if !index.is_some_and(|i| valid(self.cas[i].validity)) {
            return Err("the issuing CA isn't valid now".into());
        }
        if !intermediates.iter().all(|der| valid(validity(der))) {
            return Err("an intermediate certificate isn't valid now".into());
        }
        Ok(())
    }
}

/// The TLS material in use, the verifier and the membership, shared by the
/// server, the transport and the cluster (ADR-0041).
pub(crate) struct TlsState {
    /// The material, replaced whole by a rotation.
    material: RwLock<Arc<Material>>,
    /// How many rotations there have been, so connections made before the
    /// last one are remade.
    generation: AtomicU64,
    /// Decides which peers to accept.
    verifier: Arc<dyn PeerVerifier>,
    /// How long after expiry a certificate is offered to the verifier.
    grace: Duration,
    /// The membership as this node last saw it.
    membership: RwLock<MembershipView>,
    /// The name this member asks for when it connects.
    pub(crate) server_name: String,
    /// The crypto provider.
    pub(crate) provider: Arc<CryptoProvider>,
    /// This member's ID, as the expiry gauge's label.
    node: String,
}

impl std::fmt::Debug for TlsState {
    /// Render the state's server name.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsState")
            .field("server_name", &self.server_name)
            .finish_non_exhaustive()
    }
}

impl TlsState {
    /// Load `config`'s material.
    ///
    /// # Errors
    ///
    /// Returns what is wrong with the material.
    pub(crate) fn new(config: &TlsConfig, node: NodeId) -> Result<Self, String> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let material = Material::load(&config.material(), &provider, false)?;
        let state = Self {
            material: RwLock::new(Arc::new(material)),
            generation: AtomicU64::new(0),
            verifier: config.verifier(),
            grace: config.expired_grace,
            membership: RwLock::new(MembershipView::default()),
            server_name: config.server_name.clone(),
            provider,
            node: node.to_string(),
        };
        state.record_expiry();
        Ok(state)
    }

    /// Return the material in use.
    pub(crate) fn material(&self) -> Arc<Material> {
        Arc::clone(&self.material.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Replace the material with `pem`, after checking it.
    ///
    /// # Errors
    ///
    /// Returns what is wrong with `pem`; the material in use stays.
    pub(crate) fn rotate(&self, pem: &TlsMaterial) -> Result<(), String> {
        let material = Material::load(pem, &self.provider, true)?;
        *self
            .material
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Arc::new(material);
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.record_expiry();
        Ok(())
    }

    /// Return how many rotations there have been.
    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Record the seconds until this member's certificate expires.
    pub(crate) fn record_expiry(&self) {
        if let Some(not_after) = self.material().not_after {
            let seconds = match not_after.duration_since(SystemTime::now()) {
                Ok(left) => left.as_secs_f64(),
                Err(past) => -past.duration().as_secs_f64(),
            };
            ::metrics::gauge!(telemetry::CERTIFICATE_EXPIRY, "node" => self.node.clone())
                .set(seconds);
        }
    }

    /// Replace the membership the verifier sees.
    pub(crate) fn set_membership(&self, view: MembershipView) {
        *self
            .membership
            .write()
            .unwrap_or_else(PoisonError::into_inner) = view;
    }

    /// Check a peer's certificate chain at `now`: against the trusted CAs,
    /// with the grace period for an expired one, then with the verifier.
    ///
    /// # Errors
    ///
    /// Returns why the peer is refused, after counting it.
    pub(crate) fn check(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
        direction: Direction,
        dialled: Option<NodeId>,
    ) -> Result<(), PeerRefused> {
        let result = self.check_uncounted(end_entity, intermediates, now, direction, dialled);
        if let Err(refused) = &result {
            ::metrics::counter!(telemetry::PEER_REFUSED, "reason" => refused.reason).increment(1);
        }
        result
    }

    /// Check a peer as [`check`](Self::check) does, without counting.
    fn check_uncounted(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
        direction: Direction,
        dialled: Option<NodeId>,
    ) -> Result<(), PeerRefused> {
        let material = self.material();
        let chain_error = |e: &dyn std::fmt::Display| PeerRefused::new("chain", e.to_string());
        let cert = EndEntityCert::try_from(end_entity).map_err(|e| chain_error(&e))?;
        let usage = match direction {
            Direction::Incoming => KeyUsage::client_auth(),
            Direction::Outgoing => KeyUsage::server_auth(),
        };
        let verify = |at: UnixTime| {
            cert.verify_for_usage(
                webpki::ALL_VERIFICATION_ALGS,
                &material.anchors,
                intermediates,
                at,
                usage,
                None,
                None,
            )
        };
        let now_time = system(now);
        let (path, expired_for) = match verify(now) {
            Ok(path) => (path, None),
            Err(webpki::Error::CertExpired { not_after, .. }) => {
                let ago = now.as_secs().saturating_sub(not_after.as_secs());
                if ago > self.grace.as_secs() || self.grace.is_zero() {
                    return Err(PeerRefused::new(
                        "expired",
                        format!("certificate expired {ago}s ago"),
                    ));
                }
                let before = UnixTime::since_unix_epoch(Duration::from_secs(
                    not_after.as_secs().saturating_sub(1),
                ));
                let path = verify(before).map_err(|e| chain_error(&e))?;
                (path, Some(Duration::from_secs(ago)))
            }
            Err(e) => return Err(chain_error(&e)),
        };
        let intermediate_ders: Vec<Vec<u8>> = path
            .intermediate_certificates()
            .map(|c| c.der().as_ref().to_vec())
            .collect();
        let intermediate_refs: Vec<&[u8]> = intermediate_ders.iter().map(Vec::as_slice).collect();
        material
            .issuers_valid(path.anchor(), &intermediate_refs, now_time)
            .map_err(|e| PeerRefused::new("chain", e))?;
        let peer = PeerCertificate {
            chain: std::iter::once(end_entity.clone().into_owned())
                .chain(intermediates.iter().map(|c| c.clone().into_owned()))
                .collect(),
            dns_names: cert.valid_dns_names().map(str::to_owned).collect(),
            uris: cert.valid_uri_names().map(str::to_owned).collect(),
            not_after: validity(end_entity).map_or(now_time, |(_, end)| end),
            expired_for,
        };
        let membership = self
            .membership
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let context = PeerContext {
            direction,
            dialled,
            membership: &membership,
            now: now_time,
        };
        self.verifier.verify(&peer, &context)
    }
}
