//! ATM Daemon - Session registry and broadcast server
//!
//! This binary runs as a background daemon, accepting status updates
//! from Claude Code sessions and broadcasting updates to TUI clients.
//!
//! # Usage
//!
//! ```bash
//! # Start the daemon (foreground)
//! atmd start
//!
//! # Start the daemon (background/daemonized)
//! atmd start -d
//!
//! # Stop the daemon
//! atmd stop
//!
//! # Check daemon status
//! atmd status
//! ```

use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use atmd::discovery::DiscoveryService;
use atmd::monitor::spawn_monitor_task;
use atmd::registry::spawn_registry;
use atmd::server::{DaemonServer, DEFAULT_SOCKET_PATH};

/// ATM daemon - Claude Code session manager
#[derive(Parser, Debug)]
#[command(name = "atmd", version, about)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Start the daemon
    Start {
        /// Run as a background daemon (fork to background)
        #[arg(short = 'd', long)]
        daemon: bool,
    },
    /// Stop the running daemon
    Stop,
    /// Show daemon status
    Status,
}

fn pid_file_path() -> PathBuf {
    let state_dir = dirs::state_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("atm");
    state_dir.join("atmd.pid")
}

fn log_file_path() -> PathBuf {
    let state_dir = dirs::state_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("atm");
    state_dir.join("atm.log")
}

fn read_pid() -> Option<u32> {
    let path = pid_file_path();
    let mut file = File::open(&path).ok()?;
    let mut contents = String::new();
    file.read_to_string(&mut contents).ok()?;
    contents.trim().parse().ok()
}

fn write_pid() -> Result<()> {
    let path = pid_file_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("Failed to create state directory")?;
    }
    let mut file = File::create(&path).context("Failed to create PID file")?;
    write!(file, "{}", process::id()).context("Failed to write PID")?;
    Ok(())
}

fn remove_pid_file() {
    let path = pid_file_path();
    let _ = fs::remove_file(path);
}

fn is_process_running(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}

fn is_daemon_running() -> Option<u32> {
    if let Some(pid) = read_pid() {
        if is_process_running(pid) {
            return Some(pid);
        }
        remove_pid_file();
    }
    None
}

fn stop_daemon(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        if result != 0 {
            bail!("Failed to send SIGTERM to process {pid}");
        }
    }
    #[cfg(not(unix))]
    {
        bail!("Stop command is only supported on Unix systems");
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();

    let command = args.command.unwrap_or(Command::Start { daemon: false });

    match command {
        Command::Start { daemon } => {
            if let Some(pid) = is_daemon_running() {
                eprintln!("Daemon is already running (PID {pid})");
                eprintln!("Use 'atmd stop' to stop it first.");
                process::exit(1);
            }

            if daemon {
                daemonize()?;
            }

            write_pid()?;

            let result = run_daemon();

            remove_pid_file();

            result
        }
        Command::Stop => {
            if let Some(pid) = is_daemon_running() {
                println!("Stopping daemon (PID {pid})...");
                stop_daemon(pid)?;

                for _ in 0..50 {
                    if !is_process_running(pid) {
                        println!("Daemon stopped.");
                        return Ok(());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }

                eprintln!("Daemon did not stop within 5 seconds.");
                process::exit(1);
            } else {
                println!("Daemon is not running.");
                Ok(())
            }
        }
        Command::Status => {
            if let Some(pid) = is_daemon_running() {
                println!("Daemon is running (PID {pid})");

                let socket_path =
                    env::var("ATM_SOCKET").unwrap_or_else(|_| DEFAULT_SOCKET_PATH.to_string());
                if PathBuf::from(&socket_path).exists() {
                    println!("Socket: {socket_path}");
                }

                Ok(())
            } else {
                println!("Daemon is not running.");
                process::exit(1);
            }
        }
    }
}

fn daemonize() -> Result<()> {
    use daemonize::Daemonize;

    let log_path = log_file_path();

    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent).context("Failed to create log directory")?;
    }

    let stdout = File::create(&log_path).context("Failed to create log file for stdout")?;
    let stderr = File::create(&log_path).context("Failed to create log file for stderr")?;

    let daemonize = Daemonize::new()
        .working_directory("/")
        .stdout(stdout)
        .stderr(stderr);

    daemonize.start().context("Failed to daemonize")?;

    Ok(())
}

#[tokio::main]
async fn run_daemon() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("atmd=info".parse()?)
                .add_directive("atm_core=info".parse()?)
                .add_directive("atm_protocol=info".parse()?),
        )
        .init();

    info!(
        version = env!("CARGO_PKG_VERSION"),
        pid = process::id(),
        "ATM daemon starting"
    );

    let socket_path = env::var("ATM_SOCKET").unwrap_or_else(|_| DEFAULT_SOCKET_PATH.to_string());
    let cancel_token = CancellationToken::new();

    let shutdown_token = cancel_token.clone();
    tokio::spawn(async move {
        if let Err(e) = wait_for_shutdown_signal().await {
            error!(error = %e, "Error waiting for shutdown signal");
        }
        info!("Shutdown signal received");
        shutdown_token.cancel();
    });

    let registry = spawn_registry();
    info!("Session registry started");

    let discovery = DiscoveryService::new(registry.clone());
    let discovery_result = discovery.discover().await;
    if discovery_result.discovered > 0 {
        info!(
            discovered = discovery_result.discovered,
            failed = discovery_result.failed,
            "Initial session discovery complete"
        );
    }

    let _monitor_handle = spawn_monitor_task(cancel_token.clone());
    info!("Process monitor started");

    let server = DaemonServer::new(&socket_path, registry, cancel_token);

    info!(socket = %socket_path, "Starting server");

    if let Err(e) = server.run().await {
        error!(error = %e, "Server error");
        return Err(e.into());
    }

    info!("ATM daemon stopped");
    Ok(())
}

async fn wait_for_shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigint = signal(SignalKind::interrupt())?;

        tokio::select! {
            _ = sigterm.recv() => {
                info!("Received SIGTERM");
            }
            _ = sigint.recv() => {
                info!("Received SIGINT");
            }
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
        info!("Received Ctrl+C");
    }

    Ok(())
}
