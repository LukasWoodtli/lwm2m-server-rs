//! CoAP block-wise transfer (RFC 7959) as a stand-alone Tower service.
//!
//! [`BlockService`] is placed at the outermost position of the service stack.
//! It delegates to [`coap_lite::block_handler::BlockHandler`] to reassemble
//! Block1 (request-body upload) sequences and to fragment large responses into
//! Block2 sequences.
//!
//! # Protocol behaviour
//!
//! | Situation | Action |
//! |-----------|--------|
//! | Intermediate Block1 (`M = 1`) | Reply `2.31 Continue`; inner service not called |
//! | Oversized payload without Block1 | Reply `4.13 Request Entity Too Large` with hint |
//! | Block2 follow-up (GET + Block2 option) | Serve cached chunk; inner service not called |
//! | Final Block1 (`M = 0`) | Reassemble full body; call inner service; echo Block1 |
//! | Non-block request | Pass through unchanged |
//!
//! # References
//!
//! - RFC 7959 §2 — Block1 / Block2 option format and state machine
//! - RFC 7959 §2.3 — Block1 control usage in the server's response
//! - RFC 7959 §2.5 — Final block semantics; `4.08 Request Entity Incomplete`
//! - LwM2M 1.1 §6.8.1 — UDP binding defers to RFC 7959; no parameter overrides

use coap_lite::block_handler::{BlockHandler, BlockHandlerConfig, BlockValue};
use coap_lite::{CoapOption, CoapRequest, Packet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tower::{Layer, Service};

use crate::transport::TransportMessage;

// ─── Config ──────────────────────────────────────────────────────────────────

/// Returns a [`BlockHandlerConfig`] calibrated for LwM2M 1.1 over CoAP/UDP.
///
/// | Field | Value | Source |
/// |-------|-------|--------|
/// | `max_total_message_size` | 1152 B | RFC 7252 §4.6 `DEFAULT_MAX_MESSAGE_SIZE` |
/// | `cache_expiry_duration`  | 247 s  | RFC 7252 §4.8.2 `EXCHANGE_LIFETIME`     |
///
/// The 1152-byte limit yields a maximum block payload of 1024 bytes (SZX = 6).
/// LwM2M 1.1 §6.8.1 (UDP binding) references RFC 7959 for block-wise transfers
/// and does not override these CoAP defaults.
pub fn lwm2m_block_handler_config() -> BlockHandlerConfig {
    BlockHandlerConfig {
        max_total_message_size: 1152,
        // Partial block state is purged after the client's exchange must have expired.
        cache_expiry_duration: super::COAP_EXCHANGE_LIFETIME,
    }
}

// ─── Service ─────────────────────────────────────────────────────────────────

/// Tower service that implements the server side of CoAP block-wise transfers
/// (RFC 7959) as a composable middleware layer.
///
/// All clones share a single `Arc<Mutex<BlockHandler<String>>>`, so concurrent
/// tasks from the same client contribute to the same assembly buffer and response
/// cache.
pub struct BlockService<S> {
    inner: S,
    block_handler: Arc<Mutex<BlockHandler<String>>>,
}

impl<S: Clone> Clone for BlockService<S> {
    fn clone(&self) -> Self {
        BlockService {
            inner: self.inner.clone(),
            block_handler: Arc::clone(&self.block_handler),
        }
    }
}

impl<S> BlockService<S> {
    /// Creates a new [`BlockService`] wrapping `inner` with the provided
    /// [`BlockHandlerConfig`].  Use [`lwm2m_block_handler_config`] for
    /// spec-compliant defaults.
    pub fn new(inner: S, config: BlockHandlerConfig) -> Self {
        BlockService {
            inner,
            block_handler: Arc::new(Mutex::new(BlockHandler::new(config))),
        }
    }
}

impl<S> Service<TransportMessage> for BlockService<S>
where
    S: Service<TransportMessage, Response = TransportMessage>,
    S::Error: Send + 'static,
    S::Future: Send + 'static,
{
    type Response = TransportMessage;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<TransportMessage, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, msg: TransportMessage) -> Self::Future {
        let peer_addr = msg.peer_addr.clone();
        let block_handler = Arc::clone(&self.block_handler);

        // ── 1. Parse raw bytes ────────────────────────────────────────────────
        // Forward unparseable bytes to the inner service unchanged; it will
        // return ServerError::CoapParsing through its own parse attempt.
        let Ok(packet) = Packet::from_bytes(&msg.message_buf) else {
            return Box::pin(self.inner.call(msg));
        };
        let mut request = CoapRequest::from_packet(packet, peer_addr.clone());

        // ── 2. Block intercept (before application logic) ─────────────────────
        //
        //   true  → block handler consumed the exchange:
        //             M=1 (intermediate) → 2.31 Continue in request.response
        //             No Block1, oversized → 4.13 Too Large in request.response
        //             Block2 follow-up  → chunk served from cache
        //           Serialise request.response and return; inner service NOT called.
        //
        //   false → pass to application:
        //             M=0 (final block)  → payload replaced with reassembled body;
        //                                  Block1 ack option added to request.response
        //             Non-block request  → no changes
        let handled = {
            let mut h = block_handler.lock().expect("BlockHandler mutex poisoned");
            h.intercept_request(&mut request).unwrap_or(false)
        };

        if handled {
            let bytes = request
                .response
                .and_then(|r| r.message.to_bytes().ok())
                .unwrap_or_default();
            return Box::pin(async move { Ok(TransportMessage::new(peer_addr, bytes)) });
        }

        // ── 3. Forward to inner service ───────────────────────────────────────
        //
        // For a final Block1 request, intercept_request has:
        //   (a) spliced the fully-reassembled body into request.message.payload
        //   (b) pre-set the Block1 ack option in request.response
        //
        // Save the ack now — the inner service's response will overwrite
        // request.response, and we must re-attach it afterwards (RFC 7959 §2.3).
        let block1_ack = request.response.as_ref().and_then(|r| {
            r.message
                .get_first_option_as::<BlockValue>(CoapOption::Block1)
                .and_then(|x| x.ok())
        });

        // Re-serialise the (possibly payload-replaced) request so the inner
        // service receives the full body via the standard TransportMessage path.
        let Ok(fwd_bytes) = request.message.to_bytes() else {
            return Box::pin(self.inner.call(msg));
        };
        let inner_fut = self.inner.call(TransportMessage::new(peer_addr, fwd_bytes));

        Box::pin(async move {
            let resp_msg = inner_fut.await?;

            // ── 4. Post-intercept (after application logic) ───────────────────
            //
            // Replace request.response with the inner service's response, then
            // run intercept_response so the block handler can cache and fragment
            // oversized bodies (Block2), and finally re-attach the Block1 ack.

            let Ok(resp_packet) = Packet::from_bytes(&resp_msg.message_buf) else {
                return Ok(resp_msg);
            };
            let Some(r) = request.response.as_mut() else {
                return Ok(resp_msg);
            };
            r.message = resp_packet;

            {
                let mut h = block_handler.lock().expect("BlockHandler mutex poisoned");
                if let Err(e) = h.intercept_response(&mut request) {
                    eprintln!("BlockHandler intercept_response error: {e:?}");
                }
            }

            // RFC 7959 §2.3: echo the Block1 option so the client can verify
            // which block was acknowledged.
            if let (Some(ack), Some(r)) = (block1_ack, request.response.as_mut()) {
                r.message.add_option_as(CoapOption::Block1, ack);
            }

            match request.response.and_then(|r| r.message.to_bytes().ok()) {
                Some(bytes) => Ok(TransportMessage::new(resp_msg.peer_addr, bytes)),
                None => Ok(resp_msg),
            }
        })
    }
}

