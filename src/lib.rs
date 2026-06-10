mod transport;
use coap_lite::option_value::OptionValueString;
use coap_lite::CoapOption::LocationPath;
use coap_lite::{CoapRequest, CoapResponse, Packet, RequestType, ResponseType};
use rand::distr::Alphanumeric;
use rand::Rng;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tower::{Service, ServiceBuilder};
use transport::{Transport, TransportMessage, UdpTransport};

/// Errors that can occur during server operations.
#[derive(Debug, Clone)]
pub enum ServerError {
    CoapParsing,
    WrongPathOrMethod,
}

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

/// An LwM2M server that handles device bootstrap, registration and management.
///
/// The server listens for incoming CoAP messages over a configured transport
/// and responds according to the LwM2M protocol specification.
pub struct Lwm2mServer {
    transport: Box<dyn Transport>,
    message_handler: MessageHandler,
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
            message_handler: ServiceBuilder::new().service(MessageHandler::new()),
        }
    }

    /// Starts the server's main event loop.
    ///
    /// This method runs indefinitely, receiving messages from the transport,
    /// processing them through the service stack, and sending responses back to clients.
    /// Errors during message handling are logged to stderr.
    pub async fn run(mut self) {
        loop {
            if let Ok(msg) = self.transport.receive().await {
                match self.message_handler.call(msg).await {
                    Ok(response) => {
                        self.transport.send(response).await.unwrap_or_else(|e| {
                            eprintln!("Error sending response: {e:?}");
                        });
                    }
                    Err(e) => {
                        eprintln!("Error handling message: {e:?}");
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
    use coap_lite::option_value::OptionValueString;
    use coap_lite::MessageClass::Response;
    use coap_lite::ResponseType::Created;
    use coap_lite::{CoapOption, CoapRequest, MessageType, RequestType};
    use std::collections::HashSet;
    use tokio::net::unix::SocketAddr;
    use tokio::sync::mpsc::{Receiver, Sender};
    use tokio::task::JoinHandle;
    use tower::Service;

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
}
