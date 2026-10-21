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
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::process;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use atmd::discovery::DiscoveryService;
use atmd::monitor::spawn_monitor_task;
use atmd::registry::{spawn_registry_with_cancel, supervise_actor};
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

fn socket_path() -> String {
    env::var("ATM_SOCKET")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_SOCKET_PATH.to_string())
}

/// Takes the single-daemon lock for `socket_path`. It is held until the
/// returned file is dropped, which is process exit. `Ok(None)` means
/// another atmd holds it.
///
/// The lock is `<socket>.lock`, not the PID file. The socket is what two
/// daemons contend for, and they may use different state dirs, so
/// different PID files. The PID file is also unlinked by stale-PID cleanup,
/// which would let a second starter lock a fresh inode. For the same reason
/// the lock file is never removed.
fn lock_daemon(socket_path: &str) -> Result<Option<File>> {
    let lock_path = PathBuf::from(format!("{socket_path}.lock"));
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    // Read-only is enough for flock, and works on a lock file that
    // someone else created.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CREAT)
        .open(&lock_path)
        .with_context(|| format!("Failed to open lock file {}", lock_path.display()))?;
    // SAFETY: flock takes an fd that `file` keeps open for the call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    let err = std::io::Error::last_os_error();
    if err.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(None);
    }
    Err(err).with_context(|| format!("Failed to lock {}", lock_path.display()))
}

fn exit_already_running(pid: Option<u32>) -> ! {
    match pid {
        Some(pid) => eprintln!("Daemon is already running (PID {pid})"),
        None => eprintln!("Daemon is already running"),
    }
    eprintln!("Use 'atmd stop' to stop it first.");
    process::exit(1);
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
                exit_already_running(Some(pid));
            }

            // Only the lock holder touches the socket and the PID file. It is
            // taken before daemonize() so a losing `atmd start -d` exits 1 in
            // the foreground; the daemonized child inherits the locked file.
            let socket_path = socket_path();
            let Some(_lock) = lock_daemon(&socket_path)? else {
                exit_already_running(read_pid());
            };

            if daemon {
                daemonize()?;
            }

            write_pid()?;

            let result = run_daemon(socket_path);

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

                let socket_path = env::var("ATM_SOCKET")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| DEFAULT_SOCKET_PATH.to_string());
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
async fn run_daemon(socket_path: String) -> Result<()> {
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

    // The registry outlives the server so in-flight requests drain against a
    // live actor. Cancelling the registry (actor death) also stops the server.
    let registry_cancel = CancellationToken::new();
    let cancel_token = registry_cancel.child_token();

    let shutdown_token = cancel_token.clone();
    tokio::spawn(async move {
        if let Err(e) = wait_for_shutdown_signal().await {
            error!(error = %e, "Error waiting for shutdown signal");
        }
        info!("Shutdown signal received");
        shutdown_token.cancel();
    });

    let (registry, registry_tasks) = spawn_registry_with_cancel(registry_cancel.clone());
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

    // If the registry actor dies, the supervisor cancels the registry token and
    // the server shuts down cooperatively (drains connections, removes the socket).
    let (server_result, registry_result) = tokio::join!(
        async {
            let result = server.run().await;
            // Connections have drained (or the server failed to start)
            registry_cancel.cancel();
            result
        },
        supervise_actor(registry_tasks.actor, registry_cancel.clone()),
    );
    for task in registry_tasks.maintenance {
        if let Err(e) = task.await {
            error!(error = %e, "Registry maintenance task failed");
        }
    }

    if let Err(e) = server_result {
        error!(error = %e, "Server error");
        return Err(e.into());
    }
    registry_result?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn socket_in(dir: &std::path::Path) -> String {
        dir.join("run").join("atmd.sock").display().to_string()
    }

    #[test]
    fn test_daemon_lock_is_exclusive_until_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        // The socket's parent doesn't exist yet, as on a fresh runtime dir.
        let socket = socket_in(dir.path());

        let first = lock_daemon(&socket).expect("lock").expect("first holder");
        assert!(lock_daemon(&socket).expect("lock").is_none(), "lock shared");
        drop(first);
        assert!(lock_daemon(&socket).expect("lock").is_some(), "lock kept");
    }

    #[test]
    fn test_daemon_lock_takes_a_read_only_lock_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = socket_in(dir.path());
        drop(lock_daemon(&socket).expect("lock"));
        let lock_file = format!("{socket}.lock");
        fs::set_permissions(&lock_file, fs::Permissions::from_mode(0o444)).expect("chmod");

        assert!(lock_daemon(&socket).expect("lock").is_some());
    }

    #[test]
    fn test_daemon_lock_fails_hard_when_the_lock_file_cannot_be_opened() {
        let dir = tempfile::tempdir().expect("tempdir");
        let not_a_dir = dir.path().join("file");
        fs::write(&not_a_dir, "").expect("write file");
        let socket = not_a_dir.join("atmd.sock").display().to_string();

        let err = lock_daemon(&socket).expect_err("lock under a file");
        assert!(format!("{err:#}").contains(&not_a_dir.display().to_string()));
    }
}
