use async_trait::async_trait;
use coap_lite::option_value::OptionValueString;
use coap_lite::CoapOption::LocationPath;
use coap_lite::{CoapRequest, Packet, ResponseType};
use std::io::Error;
use tokio::net::UdpSocket;

pub struct TransportMessage {
    peer_addr: String,
    message_buf: Vec<u8>,
}

impl TransportMessage {
    fn new(peer_addr: String, message_buf: Vec<u8>) -> Self {
        TransportMessage {
            peer_addr,
            message_buf,
        }
    }
}

#[derive(Debug)]
pub enum TransportError {
    SetupError,
}

#[async_trait]
pub trait Transport: Send + Sync {
    async fn send(&mut self, msg: TransportMessage) -> Result<(), Error>;
    async fn receive(&mut self) -> Result<TransportMessage, Error>;
}

pub struct UdpTransport {
    socket: UdpSocket,
}

impl UdpTransport {
    async fn from_address(address: &str) -> Result<Self, TransportError> {
        let socket = UdpSocket::bind(address).await;
        match socket {
            Ok(socket) => Ok(UdpTransport { socket }),
            Err(_) => Err(TransportError::SetupError),
        }
    }
}

#[async_trait]
impl Transport for UdpTransport {
    async fn send(&mut self, msg: TransportMessage) -> Result<(), Error> {
        let _len = self
            .socket
            .send_to(&msg.message_buf[..], msg.peer_addr)
            .await?;
        Ok(())
    }

    async fn receive(&mut self) -> Result<TransportMessage, Error> {
        let mut buf = vec![0; 1024];
        let (len, addr) = self.socket.recv_from(&mut buf).await?;
        Ok(TransportMessage::new(
            addr.to_string(),
            Vec::from(&buf[..len]),
        ))
    }
}

pub struct Lwm2mServer {
    transport: Box<dyn Transport>,
}

impl Lwm2mServer {
    pub async fn new_udp(address: &str) -> Self {
        let transport = Box::new(
            UdpTransport::from_address(address)
                .await
                .expect("Failed to initialize transport"),
        );

        Lwm2mServer { transport }
    }

    pub async fn new_from_transport(transport: Box<dyn Transport>) -> Self {
        Lwm2mServer { transport }
    }
    pub async fn run(mut self) -> std::io::Result<()> {
        loop {
            let msg = self.transport.receive().await?;

            let response = self.handle_message(msg).await?;

            self.transport.send(response).await?;
        }
    }

    async fn handle_message(&self, msg: TransportMessage) -> Result<TransportMessage, Error> {
        if let Ok(packet) = Packet::from_bytes(&msg.message_buf[..]) {
            let request = CoapRequest::from_packet(packet, msg.peer_addr.clone());

            if let Some(mut response) = request.response {
                response.set_status(ResponseType::Created);
                response.message.clear_all_options();
                response.message.add_option(LocationPath, b"rd".to_vec());
                response
                    .message
                    .add_option_as(LocationPath, OptionValueString("01234".to_owned()));

                if let Ok(buffer) = response.message.to_bytes() {
                    return Ok(TransportMessage::new(msg.peer_addr.clone(), buffer));
                } else {
                    todo!()
                }
            }
        }
        todo!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coap_lite::option_value::OptionValueString;
    use coap_lite::MessageClass::Response;
    use coap_lite::ResponseType::Created;
    use coap_lite::{CoapOption, CoapRequest, MessageType, RequestType};
    use tokio::net::unix::SocketAddr;
    use tokio::sync::mpsc::{Receiver, Sender};
    use tokio::task::JoinHandle;

    struct InMemoryTransport {
        to_server: Receiver<TransportMessage>,
        from_server: Sender<TransportMessage>,
    }

    impl InMemoryTransport {
        fn new(
            to_server: Receiver<TransportMessage>,
            from_server: Sender<TransportMessage>,
        ) -> Self {
            InMemoryTransport {
                to_server,
                from_server,
            }
        }
    }

    #[async_trait]
    impl Transport for InMemoryTransport {
        async fn send(&mut self, msg: TransportMessage) -> Result<(), Error> {
            self.from_server.send(msg).await.unwrap();
            Ok(())
        }

        async fn receive(&mut self) -> Result<TransportMessage, Error> {
            Ok(self.to_server.recv().await.unwrap())
        }
    }

    struct TestClient {
        to_server_sender: Sender<TransportMessage>,
        from_server_receiver: Receiver<TransportMessage>,
    }

    struct TestClientsAndServer {
        _server_join_handle: JoinHandle<()>,
        clients: Vec<TestClient>,
    }

    fn spawn_server_for_tests() -> TestClientsAndServer {
        let (to_server_sender, to_server_receiver) = tokio::sync::mpsc::channel(1);
        let (from_server_sender, from_server_receiver) = tokio::sync::mpsc::channel(1);

        let client = TestClient {
            to_server_sender,
            from_server_receiver,
        };

        let transport = Box::new(InMemoryTransport::new(
            to_server_receiver,
            from_server_sender,
        ));

        TestClientsAndServer {
            _server_join_handle: tokio::spawn(async move {
                let s: Lwm2mServer = Lwm2mServer::new_from_transport(transport).await;
                s.run().await.unwrap();
            }),
            clients: vec![client],
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
        let mut client_and_server = spawn_server_for_tests();

        let req = create_reg_message_for_tests();
        let req = req.message.to_bytes().unwrap();
        let req = TransportMessage::new("127.0.0.1".to_string(), req);

        let test_client = &mut client_and_server.clients[0];
        test_client.to_server_sender.send(req).await.unwrap();
        let resp = &test_client.from_server_receiver.recv().await.unwrap();
        let resp = Packet::from_bytes(&resp.message_buf).unwrap();

        assert_eq!(resp.header.get_type(), MessageType::Acknowledgement);
        assert_eq!(resp.header.code, Response(Created));
        assert!(resp.payload.is_empty());

        let values = ["rd", "01234"];

        let expected = values
            .iter()
            .map(|&x| Ok(OptionValueString(x.to_owned())))
            .collect();
        let actual = resp.get_options_as::<OptionValueString>(LocationPath);
        assert_eq!(actual, Some(expected));

        Ok(())
    }
}
