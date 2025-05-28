use coap_lite::option_value::OptionValueString;
use coap_lite::CoapOption::LocationPath;
use coap_lite::MessageClass::Response;
use coap_lite::ResponseType::Created;
use coap_lite::{CoapOption, CoapRequest, MessageType, Packet, RequestType};
use lwm2m_server_rs::Lwm2mServer;
use std::io::Error;
use std::net::SocketAddr;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

fn spawn_server_for_tests(server_address: &'static str) -> JoinHandle<()> {
    tokio::spawn(async move {
        let s: Lwm2mServer = Lwm2mServer::new_udp(server_address).await;
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

    let values = ["rd", "regid_0"];

    let expected = values
        .iter()
        .map(|&x| Ok(OptionValueString(x.to_owned())))
        .collect();
    let actual = resp.get_options_as::<OptionValueString>(LocationPath);
    assert_eq!(actual, Some(expected));

    Ok(())
}
