//! Requests to other replicas over gRPC with mutual TLS.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use interlaken_replication::types::NodeId;
use interlaken_replication::{Request, Response, Transport, TransportError};
use tonic::codegen::http::uri::PathAndQuery;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint, Identity};

use crate::codec::BytesCodec;
use crate::metrics::Counters;
use crate::telemetry;
use crate::tls::TlsState;

/// The gRPC method every request goes through.
pub(crate) const PATH: &str = "/interlaken.Replica/Call";
/// The largest message either side accepts. Snapshots travel in chunks of
/// about 1 MiB; a chunk holds at least one whole key and value, which can
/// be larger.
pub(crate) const MAX_MESSAGE_BYTES: usize = 64 << 20;

/// Node addresses, shared between the transport and whoever keeps them current.
pub(crate) type AddressBook = Arc<RwLock<BTreeMap<NodeId, String>>>;

/// Sends requests to other replicas over gRPC.
///
/// Each peer gets one HTTP/2 connection, made on first use and remade when
/// its address changes or the TLS material is rotated. A request that fails because the connection was
/// refused surely didn't arrive; any other failure might have.
pub struct GrpcTransport {
    /// Each node's address.
    book: AddressBook,
    /// Each peer's channel, with the address it connects to and the TLS
    /// generation it was made with.
    channels: Mutex<HashMap<NodeId, (String, u64, Channel)>>,
    /// The TLS material and verifier.
    tls: Arc<TlsState>,
    /// The deadline for each request.
    timeout: Duration,
    /// Traffic counters.
    counters: Arc<Counters>,
}

impl std::fmt::Debug for GrpcTransport {
    /// Render the transport's address book.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrpcTransport")
            .field(
                "book",
                &*self.book.read().unwrap_or_else(PoisonError::into_inner),
            )
            .finish_non_exhaustive()
    }
}

/// Return `true` if `status` came from a connection the peer refused, so
/// the request surely wasn't delivered.
fn refused(status: &tonic::Status) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(status);
    while let Some(error) = source {
        if let Some(io) = error.downcast_ref::<std::io::Error>()
            && io.kind() == std::io::ErrorKind::ConnectionRefused
        {
            return true;
        }
        source = error.source();
    }
    false
}

impl GrpcTransport {
    /// Construct a transport resolving nodes through `book`.
    pub(crate) fn new(
        book: AddressBook,
        tls: Arc<TlsState>,
        timeout: Duration,
        counters: Arc<Counters>,
    ) -> Self {
        Self {
            book,
            channels: Mutex::new(HashMap::new()),
            tls,
            timeout,
            counters,
        }
    }

    /// Return the channel to `target`, making one if its address is new.
    fn channel(&self, target: NodeId) -> Result<Channel, TransportError> {
        let addr = self
            .book
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&target)
            .cloned()
            .ok_or_else(|| TransportError::NotDelivered(format!("no address for node {target}")))?;
        let generation = self.tls.generation();
        let mut channels = self.channels.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((known, made, channel)) = channels.get(&target)
            && *known == addr
            && *made == generation
        {
            return Ok(channel.clone());
        }
        let not_delivered =
            |e: tonic::transport::Error| TransportError::NotDelivered(e.to_string());
        // This member's current identity; the verifier checks the peer
        // against the current CAs and the node dialled (ADR-0041).
        let pem = self.tls.material().pem.clone();
        let client = ClientTlsConfig::new()
            .identity(Identity::from_pem(&pem.cert, &pem.key))
            .domain_name(self.tls.server_name.clone());
        let channel = Endpoint::from_shared(format!("https://{addr}"))
            .map_err(not_delivered)?
            .tls_config_with_verifier(client, self.tls.client_verifier(target))
            .map_err(not_delivered)?
            .connect_timeout(Duration::from_secs(1))
            .timeout(self.timeout)
            .connect_lazy();
        channels.insert(target, (addr, generation, channel.clone()));
        Ok(channel)
    }

    /// Send `request` to `target` and decode its response.
    async fn exchange(
        &self,
        target: NodeId,
        request: &Request,
    ) -> Result<Response, TransportError> {
        let channel = self.channel(target)?;
        let bytes = postcard::to_stdvec(request)
            .map_err(|e| TransportError::NotDelivered(e.to_string()))?;
        let peer = target.to_string();
        ::metrics::counter!(telemetry::BYTES, "peer" => peer.clone(), "direction" => "sent")
            .increment(bytes.len() as u64);
        let mut grpc = tonic::client::Grpc::new(channel)
            .max_decoding_message_size(MAX_MESSAGE_BYTES)
            .max_encoding_message_size(MAX_MESSAGE_BYTES);
        grpc.ready()
            .await
            .map_err(|e| TransportError::NotDelivered(e.to_string()))?;
        let response = grpc
            .unary(
                tonic::Request::new(bytes),
                PathAndQuery::from_static(PATH),
                BytesCodec,
            )
            .await
            .map_err(|status| {
                if refused(&status) {
                    TransportError::NotDelivered(status.to_string())
                } else {
                    TransportError::NoReply(status.to_string())
                }
            })?;
        ::metrics::counter!(telemetry::BYTES, "peer" => peer, "direction" => "received")
            .increment(response.get_ref().len() as u64);
        postcard::from_bytes(response.get_ref()).map_err(|e| TransportError::NoReply(e.to_string()))
    }
}

impl Transport for GrpcTransport {
    /// Send `request` to `target`, counting it.
    async fn send(&self, target: NodeId, request: Request) -> Result<Response, TransportError> {
        let result = self.exchange(target, &request).await;
        self.counters.sent(target, result.is_err());
        let outcome = match &result {
            Ok(_) => "ok",
            Err(TransportError::NotDelivered(_)) => "not_delivered",
            Err(TransportError::NoReply(_)) => "no_reply",
        };
        ::metrics::counter!(
            telemetry::REQUESTS,
            "peer" => target.to_string(),
            "kind" => request.kind(),
            "outcome" => outcome
        )
        .increment(1);
        result
    }
}
