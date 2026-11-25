---
id: 0041
title: Peer Identity and Certificate Rotation
status: implemented  # proposal | accepted | implemented | superseded
tags: [network, replication, security]
feature-flag:
created: 2026-11-25
accepted: 2026-11-25
implemented: 2026-11-25
spec-sections: []
superseded-by:
---

# ADR-0041: Peer Identity and Certificate Rotation

## Status

Implemented, in all three phases at once. The *As implemented* notes after the phases record where the result differs from this design, chiefly: `TlsConfig::ca` stays one field holding one or more CAs; a member's own chain is checked on rotation, not at startup; and every chain's CA must be valid now, not only in the grace period.

## Summary

Cluster members authenticate each other with mutual TLS (ADR-0037), but only in the simplest way:

- A peer is accepted if its certificate chains to the cluster CA and carries one shared DNS name, `interlaken`. Any certificate the CA ever issued admits its holder as a member, including one issued to a node removed from the cluster.
- An expired certificate is refused immediately, so a cluster whose certificate issuer is unreachable stops replicating when the first certificate expires.
- Certificates and the CA are read once, at `Cluster::open`. Short-lived certificates can't be renewed without restarting the node.

This ADR decides:

1. **Interlaken builds its own rustls configurations** for both directions, instead of tonic's, so it can verify peers and swap material itself.
2. **A `PeerVerifier` hook** runs after the chain check, with the peer's certificate, the direction, the node dialled (outgoing) and the current Raft membership. The default keeps today's behaviour; a provided `MemberIdentity` verifier maps a URI name in the certificate to a node ID that must be a member.
3. **Expired certificates within a grace period** are offered to the verifier instead of refused, when the CA chain is valid now. The verifier decides; `MemberIdentity` accepts a current member.
4. **Rotation while running:** `Cluster::rotate_tls` replaces this node's certificate and key and the trusted CAs. New connections use them at once, and connections older than a maximum age are remade, so they are re-verified.

## Context

### How TLS works today

- `TlsConfig` holds PEM bytes: the CA certificate, this node's certificate and key, and the expected server name.
- `TlsConfig::server()` and `client()` build tonic's `ServerTlsConfig` and `ClientTlsConfig`, consumed when `Cluster::open` starts the server and the transport. `Cluster` keeps neither.
- **Server side:** tonic builds a rustls `ServerConfig` with `WebPkiClientVerifier`, which checks the client's chain and validity but no name. tonic 0.14 has no way to supply another client verifier or a prebuilt `ServerConfig`. It does accept any stream of TLS connections through `Server::serve_with_incoming`, and exposes the peer's certificates to requests from a `tokio_rustls` stream.
- **Client side:** tonic uses rustls' default server verifier with the CA as the only root and `interlaken` as the name. `Endpoint::tls_config_with_verifier` accepts a custom `ServerCertVerifier`.
- **Connections:** the transport keeps one channel per peer, forever, remade only when the peer's address changes. HTTP/2 connections are long-lived, and TLS verifies a peer only at the handshake.
- **The crypto provider is ring**, through tonic's `tls-ring` feature. No Interlaken crate depends on rustls directly.

### What the network layer knows about peers

- A node ID is a `u64`; a node's address is a `host:port` string in openraft's membership.
- The transport is built before the replica node and passed into it, so it can't hold the node. The cluster copies member addresses from the node's membership into the transport's address book every 200 ms.
- When dialling, the client knows which node it means. The server doesn't: requests carry no sender ID, and the handshake happens before any request.
- The cluster has no name or ID of its own.

### What operators need

- **Certificates that identify members.** An organisation running many clusters from one CA needs a certificate for one cluster to be refused by another, and a removed node's certificate refused by its old cluster.
- **Short-lived certificates.** Certificates that last a day and are renewed twice a day limit what a stolen key is worth, but need renewal without restarts.
- **Surviving an issuer outage.** If the service that issues certificates is down for days, members should keep replicating with each other, but not admit anyone new.
- **CA rotation:** trusting an old and a new CA together while certificates move from one to the other.

## Decision

### 1. Interlaken's own rustls configuration

`interlaken-net` depends on `rustls` 0.23 and `tokio-rustls` 0.26 directly (both already in the build, with ring).

- **Server:** a rustls `ServerConfig` with Interlaken's `ClientCertVerifier` and a certificate resolver, behind a `tokio_rustls::TlsAcceptor`. The listener's accepted TCP streams go through the acceptor and into `Server::serve_with_incoming`. ALPN offers `h2`, as tonic's configuration does.
- **Client:** `Endpoint::tls_config_with_verifier` with Interlaken's `ServerCertVerifier`, and a client certificate resolver.
- **One shared `TlsState`** behind both: the current certificate chain and key, the trusted CA roots, and the verifier. A rotation replaces it with one swap; every handshake reads the current value.

### 2. The verification hook

