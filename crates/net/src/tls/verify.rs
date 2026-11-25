//! Deciding which peers to accept, once their chain checks out (ADR-0041).

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime};

use interlaken_replication::types::NodeId;
use rustls::pki_types::CertificateDer;

/// Which way a connection goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    /// A peer connected to this node.
    Incoming,
    /// This node connected to a peer.
    Outgoing,
}

/// The cluster's membership as this node last saw it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MembershipView {
    /// Whether this node has seen a membership yet. A node waiting to join
    /// hasn't: the leader adding it is unknown to it until it does.
    pub known: bool,
    /// The voters.
    pub voters: BTreeSet<NodeId>,
    /// The learners.
    pub learners: BTreeSet<NodeId>,
}

impl MembershipView {
    /// Return `true` if `id` is a voter or learner.
    #[must_use]
    pub fn contains(&self, id: NodeId) -> bool {
        self.voters.contains(&id) || self.learners.contains(&id)
    }
}

/// A peer's certificate, after its chain has been checked against the
/// trusted CAs.
#[derive(Clone, Debug)]
pub struct PeerCertificate {
    /// The chain the peer presented, its own certificate first.
    pub chain: Vec<CertificateDer<'static>>,
    /// The DNS names the certificate carries.
    pub dns_names: Vec<String>,
    /// The URI names the certificate carries.
    pub uris: Vec<String>,
    /// When the certificate expires.
    pub not_after: SystemTime,
    /// Set when the certificate has expired, but by less than the grace
    /// period: how long ago.
    pub expired_for: Option<Duration>,
}

/// What a verifier knows about a connection besides the certificate.
#[derive(Debug)]
pub struct PeerContext<'a> {
    /// Which way the connection goes.
    pub direction: Direction,
    /// The node an outgoing connection is for; `None` for incoming ones.
    pub dialled: Option<NodeId>,
    /// The membership as this node last saw it.
    pub membership: &'a MembershipView,
    /// The time the chain was checked at.
    pub now: SystemTime,
}

/// Why a verifier refused a peer.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("peer refused ({reason}): {detail}")]
pub struct PeerRefused {
    /// A short, fixed reason, used as a metric label: `name`,
    /// `membership` or `expired`, or another of the verifier's own.
    pub reason: &'static str,
    /// What was wrong, for the log.
    pub detail: String,
}

impl PeerRefused {
    /// Construct a refusal for `reason`, explained by `detail`.
    #[must_use]
    pub fn new(reason: &'static str, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: detail.into(),
        }
    }
}

/// Decides whether to accept a peer whose certificate chains to a trusted
/// CA (ADR-0041).
///
/// It runs during the TLS handshake, in both directions, so it must not
/// block. It sees expired certificates only within the configured grace
/// period, with [`PeerCertificate::expired_for`] set; refuse them unless
/// the peer is otherwise known to be safe to admit.
pub trait PeerVerifier: Send + Sync + 'static {
    /// Accept the peer, or say why not.
    ///
    /// # Errors
    ///
    /// Returns why the peer is refused.
    fn verify(&self, peer: &PeerCertificate, context: &PeerContext<'_>) -> Result<(), PeerRefused>;
}

/// Accepts a peer whose certificate carries one DNS name, the same for
/// every member: ADR-0037's check, and the default. It refuses expired
/// certificates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DnsName(pub String);

impl PeerVerifier for DnsName {
    /// Accept a certificate naming the configured DNS name that hasn't expired.
    fn verify(
        &self,
        peer: &PeerCertificate,
        _context: &PeerContext<'_>,
    ) -> Result<(), PeerRefused> {
        if let Some(ago) = peer.expired_for {
            return Err(PeerRefused::new(
                "expired",
                format!("certificate expired {}s ago", ago.as_secs()),
            ));
        }
        if peer.dns_names.contains(&self.0) {
            Ok(())
        } else {
            Err(PeerRefused::new(
                "name",
                format!("certificate doesn't name {}", self.0),
            ))
        }
    }
}

/// Identifies members by a URI name in their certificates: the node ID
/// follows a configured prefix, such as
/// `spiffe://example.org/cluster/c1/node/` for node 7's
/// `spiffe://example.org/cluster/c1/node/7`.
///
/// It accepts a peer when exactly one of its URIs has the prefix, the rest
/// is a node ID, an outgoing connection's ID is the node dialled, and the
/// ID is a voter or learner. Before this node has seen a membership it
/// accepts any such ID, but not an expired certificate; an expired one is
/// accepted only from a member of a known membership.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberIdentity {
    /// The URI prefix the node ID follows.
    pub prefix: String,
}

