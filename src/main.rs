mod server;

use crate::server::Lwm2mServer;
use std::error::Error;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let s = Lwm2mServer::new()?;
    let _ = s.run_coap_server().await;
    Ok(())
}
