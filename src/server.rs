use async_trait::async_trait;
use coap::request::Method;
use coap::server::{Listener, Responder, TransportRequestSender};
use coap::{Server, UdpCoAPClient};
use coap_lite::CoapRequest;
use log::debug;
use std::io::{Error, ErrorKind};
use std::net;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::select;
use tokio::sync::mpsc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

type UdpResponseReceiver = UnboundedReceiver<(Vec<u8>, SocketAddr)>;
type UdpResponseSender = UnboundedSender<(Vec<u8>, SocketAddr)>;

#[derive(Clone)]
struct UdpResponder {
    address: SocketAddr, // this is the address we are sending to
    tx: UdpResponseSender,
}

#[async_trait]
impl Responder for UdpResponder {
    async fn respond(&self, response: Vec<u8>) {
        let _ = self.tx.send((response, self.address));
    }
    fn address(&self) -> SocketAddr {
        self.address
    }
}

struct Lwm2mServerListener {
    socket: UdpSocket,
    response_receiver: UdpResponseReceiver,
    response_sender: UdpResponseSender,
}

impl Lwm2mServerListener {
    pub fn new<A: ToSocketAddrs>(addr: A) -> Result<Self, Error> {
        let std_socket = net::UdpSocket::bind(addr)?;
        std_socket.set_nonblocking(true)?;
        let socket = UdpSocket::from_std(std_socket)?;

        let (tx, rx) = mpsc::unbounded_channel();

        Ok(Self {
            socket,
            response_receiver: rx,
            response_sender: tx,
        })
    }

    pub async fn receive_loop(mut self, sender: TransportRequestSender) -> std::io::Result<()> {
        loop {
            let mut recv_vec = Vec::with_capacity(u16::MAX as usize);
            select! {
                message =self.socket.recv_buf_from(&mut recv_vec)=> {
                    match message {
                        Ok((_size, from)) => {
                            sender.send((recv_vec, Arc::new(UdpResponder{address: from, tx: self.response_sender.clone()}))).map_err( |_| std::io::Error::new(ErrorKind::Other, "server channel error"))?;
                        }
                        Err(e) => {
                            return Err(e);
                        }
                    }
                },
                response = self.response_receiver.recv() => {
                    if let Some((bytes, to)) = response{
                        debug!("sending {:?} to {:?}", &bytes,  &to);
                        self.socket.send_to(&bytes, to).await?;
                    }
                    else {
                        // in case nobody is listening to us, we can just terminate, though this
                        // should never happen for UDP
                        return Ok(());
                    }

                }
            }
        }
    }
}

#[async_trait]
impl Listener for Lwm2mServerListener {
    async fn listen(
        mut self: Box<Self>,
        sender: TransportRequestSender,
    ) -> std::io::Result<JoinHandle<std::io::Result<()>>> {
        return Ok(tokio::spawn(self.receive_loop(sender)));
    }
}

pub struct Lwm2mServer {
    coap_server: Server,
}

impl Lwm2mServer {
    pub fn new() -> Result<Self, Error> {
        let addr = "[::1]:5683";
        let listener = Box::new(Lwm2mServerListener::new(addr)?);
        let server = Server::from_listeners(vec![listener]);
        Ok(Lwm2mServer {
            coap_server: server,
        })
    }
    pub(crate) async fn run_coap_server(self) -> std::io::Result<()> {
        self.coap_server
            .run(|mut request: Box<CoapRequest<SocketAddr>>| async {
                let msg: String = match request.get_method() {
                    &Method::Get => format!("request by GET /{}", request.get_path()),
                    _ => "request by other method".to_string(),
                };

                if let Some(ref mut message) = request.response {
                    message.message.payload =
                        format!("OK for request on {}", msg).as_bytes().to_vec();
                };
                request
            })
            .await?;
        Ok(())
    }
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
    use crate::server::{run_coap_client, Lwm2mServer};
    use coap::request::Method;
    use coap::{Server, UdpCoAPClient};
    use coap_lite::CoapRequest;
    use std::io::Error;
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
    async fn test_coap_server() -> Result<(), Error> {
        let s = Lwm2mServer::new()?;
        tokio::spawn(async { s.run_coap_server().await });

        let url = "coap://[::1]:5683/bs";
        println!("Client request: {}", url);

        let response = UdpCoAPClient::get(url).await.unwrap();
        assert_eq!(
            String::from_utf8(response.message.payload).unwrap(),
            "OK for request on request by GET /bs"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_coap_client() {
        run_test_coap_server().await;
        run_coap_client().await;
    }
}
