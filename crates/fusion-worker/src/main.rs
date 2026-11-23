fn main() {
    if let Err(error) = run() {
        eprintln!("fusion-worker: {error:#}");
        std::process::exit(1);
    }
}

#[tokio::main]
async fn run() -> anyhow::Result<()> {
    use clap::Parser;
    use fusion_worker::WorkerCommand;

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        command: WorkerCommand,
    }

    fusion_worker::run(Cli::parse().command).await
}
