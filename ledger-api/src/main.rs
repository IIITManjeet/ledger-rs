#[tokio::main]
async fn main() -> anyhow::Result<()> {
    ledger_api::run().await
}
