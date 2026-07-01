mod block;
mod transport;

use block::{BlockLayer, lwm2m_block_handler_config};

use coap_lite::CoapOption::LocationPath;
use coap_lite::option_value::OptionValueString;
use coap_lite::{CoapRequest, CoapResponse, Packet, RequestType, ResponseType};
use rand::Rng;
use rand::distr::Alphanumeric;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tower::layer::util::Stack;
use tower::util::{BoxService, MapErrLayer};
use tower::{Layer, Service, ServiceBuilder};
use tower_resilience_cache::{CacheError, CacheLayer as CacheLayerImpl, EvictionPolicy};
use tower_resilience_coalesce::{CoalesceError, CoalesceLayer as CoalesceLayerImpl};
use tower_resilience_retry::{ExponentialRandomBackoff, RetryLayer};
use transport::{Transport, TransportMessage, UdpTransport};

/// Errors that can occur during server operations.
#[derive(Debug, Clone)]
pub enum ServerError {
    /// The incoming bytes could not be parsed as a valid CoAP packet.
    ///
    /// This is a **permanent** error: re-processing the same bytes will always
    /// fail, so the retry layer never retries it.
    CoapParsing,
    /// The request addressed a path or used a method not handled by this server.
    ///
    /// This is a **permanent** error for the same reason as [`CoapParsing`][Self::CoapParsing].
    WrongPathOrMethod,
    /// A transient failure that may resolve on a subsequent attempt.
    ///
    /// Examples include a temporarily unavailable backend resource.
    /// The retry layer **will** retry this error up to [`COAP_MAX_RETRANSMIT`] times
    /// with exponential random backoff per RFC 7252 §4.2.
    Transient,
    CoalesceLeaderCancelled,
    CoalesceRecvError,
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServerError::CoapParsing => write!(f, "CoAP parsing error"),
            ServerError::WrongPathOrMethod => write!(f, "wrong CoAP path or method"),
            ServerError::Transient => write!(f, "transient error"),
            ServerError::CoalesceLeaderCancelled => {
                write!(f, "coalesce leader request was cancelled")
            }
            ServerError::CoalesceRecvError => {
                write!(f, "coalesce failed to receive result from leader")
            }
        }
    }
}

impl std::error::Error for ServerError {}

/// A tower service for processing LwM2M messages.
///
/// This service handles incoming LwM2M transport messages, parses them,
/// routes them to appropriate handlers, and returns responses.
#[derive(Clone)]
pub struct MessageHandler;

impl Default for MessageHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl MessageHandler {
    /// Creates a new message handler service.
    pub fn new() -> Self {
        MessageHandler
    }

    /// Generates a unique registration ID.
    fn generate_registration_id() -> String {
        let rng = rand::rng();
        rng.sample_iter(&Alphanumeric)
            .take(10)
            .map(char::from)
            .collect()
    }

    /// Handles registration requests.
    async fn handle_registration(packet: Lwm2mPacket<'_>) -> Option<CoapResponse> {
        if let Some(mut response) = packet.message.response {
            let reg_id = Self::generate_registration_id();
            response.set_status(ResponseType::Created);
            response.message.clear_all_options();
            response.message.add_option(LocationPath, b"rd".to_vec());
            response
                .message
                .add_option_as(LocationPath, OptionValueString(reg_id));
            return Some(response);
        }
        None
    }

    /// Processes a transport message and returns a response.
    async fn process_message(msg: TransportMessage) -> Result<TransportMessage, ServerError> {
        if let Ok(packet) = Packet::from_bytes(&msg.message_buf[..]) {
            let lwm2m_packet = Lwm2mPacket {
                message: CoapRequest::from_packet(packet, msg.peer_addr.clone()),
                transport_message: &msg,
            };

            let path = lwm2m_packet.message.get_path().clone();
            let method = lwm2m_packet.message.get_method();
            let response = match (path.as_str(), method) {
                ("rd", RequestType::Post) => Self::handle_registration(lwm2m_packet).await,
                _ => {
                    if let Some(mut resp) = lwm2m_packet.message.response {
                        resp.set_status(ResponseType::InternalServerError);
                        resp.message.clear_all_options();
                        Some(resp)
                    } else {
                        return Err(ServerError::WrongPathOrMethod);
                    }
                }
            };

            if let Some(response) = response {
                let buffer = response.message.to_bytes().unwrap_or_else(|_| Vec::new());
                return Ok(TransportMessage::new(msg.peer_addr.clone(), buffer));
            }
        }
        Err(ServerError::CoapParsing)
    }
}

impl Service<TransportMessage> for MessageHandler {
    type Response = TransportMessage;
    type Error = ServerError;
    type Future = Pin<Box<dyn Future<Output = Result<TransportMessage, ServerError>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, msg: TransportMessage) -> Self::Future {
        Box::pin(Self::process_message(msg))
    }
}

/// A parsed LwM2M packet containing the CoAP request and underlying transport message.
pub struct Lwm2mPacket<'a> {
    /// The parsed CoAP request.
    pub message: CoapRequest<String>,
    pub transport_message: &'a TransportMessage,
}

/// CoAP message deduplication key per RFC 7252 §4.5.
///
/// RFC 7252 §4.5 requires servers to keep track of recently received
/// (source endpoint, Message ID) pairs to suppress duplicate CON/NON
/// messages caused by client retransmissions.
///
/// The [`CoalesceLayer`] uses this key to ensure at most one in-flight
/// handler exists per (endpoint, MID) pair: when a duplicate CON arrives
/// while the original is still being processed, the duplicate waits for
/// the first call's result and receives the same response — preventing
/// duplicate registrations and double-processing.
///
/// Non-CoAP bytes (parse failures) use MID 0, which collapses all
/// simultaneous malformed packets from the same peer into one parse-error
/// call — an acceptable and safe approximation.
fn coap_dedup_key(msg: &TransportMessage) -> (String, u16) {
    let mid = Packet::from_bytes(&msg.message_buf)
        .map(|p| p.header.message_id)
        .unwrap_or(0);
    (msg.peer_addr.clone(), mid)
}