```rust
pub trait PeerVerifier: Send + Sync + 'static {
    /// Decide whether to accept a peer whose chain has already been checked.
    fn verify(&self, peer: &PeerCertificate, context: &PeerContext<'_>) -> Result<(), PeerRefused>;
}

pub struct PeerCertificate {
    pub chain: Vec<CertificateDer<'static>>,
    pub dns_names: Vec<String>,
    pub uris: Vec<String>,
    pub not_after: SystemTime,
    /// Set when the certificate has expired but is within the grace period.
    pub expired_for: Option<Duration>,
}

pub struct PeerContext<'a> {
    pub direction: Direction,            // Incoming or Outgoing
    pub dialled: Option<NodeId>,         // the node an outgoing connection is for
    pub membership: &'a MembershipView,  // voters, learners, and whether it is known yet
    pub now: SystemTime,
}
```

- **Built-in checks first.** The chain must reach a trusted CA, with validity checked as in section 3. Only then is the verifier called.
- **Membership** comes from a `MembershipView` the cluster updates from the node's membership, as it updates the address book. Before the node has joined, the view is marked unknown: a joining node is contacted by a leader it has no record of yet. Verifiers must decide what to accept then.
- **`DnsName`** (the default) accepts a peer carrying the configured DNS name. That is today's behaviour.
- **`MemberIdentity`** reads a node ID from a URI name with a configured prefix. For example, with the prefix `spiffe://example.org/cluster/c1/node/`, a certificate naming `spiffe://example.org/cluster/c1/node/7` is node 7. It accepts the peer if:
  - exactly one URI has the prefix and its remainder is a node ID;
  - outgoing, that ID is the node dialled;
  - the ID is a voter or learner in the current membership, or the membership is unknown and the certificate hasn't expired.

  The prefix scopes certificates to one cluster, and membership scopes them to its current members.
- **Programs can implement `PeerVerifier`** for anything else. The verifier is called on the handshake path, so it must not block.
- **Refusals** are counted in `interlaken_net_peer_refused_total{reason}` (ADR-0039), with reasons `chain`, `expired`, `name` and `membership`. The verifier's message is logged, but not the certificate.

Identity is checked when a connection opens; requests on it still carry no sender ID. A member that holds a valid certificate can still claim to be another member inside Raft messages. Every member holds all the data, so this ADR doesn't change ADR-0037's position that members are trusted equally; it narrows who counts as a member.

### 3. The grace period for expired certificates

`TlsConfig::expired_grace: Duration` defaults to zero, which keeps today's behaviour.

When a peer's certificate expired less than the grace period ago:

1. The chain is verified as of one second before the leaf's `not_after`, so everything else about it (signature, name constraints, key usage) is checked as of when it was valid.
2. Every CA certificate in the chain must also be valid *now*: an expired CA is never excused.
3. The verifier is called with `expired_for` set. `DnsName` refuses; `MemberIdentity` accepts only if the peer is a member of a known membership.

So a cluster cut off from its issuer keeps replicating among its members for the grace period, while a new or removed node with an expired certificate is refused. This node presents its own certificate whether or not it has expired; its peers decide.

The certificate expiry of this node is exported as a gauge, `interlaken_net_certificate_expiry_seconds`, so monitoring can alert before the grace period starts.

### 4. Rotation while running

```rust
cluster.rotate_tls(TlsMaterial { cas: vec![old_ca, new_ca], cert, key })?;
```

- **Checked before swapping:** the key matches the certificate, and the certificate chains to one of the CAs. A bad set is refused and the current material stays.
- **Several CAs** can be trusted at once (`TlsConfig::cas` replaces `ca`), so a CA can be rotated in steps: trust both, move every node's certificate to the new CA, then drop the old one.
- **New handshakes** use the new material at once.
- **Existing connections** keep theirs until they're remade. The server closes connections older than `ClusterConfig::max_connection_age` (default 1 hour, with tonic's `max_connection_age`), and the transport drops its cached channels on rotation. Every connection is re-verified at least that often, including against membership, so a removed node's connection doesn't outlive the setting.
- A program embedding the engine rotates through `Engine::cluster()`, which already exposes the cluster.

### Compatibility

`TlsConfig::new(ca, cert, key)` keeps working, with the `DnsName` verifier, no grace period, and one CA. Existing clusters behave as before.

## Alternatives considered

- **Keep tonic's TLS and check peers per request** through `Request::peer_certs()`. That covers the server side without a verifier, but a refused peer would still complete handshakes and hold connections, and the client side would still need the verifier hook.
- **A cluster name in a DNS name** (`c1.interlaken`) instead of a URI. It works with today's single-name check, but node IDs in DNS names are awkward. URI names are the common way to carry workload identities.
- **Sender IDs in every request, checked against the certificate.** It would bind Raft messages to identities. It changes the wire format and every handler, and members are trusted with all data anyway; left as future work.
- **Never accept expired certificates;** require issuance to be highly available. Simpler, but it makes the issuer a dependency of replication itself.

## Testing

