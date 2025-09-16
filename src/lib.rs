mod clients;
mod transport;

use coap_lite::option_value::OptionValueString;
use coap_lite::CoapOption::LocationPath;
use coap_lite::{CoapRequest, CoapResponse, Packet, RequestType, ResponseType};
use rand::distr::Alphanumeric;
use rand::Rng;
use transport::{Transport, TransportMessage, UdpTransport};

#[derive(Debug)]
pub enum ServerError {
    CoapParsing,
    WrongPathOrMethod,
}

pub struct Lwm2mPacket<'a> {
    pub message: CoapRequest<String>,
    pub transport_message: &'a TransportMessage,
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

    pub async fn run(mut self) {
        loop {
            if let Ok(msg) = self.transport.receive().await {
                let response = self.handle_message(msg).await;
                match response {
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

    async fn handle_message(&self, msg: TransportMessage) -> Result<TransportMessage, ServerError> {
        if let Ok(packet) = Packet::from_bytes(&msg.message_buf[..]) {
            let lwm2m_packet = Lwm2mPacket {
                message: CoapRequest::from_packet(packet, msg.peer_addr.clone()),
                transport_message: &msg,
            };

            let path = lwm2m_packet.message.get_path().clone();
            let method = lwm2m_packet.message.get_method();
            let response = match (path.as_str(), method) {
                ("rd", RequestType::Post) => self.handle_registration(lwm2m_packet).await,
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

    fn generate_registration_id(&self) -> String {
        rand::rng()
            .sample_iter(&Alphanumeric)
            .take(10)
            .map(char::from)
            .collect()
    }

    async fn handle_registration(&self, packet: Lwm2mPacket<'_>) -> Option<CoapResponse> {
        if let Some(mut response) = packet.message.response {
            let reg_id = self.generate_registration_id();
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::tests::InMemoryTransport;
    use coap_lite::link_format::{
        LinkFormatWrite, LINK_ATTR_CONTENT_FORMAT, LINK_ATTR_RESOURCE_TYPE,
    };
    use coap_lite::option_value::OptionValueString;
    use coap_lite::ContentFormat::ApplicationSenmlCBOR;
    use coap_lite::MessageClass::Response;
    use coap_lite::ResponseType::Created;
    use coap_lite::{CoapOption, CoapRequest, MessageType, RequestType};
    use regex::Regex;
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::rc::Rc;
    use std::sync::Mutex;
    use tokio::net::unix::SocketAddr;
    use tokio::sync::mpsc::{Receiver, Sender};
    use tokio::task::JoinHandle;

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
        server: Rc<RefCell<Lwm2mServer>>,
        _server_join_handle: JoinHandle<()>,
        clients: Vec<TestClient>,
    }

    async fn spawn_server_for_tests(num_clients: u8) -> TestClientsAndServer {
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

        let server = Rc::new(RefCell::new(
            Lwm2mServer::new_from_transport(transport).await,
        ));

        TestClientsAndServer {
            server: server.clone(),
            _server_join_handle: tokio::spawn(async move {
                let s = server.clone().borrow_mut();
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

        let mut buffer = String::new();
        let mut write = LinkFormatWrite::new(&mut buffer);

        let ct: usize = ApplicationSenmlCBOR.into();
        write
            .link("/")
            .attr_quoted(LINK_ATTR_RESOURCE_TYPE, "oma.lwm2m")
            .attr(LINK_ATTR_CONTENT_FORMAT, &ct.to_string());
        write.link("/1/1");
        write.link("/3").attr("ver", "1.0");
        write.link("/3/0");
        write.link("/5").attr("ver", "1.0");
        write.link("/5/0");
        write.finish().unwrap();

        // Workaround: dot separated version numbers are escaped in `coap-lite`
        let r = Regex::new(r#"ver="([0-9.]+)""#).unwrap();
        let link = r.replace_all(buffer.as_str(), "ver=$1").to_string();

        request.message.payload = link.into_bytes();

        request
    }
    #[tokio::test]
    async fn test_registration_msg() -> std::io::Result<()> {
        let client_and_server = spawn_server_for_tests(1);

        let req = create_reg_message_for_tests();
        let req = req.message.to_bytes().unwrap();
        let test_client = &mut client_and_server.await.clients[0];
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
        let client_and_server = spawn_server_for_tests(1);

        let mut req: CoapRequest<SocketAddr> = CoapRequest::new();

        req.set_method(RequestType::Post);
        req.message.header.set_type(MessageType::Confirmable);

        req.set_path("/wrong_url");

        let req = req.message.to_bytes().unwrap();
        let test_client = &mut client_and_server.await.clients[0];
        test_client.send_to_server(req).await;
        let resp = &test_client.from_server_receiver.recv().await.unwrap();
        let resp = Packet::from_bytes(&resp.message_buf).unwrap();
        assert_eq!(resp.header.get_code(), "5.00");

        Ok(())
    }

    #[tokio::test]
    async fn test_registration_msg_2_clients() -> std::io::Result<()> {
        let mut client_and_server = spawn_server_for_tests(2).await;

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
}
