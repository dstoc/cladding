use mcp_run::{AppConfig, serve};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_target(true).init();

    let config = AppConfig::from_env()?;
    serve(config).await?;
    Ok(())
}
