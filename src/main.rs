mod accessibility;
mod android_accessibility;
mod android_grpc;
mod cli;
mod frame_cache;
mod install_mcp;
mod ios;
mod ios_device;
mod ios_lifecycle;
mod mcp;
mod observation;
mod platform;
mod process;
mod session;
mod visual_wait;

use clap::Parser;
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args_os().len() > 1 {
        let cli = cli::Cli::parse();
        match cli.command {
            cli::CliCommand::InstallMcp(arguments) => install_mcp::run(arguments)?,
        }
        return Ok(());
    }

    // Dependencies may trace full MCP messages, images or HTTP metadata. Only
    // our redacted metrics are eligible for diagnostics, even with RUST_LOG=trace.
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .with_filter(tracing_subscriber::filter::filter_fn(|metadata| {
                    metadata.target().starts_with("device_simulator_mcp")
                })),
        )
        .init();

    let server = mcp::DeviceSimulatorMcp::default();
    let service = server.clone().serve(stdio()).await?;
    let result = service.waiting().await;
    server.shutdown().await;
    result?;
    Ok(())
}