// ---------------------------------------------------------------------------
// CoAP retransmission parameters — RFC 7252 §4.8
// ---------------------------------------------------------------------------
// LwM2M 1.1 §6.8.1 (UDP binding) defers entirely to RFC 7252; no overrides.
// LwM2M 1.1 §5.2.9 additionally recommends exponential back-off for DTLS
// reconnection attempts.

/// RFC 7252 §4.8: base retransmission timeout.
const COAP_ACK_TIMEOUT: Duration = Duration::from_secs(2);

/// Jitter fraction applied to each retry interval.
///
/// RFC 7252 §4.2 randomises only the *initial* timeout uniformly over
/// `[ACK_TIMEOUT, ACK_TIMEOUT × ACK_RANDOM_FACTOR]` = `[2 s, 3 s]`.
/// `ExponentialRandomBackoff` applies symmetric ± jitter at every step,
/// so `0.25` gives `[1.5 s, 2.5 s]` on the first retry — a reasonable
/// approximation of the spec's intent to desynchronise senders.
const COAP_BACKOFF_JITTER: f64 = 0.25;

/// RFC 7252 §4.8: maximum retransmission count after the initial attempt.
const COAP_MAX_RETRANSMIT: usize = 4;

/// [`MessageHandler`] wrapped with CoAP-paced exponential-random retry
/// (RFC 7252 §4.2 / §4.8).
///
/// Only transient (non-permanent) errors are retried; [`ServerError::CoapParsing`]
/// and [`ServerError::WrongPathOrMethod`] are returned immediately because
/// re-processing the same bytes cannot produce a different outcome.
/// Builds the [`RetryLayer`] with parameters derived from RFC 7252 §4.2 / §4.8.
///
/// | Parameter           | RFC 7252 value | Used here                              |
/// |---------------------|----------------|----------------------------------------|
/// | `MAX_RETRANSMIT`    | 4              | `max_attempts` = 4 + 1 = **5**         |
/// | `ACK_TIMEOUT`       | 2 s            | base interval = **2 s**                |
/// | `ACK_RANDOM_FACTOR` | 1.5            | approximated by ±25 % jitter per step  |
///
/// Only **transient** errors are retried.  [`ServerError::CoapParsing`] and
/// [`ServerError::WrongPathOrMethod`] are permanent: re-sending the same bytes
/// cannot produce a different outcome.  When future variants representing
/// transient backend failures are added to [`ServerError`], extend the
/// `retry_on` predicate to allow them through.
///
/// LwM2M 1.1 §6.8.1 specifies no UDP-binding overrides; §5.2.9 recommends
/// exponential back-off for DTLS reconnections.  Both are consistent with
/// these RFC 7252 defaults.
fn coap_retry_layer() -> RetryLayer<TransportMessage, TransportMessage, ServerError> {
    RetryLayer::builder()
        .name("coap-request")
        .max_attempts(COAP_MAX_RETRANSMIT + 1)
        .backoff(ExponentialRandomBackoff::new(
            COAP_ACK_TIMEOUT,
            COAP_BACKOFF_JITTER,
        ))
        .retry_on(|err: &ServerError| {
            !matches!(
                err,
                ServerError::CoapParsing | ServerError::WrongPathOrMethod
            )
        })
        .build()
}

/// Deduplication key: (source endpoint address, CoAP Message ID).
type DedupKey = (String, u16);

// ---------------------------------------------------------------------------
// CacheLayer — RFC 7252 §4.5 response cache (historical deduplication)
// ---------------------------------------------------------------------------

/// Inner type alias for the composed cache-then-map-error stack.
type CacheLayerInner<Req, E> =
    Stack<CacheLayerImpl<Req, DedupKey>, MapErrLayer<fn(CacheError<E>) -> E>>;

/// RFC 7252 §4.8: assumed maximum one-way network latency.
const COAP_MAX_LATENCY: Duration = Duration::from_secs(100);

/// RFC 7252 §4.8.2: window in which a CON exchange can remain in flight.
///
/// `EXCHANGE_LIFETIME = MAX_TRANSMIT_SPAN + 2 × MAX_LATENCY + PROCESSING_DELAY`
///                    = 45 s              + 200 s             + 2 s = **247 s**
///
/// The server cache retains responses for this duration to replay them for
/// any retransmitted copy of the same CON (RFC 7252 §4.5).
const COAP_EXCHANGE_LIFETIME: Duration =
    Duration::from_secs(45 + 2 * COAP_MAX_LATENCY.as_secs() + 2);

/// A [`tower::Layer`] matching the `<Req, Res, E>` convention of [`RetryLayer`].
///
/// Caches successful responses keyed on `(source endpoint, Message ID)` for
/// [`COAP_EXCHANGE_LIFETIME`] (247 s) and returns the cached response
/// immediately when a duplicate CON/NON message arrives, without invoking the
/// inner service again (RFC 7252 §4.5).
///
/// This layer sits **outermost** in the middleware stack so that historical
/// retransmissions are short-circuited before reaching the coalescing and
/// retry layers.  Errors are not cached; only `Ok(TransportMessage)` responses
/// are stored.
///
/// Eviction uses FIFO so that the oldest entries — those nearest their
/// natural expiry — are replaced first when the cache reaches capacity.
struct CacheLayer<Req, Res, E>(CacheLayerInner<Req, E>, PhantomData<Res>);

impl<S, Req, Res, E> Layer<S> for CacheLayer<Req, Res, E>
where
    CacheLayerInner<Req, E>: Layer<S>,
{
    type Service = <CacheLayerInner<Req, E> as Layer<S>>::Service;
    fn layer(&self, service: S) -> Self::Service {
        self.0.layer(service)
    }
}

impl From<CacheError<ServerError>> for ServerError {
    fn from(e: CacheError<ServerError>) -> Self {
        e.into_inner()
    }
}

