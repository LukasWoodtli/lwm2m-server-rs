use coap::request::Method;
use coap::{Server, UdpCoAPClient};
use coap_lite::CoapRequest;
use std::net::SocketAddr;

pub(crate) async fn run_coap_server() {
    let addr = "[::1]:5683";
    let server = Server::new_udp(addr).unwrap();

    server
        .run(|mut request: Box<CoapRequest<SocketAddr>>| async {
            let msg: String = match request.get_method() {
                &Method::Get => format!("request by GET /{}", request.get_path()),
                _ => "request by other method".to_string(),
            };

            if let Some(ref mut message) = request.response {
                message.message.payload = format!("OK for request on {}", msg).as_bytes().to_vec();
            }

            request
        })
        .await
        .unwrap();
}

#[allow(dead_code)]
async fn run_coap_client() {
    let url = "coap://[::1]:5690/1";
    println!("Client request: {}", url);

    let response = UdpCoAPClient::get(url).await.unwrap();
    println!(
        "Server reply: {}",
        String::from_utf8(response.message.payload).unwrap()
    );
}

#[cfg(test)]
mod tests {
    use super::{run_coap_client, run_coap_server};
    use coap::request::Method;
    use coap::{Server, UdpCoAPClient};
    use coap_lite::CoapRequest;
    use std::net::SocketAddr;

    async fn run_test_coap_server() {
        tokio::spawn(async {
            let addr = "[::1]:5690";
            let server = Server::new_udp(addr).unwrap();

            server
                .run(|mut request: Box<CoapRequest<SocketAddr>>| async {
                    assert_eq!(request.get_method(), &Method::Get);
                    assert_eq!(request.get_path(), "1");

                    assert!(&request.response.is_some());
                    let r = &mut request.response.as_mut().unwrap();
                    r.message.payload = "OK for GET on /1".as_bytes().to_vec();
                    request
                })
                .await
                .unwrap();
        });
    }

    #[tokio::test]
    async fn test_coap_server() {
        tokio::spawn(async { run_coap_server().await });

        let url = "coap://[::1]:5683/bs";
        println!("Client request: {}", url);

        let response = UdpCoAPClient::get(url).await.unwrap();
        assert_eq!(
            String::from_utf8(response.message.payload).unwrap(),
            "OK for request on request by GET /bs"
        );
    }

    #[tokio::test]
    async fn test_coap_client() {
        run_test_coap_server().await;
        run_coap_client().await;
    }
}
