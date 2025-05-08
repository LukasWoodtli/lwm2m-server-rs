use async_trait::async_trait;
use std::io::Error;
use tokio::net::UdpSocket;

pub struct TransportMessage {
    pub peer_addr: String,
    pub message_buf: Vec<u8>,
}

impl TransportMessage {
    pub fn new(peer_addr: String, message_buf: Vec<u8>) -> Self {
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
    pub async fn from_address(address: &str) -> Result<Self, TransportError> {
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
    use tokio_bichannel::Channel;

    pub(crate) struct InMemoryTransport {
        channel: Channel<Vec<u8>, Vec<u8>>,
    }

    impl InMemoryTransport {
        pub(crate) fn new(channel: Channel<Vec<u8>, Vec<u8>>) -> Self {
            InMemoryTransport { channel }
        }
    }

    #[async_trait]
    impl Transport for InMemoryTransport {
        async fn send(&mut self, msg: TransportMessage) -> Result<(), Error> {
            self.channel.send(msg.message_buf).await.unwrap();
            Ok(())
        }

        async fn receive(&mut self) -> Result<TransportMessage, Error> {
            let buf = self.channel.recv().await.unwrap();
            Ok(TransportMessage::new("127.0.0.1".to_string(), buf))
        }
    }
}
