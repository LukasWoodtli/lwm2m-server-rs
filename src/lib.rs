mod transport;
use coap_lite::option_value::OptionValueString;
use coap_lite::CoapOption::LocationPath;
use coap_lite::{CoapRequest, Packet, ResponseType};
use std::io::Error;
use transport::{Transport, TransportMessage, UdpTransport};

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

            let reg_id = format!("regid_{}", msg.peer_addr.chars().last().unwrap());

            if let Some(mut response) = request.response {
                response.set_status(ResponseType::Created);
                response.message.clear_all_options();
                response.message.add_option(LocationPath, b"rd".to_vec());
                response
                    .message
                    .add_option_as(LocationPath, OptionValueString(reg_id));

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
    use crate::transport::tests::InMemoryTransport;
    use coap_lite::option_value::OptionValueString;
    use coap_lite::MessageClass::Response;
    use coap_lite::ResponseType::Created;
    use coap_lite::{CoapOption, CoapRequest, MessageType, RequestType};
    use std::collections::HashSet;
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
                s.run().await.unwrap();
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

        let values = ["rd", "regid_1"];

        let expected = values
            .iter()
            .map(|&x| Ok(OptionValueString(x.to_owned())))
            .collect();
        let actual = resp.get_options_as::<OptionValueString>(LocationPath);
        assert_eq!(actual, Some(expected));

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
}
