mod codegen;
mod coverage;
mod dev_env_probe;
mod docs;
mod secret_lint;

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
    Codegen {
        /// Path or URL to the JSON-RPC contract artifact.
        #[arg(value_name = "CONTRACT_SOURCE")]
        contract_source: Option<String>,
    },
    /// Run the full verification suite.
    FullCheck,
    /// Probe external dev-env services (placeholder).
    DevEnvProbe,
    /// Generate and validate the requirement-coverage report.
    Coverage,
    /// Lint production source for plain-string secret fields.
    SecretLint,
    /// Generate the LLM-readable operator's guide.
    Docs,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Codegen { contract_source } => codegen::run(contract_source.as_deref()),
        Command::FullCheck => {
            secret_lint::run()?;
            codegen::run(None)?;
            coverage::run()?;
            println!("full-check: ok");
            Ok(())
        }
        Command::DevEnvProbe => dev_env_probe::run(),
        Command::Coverage => coverage::run(),
        Command::SecretLint => secret_lint::run(),
        Command::Docs => docs::run(),
    }
}
