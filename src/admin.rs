use clap::{Parser, Subcommand};
use std::process;

#[derive(Parser, Debug)]
#[command(name = "pacto-bot-admin")]
#[command(about = "Pacto bot admin CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Create a new bot identity config snippet.
    New {
        #[arg(value_name = "BOT_ID")]
        bot_id: String,
    },
    /// Publish a bot profile (kind:0) event.
    PublishProfile {
        #[arg(value_name = "BOT_ID")]
        bot_id: String,
    },
    /// Test a NIP-46 bunker connection and pubkey match.
    TestBunker {
        #[arg(value_name = "BOT_ID")]
        bot_id: String,
    },
    /// Export bot daemon-local state to JSON.
    Export {
        #[arg(value_name = "BOT_ID")]
        bot_id: String,
    },
    /// Import bot daemon-local state from JSON.
    Import {
        #[arg(value_name = "BOT_ID")]
        bot_id: String,
        #[arg(value_name = "STATE_FILE")]
        state_file: String,
    },
    /// Validate the daemon configuration file.
    ValidateConfig {
        #[arg(short, long, value_name = "PATH", default_value = "pacto-bot-api.toml")]
        config: String,
    },
    /// Rotate the HTTP secret token.
    RotateHttpToken,
    /// Emit structured daemon diagnostics.
    Diagnose {
        #[arg(short, long, value_name = "FORMAT", default_value = "text")]
        format: String,
    },
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::New { bot_id } => {
            println!("# Placeholder config snippet for bot '{}':", bot_id);
            println!("[[bots]]");
            println!("id = \"{}\"", bot_id);
            println!("npub = \"npub1...\"");
            println!("signing = {{ backend = \"bunker_remote\", uri = \"bunker://...\" }}");
        }
        Command::PublishProfile { bot_id } => {
            println!("publish-profile: {} (not yet implemented)", bot_id);
            process::exit(0);
        }
        Command::TestBunker { bot_id } => {
            println!("test-bunker: {} (not yet implemented)", bot_id);
            process::exit(0);
        }
        Command::Export { bot_id } => {
            println!("export: {} (not yet implemented)", bot_id);
            process::exit(0);
        }
        Command::Import { bot_id, state_file } => {
            println!(
                "import: {} from {} (not yet implemented)",
                bot_id, state_file
            );
            process::exit(0);
        }
        Command::ValidateConfig { config } => {
            match pacto_bot_api::config::DaemonConfig::load(&config) {
                Ok(_) => {
                    println!("config is valid: {}", config);
                    process::exit(0);
                }
                Err(e) => {
                    eprintln!("config validation failed: {}", e);
                    process::exit(1);
                }
            }
        }
        Command::RotateHttpToken => {
            println!("rotate-http-token: not yet implemented");
            process::exit(0);
        }
        Command::Diagnose { format } => {
            println!("{{ \"status\": \"ok\", \"format\": \"{}\" }}", format);
            process::exit(0);
        }
    }
}
