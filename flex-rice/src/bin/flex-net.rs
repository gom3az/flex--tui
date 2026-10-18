//! `flex-net` binary: network throughput monitor & process bandwidth manager.
//!
//! Provides both headless Waybar reporting (default, `--stream`), an interactive
//! Wiremix TUI (`flex-net -m` or `flex net`) showing Top Bandwidth Consumers, Network Interfaces,
//! and Speedtest Benchmark, and a fast terminal benchmark (`flex-net -B` / `flex-net --speedtest`).

use std::time::Duration;

use clap::Parser;
use flex_core::backend::EXIT_CANCELLED;
use flex_core::Outcome;
use flex_rice::exec::net::{self, format_speed, snapshot_bandwidth};
use flex_rice::exec::speedtest;
use flex_rice::runner::{self, GlobalStyle, Provider};

/// Network throughput monitor and bandwidth manager.
#[derive(Debug, Parser)]
#[allow(clippy::struct_excessive_bools)]
#[command(
    name = "flex-net",
    version,
    about = "Network throughput monitor and process bandwidth manager"
)]
struct Cli {
    /// Global presentation flags (upstream `-s/-t/-p/--filter-mode`).
    #[command(flatten)]
    style: GlobalStyle,

    /// Open interactive Wiremix TUI popup menu.
    #[arg(short = 'm', long)]
    menu: bool,

    /// Print top network consuming processes to stdout.
    #[arg(short = 'T', long)]
    top: bool,

    /// Run speedtest benchmark and print summary to stdout.
    #[arg(short = 'B', long)]
    speedtest: bool,

    /// Run continuously, streaming JSON lines at the specified interval in seconds.
    #[arg(short = 'S', long, value_name = "SECS")]
    stream: Option<u64>,

    /// Print the selected `ACTION:` line without executing it.
    #[arg(long)]
    print_action: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(err) = run().await {
        runner::fail(&err);
    }
}

/// Headless top talkers: interface total, TCP-attributed table, remainder.
fn print_top() {
    let snapshot = snapshot_bandwidth();
    if let Some((iface, rx_rate, tx_rate)) = snapshot.iface.as_ref() {
        println!(
            "{iface}: ⬇ {}  ⬆ {}  (interface total, matches waybar)",
            format_speed(*rx_rate),
            format_speed(*tx_rate)
        );
    }
    if snapshot.talkers.is_empty() {
        println!("No attributed TCP processes found.");
    } else {
        println!(
            "{:<8} {:<20} {:<15} {:<15} {:<8}",
            "PID", "COMMAND", "RX RATE", "TX RATE", "SHARE"
        );
        for process in &snapshot.talkers {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let share_pct = (process.share * 100.0).round() as u32;
            println!(
                "{:<8} {:<20} {:<15} {:<15} {:>3}%",
                process.pid,
                process.comm,
                format_speed(process.rx_rate),
                format_speed(process.tx_rate),
                share_pct
            );
        }
    }
    let (unattributed_rx, unattributed_tx) = snapshot.unattributed;
    if unattributed_rx > 0.0 || unattributed_tx > 0.0 {
        println!(
            "unattributed (UDP/short-lived/kernel): ⬇ {}  ⬆ {}",
            format_speed(unattributed_rx),
            format_speed(unattributed_tx)
        );
    }
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.speedtest {
        return speedtest::run_cli_benchmark();
    }

    if cli.top {
        print_top();
        return Ok(());
    }

    if let Some(secs) = cli.stream {
        let interval = Duration::from_secs(secs.max(1));
        return net::stream(interval);
    }

    if cli.menu || cli.print_action {
        let style = cli.style.options();
        runner::popup_guard(Provider::Net)?;

        if cli.print_action {
            return runner::run_select(Provider::Net, style).await;
        }

        let mut active_tab = 0;
        loop {
            let mut menu = runner::build_menu(Provider::Net, style)?;
            if active_tab < menu.app.tabs.len() {
                menu.app.switch_tab(active_tab);
            }
            match flex_core::run::run_capture(menu).await? {
                Outcome::Chosen { action_id, .. } => {
                    if let Some(pid) = action_id.strip_prefix("proc:") {
                        net::execute(&format!("signal:{pid}:SIGTERM"))?;
                        return Ok(());
                    }
                    net::execute(&action_id)?;
                    if action_id == "speedtest:run" || action_id.starts_with("speedtest") {
                        active_tab = 2;
                        continue;
                    }
                    return Ok(());
                }
                Outcome::Delete { action_id, .. } => {
                    if let Some(pid) = action_id.strip_prefix("proc:") {
                        net::execute(&format!("signal:{pid}:SIGKILL"))?;
                    }
                    return Ok(());
                }
                Outcome::Toggle { action_id, .. } => {
                    if let Some(pid) = action_id.strip_prefix("proc:") {
                        net::execute(&format!("signal:{pid}:SIGSTOP"))?;
                    }
                    return Ok(());
                }
                Outcome::Target { target, .. } => {
                    net::execute(&target)?;
                    if target.starts_with("speedtest") {
                        active_tab = 2;
                        continue;
                    }
                    return Ok(());
                }
                Outcome::Quit { code } => {
                    std::process::exit(code);
                }
                Outcome::Cancelled => {
                    std::process::exit(EXIT_CANCELLED);
                }
            }
        }
    }
    println!("{}", net::sample_json());
    Ok(())
}
