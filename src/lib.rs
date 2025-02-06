use std::io::Error;
use std::net::SocketAddr;
use tokio::net::UdpSocket;

pub struct Lwm2mServer {
    socket: UdpSocket,
}

impl Lwm2mServer {
    pub async fn new(address: &str) -> Result<Self, Error> {
        let socket = UdpSocket::bind(address).await?;

        Ok(Lwm2mServer { socket })
    }
    pub async fn run(self) -> std::io::Result<()> {
        loop {
            let (addr, buf) = self.receive().await?;

            let mut msg = String::from_utf8_lossy(&buf);
            msg += " sent back";

            let len = self.socket.send_to(msg.as_bytes(), addr).await?;
            println!("Sent {} bytes", len);
        }
    }

    async fn receive(&self) -> Result<(SocketAddr, Vec<u8>), Error> {
        let mut buf = vec![0; 1024];
        let (len, addr) = self.socket.recv_from(&mut buf).await?;
        println!("Received {} bytes from {:?}", len, addr);
        let buf = Vec::from(&buf[..len]);
        Ok((addr, buf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UdpSocket;

    #[tokio::test]
    async fn run_test_connect_coap_server() -> std::io::Result<()> {
        let server_addr = "[::1]:5683";

        let _task = tokio::spawn(async move {
            let s = Lwm2mServer::new(server_addr).await.unwrap();
            s.run().await.unwrap();
        });

        let client_add = "[::1]:5690";
        let client_socket = UdpSocket::bind(client_add).await?;

        client_socket.connect(server_addr).await?;

        let msg = "Hello, world!".as_bytes();
        let len = client_socket.send(msg).await?;
        assert_eq!(len, 13);

        let mut buf = vec![0; 1024];
        let len = client_socket.recv(&mut buf).await?;
        let received_msg = String::from_utf8_lossy(&buf[..len]);
        assert_eq!(received_msg, "Hello, world! sent back");
        Ok(())
    }
}
