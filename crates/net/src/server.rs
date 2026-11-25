//! Answering other replicas' requests over gRPC with mutual TLS.

use std::sync::Arc;
use std::time::Duration;

use interlaken_replication::{Handler, Request};
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use tonic::codegen::{BoxFuture, Context, Poll, Service, StdError, http};
use tonic::server::{NamedService, UnaryService};
use tonic::transport::Server;
use tonic::{Status, body::Body};

use crate::codec::BytesCodec;
use crate::metrics::Counters;
use crate::telemetry;
use crate::tls::TlsState;
use crate::transport::{MAX_MESSAGE_BYTES, PATH};

/// How long a peer has to complete the TLS handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Handshaken connections waiting for the server to take them.
const ACCEPTED_QUEUE: usize = 64;

/// The gRPC service a replica serves: one method taking every request.
#[derive(Clone)]
struct ReplicaService {
    /// The replica's request handler.
    handler: Arc<dyn Handler>,
    /// Traffic counters.
    counters: Arc<Counters>,
}

impl NamedService for ReplicaService {
    const NAME: &'static str = "interlaken.Replica";
}

/// One call of the service's method.
struct Call(ReplicaService);

impl UnaryService<Vec<u8>> for Call {
    type Response = Vec<u8>;
    type Future = BoxFuture<tonic::Response<Vec<u8>>, Status>;

    /// Decode the request, answer it and encode the response.
    fn call(&mut self, request: tonic::Request<Vec<u8>>) -> Self::Future {
        let service = self.0.clone();
        Box::pin(async move {
            ::metrics::counter!(telemetry::SERVED_BYTES, "direction" => "received")
                .increment(request.get_ref().len() as u64);
            let request: Request = postcard::from_bytes(request.get_ref())
                .map_err(|e| Status::invalid_argument(e.to_string()))?;
            let kind = request.kind();
            let response = service.handler.handle(request).await;
            service.counters.served();
            ::metrics::counter!(telemetry::SERVED, "kind" => kind).increment(1);
            let bytes =
                postcard::to_stdvec(&response).map_err(|e| Status::internal(e.to_string()))?;
            ::metrics::counter!(telemetry::SERVED_BYTES, "direction" => "sent")
                .increment(bytes.len() as u64);
            Ok(tonic::Response::new(bytes))
        })
    }
}

impl<B> Service<http::Request<B>> for ReplicaService
where
    B: tonic::codegen::Body + Send + 'static,
    B::Error: Into<StdError> + Send + 'static,
{
    type Response = http::Response<Body>;
    type Error = std::convert::Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;

    /// Accept requests at any time; the server limits concurrency.
    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    /// Route the one method; refuse anything else.
    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        if request.uri().path() != PATH {
            return Box::pin(async { Ok(Status::unimplemented("unknown method").into_http()) });
        }
        let call = Call(self.clone());
        Box::pin(async move {
            let mut grpc = tonic::server::Grpc::new(BytesCodec)
                .max_decoding_message_size(MAX_MESSAGE_BYTES)
                .max_encoding_message_size(MAX_MESSAGE_BYTES);
            Ok(grpc.unary(call, request).await)
        })
    }
}

/// Limits on the server.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    /// The deadline for answering each request.
    pub(crate) timeout: Duration,
    /// Requests served at once on each connection; more are shed.
    pub(crate) concurrency: usize,
    /// How long a connection may live before the server closes it, so
    /// peers reconnect and are verified again (ADR-0041).
    pub(crate) max_connection_age: Duration,
}

/// Accept TCP connections on `listener` and complete each one's TLS
/// handshake in a task of its own, sending the established ones on `ready`,
/// until the server stops taking them.
async fn accept(
    listener: tokio::net::TcpListener,
    acceptor: TlsAcceptor,
    ready: tokio::sync::mpsc::Sender<std::io::Result<TlsStream<TcpStream>>>,
) {
    loop {
        let tcp = match listener.accept().await {
            Ok((tcp, _)) => tcp,
            Err(error) => {
                // Out of file descriptors, or similar: back off briefly.
                eprintln!("interlaken-net: accepting a connection failed: {error}");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        if ready.is_closed() {
            return;
        }
        let (acceptor, ready) = (acceptor.clone(), ready.clone());
        tokio::spawn(async move {
            // A refused or stalled handshake just closes the connection;
            // the verifier has counted a refusal.
            if let Ok(Ok(stream)) =
                tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await
            {
                let _ = ready.send(Ok(stream)).await;
            }
        });
    }
}

/// Serve `handler` on `listener` with mutual TLS, checked against `tls`'s
/// current material, until `shutdown` resolves.
///
/// # Errors
///
/// Returns the server's error if TLS can't be set up or serving fails.
pub(crate) async fn serve(
    listener: tokio::net::TcpListener,
    tls: Arc<TlsState>,
    handler: Arc<dyn Handler>,
    counters: Arc<Counters>,
    limits: Limits,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let acceptor = TlsAcceptor::from(tls.server_config()?);
    let (ready, accepted) = tokio::sync::mpsc::channel(ACCEPTED_QUEUE);
    let accepting = tokio::spawn(accept(listener, acceptor, ready));
    let served = Server::builder()
        .timeout(limits.timeout)
        .concurrency_limit_per_connection(limits.concurrency)
        .load_shed(true)
        .max_connection_age(limits.max_connection_age)
        .add_service(ReplicaService { handler, counters })
        .serve_with_incoming_shutdown(
            tokio_stream::wrappers::ReceiverStream::new(accepted),
            shutdown,
        )
        .await;
    accepting.abort();
    Ok(served?)
}
