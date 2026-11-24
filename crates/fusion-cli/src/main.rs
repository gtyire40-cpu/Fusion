use anyhow::Result;
use clap::{Parser, Subcommand};
use fusion_master::MasterCommand;
use fusion_worker::WorkerCommand;

#[derive(Debug, Parser)]
#[command(name = "fusion", about = "Distributed computing for Linux")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run Master operations.
    Master {
        #[command(subcommand)]
        command: MasterCommand,
    },
    /// Run Worker operations.
    Worker {
        #[command(subcommand)]
        command: WorkerCommand,
    },
    /// Inspect cluster nodes.
    Nodes {
        #[command(subcommand)]
        command: NodesCommand,
    },
}

#[derive(Debug, Subcommand)]
enum NodesCommand {
    /// List paired workers and their current status.
    List {
        #[arg(long)]
        data_dir: Option<std::path::PathBuf>,
    },
    /// Show hardware capabilities for a paired worker.
    Show {
        node_id: String,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        data_dir: Option<std::path::PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Master { command } => fusion_master::run(command).await,
        Command::Worker { command } => fusion_worker::run(command).await,
        Command::Nodes {
            command: NodesCommand::List { data_dir },
        } => fusion_master::list_nodes(&fusion_master::resolve_data_dir(data_dir)?),
        Command::Nodes {
            command:
                NodesCommand::Show {
                    node_id,
                    json,
                    data_dir,
                },
        } => fusion_master::show_node(&fusion_master::resolve_data_dir(data_dir)?, &node_id, json),
    }
}
