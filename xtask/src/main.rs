mod codegen;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask")]
#[command(about = "Build/task runner for pacto-bot-api")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run schema/code generation tasks.
    Codegen,
    /// Run the full verification suite (placeholder).
    FullCheck,
    /// Probe external dev-env services (placeholder).
    DevEnvProbe,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Codegen => codegen::run(),
        Command::FullCheck => {
            println!("full-check: not yet implemented");
            Ok(())
        }
        Command::DevEnvProbe => {
            println!("dev-env-probe: not yet implemented");
            Ok(())
        }
    }
}
