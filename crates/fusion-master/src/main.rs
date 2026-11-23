fn main() {
    if let Err(error) = run() {
        eprintln!("fusion-master: {error:#}");
        std::process::exit(1);
    }
}

#[tokio::main]
async fn run() -> anyhow::Result<()> {
    use clap::Parser;
    use fusion_master::MasterCommand;

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        command: MasterCommand,
    }

    fusion_master::run(Cli::parse().command).await
}
