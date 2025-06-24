use lwm2m_server_rs::Lwm2mServer;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let address = "[::1]:5683";
    let s: Lwm2mServer = Lwm2mServer::new_udp(address).await;
    s.run().await.unwrap();
}
