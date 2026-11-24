//! Interlaken's production network for replication (ADR-0037).
//!
//! Replicas talk gRPC over HTTP/2 with mutual TLS, through tonic: every
//! message goes through one method as postcard bytes, so no protobuf
//! schema or code generation is involved. Each node's certificate is
//! signed by the cluster's CA, and peers without one are refused.
//!
//! The server sets a deadline on every request, limits concurrent requests
//! per connection and sheds the excess. The client keeps one connection
//! per peer and counts requests sent and failed.
//!
//! [`Cluster`] runs one member: its replica, its server, and an address
//! book kept current from the cluster's membership.

mod cluster;
mod codec;
mod metrics;
mod server;
pub mod telemetry;
mod tls;
mod transport;

pub use cluster::{Cluster, ClusterConfig};
pub use metrics::{NetMetrics, PeerTraffic};
pub use tls::{DEFAULT_SERVER_NAME, TlsConfig};
pub use transport::GrpcTransport;
