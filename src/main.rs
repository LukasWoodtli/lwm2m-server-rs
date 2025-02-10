mod server;

use crate::server::run_coap_server;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    run_coap_server().await;
}
