//! `map-cli` — the headless command line of the Hytale seed explorer.
//!
//! Skeleton (T06): the real subcommands arrive with their tasks —
//! `render` (T11), `fidelity` (T05's harness), `bench` (T04's numbers).
//! Everything delegates to `map-core`; no GUI dependency exists here.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "map-cli",
    version,
    about = "Headless Hytale seed explorer: render maps and search seeds without a GUI"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the generator identity this build resolves against.
    Info,
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Info => {
            println!(
                "generator_version {} (map-core {}, headless)",
                map_core::GENERATOR_VERSION,
                env!("CARGO_PKG_VERSION")
            );
        }
    }
}