- **Unit tests for verification:** URI parsing, prefix mismatch, several matching URIs, node ID mismatch when dialling, membership known and unknown.
- **Grace:** a certificate expired within the grace is accepted from a member and refused from a non-member, an expired CA is refused, and a certificate expired beyond the grace is refused.
- **Cluster tests** (rcgen certificates with URI names):
  - a node with a certificate for another cluster prefix is refused, both ways;
  - a removed member is refused after `max_connection_age`;
  - certificates rotated on every node in turn, then the old CA dropped, while writes continue;
  - a bad rotation (mismatched key, unknown CA) is refused and changes nothing.
- **Processes under faults** (ADR-0037 phase 5): rotation in the middle of network faults.
- **Existing tests** for unknown and expired certificates pass unchanged with the defaults.

## Implementation Phases

### Phase 1: own TLS configuration

- rustls server and client configurations, the acceptor in front of the server, and `TlsState`.
- Several CAs; `max_connection_age`.
- The existing behaviour as the `DnsName` verifier, with today's tests passing.

### Phase 2: verifiers and grace

- `PeerVerifier`, `PeerCertificate`, `PeerContext` and `MembershipView`.
- `MemberIdentity`, the grace period, and refusal and expiry metrics.

### Phase 3: rotation

- `Cluster::rotate_tls`.
- The book's cluster chapter: identities, rotation, the grace period and what to monitor.

### As implemented

- **The acceptor:** the server accepts TCP connections itself and completes each TLS handshake in a task of its own, with a 10-second limit, passing established connections to tonic's `serve_with_incoming_shutdown`. A refused or stalled handshake just closes the connection.
- **The client** builds each peer's channel with `Endpoint::tls_config_with_verifier`, from the current certificate and key and a verifier that knows the node dialled. Instead of dropping channels on rotation, the TLS state counts rotations, and a channel made before the last one is remade on its next use.
- **One `ca` field:** `TlsConfig::ca` and `TlsMaterial::ca` hold one or more PEM certificates, one after another, rather than a separate `cas` list; existing configurations hold one. `with_verifier` and `with_expired_grace` set the new fields.
- **Own chain on rotation only:** `rotate_tls` requires the key to match the certificate and the certificate to chain to one of the CAs, as of the certificate's own validity, so an expired certificate can be loaded during an outage. Opening a cluster checks only the key, so a misconfigured member still starts and is refused by its peers, as the existing tests expect.
- **CAs valid now, always:** webpki doesn't check trust anchors' dates, and in the grace period checks the chain as of one second before the certificate expired. Every chain's CA, and its intermediates, are therefore checked against the current time separately, with a small reader for certificates' validity dates. A CA past its own expiry is refused in every case, which tonic's configuration didn't do.
- **Unknown membership:** `MemberIdentity` accepts any ID with its prefix before a membership is known, as designed, but never an expired certificate then.
- **Metrics:** refusals are counted with the reason `chain` as well (no chain to a trusted CA, or a CA not valid now). The expiry gauge carries a `node` label, like the Raft gauges, since several members can share a process.
- **Root hints:** the CA names the server offers clients as hints are those at startup. A client presents its one certificate regardless, so a CA rotation doesn't depend on them.
- **Tests:** unit tests for both verifiers and the date reader; cluster tests for identities across clusters, a removed member refused once its connections age out, a member expired within the grace period kept and beyond it refused, a non-member with an expired certificate refused, rotation to a new CA in two steps while writing, and bad rotations refused. Each asserts the refusal counter grew for the expected reason. Rotation during network faults (the process tests) isn't covered yet.

## Consequences

### Positive

- A CA can serve many clusters, and a certificate admits its holder to one cluster as one member.
- Removed members are refused within `max_connection_age`.
- Certificates can be short-lived and renewed without restarts.
- A cluster survives an outage of its issuer for a bounded, configured time.

### Negative

- Interlaken owns TLS configuration that tonic did for it: ALPN, the provider, and the acceptor. tonic and rustls upgrades need more care.
- Connections are remade at least every `max_connection_age`, costing a handshake per peer each time.
- A verifier runs on every handshake, so a slow one delays connections.
- Expired certificates are accepted for the grace period, which a stolen key from a current member can use.

## Open Questions

- Should the grace period also require the peer to have been connected before (a known peer), not just a member?
- Should `max_connection_age` default lower when the grace period is set?

## Future Work

- Sender IDs in requests, checked against the connection's identity.
- Certificate revocation lists, if short lifetimes aren't enough.
- TLS for the OTLP exporter (ADR-0039), which shares the question of whose rustls configuration to use.

## References

- [ADR-0037](0037-replication-with-raft.md): the cluster, its transport, and mutual TLS.
- [ADR-0039](0039-telemetry-and-cost-attribution.md): network metrics.
- rustls `ClientCertVerifier` and `ServerCertVerifier`; tonic `Server::serve_with_incoming` and `Endpoint::tls_config_with_verifier`.
