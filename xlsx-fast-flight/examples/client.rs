use anyhow::{anyhow, Result};
use arrow_flight::flight_service_client::FlightServiceClient;
use arrow_flight::FlightDescriptor;
use futures::StreamExt;
use tonic::transport::Channel;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let endpoint = args
        .next()
        .unwrap_or_else(|| "http://127.0.0.1:50051".to_string());
    let path = args.next().unwrap_or_else(|| "sample.xlsx".to_string());

    let descriptor = FlightDescriptor::new_path(vec![path]);

    let channel = Channel::from_shared(endpoint)?.connect().await?;
    let mut client = FlightServiceClient::new(channel);
    let info = client.get_flight_info(descriptor).await?.into_inner();
    let ticket = info
        .endpoint
        .first()
        .and_then(|ep| ep.ticket.clone())
        .ok_or_else(|| anyhow!("flight info missing ticket"))?;

    let mut stream = client.do_get(ticket).await?.into_inner();
    let mut messages = 0usize;
    while let Some(_data) = stream.next().await.transpose()? {
        messages += 1;
    }

    println!("received {} FlightData messages", messages);
    Ok(())
}
