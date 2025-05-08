use async_trait::async_trait;
use std::io::Error;
use tokio::net::UdpSocket;

pub struct TransportMessage {
    pub peer_addr: String,
    pub message_buf: Vec<u8>,
}

impl TransportMessage {
    pub(crate) fn new(peer_addr: String, message_buf: Vec<u8>) -> Self {
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
    pub(crate) async fn from_address(address: &str) -> Result<Self, TransportError> {
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use tokio::sync::mpsc::{Receiver, Sender};

    pub(crate) struct InMemoryTransport {
        to_server: Receiver<TransportMessage>,
        clients: HashMap<String, Sender<TransportMessage>>,
    }

    impl InMemoryTransport {
        pub(crate) fn new(to_server: Receiver<TransportMessage>) -> Self {
            InMemoryTransport {
                to_server,
                clients: HashMap::new(),
            }
        }

        pub(crate) fn add_client(&mut self, address: &str, to_client: Sender<TransportMessage>) {
            self.clients.insert(address.to_string(), to_client);
        }
    }

    #[async_trait]
    impl Transport for InMemoryTransport {
        async fn send(&mut self, msg: TransportMessage) -> Result<(), Error> {
            let client = self.clients.get(&msg.peer_addr).unwrap();
            client.send(msg).await.unwrap();
            Ok(())
        }

        async fn receive(&mut self) -> Result<TransportMessage, Error> {
            Ok(self.to_server.recv().await.unwrap())
        }
    }

    #[tokio::test]
    async fn test_bind_error() {
        let server_address = "invalid_address";
        let res = UdpTransport::from_address(server_address).await;
        assert!(res.is_err());
        assert!(matches!(res, Err(TransportError::SetupError)));
    }
}