/// Builds the outermost [`CacheLayer`] that implements RFC 7252 §4.5
/// duplicate-response caching.
///
/// | Parameter            | Value       | Source                      |
/// |----------------------|-------------|-----------------------------|
/// | Cache key            | (peer, MID) | RFC 7252 §4.5               |
/// | TTL                  | 247 s       | `EXCHANGE_LIFETIME` §4.8.2  |
/// | Max entries          | 1 024       | practical default           |
/// | Eviction policy      | FIFO        | time-ordered entries        |
fn coap_cache_layer() -> CacheLayer<TransportMessage, TransportMessage, ServerError> {
    CacheLayer(
        Stack::new(
            CacheLayerImpl::<TransportMessage, DedupKey>::builder()
                .name("coap-dedup-cache")
                .max_size(1024)
                .ttl(COAP_EXCHANGE_LIFETIME)
                .eviction_policy(EvictionPolicy::Fifo)
                .key_extractor(coap_dedup_key)
                .build()
                .unwrap(),
            MapErrLayer::new(ServerError::from as fn(CacheError<ServerError>) -> ServerError),
        ),
        PhantomData,
    )
}

// ---------------------------------------------------------------------------
// CoalesceLayer — concurrent in-flight deduplication (singleflight)
// ---------------------------------------------------------------------------

/// Inner type alias for the composed coalesce-then-map-error stack.
type CoalesceLayerInner<Req, E> = Stack<
    CoalesceLayerImpl<DedupKey, Req, fn(&Req) -> DedupKey>,
    MapErrLayer<fn(CoalesceError<E>) -> E>,
>;

/// A [`tower::Layer`] matching the `<Req, Res, E>` convention of [`RetryLayer`].
///
/// Coalesces duplicate in-flight requests (singleflight) and maps
/// [`CoalesceError<E>`] back to `E`, so the wrapped service keeps
/// `Error = E` throughout the [`ServiceBuilder`] stack.
struct CoalesceLayer<Req, Res, E>(CoalesceLayerInner<Req, E>, PhantomData<Res>);

impl<S, Req, Res, E> Layer<S> for CoalesceLayer<Req, Res, E>
where
    CoalesceLayerInner<Req, E>: Layer<S>,
{
    type Service = <CoalesceLayerInner<Req, E> as Layer<S>>::Service;
    fn layer(&self, service: S) -> Self::Service {
        self.0.layer(service)
    }
}

impl From<CoalesceError<ServerError>> for ServerError {
    fn from(e: CoalesceError<ServerError>) -> Self {
        match e {
            CoalesceError::Service(inner) => inner,
            CoalesceError::LeaderCancelled => ServerError::CoalesceLeaderCancelled,
            CoalesceError::RecvError => ServerError::CoalesceRecvError,
        }
    }
}

fn coap_coalesce_layer() -> CoalesceLayer<TransportMessage, TransportMessage, ServerError> {
    CoalesceLayer(
        Stack::new(
            CoalesceLayerImpl::builder(coap_dedup_key as fn(&TransportMessage) -> DedupKey)
                .name("coap-coalesce")
                .build(),
            MapErrLayer::new(ServerError::from as fn(CoalesceError<ServerError>) -> ServerError),
        ),
        PhantomData,
    )
}

/// An LwM2M server that handles device bootstrap, registration and management.
///
/// The server listens for incoming CoAP messages over a configured transport
/// and responds according to the LwM2M protocol specification.
///
/// Incoming messages pass through a layered middleware stack (outermost first):
///
/// | Layer | Purpose |
/// |-------|---------|
/// | [`BlockLayer`] | RFC 7959 block-wise transfer (Block1 reassembly, Block2 fragmentation) |
/// | [`CacheLayer`] | RFC 7252 §4.5 response cache — replays responses for CON retransmissions |
/// | [`CoalesceLayer`] | Singleflight dedup — concurrent duplicates wait for the leader result |
/// | [`RetryLayer`] | RFC 7252 §4.2 exponential-backoff retry on transient errors |
/// | [`MessageHandler`] | CoAP routing and application logic |
pub struct Lwm2mServer {
    transport: Box<dyn Transport>,
    message_handler: BoxService<TransportMessage, TransportMessage, ServerError>,
}

impl Lwm2mServer {
    /// Creates a new LwM2M server bound to a UDP socket at the given address.
    ///
    /// # Arguments
    /// * `address` - The socket address to bind to (e.g., `"0.0.0.0:5683"`).
    ///
    /// # Panics
    /// Panics if the UDP socket cannot be bound to the specified address.
    pub async fn new_udp(address: &str) -> Self {
        let transport = Box::new(
            UdpTransport::from_address(address)
                .await
                .expect("Failed to initialize transport"),
        );

        Self::new_from_transport(transport).await
    }

    /// Creates a new LwM2M server using a custom transport implementation.
    ///
    /// # Arguments
    /// * `transport` - A boxed transport implementing the `Transport` trait.
    pub async fn new_from_transport(transport: Box<dyn Transport>) -> Self {
        Lwm2mServer {
            transport,
            message_handler: ServiceBuilder::new()
                .boxed()
                .layer(BlockLayer::new(lwm2m_block_handler_config()))
                .layer(coap_cache_layer())
                .layer(coap_coalesce_layer())
                .layer(coap_retry_layer())
                .service(MessageHandler::new()),
        }
    }

