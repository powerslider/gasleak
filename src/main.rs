use clap::Parser;

use gasleak::cli::Cli;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    gasleak::init_tracing(cli.verbose);
    let code = gasleak::run(cli).await?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}
