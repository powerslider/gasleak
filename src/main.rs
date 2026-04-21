use clap::Parser;

use gasleak::cli::Cli;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    gasleak::init_tracing(cli.verbose);
    gasleak::run(cli).await?;
    Ok(())
}
