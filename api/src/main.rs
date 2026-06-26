#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    nano_bank_api::run_server().await
}