impl MemberIdentity {
    /// Identify members by URIs starting with `prefix`.
    #[must_use]
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
        }
    }

    /// Return the node ID a certificate names, or why it names none.
    ///
    /// # Errors
    ///
    /// Returns a `name` refusal if no URI, or more than one, has the prefix,
    /// or the rest isn't a node ID.
    pub fn node_id(&self, peer: &PeerCertificate) -> Result<NodeId, PeerRefused> {
        let mut ids = peer
            .uris
            .iter()
            .filter_map(|uri| uri.strip_prefix(&self.prefix));
        let (Some(id), None) = (ids.next(), ids.next()) else {
            return Err(PeerRefused::new(
                "name",
                format!("certificate needs exactly one URI starting {}", self.prefix),
            ));
        };
        id.parse().map_err(|_| {
            PeerRefused::new(
                "name",
                format!("{}{id} doesn't end in a node ID", self.prefix),
            )
        })
    }
}

impl PeerVerifier for MemberIdentity {
    /// Accept a certificate naming the node dialled, or any member.
    fn verify(&self, peer: &PeerCertificate, context: &PeerContext<'_>) -> Result<(), PeerRefused> {
        let id = self.node_id(peer)?;
        if let Some(dialled) = context.dialled
            && dialled != id
        {
            return Err(PeerRefused::new(
                "name",
                format!("dialled node {dialled}, but the certificate names node {id}"),
            ));
        }
        let membership = context.membership;
        if membership.known && !membership.contains(id) {
            return Err(PeerRefused::new(
                "membership",
                format!("node {id} isn't a member"),
            ));
        }
        if let Some(ago) = peer.expired_for
            && !membership.known
        {
            return Err(PeerRefused::new(
                "expired",
                format!(
                    "node {id}'s certificate expired {}s ago, and no membership is known yet",
                    ago.as_secs()
                ),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Return a certificate with `uris`, expired for `expired` seconds if set.
    fn peer(uris: &[&str], expired: Option<u64>) -> PeerCertificate {
        PeerCertificate {
            chain: Vec::new(),
            dns_names: vec!["interlaken".into()],
            uris: uris.iter().map(|u| (*u).to_owned()).collect(),
            not_after: SystemTime::UNIX_EPOCH,
            expired_for: expired.map(Duration::from_secs),
        }
    }

    /// Return a membership of voters 1 and 2, known or not.
    fn members(known: bool) -> MembershipView {
        MembershipView {
            known,
            voters: [1, 2].into(),
            learners: BTreeSet::new(),
        }
    }

    /// Return the reason `verifier` refuses, or `None` if it accepts.
    fn reason(
        verifier: &dyn PeerVerifier,
        peer: &PeerCertificate,
        dialled: Option<NodeId>,
        membership: &MembershipView,
    ) -> Option<&'static str> {
        let context = PeerContext {
            direction: if dialled.is_some() {
                Direction::Outgoing
            } else {
                Direction::Incoming
            },
            dialled,
            membership,
            now: SystemTime::now(),
        };
        verifier.verify(peer, &context).err().map(|e| e.reason)
    }

    #[test]
    /// Assert member identities are read from URIs and checked against
    /// the node dialled and the membership.
    fn member_identity() {
        let v = MemberIdentity::new("spiffe://x/cluster/c1/node/");
        let known = members(true);
        let one = peer(&["spiffe://x/cluster/c1/node/1"], None);
        assert_eq!(reason(&v, &one, None, &known), None);
        assert_eq!(reason(&v, &one, Some(1), &known), None);
        assert_eq!(reason(&v, &one, Some(2), &known), Some("name"));
        let other_cluster = peer(&["spiffe://x/cluster/c2/node/1"], None);
        assert_eq!(reason(&v, &other_cluster, None, &known), Some("name"));
        let two_ids = peer(
            &[
                "spiffe://x/cluster/c1/node/1",
                "spiffe://x/cluster/c1/node/2",
            ],
            None,
        );
        assert_eq!(reason(&v, &two_ids, None, &known), Some("name"));
        let junk = peer(&["spiffe://x/cluster/c1/node/one"], None);
        assert_eq!(reason(&v, &junk, None, &known), Some("name"));
        let outsider = peer(&["spiffe://x/cluster/c1/node/9"], None);
        assert_eq!(reason(&v, &outsider, None, &known), Some("membership"));
        assert_eq!(reason(&v, &outsider, None, &members(false)), None);
    }

    #[test]
    /// Assert expired certificates in the grace period pass only from
    /// members of a known membership, and never with `DnsName`.
    fn grace() {
        let v = MemberIdentity::new("n/");
        let expired_member = peer(&["n/1"], Some(60));
        assert_eq!(reason(&v, &expired_member, None, &members(true)), None);
        assert_eq!(
            reason(&v, &expired_member, None, &members(false)),
            Some("expired")
        );
        let expired_outsider = peer(&["n/9"], Some(60));
        assert_eq!(
            reason(&v, &expired_outsider, None, &members(true)),
            Some("membership")
        );
        let dns = DnsName("interlaken".into());
        assert_eq!(reason(&dns, &peer(&[], None), None, &members(true)), None);
        assert_eq!(
            reason(&dns, &peer(&[], Some(1)), None, &members(true)),
            Some("expired")
        );
        assert_eq!(
            reason(
                &DnsName("other".into()),
                &peer(&[], None),
                None,
                &members(true)
            ),
            Some("name")
        );
    }
}