    /// Starts the server's main event loop.
    ///
    /// Receives messages from the transport and dispatches each one to a
    /// dedicated Tokio task.  An internal channel collects responses from
    /// finished tasks and forwards them back through the transport.
    ///
    /// Concurrent dispatch is required for the [`CoalesceLayer`] to coalesce
    /// duplicate CON retransmissions that arrive while the original is still
    /// being processed.
    pub async fn run(mut self) {
        // Each call to `message_handler.call()` returns a `Send + 'static`
        // future.  We collect them in a `JoinSet` so multiple in-flight
        // requests can be processed concurrently while the CoalesceLayer
        // deduplicates CON retransmissions that share the same (endpoint,
        // Message-ID) key.
        let mut tasks: tokio::task::JoinSet<Result<TransportMessage, ServerError>> =
            tokio::task::JoinSet::new();

        loop {
            tokio::select! {
                // Wait for the next incoming packet.
                recv_result = self.transport.receive() => {
                    if let Ok(msg) = recv_result {
                        tasks.spawn(self.message_handler.call(msg));
                    }
                }
                // Forward any response produced by a processing task.
                Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                    match result {
                        Ok(Ok(response)) => {
                            self.transport.send(response).await.unwrap_or_else(|e| {
                                eprintln!("Error sending response: {e:?}");
                            });
                        }
                        Ok(Err(e)) => {
                            eprintln!("Error handling message: {e:?}");
                        }
                        Err(e) => {
                            eprintln!("Task panicked or was cancelled: {e:?}");
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::tests::InMemoryTransport;
    use coap_lite::MessageClass::Response;
    use coap_lite::ResponseType::Created;
    use coap_lite::option_value::OptionValueString;
    use coap_lite::{CoapOption, CoapRequest, MessageType, RequestType};
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::unix::SocketAddr;
    use tokio::sync::mpsc::{Receiver, Sender};
    use tokio::task::JoinHandle;
    use tower::{Layer, Service, ServiceExt};

    struct TestClient {
        address: String,
        to_server_sender: Sender<TransportMessage>,
        from_server_receiver: Receiver<TransportMessage>,
    }

    impl TestClient {
        async fn send_to_server(&self, msg_buf: Vec<u8>) {
            self.to_server_sender
                .send(TransportMessage::new(self.address.clone(), msg_buf))
                .await
                .unwrap()
        }
    }

    struct TestClientsAndServer {
        _server_join_handle: JoinHandle<()>,
        clients: Vec<TestClient>,
    }

    fn spawn_server_for_tests(num_clients: u8) -> TestClientsAndServer {
        let (to_server_sender, to_server_receiver) = tokio::sync::mpsc::channel(1);

        let mut transport = Box::new(InMemoryTransport::new(to_server_receiver));

        let mut clients = Vec::with_capacity(num_clients as usize);
        for i in 0..num_clients {
            let (from_server_sender, from_server_receiver) = tokio::sync::mpsc::channel(1);
            let address = format!("2025:beef::{}", i + 1);

            transport.add_client(&address.clone(), from_server_sender);

            clients.push(TestClient {
                address,
                to_server_sender: to_server_sender.clone(),
                from_server_receiver,
            });
        }

        TestClientsAndServer {
            _server_join_handle: tokio::spawn(async move {
                let s: Lwm2mServer = Lwm2mServer::new_from_transport(transport).await;
                s.run().await;
            }),
            clients,
        }
    }

    fn create_reg_message_for_tests() -> CoapRequest<SocketAddr> {
        let mut request: CoapRequest<SocketAddr> = CoapRequest::new();

        request.set_method(RequestType::Post);
        request.message.header.set_type(MessageType::Confirmable);

        request.set_path("/rd");
        request
            .message
            .set_content_format(coap_lite::ContentFormat::ApplicationLinkFormat);
        request
            .message
            .add_option(CoapOption::UriQuery, b"lwm2m=1.1".to_vec());
        request
            .message
            .add_option(CoapOption::UriQuery, b"ep=test-device".to_vec());
        request
            .message
            .add_option(CoapOption::UriQuery, b"lt=86400".to_vec());

        request.message.payload =
            br#"</>;rt="oma.lwm2m";ct=112,</1/1>,</3>;ver=1.0,</3/0>,</5>;ver=1.0,</5/0>"#.to_vec();

        request
    }

    #[tokio::test]
    async fn test_registration_msg() -> std::io::Result<()> {
        let mut client_and_server = spawn_server_for_tests(1);

        let req = create_reg_message_for_tests();
        let req = req.message.to_bytes().unwrap();
        let test_client = &mut client_and_server.clients[0];
        test_client.send_to_server(req).await;
        let resp = &test_client.from_server_receiver.recv().await.unwrap();
        let resp = Packet::from_bytes(&resp.message_buf).unwrap();

        assert_eq!(resp.header.get_type(), MessageType::Acknowledgement);
        assert_eq!(resp.header.code, Response(Created));
        assert!(resp.payload.is_empty());

        let actual = resp
            .get_options_as::<OptionValueString>(LocationPath)
            .unwrap();
        let actual = actual
            .iter()
            .map(|x| x.as_ref().cloned())
            .collect::<Vec<_>>();
        assert_eq!(actual.len(), 2);
        let rd = actual[0].as_ref().unwrap();
        assert_eq!(rd.0, "rd");
        let reg_id = actual[1].as_ref().unwrap();
        let reg_id = &reg_id.0;
        assert_eq!(reg_id.len(), 10);
        assert!(reg_id.chars().all(char::is_alphanumeric));

        Ok(())
    }

    #[tokio::test]
    async fn test_wrong_path() -> std::io::Result<()> {
        let mut client_and_server = spawn_server_for_tests(1);

        let mut req: CoapRequest<SocketAddr> = CoapRequest::new();

        req.set_method(RequestType::Post);
        req.message.header.set_type(MessageType::Confirmable);

        req.set_path("/wrong_url");

        let req = req.message.to_bytes().unwrap();
        let test_client = &mut client_and_server.clients[0];
        test_client.send_to_server(req).await;
        let resp = &test_client.from_server_receiver.recv().await.unwrap();
        let resp = Packet::from_bytes(&resp.message_buf).unwrap();
        assert_eq!(resp.header.get_code(), "5.00");

        Ok(())
    }

    #[tokio::test]
    async fn test_registration_msg_2_clients() -> std::io::Result<()> {
        let mut client_and_server = spawn_server_for_tests(2);

        let req = create_reg_message_for_tests();
        let req = req.message.to_bytes().unwrap();

        assert_eq!(client_and_server.clients.len(), 2);

        let mut reg_ids: HashSet<String> = HashSet::new();
        for i in 0..client_and_server.clients.len() {
            let test_client = &mut client_and_server.clients[i];
            test_client.send_to_server(req.clone()).await;
            let resp = &test_client.from_server_receiver.recv().await.unwrap();
            let resp = Packet::from_bytes(&resp.message_buf).unwrap();

            assert_eq!(resp.header.get_type(), MessageType::Acknowledgement);
            assert_eq!(resp.header.code, Response(Created));
            assert!(resp.payload.is_empty());

            let location_path = resp
                .get_options_as::<OptionValueString>(LocationPath)
                .unwrap();
            let mut iter = location_path.iter();
            assert_eq!(
                iter.next(),
                Some(Ok(OptionValueString("rd".to_owned()))).as_ref()
            );

            let actual_regid = iter.next();
            let actual_regid = actual_regid.unwrap();
            let actual_regid = actual_regid.as_ref().unwrap().0.to_owned();

            assert!(!reg_ids.contains(&actual_regid));
            reg_ids.insert(actual_regid);
        }

        assert_eq!(reg_ids.len(), client_and_server.clients.len());

        Ok(())
    }

    /// RFC 7252 §4.5 – duplicate CON suppression via the coalesce layer.
    ///
    /// Two messages from the *same* peer with the *same* CoAP Message ID
    /// (simulating a client retransmission) are both expected to receive a
    /// valid 2.01 Created response.  Because the second arrives after the
    /// first completes in this sequential test, it is processed independently
    /// and may return a different registration ID.  The key assertion is that
    /// BOTH duplicates receive a well-formed, successful response rather than
    /// being silently dropped or producing an error.
    #[tokio::test]
    async fn test_duplicate_mid_both_receive_response() -> std::io::Result<()> {
        // Use a larger channel so we can queue two messages before reading
        // any response.
        let (to_server_sender, to_server_receiver) = tokio::sync::mpsc::channel(8);
        let (from_server_sender, mut from_server_receiver) = tokio::sync::mpsc::channel(8);
        let address = "2025:beef::1".to_string();

        let mut transport = Box::new(InMemoryTransport::new(to_server_receiver));
        transport.add_client(&address, from_server_sender);

        tokio::spawn(async move {
            let s = Lwm2mServer::new_from_transport(transport).await;
            s.run().await;
        });

        // Build a registration CON with an explicit, fixed Message ID so both
        // sends look identical to the server (same endpoint + same MID).
        let mut req = create_reg_message_for_tests();
        req.message.header.message_id = 0xBEEF;
        let raw = req.message.to_bytes().unwrap();

        // Send the original and its retransmission before reading any response.
        to_server_sender
            .send(TransportMessage::new(address.clone(), raw.clone()))
            .await
            .unwrap();
        to_server_sender
            .send(TransportMessage::new(address.clone(), raw.clone()))
            .await
            .unwrap();

        // Both the original and the duplicate must produce a 2.01 Created.
        for _ in 0..2 {
            let resp = from_server_receiver.recv().await.unwrap();
            let resp = Packet::from_bytes(&resp.message_buf).unwrap();
            assert_eq!(resp.header.get_type(), MessageType::Acknowledgement);
            assert_eq!(resp.header.code, Response(Created));
        }

        Ok(())
    }

    fn make_transport_msg(bytes: Vec<u8>) -> TransportMessage {
        TransportMessage::new("127.0.0.1:1234".to_string(), bytes)
    }

    #[tokio::test]
    async fn test_message_handler_direct_registration() {
        // Call MessageHandler directly (bypassing Lwm2mServer) for a registration request.
        let mut handler = MessageHandler::new();

        let req = create_reg_message_for_tests();
        let msg = make_transport_msg(req.message.to_bytes().unwrap());

        let response = handler.call(msg).await.unwrap();
        let packet = Packet::from_bytes(&response.message_buf).unwrap();

        assert_eq!(packet.header.get_type(), MessageType::Acknowledgement);
        assert_eq!(packet.header.code, Response(Created));
        assert!(packet.payload.is_empty());

        let location_path = packet
            .get_options_as::<OptionValueString>(LocationPath)
            .unwrap();
        let segments: Vec<_> = location_path
            .iter()
            .map(|x| x.as_ref().unwrap().0.clone())
            .collect();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0], "rd");
        assert_eq!(segments[1].len(), 10);
        assert!(segments[1].chars().all(char::is_alphanumeric));
    }

    #[tokio::test]
    async fn test_message_handler_invalid_bytes_returns_coap_parsing_error() {
        let mut handler = MessageHandler::new();
        let msg = make_transport_msg(vec![0xFF, 0xFE, 0xFD]);
        let result = handler.call(msg).await;
        assert!(matches!(result, Err(ServerError::CoapParsing)));
    }

    #[tokio::test]
    async fn test_message_handler_non_confirmable_wrong_path_returns_internal_server_error() {
        // A Non-Confirmable message to an unknown path should still receive a
        // 5.00 Internal Server Error response (coap-lite creates a response for
        // Non-Confirmable messages just as it does for Confirmable ones).
        let mut handler = MessageHandler::new();

        let mut req: CoapRequest<SocketAddr> = CoapRequest::new();
        req.set_method(RequestType::Post);
        req.message.header.set_type(MessageType::NonConfirmable);
        req.set_path("/unknown");

        let msg = make_transport_msg(req.message.to_bytes().unwrap());
        let result = handler.call(msg).await.unwrap();
        let packet = Packet::from_bytes(&result.message_buf).unwrap();
        assert_eq!(packet.header.get_code(), "5.00");
    }

    // -------------------------------------------------------------------------
    // Retry-layer unit tests
    //
    // These tests exercise `coap_retry_layer()` directly — the production retry
    // configuration — using a lightweight `tower::service_fn` mock instead of a
    // full `Lwm2mServer`.  `start_paused = true` lets Tokio auto-advance time
    // through backoff sleeps so the tests complete instantly.
    // -------------------------------------------------------------------------

    /// Builds a minimal `TransportMessage` for use in retry tests.
    fn make_retry_msg() -> TransportMessage {
        make_transport_msg(vec![0x40, 0x01, 0x00, 0x01]) // valid-ish CoAP header bytes
    }

    /// `CoapParsing` is a permanent error: the retry layer must **not** retry it.
    #[tokio::test(start_paused = true)]
    async fn retry_coap_parsing_error_is_not_retried() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let cc = Arc::clone(&call_count);

        let inner = tower::service_fn(move |_msg: TransportMessage| {
            let cc = Arc::clone(&cc);
            async move {
                cc.fetch_add(1, Ordering::SeqCst);
                Err::<TransportMessage, ServerError>(ServerError::CoapParsing)
            }
        });

        let mut svc = coap_retry_layer().layer(inner);
        let result = svc.ready().await.unwrap().call(make_retry_msg()).await;

        assert!(matches!(result, Err(ServerError::CoapParsing)));
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "CoapParsing must short-circuit without any retries"
        );
    }

    /// `WrongPathOrMethod` is a permanent error: the retry layer must **not** retry it.
    #[tokio::test(start_paused = true)]
    async fn retry_wrong_path_or_method_is_not_retried() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let cc = Arc::clone(&call_count);

        let inner = tower::service_fn(move |_msg: TransportMessage| {
            let cc = Arc::clone(&cc);
            async move {
                cc.fetch_add(1, Ordering::SeqCst);
                Err::<TransportMessage, ServerError>(ServerError::WrongPathOrMethod)
            }
        });

        let mut svc = coap_retry_layer().layer(inner);
        let result = svc.ready().await.unwrap().call(make_retry_msg()).await;

        assert!(matches!(result, Err(ServerError::WrongPathOrMethod)));
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "WrongPathOrMethod must short-circuit without any retries"
        );
    }

    /// A transient error is retried.  After two failures the service succeeds and
    /// the retry layer surfaces that success to the caller.
    #[tokio::test(start_paused = true)]
    async fn retry_transient_error_retries_until_success() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let cc = Arc::clone(&call_count);

        let inner = tower::service_fn(move |msg: TransportMessage| {
            let cc = Arc::clone(&cc);
            async move {
                let attempt = cc.fetch_add(1, Ordering::SeqCst);
                if attempt < 2 {
                    Err::<TransportMessage, ServerError>(ServerError::Transient)
                } else {
                    Ok(msg)
                }
            }
        });

        let mut svc = coap_retry_layer().layer(inner);
        let result = svc.ready().await.unwrap().call(make_retry_msg()).await;

        assert!(result.is_ok(), "should succeed after retries");
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            3,
            "should attempt exactly 3 times (2 transient failures + 1 success)"
        );
    }

    /// A persistent transient error exhausts all `COAP_MAX_RETRANSMIT + 1 = 5`
    /// attempts and then returns the final error.
    #[tokio::test(start_paused = true)]
    async fn retry_transient_error_exhausts_all_attempts() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let cc = Arc::clone(&call_count);

        let inner = tower::service_fn(move |_msg: TransportMessage| {
            let cc = Arc::clone(&cc);
            async move {
                cc.fetch_add(1, Ordering::SeqCst);
                Err::<TransportMessage, ServerError>(ServerError::Transient)
            }
        });

        let mut svc = coap_retry_layer().layer(inner);
        let result = svc.ready().await.unwrap().call(make_retry_msg()).await;

        assert!(
            matches!(result, Err(ServerError::Transient)),
            "exhausted retries must propagate the last error"
        );
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            COAP_MAX_RETRANSMIT + 1,
            "must attempt exactly COAP_MAX_RETRANSMIT + 1 = 5 times per RFC 7252 §4.8"
        );
    }

    /// A request that succeeds immediately is never retried.
    #[tokio::test(start_paused = true)]
    async fn retry_success_on_first_attempt_does_not_retry() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let cc = Arc::clone(&call_count);

        let inner = tower::service_fn(move |msg: TransportMessage| {
            let cc = Arc::clone(&cc);
            async move {
                cc.fetch_add(1, Ordering::SeqCst);
                Ok::<TransportMessage, ServerError>(msg)
            }
        });

        let mut svc = coap_retry_layer().layer(inner);
        let result = svc.ready().await.unwrap().call(make_retry_msg()).await;

        assert!(result.is_ok());
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "a first-attempt success must not trigger any retry"
        );
    }

    /// The service succeeds on the very last allowed attempt (`COAP_MAX_RETRANSMIT + 1`).
    /// This verifies the boundary: the retry layer must not give up one attempt too early.
    #[tokio::test(start_paused = true)]
    async fn retry_transient_error_succeeds_on_final_attempt() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let cc = Arc::clone(&call_count);
        let max_attempts = COAP_MAX_RETRANSMIT + 1; // 5

        let inner = tower::service_fn(move |msg: TransportMessage| {
            let cc = Arc::clone(&cc);
            async move {
                let attempt = cc.fetch_add(1, Ordering::SeqCst);
                if attempt + 1 < max_attempts {
                    Err::<TransportMessage, ServerError>(ServerError::Transient)
                } else {
                    Ok(msg)
                }
            }
        });

        let mut svc = coap_retry_layer().layer(inner);
        let result = svc.ready().await.unwrap().call(make_retry_msg()).await;

        assert!(
            result.is_ok(),
            "must succeed when service recovers on the last attempt"
        );
        assert_eq!(call_count.load(Ordering::SeqCst), max_attempts);
    }

    /// Unit-tests the retry predicate in isolation — independent of the Tower
    /// service machinery — to document which variants are permanent vs transient.
    #[test]
    fn retry_predicate_classifies_all_error_variants() {
        // Mirror the closure used inside `coap_retry_layer()`.
        let should_retry = |err: &ServerError| -> bool {
            !matches!(
                err,
                ServerError::CoapParsing | ServerError::WrongPathOrMethod
            )
        };

        assert!(
            !should_retry(&ServerError::CoapParsing),
            "CoapParsing must be permanent (not retried)"
        );
        assert!(
            !should_retry(&ServerError::WrongPathOrMethod),
            "WrongPathOrMethod must be permanent (not retried)"
        );
        assert!(
            should_retry(&ServerError::Transient),
            "Transient must be retryable"
        );
    }

    // -------------------------------------------------------------------------
    // Coalesce-layer unit tests
    //
    // These tests exercise `coap_coalesce_layer()` directly using lightweight
    // `tower::service_fn` mocks, mirroring the retry-layer test pattern.
    // -------------------------------------------------------------------------

    /// Builds a minimal 4-byte CoAP CON header with the given peer address and
    /// Message ID.  `coap_dedup_key` derives the dedup key from these two fields.
    fn make_coalesce_msg(peer: &str, mid: u16) -> TransportMessage {
        // CoAP fixed header (RFC 7252 §3): Ver=1 | Type=CON | TKL=0 | Code=0.01
        let bytes = vec![0x40, 0x01, (mid >> 8) as u8, (mid & 0xFF) as u8];
        TransportMessage::new(peer.to_string(), bytes)
    }

    /// All three `CoalesceError` variants map to the correct `ServerError` variant.
    #[test]
    fn coalesce_from_error_mapping() {
        use tower_resilience_coalesce::CoalesceError;

        assert!(matches!(
            ServerError::from(CoalesceError::Service(ServerError::CoapParsing)),
            ServerError::CoapParsing
        ));
        assert!(matches!(
            ServerError::from(CoalesceError::<ServerError>::LeaderCancelled),
            ServerError::CoalesceLeaderCancelled
        ));
        assert!(matches!(
            ServerError::from(CoalesceError::<ServerError>::RecvError),
            ServerError::CoalesceRecvError
        ));
    }

    /// A unique request reaches the inner service exactly once.
    #[tokio::test]
    async fn coalesce_unique_request_passes_through() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let cc = Arc::clone(&call_count);

        let inner = tower::service_fn(move |msg: TransportMessage| {
            let cc = Arc::clone(&cc);
            async move {
                cc.fetch_add(1, Ordering::SeqCst);
                Ok::<TransportMessage, ServerError>(msg)
            }
        });

        let mut svc = coap_coalesce_layer().layer(inner);
        let result = svc
            .ready()
            .await
            .unwrap()
            .call(make_coalesce_msg("10.0.0.1:1234", 0x0042))
            .await;

        assert!(result.is_ok());
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    /// Requests with distinct dedup keys each invoke the inner service independently.
    /// Distinct peer addresses and distinct Message IDs are tested separately.
    #[tokio::test]
    async fn coalesce_distinct_keys_not_coalesced() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let cc = Arc::clone(&call_count);

        let inner = tower::service_fn(move |msg: TransportMessage| {
            let cc = Arc::clone(&cc);
            async move {
                cc.fetch_add(1, Ordering::SeqCst);
                Ok::<TransportMessage, ServerError>(msg)
            }
        });

        let mut svc = coap_coalesce_layer().layer(inner);

        // Different MIDs, same peer
        svc.ready()
            .await
            .unwrap()
            .call(make_coalesce_msg("10.0.0.1:1234", 0x0001))
            .await
            .unwrap();
        svc.ready()
            .await
            .unwrap()
            .call(make_coalesce_msg("10.0.0.1:1234", 0x0002))
            .await
            .unwrap();
        // Same MID, different peers
        svc.ready()
            .await
            .unwrap()
            .call(make_coalesce_msg("10.0.0.2:1234", 0x0001))
            .await
            .unwrap();

        assert_eq!(
            call_count.load(Ordering::SeqCst),
            3,
            "each distinct (peer, MID) pair must invoke the inner service once"
        );
    }

    /// Two concurrent requests with the same (peer, MID) key are coalesced:
    /// the inner service is invoked once and both callers receive its result.
    #[tokio::test]
    async fn coalesce_concurrent_duplicate_invokes_service_once() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let cc = Arc::clone(&call_count);
        let notify = Arc::new(tokio::sync::Notify::new());
        let n = Arc::clone(&notify);

        // The inner service blocks until signalled, keeping the leader in-flight
        // long enough for the second caller to subscribe as a waiter.
        let inner = tower::service_fn(move |msg: TransportMessage| {
            let cc = Arc::clone(&cc);
            let n = Arc::clone(&n);
            async move {
                cc.fetch_add(1, Ordering::SeqCst);
                n.notified().await;
                Ok::<TransportMessage, ServerError>(msg)
            }
        });

        // Cloning the service shares the in-flight map (Arc inside CoalesceService).
        let svc = coap_coalesce_layer().layer(inner);
        let mut svc1 = svc.clone();
        let mut svc2 = svc.clone();

        let h1 = tokio::spawn(async move {
            svc1.ready()
                .await
                .unwrap()
                .call(make_coalesce_msg("10.0.0.1:1234", 0xBEEF))
                .await
        });
        tokio::task::yield_now().await; // let h1 register as leader and block on notified()
        let h2 = tokio::spawn(async move {
            svc2.ready()
                .await
                .unwrap()
                .call(make_coalesce_msg("10.0.0.1:1234", 0xBEEF))
                .await
        });
        tokio::task::yield_now().await; // let h2 subscribe as waiter

        notify.notify_one(); // release the leader; it broadcasts the result to the waiter

        assert!(h1.await.unwrap().is_ok(), "leader must succeed");
        assert!(
            h2.await.unwrap().is_ok(),
            "waiter must receive the coalesced result"
        );
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "inner service must be invoked only once for both duplicate requests"
        );
    }

    // -------------------------------------------------------------------------
    // Cache-layer unit tests
    //
    // These tests exercise `coap_cache_layer()` directly using lightweight
    // `tower::service_fn` mocks.  The cache layer sits outermost in the
    // production stack and implements RFC 7252 §4.5 response caching: a
    // response for a given (peer, MID) key is stored on the first successful
    // call and replayed for any subsequent duplicate within EXCHANGE_LIFETIME.
    // -------------------------------------------------------------------------

    /// `CacheError::Inner(e)` unwraps back to `e`.
    #[test]
    fn cache_from_error_mapping() {
        use tower_resilience_cache::CacheError;

        let e: ServerError = CacheError::Inner(ServerError::CoapParsing).into();
        assert!(matches!(e, ServerError::CoapParsing));
    }

    /// The first call for a key reaches the inner service; the response is cached.
    /// A second call with the same key returns the cached response without
    /// invoking the inner service again (RFC 7252 §4.5).
    #[tokio::test]
    async fn cache_hit_returns_cached_response_without_calling_service() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let cc = Arc::clone(&call_count);

        let inner = tower::service_fn(move |msg: TransportMessage| {
            let cc = Arc::clone(&cc);
            async move {
                cc.fetch_add(1, Ordering::SeqCst);
                Ok::<TransportMessage, ServerError>(msg)
            }
        });

        let mut svc = coap_cache_layer().layer(inner);
        let key_msg = make_coalesce_msg("10.0.0.1:5683", 0x0001);

        // First call — cache miss, inner service invoked.
        svc.ready()
            .await
            .unwrap()
            .call(key_msg.clone())
            .await
            .unwrap();
        assert_eq!(call_count.load(Ordering::SeqCst), 1);

        // Second call — cache hit, inner service NOT invoked.
        svc.ready().await.unwrap().call(key_msg).await.unwrap();
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "duplicate (peer, MID) must be served from cache"
        );
    }

    /// Requests with distinct dedup keys each miss the cache and invoke the
    /// inner service independently.
    #[tokio::test]
    async fn cache_distinct_keys_each_invoke_service() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let cc = Arc::clone(&call_count);

        let inner = tower::service_fn(move |msg: TransportMessage| {
            let cc = Arc::clone(&cc);
            async move {
                cc.fetch_add(1, Ordering::SeqCst);
                Ok::<TransportMessage, ServerError>(msg)
            }
        });

        let mut svc = coap_cache_layer().layer(inner);

        // Three distinct keys — one per combination of peer/MID.
        svc.ready()
            .await
            .unwrap()
            .call(make_coalesce_msg("10.0.0.1:5683", 0x0001))
            .await
            .unwrap();
        svc.ready()
            .await
            .unwrap()
            .call(make_coalesce_msg("10.0.0.1:5683", 0x0002))
            .await
            .unwrap();
        svc.ready()
            .await
            .unwrap()
            .call(make_coalesce_msg("10.0.0.2:5683", 0x0001))
            .await
            .unwrap();

        assert_eq!(
            call_count.load(Ordering::SeqCst),
            3,
            "each distinct (peer, MID) pair must invoke the inner service once"
        );
    }

    /// Errors are not cached: if the inner service fails, a subsequent request
    /// with the same key must retry the inner service (RFC 7252 §4.5 only
    /// requires caching of responses, not errors).
    #[tokio::test]
    async fn cache_errors_are_not_cached() {
        let call_count = Arc::new(AtomicUsize::new(0));
        let cc = Arc::clone(&call_count);

        // Service always fails.
        let inner = tower::service_fn(move |_: TransportMessage| {
            let cc = Arc::clone(&cc);
            async move {
                cc.fetch_add(1, Ordering::SeqCst);
                Err::<TransportMessage, ServerError>(ServerError::CoapParsing)
            }
        });

        let mut svc = coap_cache_layer().layer(inner);
        let key_msg = make_coalesce_msg("10.0.0.1:5683", 0xABCD);

        let _ = svc.ready().await.unwrap().call(key_msg.clone()).await;
        let _ = svc.ready().await.unwrap().call(key_msg).await;

        assert_eq!(
            call_count.load(Ordering::SeqCst),
            2,
            "failed responses must not be cached; each attempt must reach the inner service"
        );
    }

    /// After the TTL elapses, a cached entry expires and the inner service is
    /// called again for the same key.
    #[tokio::test]
    async fn cache_entry_expires_after_ttl() {
        // Build a one-shot cache with a very short TTL so this test is fast.
        use tower_resilience_cache::{CacheLayer as CacheLayerImpl, EvictionPolicy};

        let call_count = Arc::new(AtomicUsize::new(0));
        let cc = Arc::clone(&call_count);

        let inner = tower::service_fn(move |msg: TransportMessage| {
            let cc = Arc::clone(&cc);
            async move {
                cc.fetch_add(1, Ordering::SeqCst);
                Ok::<TransportMessage, ServerError>(msg)
            }
        });

        // 50 ms TTL — deliberately short for testing only.
        let short_ttl_layer = CacheLayerImpl::<TransportMessage, DedupKey>::builder()
            .max_size(16)
            .ttl(Duration::from_millis(50))
            .eviction_policy(EvictionPolicy::Fifo)
            .key_extractor(coap_dedup_key)
            .build()
            .unwrap();

        let mut svc = short_ttl_layer.layer(inner);
        let key_msg = make_coalesce_msg("10.0.0.1:5683", 0xFACE);

        svc.ready()
            .await
            .unwrap()
            .call(key_msg.clone())
            .await
            .unwrap();
        assert_eq!(call_count.load(Ordering::SeqCst), 1);

        // Cache hit before expiry.
        svc.ready()
            .await
            .unwrap()
            .call(key_msg.clone())
            .await
            .unwrap();
        assert_eq!(call_count.load(Ordering::SeqCst), 1, "should hit cache");

        // Wait for TTL to elapse, then the entry must be treated as a miss.
        tokio::time::sleep(Duration::from_millis(100)).await;

        svc.ready().await.unwrap().call(key_msg).await.unwrap();
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            2,
            "expired cache entry must cause a cache miss"
        );
    }
}