// ─── Layer ───────────────────────────────────────────────────────────────────

/// Tower [`Layer`] that wraps a service with [`BlockService`].
///
/// [`BlockHandlerConfig`] does not implement [`Clone`], so the config fields
/// are stored individually and a fresh `BlockHandlerConfig` is reconstructed
/// in [`Layer::layer`].  All clones of the produced service share a single
/// `Arc<Mutex<BlockHandler>>`.
///
/// # Example
///
/// ```rust,ignore
/// use crate::block::{BlockLayer, lwm2m_block_handler_config};
///
/// ServiceBuilder::new()
///     .layer(BlockLayer::new(lwm2m_block_handler_config()))
///     .layer(coap_cache_layer())
///     .layer(coap_coalesce_layer())
///     .layer(coap_retry_layer())
///     .service(MessageHandler::new())
/// ```
#[derive(Clone, Copy)]
pub struct BlockLayer {
    max_total_message_size: usize,
    cache_expiry_duration: std::time::Duration,
}

impl BlockLayer {
    /// Creates a new layer from a [`BlockHandlerConfig`].
    pub fn new(config: BlockHandlerConfig) -> Self {
        BlockLayer {
            max_total_message_size: config.max_total_message_size,
            cache_expiry_duration: config.cache_expiry_duration,
        }
    }
}

