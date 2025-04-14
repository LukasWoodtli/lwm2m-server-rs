use coap_lite::option_value::OptionValueString;
use coap_lite::CoapOption::LocationPath;
use coap_lite::{CoapRequest, Packet, ResponseType};
use std::io::Error;
use std::net::SocketAddr;
use tokio::net::{ToSocketAddrs, UdpSocket};

pub struct TransportMessage {
    peer_addr: SocketAddr,
    message_buf: Vec<u8>,
}

impl TransportMessage {
    fn new(peer_addr: SocketAddr, message_buf: Vec<u8>) -> Self {
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

trait Transport {
    async fn send(&self, msg: TransportMessage) -> Result<(), Error>;
    async fn receive(&self) -> Result<TransportMessage, Error>;
}

struct UdpTransport {
    socket: UdpSocket,
}

impl UdpTransport {
    async fn from_address<A: ToSocketAddrs>(address: A) -> Result<Self, TransportError> {
        let socket = UdpSocket::bind(address).await;
        match socket {
            Ok(socket) => Ok(UdpTransport { socket }),
            Err(_) => Err(TransportError::SetupError),
        }
    }
}

impl Transport for UdpTransport {
    async fn send(&self, msg: TransportMessage) -> Result<(), Error> {
        let len = self
            .socket
            .send_to(&msg.message_buf[..], msg.peer_addr)
            .await?;
        println!("Sent {} bytes", len);
        Ok(())
    }

    async fn receive(&self) -> Result<TransportMessage, Error> {
        let mut buf = vec![0; 1024];
        let (len, addr) = self.socket.recv_from(&mut buf).await?;
        println!("Received {} bytes from {:?}", len, addr);
        Ok(TransportMessage::new(addr, Vec::from(&buf[..len])))
    }
}

pub struct Lwm2mServer {
    transport: UdpTransport,
}

impl Lwm2mServer {
    pub async fn new(address: &str) -> Self {
        let transport = UdpTransport::from_address(address)
            .await
            .expect("Failed to initialize transport");

        Lwm2mServer { transport }
    }
    pub async fn run(self) -> std::io::Result<()> {
        loop {
            let msg = self.transport.receive().await?;

            let response = self.handle_message(msg).await?;

            self.transport.send(response).await?;
        }
    }

    async fn handle_message(&self, msg: TransportMessage) -> Result<TransportMessage, Error> {
        if let Ok(packet) = Packet::from_bytes(&msg.message_buf[..]) {
            let request = CoapRequest::from_packet(packet, msg.peer_addr);

            if let Some(mut response) = request.response {
                response.set_status(ResponseType::Created);
                response.message.clear_all_options();
                response.message.add_option(LocationPath, b"rd".to_vec());
                response
                    .message
                    .add_option_as(LocationPath, OptionValueString("01234".to_owned()));

                if let Ok(buffer) = response.message.to_bytes() {
                    return Ok(TransportMessage::new(msg.peer_addr, buffer));
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
    use tokio::net::UdpSocket;
    use tokio::task::JoinHandle;

    fn spawn_server_for_tests(server_address: &'static str) -> JoinHandle<()> {
        tokio::spawn(async move {
            let s = Lwm2mServer::new(server_address).await;
            s.run().await.unwrap();
        })
    }

    async fn run_test_client(server_address: &str, req: &[u8]) -> Result<Packet, Error> {
        let client_address = "[::1]:5690";
        let client_socket = UdpSocket::bind(client_address).await?;
        client_socket.connect(server_address).await?;

        let _len = client_socket.send(req).await?;

        let mut buf = vec![0; 1024];
        let len = client_socket.recv(&mut buf).await?;
        let resp = Packet::from_bytes(&buf[..len]).unwrap();
        Ok(resp)
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
        let server_address = "[::1]:5683";

        let _server = spawn_server_for_tests(server_address);

        let req = create_reg_message_for_tests();
        let req = req.message.to_bytes().unwrap();

        let resp = run_test_client(server_address, &req).await?;

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
