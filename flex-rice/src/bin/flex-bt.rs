//! `flex-bt` binary: native Bluetooth manager Wiremix popup.
//!
//! Provides an interactive Bluetooth manager with device connection, battery level,
//! audio profile switching, adapter toggles, and background status refreshing.

use clap::Parser;
use flex_core::Outcome;
use flex_rice::exec::bt;
use flex_rice::runner::{self, GlobalStyle, Provider};

/// Native Bluetooth manager Wiremix popup.
#[derive(Debug, Parser)]
#[command(name = "flex-bt", version, about = "Native Bluetooth manager")]
struct Cli {
    /// Global presentation flags (upstream `-s/-t/-p/--filter-mode`).
    #[command(flatten)]
    style: GlobalStyle,

    /// Print the selected `ACTION:` line without executing it.
    #[arg(long)]
    print_action: bool,

    /// Output JSON status for Waybar polling (no TUI, no popup).
    #[arg(long = "status")]
    status: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    runner::init_logging();
    if let Err(err) = run().await {
        runner::fail(&err);
    }
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if cli.status {
        println!("{}", bt::status_json());
        return Ok(());
    }
    let style = cli.style.options();

    runner::run_standard_cli(
        Provider::Bt,
        style,
        cli.print_action,
        |outcome| match outcome {
            Outcome::Chosen {
                action_id, label, ..
            }
            | Outcome::Toggle {
                action_id, label, ..
            } => {
                bt::execute(&action_id, &label, None)?;
                Ok(())
            }
            Outcome::Target { target, title, .. } => {
                bt::execute(&target, &title, None)?;
                Ok(())
            }
            Outcome::Delete { action_id, .. } => {
                if let Some(mac) = action_id.strip_prefix("device:") {
                    bt::remove(mac, None)?;
                    Ok(())
                } else {
                    anyhow::bail!("bt: unexpected delete outcome for '{action_id}'")
                }
            }
            _ => unreachable!(),
        },
    )
    .await
}
