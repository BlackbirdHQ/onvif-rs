extern crate onvif;
use onvif::discovery;
use std::str::FromStr;

#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();
    tracing_subscriber::fmt::init();

    use futures_util::stream::StreamExt;
    const MAX_CONCURRENT_JUMPERS: usize = 100;

    discovery::DiscoveryBuilder::default().listen_address(std::net::IpAddr::from_str("192.168.254.1").unwrap())
        .run()
        .await
        .unwrap()
        .for_each_concurrent(MAX_CONCURRENT_JUMPERS, |addr| async move {
            println!("Device found: {:?}", addr);
        })
        .await;
}