impl<S> Layer<S> for BlockLayer {
    type Service = BlockService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        BlockService::new(
            inner,
            BlockHandlerConfig {
                max_total_message_size: self.max_total_message_size,
                cache_expiry_duration: self.cache_expiry_duration,
            },
        )
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use coap_lite::block_handler::BlockValue;
    use coap_lite::{
        CoapOption, CoapRequest, MessageClass, MessageType, RequestType, ResponseType,
    };
    use std::sync::Mutex;
    use std::task::{Context, Poll};
    use std::time::Duration;
    use tower::Service;

    use crate::ServerError;
    use crate::transport::TransportMessage;

    // ── Mock inner service ────────────────────────────────────────────────────

    /// Simple inner service that returns `2.01 Created` for any POST request.
    /// Used to verify what the BlockService passes through to the application.
    #[derive(Clone, Default)]
    struct CreatedService;

    impl Service<TransportMessage> for CreatedService {
        type Response = TransportMessage;
        type Error = ServerError;
        type Future = Pin<Box<dyn Future<Output = Result<TransportMessage, ServerError>> + Send>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, msg: TransportMessage) -> Self::Future {
            let peer = msg.peer_addr.clone();
            Box::pin(async move {
                let packet =
                    Packet::from_bytes(&msg.message_buf).map_err(|_| ServerError::CoapParsing)?;
                let mut req = CoapRequest::<String>::from_packet(packet, peer.clone());
                if let Some(resp) = req.response.as_mut() {
                    resp.set_status(ResponseType::Created);
                    resp.message.clear_all_options();
                }
                let buf = req
                    .response
                    .and_then(|r| r.message.to_bytes().ok())
                    .unwrap_or_default();
                Ok(TransportMessage::new(peer, buf))
            })
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    /// Serialises a CoAP POST to /test as a raw [`TransportMessage`].
    fn make_post(payload: &[u8], block1: Option<BlockValue>, mid: u16) -> TransportMessage {
        let mut packet = Packet::new();
        packet.header.code = MessageClass::Request(RequestType::Post);
        packet.header.message_id = mid;
        packet.header.set_type(MessageType::Confirmable);
        // URI-Path: "test"
        packet.add_option(CoapOption::UriPath, b"test".to_vec());
        packet.payload = payload.to_vec();
        if let Some(b) = block1 {
            packet.add_option_as(CoapOption::Block1, b);
        }
        TransportMessage::new(
            "192.0.2.1:5683".to_string(),
            packet.to_bytes().expect("test packet must serialise"),
        )
    }

    fn make_service(max_msg_size: usize) -> BlockService<CreatedService> {
        BlockService::new(
            CreatedService,
            BlockHandlerConfig {
                max_total_message_size: max_msg_size,
                cache_expiry_duration: Duration::from_secs(120),
            },
        )
    }

    fn response_code(resp_buf: &[u8]) -> String {
        Packet::from_bytes(resp_buf)
            .expect("response must be parseable")
            .header
            .get_code()
            .to_string()
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    /// Non-block request passes straight through; inner service returns 2.01.
    #[tokio::test]
    async fn test_non_block_passthrough() {
        let mut svc = make_service(1152);
        let msg = make_post(b"</3>;ver=1.0", None, 1);
        let resp = svc.call(msg).await.unwrap();
        assert_eq!(response_code(&resp.message_buf), "2.01");
    }

    /// Oversized payload without a Block1 option: server must reply 4.13
    /// (RFC 7959 §2.9) and include a Block1 option hinting at a suitable size.
    #[tokio::test]
    async fn test_too_large_without_block1_returns_413() {
        // max_total_message_size = 32 B leaves only ~16 B for payload after
        // the CoAP header and options; the 64-byte payload must be rejected.
        let mut svc = make_service(32);
        let big_payload = b"A".repeat(64);
        let msg = make_post(&big_payload, None, 2);
        let resp = svc.call(msg).await.unwrap();
        assert_eq!(
            response_code(&resp.message_buf),
            "4.13",
            "oversized payload without Block1 must return 4.13"
        );
        // The response must contain a Block1 option indicating preferred size.
        let p = Packet::from_bytes(&resp.message_buf).unwrap();
        assert!(
            p.get_first_option_as::<BlockValue>(CoapOption::Block1)
                .and_then(|x| x.ok())
                .is_some(),
            "4.13 response must include a Block1 option"
        );
    }

    /// Intermediate Block1 block (M = 1): server must return 2.31 Continue
    /// (RFC 7959 §2.9) without invoking the inner service.
    #[tokio::test]
    async fn test_intermediate_block1_continue() {
        let mut svc = make_service(64);
        let blk = BlockValue::new(0, true, 16).unwrap(); // num=0, M=1, size=16
        let msg = make_post(b"0123456789abcdef", Some(blk), 3);
        let resp = svc.call(msg).await.unwrap();
        assert_eq!(
            response_code(&resp.message_buf),
            "2.31",
            "intermediate block must return 2.31 Continue"
        );
    }

    /// Full two-block upload sequence.
    ///
    /// Sends Block1 num=0 (M=1) followed by Block1 num=1 (M=0).  Verifies:
    ///   - First block receives `2.31 Continue`.
    ///   - Final block receives `2.01 Created` from the inner service.
    ///   - The final response carries the Block1 echo option (RFC 7959 §2.3).
    #[tokio::test]
    async fn test_two_block_upload_sequence() {
        let block_size = 16_usize;
        let mut svc = make_service(64);

        let part_a = b"0123456789abcdef"; // exactly block_size bytes
        let part_b = b"ghijklmnopqrstuv"; // exactly block_size bytes

        // ── Block 0 (intermediate) ────────────────────────────────────────────
        let blk0 = BlockValue::new(0, true, block_size).unwrap();
        let resp0 = svc.call(make_post(part_a, Some(blk0), 10)).await.unwrap();
        assert_eq!(
            response_code(&resp0.message_buf),
            "2.31",
            "first block must get 2.31 Continue"
        );

        // ── Block 1 (final) ───────────────────────────────────────────────────
        let blk1 = BlockValue::new(1, false, block_size).unwrap();
        let resp1 = svc.call(make_post(part_b, Some(blk1), 11)).await.unwrap();
        assert_eq!(
            response_code(&resp1.message_buf),
            "2.01",
            "final block must get 2.01 Created"
        );

        // RFC 7959 §2.3: Block1 echo option must be present in the final response.
        let p = Packet::from_bytes(&resp1.message_buf).unwrap();
        let ack = p
            .get_first_option_as::<BlockValue>(CoapOption::Block1)
            .expect("Block1 ack option must be present")
            .expect("Block1 ack must be parseable");
        assert_eq!(ack.num, 1, "ack must acknowledge block num 1");
        assert!(!ack.more, "ack M-bit must be 0 for the final block");
    }

    /// Three-block upload: tests that the reassembled payload seen by the inner
    /// service is the concatenation of all blocks in order.
    #[tokio::test]
    async fn test_three_block_payload_reassembly() {
        let block_size = 16_usize;

        // Capture the payload the inner service receives.
        #[derive(Clone, Default)]
        struct CaptureService(Arc<Mutex<Vec<u8>>>);

        impl Service<TransportMessage> for CaptureService {
            type Response = TransportMessage;
            type Error = ServerError;
            type Future =
                Pin<Box<dyn Future<Output = Result<TransportMessage, ServerError>> + Send>>;

            fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }

            fn call(&mut self, msg: TransportMessage) -> Self::Future {
                let captured = Arc::clone(&self.0);
                let peer = msg.peer_addr.clone();
                Box::pin(async move {
                    let packet = Packet::from_bytes(&msg.message_buf)
                        .map_err(|_| ServerError::CoapParsing)?;
                    *captured.lock().unwrap() = packet.payload.clone();
                    let req = CoapRequest::<String>::from_packet(packet, peer.clone());
                    let buf = req
                        .response
                        .and_then(|mut r| {
                            r.set_status(ResponseType::Created);
                            r.message.to_bytes().ok()
                        })
                        .unwrap_or_default();
                    Ok(TransportMessage::new(peer, buf))
                })
            }
        }

        let captured = Arc::new(Mutex::new(Vec::new()));
        let mut svc = BlockService::new(
            CaptureService(Arc::clone(&captured)),
            BlockHandlerConfig {
                max_total_message_size: 64,
                cache_expiry_duration: Duration::from_secs(120),
            },
        );

        let chunks: &[&[u8]] = &[
            b"0123456789abcdef",
            b"ghijklmnopqrstuv",
            b"wxyz012345678901",
        ];
        let expected: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();

        for (i, chunk) in chunks.iter().enumerate() {
            let is_last = i + 1 == chunks.len();
            let blk = BlockValue::new(i, !is_last, block_size).unwrap();
            let resp = svc
                .call(make_post(chunk, Some(blk), (i + 1) as u16))
                .await
                .unwrap();
            if is_last {
                assert_eq!(response_code(&resp.message_buf), "2.01");
            } else {
                assert_eq!(response_code(&resp.message_buf), "2.31");
            }
        }

        assert_eq!(
            *captured.lock().unwrap(),
            expected,
            "inner service must see the fully-reassembled payload"
        );
    }
}
