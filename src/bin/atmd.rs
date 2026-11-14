//! ATM Daemon - Session registry and broadcast server
//!
//! This binary runs as a background daemon, accepting status updates
//! from coding-agent sessions (Claude Code, pi, Codex CLI) and
//! broadcasting updates to TUI clients.
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
//!
//! `stop` observes process exit, including unreaped zombies, not PID/socket-file
//! removal. It reports progress once after 5 seconds and gives up after 75 seconds
//! from signaling, without killing the daemon or removing its state. Nominal
//! shutdown allows 2 s for connections, 30 s for admitted control deadlines,
//! 30 s for audits, then 2 s for concurrent summary/raw flushes (64 s total).
//! Blocking I/O, including Tokio blocking-pool work at runtime drop, can outlast
//! that allowance. A stop timeout leaves the daemon potentially still shutting
//! down; successful exit does not promise every pending write was persisted.
//! Known limit (unverified in a filtered runtime): if seccomp denies pidfd_open
//! with EPERM, stop fails before signaling. Only ENOSYS enables the old-kernel
//! numeric-PID fallback; a seccomp denial does not.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(target_os = "linux")]
use std::os::fd::FromRawFd;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use atmd::discovery::DiscoveryService;
use atmd::monitor::spawn_monitor_task;
use atmd::notify::{parse_notify_command, spawn_notifier, NotifyConfig};
use atmd::registry::{spawn_registry_with_cancel, supervise_actor};
use atmd::server::{methods::MethodServices, DaemonServer, DEFAULT_SOCKET_PATH};

/// ATM daemon - coding-agent session manager
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

// Keep PID/log defaults aligned with crates/atm/src/daemon.rs, not the
// eventlog resolver: its relative-XDG/HOME fallback deliberately differs.
fn resolve_daemon_state_dir(
    override_dir: Option<PathBuf>,
    default_state_dir: Option<PathBuf>,
) -> PathBuf {
    override_dir.unwrap_or_else(|| {
        default_state_dir
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("atm")
    })
}

fn pid_file_path(state_dir: &Path) -> PathBuf {
    state_dir.join("atmd.pid")
}

fn log_file_path(state_dir: &Path) -> PathBuf {
    state_dir.join("atm.log")
}

fn read_pid(state_dir: &Path) -> Option<u32> {
    let path = pid_file_path(state_dir);
    let mut file = File::open(&path).ok()?;
    let mut contents = String::new();
    file.read_to_string(&mut contents).ok()?;
    contents.trim().parse().ok()
}

fn write_pid(state_dir: &Path) -> Result<()> {
    let path = pid_file_path(state_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("Failed to create state directory")?;
    }
    let mut file = File::create(&path).context("Failed to create PID file")?;
    write!(file, "{}", process::id()).context("Failed to write PID")?;
    Ok(())
}

fn remove_pid_file(state_dir: &Path) {
    let path = pid_file_path(state_dir);
    let _ = fs::remove_file(path);
}

fn proc_stat_has_exited(stat: &[u8]) -> bool {
    // comm may contain spaces, ')' and truncated UTF-8; use its final delimiter.
    let mut parts = stat.rsplitn(2, |&byte| byte == b')');
    let (Some(fields), Some(_)) = (parts.next(), parts.next()) else {
        return false;
    };
    let mut fields = fields
        .split(u8::is_ascii_whitespace)
        .filter(|f| !f.is_empty());
    match fields.next() {
        Some(b"X" | b"x") => true,
        // The leader is Z as soon as it exits, while the group's other threads
        // may still be exiting: only num_threads (field 20) == 1 is all of it.
        Some(b"Z") => fields.nth(16) == Some(b"1".as_slice()),
        _ => false,
    }
}

fn is_process_running(pid: u32) -> bool {
    let path = PathBuf::from(format!("/proc/{pid}"));
    // An exited process retains procfs until reaped. If its state cannot be
    // read or recognized, keep the old, conservative existence answer.
    path.exists() && !fs::read(path.join("stat")).is_ok_and(|stat| proc_stat_has_exited(&stat))
}

/// Read-only: `stop` and `status` call this without the lock, so a stale
/// PID file is left for the next lock holder's write_pid() to replace.
fn is_daemon_running(state_dir: &Path) -> Option<u32> {
    if let Some(pid) = read_pid(state_dir) {
        if is_process_running(pid) {
            return Some(pid);
        }
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
/// different PID files. The PID file is also unlinked when a daemon exits,
/// which would let a second starter lock a fresh inode. For the same reason
/// the lock file is never removed.
fn lock_daemon(socket_path: &Path) -> Result<Option<File>> {
    let mut lock_path = socket_path.as_os_str().to_owned();
    lock_path.push(".lock");
    let lock_path = PathBuf::from(lock_path);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    // Read-only is enough for flock, and works on a lock file that
    // someone else created. The default socket dir is world-writable
    // /tmp: 0600 stops other users opening the lock to hold it,
    // O_NOFOLLOW stops a planted symlink redirecting the O_CREAT, and
    // O_NONBLOCK stops a planted FIFO blocking the open.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CREAT | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .mode(0o600)
        .open(&lock_path)
        .with_context(|| format!("Failed to open lock file {}", lock_path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("Failed to stat lock file {}", lock_path.display()))?;
    if !metadata.is_file() {
        bail!("Lock file {} is not a regular file", lock_path.display());
    }
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

// Keep the candidate kit's process-exit policy aligned (scripts/atm_candidate.py).
// 64 s of nominal drains plus margin, NOT a hard bound on daemon blocking I/O.
const STOP_WAIT_TIMEOUT: Duration = Duration::from_secs(75);
const STOP_PROGRESS_AFTER: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq, Eq)]
enum StopWaitAction {
    Stopped,
    Progress,
    Wait(Duration),
    TimedOut,
}

fn stop_wait_action(elapsed: Duration, exited: bool, reported: bool) -> StopWaitAction {
    if exited {
        StopWaitAction::Stopped
    } else if elapsed >= STOP_WAIT_TIMEOUT {
        StopWaitAction::TimedOut
    } else if !reported && elapsed >= STOP_PROGRESS_AFTER {
        StopWaitAction::Progress
    } else {
        let next = if reported {
            STOP_WAIT_TIMEOUT
        } else {
            STOP_PROGRESS_AFTER
        };
        StopWaitAction::Wait(next.saturating_sub(elapsed))
    }
}

fn wait_for_exit(
    pid: u32,
    mut wait: impl FnMut(Duration) -> std::io::Result<bool>,
    mut elapsed: impl FnMut() -> Duration,
    mut progress: impl FnMut(),
) -> Result<()> {
    let mut reported = false;
    loop {
        // Check exit even at the deadline; never infer it from elapsed time.
        let exited = wait(Duration::ZERO)?;
        match stop_wait_action(elapsed(), exited, reported) {
            StopWaitAction::Stopped => return Ok(()),
            StopWaitAction::Progress => {
                progress();
                reported = true;
            }
            StopWaitAction::Wait(duration) => {
                if wait(duration)? {
                    return Ok(());
                }
            }
            StopWaitAction::TimedOut => bail!(
                "Daemon (PID {pid}) did not exit within {} seconds; it may still be shutting down.",
                STOP_WAIT_TIMEOUT.as_secs()
            ),
        }
    }
}

#[cfg(target_os = "linux")]
fn open_pidfd(pid: u32) -> std::io::Result<Option<OwnedFd>> {
    // SAFETY: pidfd_open takes a PID and flags, no pointers.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    if fd >= 0 {
        // SAFETY: the syscall returned a new, uniquely owned descriptor.
        return Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }));
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOSYS) {
        Ok(None) // Old kernels retain T6's conservative zombie-aware polling.
    } else {
        Err(error)
    }
}

fn wait_for_process(
    pid: u32,
    pidfd: Option<&OwnedFd>,
    duration: Duration,
) -> std::io::Result<bool> {
    if let Some(fd) = pidfd {
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = i32::try_from(duration.as_millis()).unwrap_or(i32::MAX);
        // SAFETY: poll points to one initialized descriptor, kept alive by fd.
        if unsafe { libc::poll(&mut poll, 1, millis) } < 0 {
            let error = std::io::Error::last_os_error();
            return if error.kind() == std::io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(error)
            };
        }
        if poll.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(std::io::Error::other("Cannot observe daemon pidfd"));
        }
        // HUP is also an exited process: the parent has already reaped it.
        Ok(poll.revents & (libc::POLLIN | libc::POLLHUP) != 0)
    } else {
        if is_process_running(pid) && !duration.is_zero() {
            std::thread::sleep(duration.min(Duration::from_millis(100)));
        }
        Ok(!is_process_running(pid))
    }
}

fn stop_daemon(pid: u32) -> Result<()> {
    let numeric_pid = i32::try_from(pid).context("Invalid daemon PID")?;
    if numeric_pid <= 0 {
        bail!("Invalid daemon PID {pid}");
    }
    #[cfg(target_os = "linux")]
    let pidfd = match open_pidfd(pid) {
        Ok(fd) => fd,
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return Ok(()),
        Err(error) => return Err(error).context("Failed to open daemon pidfd"),
    };
    #[cfg(not(target_os = "linux"))]
    let pidfd: Option<OwnedFd> = None;
    let started = Instant::now();
    #[cfg(target_os = "linux")]
    let result = if let Some(fd) = &pidfd {
        // SAFETY: the owned pidfd pins this process; null siginfo uses SIGTERM.
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                libc::SIGTERM,
                0usize,
                0,
            ) as i32
        }
    } else {
        // SAFETY: validated positive PID; old-kernel compatibility only.
        unsafe { libc::kill(numeric_pid, libc::SIGTERM) }
    };
    #[cfg(not(target_os = "linux"))]
    let result = unsafe { libc::kill(numeric_pid, libc::SIGTERM) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).with_context(|| format!("Failed to send SIGTERM to process {pid}"));
        }
    }
    wait_for_exit(
        pid,
        |duration| wait_for_process(pid, pidfd.as_ref(), duration),
        || started.elapsed(),
        || println!("Daemon is still stopping; waiting for shutdown to finish..."),
    )
}

fn main() -> Result<()> {
    let args = Args::parse();

    let command = args.command.unwrap_or(Command::Start { daemon: false });
    // Freeze relative overrides before daemonize changes cwd, but keep the
    // established dirs fallback byte-identical when no override is supplied.
    let override_dir = env::var("ATM_STATE_DIR")
        .ok()
        .filter(|state| !state.is_empty())
        .map(std::path::absolute)
        .transpose()
        .context("Failed to resolve the daemon state directory")?;
    let state_dir = resolve_daemon_state_dir(override_dir, dirs::state_dir());

    match command {
        Command::Start { daemon } => {
            // daemonize() changes to `/`, so with -d a relative ATM_SOCKET is
            // made absolute first, to keep the lock and the bind on one path.
            // A foreground start uses it as given: lock and bind share the
            // cwd, and an absolute form could exceed sun_path's 108 bytes.
            let socket_path = if daemon {
                std::path::absolute(socket_path()).context("Failed to resolve the socket path")?
            } else {
                PathBuf::from(socket_path())
            };

            // Only the lock holder touches the socket and the PID file. It is
            // taken before daemonize() so a losing `atmd start -d` exits 1 in
            // the foreground; the daemonized child inherits the locked file.
            let Some(_lock) = lock_daemon(&socket_path)? else {
                exit_already_running(read_pid(&state_dir).filter(|&pid| is_process_running(pid)));
            };

            // Under the lock, so only the holder ever replaces a stale PID file.
            if let Some(pid) = is_daemon_running(&state_dir) {
                exit_already_running(Some(pid));
            }

            // Resolve before daemonize changes cwd. Persistence opens only
            // afterward: event-log lock descriptors must not cross the fork.
            let eventlog_state_dir = std::path::absolute(atmd::eventlog::state_dir())
                .context("Failed to resolve the event-log state directory")?;
            if daemon {
                daemonize(&state_dir)?;
            }

            write_pid(&state_dir)?;

            let result = run_daemon(socket_path, eventlog_state_dir);

            remove_pid_file(&state_dir);

            result
        }
        Command::Stop => {
            if let Some(pid) = is_daemon_running(&state_dir) {
                println!("Stopping daemon (PID {pid})...");
                stop_daemon(pid)?;

                println!("Daemon stopped.");
                Ok(())
            } else {
                println!("Daemon is not running.");
                Ok(())
            }
        }
        Command::Status => {
            if let Some(pid) = is_daemon_running(&state_dir) {
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

fn daemonize(state_dir: &Path) -> Result<()> {
    use daemonize::Daemonize;

    let log_path = log_file_path(state_dir);

    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent).context("Failed to create log directory")?;
    }

    let rotate_error = rotate_log(&log_path);
    let stdout = open_log(&log_path).context("Failed to open log file for stdout")?;
    let mut stderr = open_log(&log_path).context("Failed to open log file for stderr")?;
    if let Some(e) = rotate_error {
        let _ = writeln!(stderr, "Failed to rotate {}: {e}", log_path.display());
    }

    let daemonize = Daemonize::new()
        .working_directory("/")
        .stdout(stdout)
        .stderr(stderr);

    daemonize.start().context("Failed to daemonize")?;

    Ok(())
}

/// Keeps the previous run's log as `atm.log.1`. An open TUI restarts a
/// crashed daemon within seconds, and the new log would otherwise replace
/// the one that explains the crash. Best effort: returns the error unless
/// there was no log yet.
fn rotate_log(log_path: &Path) -> Option<std::io::Error> {
    match fs::rename(log_path, log_path.with_extension("log.1")) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Some(e),
        _ => None,
    }
}

/// Opens the log for appending, never truncating: after a failed rotation
/// the old log survives, and O_APPEND stops the stdout and stderr handles
/// overwriting each other from offset 0.
fn open_log(log_path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(log_path)
}

#[tokio::main]
async fn run_daemon(socket_path: PathBuf, state_dir: PathBuf) -> Result<()> {
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

    // These locks/writers are opened only after daemonization. Independent
    // detached startup means one broken stream cannot disable the other.
    let org_cancel = CancellationToken::new();
    let (org, raw) = tokio::join!(
        atmd::org::OrgHandle::open(state_dir.clone(), org_cancel.clone()),
        atmd::org::RawHandle::open(state_dir.clone()),
    );
    let summaries = atmd::org::SummaryObserver::spawn(registry.clone(), org.clone());

    // Subscribed before discovery, so the notifier sees every session.
    let _notify_handle = notify_config().await.map(|config| {
        // Only the program: the arguments may hold a webhook URL or topic.
        let program = config.command.first().cloned().unwrap_or_default();
        let delay_secs = config.delay_secs;
        info!(
            program,
            delay_secs, "Running [notify] when a session needs input"
        );
        let delay = std::time::Duration::from_secs(delay_secs);
        spawn_notifier(
            registry.subscribe(),
            config.command,
            delay,
            cancel_token.clone(),
        )
    });

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

    let mut services = MethodServices::new(registry);
    services.state_dir = state_dir;
    services.org = org;
    services.raw = raw.clone();
    let server = DaemonServer::with_services(&socket_path, services, cancel_token);

    info!(socket = %socket_path.display(), "Starting server");

    // If the registry actor dies, the supervisor cancels the registry token and
    // the server shuts down cooperatively (drains connections, removes the socket).
    let (server_result, registry_result) = tokio::join!(
        async {
            let result = server.run().await;
            // Connections drained: preserve exact last live cost before
            // cancelling either actor. Flush failures never hold shutdown.
            let (_, raw_result) = tokio::join!(summaries.finish(), raw.flush());
            if let Err(error) = raw_result {
                warn!(%error, "Raw capture flush incomplete");
            }
            org_cancel.cancel();
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

/// The `[notify]` section of atm's `config.toml`, read once at start. A
/// missing file or section turns notifications off, and so, with a
/// warning, does a config that can't be read or parsed.
async fn notify_config() -> Option<NotifyConfig> {
    let path = atm_tui::setup::atm_config_path()?;
    let text = match read_config(path.clone(), CONFIG_READ_TIMEOUT).await {
        Ok(text) => text?,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "Cannot read config; notifications are off");
            return None;
        }
    };
    parse_notify_command(&text).unwrap_or_else(|e| {
        warn!(path = %path.display(), error = %e, "Invalid config; notifications are off");
        None
    })
}

/// How long atmd waits for `config.toml` before it starts without it.
const CONFIG_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Reads `path`, or `None` if it doesn't exist. The read runs on a
/// detached thread, so a FIFO or a stuck mount there can hang neither
/// atmd's start nor, as a `spawn_blocking` read would, its exit.
async fn read_config(path: PathBuf, timeout: std::time::Duration) -> Result<Option<String>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("atmd-config-read".to_string())
        .spawn(move || {
            let _ = tx.send(match fs::read_to_string(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                result => result.map(Some),
            });
        })
        .context("Failed to start the config read")?;
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(result)) => Ok(result?),
        Ok(Err(_)) => bail!("the config read ended without a result"),
        Err(_) => bail!("the config read timed out after {timeout:?}"),
    }
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

    // Captured: Linux 7.2.14 procfs, 2026-11-14, a leader that called
    // pthread_exit while one worker slept, under a parent that does not reap.
    // Read before and after the worker exited; the process's pidfd became
    // readable only after, when num_threads (field 20) dropped to 1.
    const ZOMBIE_LEADER_LIVE_THREAD: &str = "614980 (z) Z 614974 614970 614970 0 -1 4227084 78 0 0 0 0 0 0 0 20 0 2 0 58947771 0 0 18446744073709551615 0 0 0 0 0 0 2147221247 0 0 0 0 0 17 7 0 0 0 0 0 0 0 0 0 0 0 0 0";
    const ZOMBIE_WHOLE_GROUP: &str = "614980 (z) Z 614974 614970 614970 0 -1 4227084 81 0 0 0 0 0 0 0 20 0 1 0 58947771 0 0 18446744073709551615 0 0 0 0 0 0 2147221247 0 0 1 0 0 17 7 0 0 0 0 0 0 0 0 0 0 0 0 0";

    /// The captured whole-group line with another name, state and thread count.
    fn stat(comm: &str, state: &str, threads: &str) -> String {
        let (_, fields) = ZOMBIE_WHOLE_GROUP.rsplit_once(')').unwrap();
        let mut fields: Vec<&str> = fields.split_whitespace().collect();
        fields[0] = state;
        fields[17] = threads;
        format!("614980 ({comm}) {}", fields.join(" "))
    }

    // The leader turns Z the moment it exits, while the group's other threads
    // may still be exiting; stop reported success in that window under load.
    #[test]
    fn zombie_leader_has_not_exited_while_other_threads_live() {
        assert!(!proc_stat_has_exited(ZOMBIE_LEADER_LIVE_THREAD.as_bytes()));
        assert!(proc_stat_has_exited(ZOMBIE_WHOLE_GROUP.as_bytes()));
        for threads in ["2", "17", "0", "-1", "one"] {
            assert!(
                !proc_stat_has_exited(stat("atmd", "Z", threads).as_bytes()),
                "num_threads {threads}"
            );
        }
    }

    // Counting zombies as running breaks stop; counting stopped tasks as dead
    // would let startup ignore a still-live daemon's PID.
    #[test]
    fn proc_stat_distinguishes_exited_from_live_and_stopped_states() {
        for (state, exited) in [
            ("Z", true),
            ("X", true),
            ("x", true),
            ("R", false),
            ("S", false),
            ("D", false),
            ("T", false),
            ("t", false),
            ("I", false),
            ("?", false),
        ] {
            assert_eq!(
                proc_stat_has_exited(stat("atmd", state, "1").as_bytes()),
                exited,
                "state {state}"
            );
        }
    }

    #[test]
    fn proc_stat_state_follows_the_last_name_delimiter() {
        for (stat, exited) in [
            (stat("name with spaces", "Z", "1"), true),
            (stat("(name) with spaces", "X", "1"), true),
            (stat("name) Z still name", "S", "1"), false),
            (stat("name\nwith ) parentheses", "x", "1"), true),
        ] {
            assert_eq!(proc_stat_has_exited(stat.as_bytes()), exited, "{stat:?}");
        }
    }

    // Synthetic: captured whole-group fields with the invalid UTF-8 comm
    // reproduced by PR_SET_NAME truncation on Linux 7.2.14 (2026-11-14).
    #[test]
    fn proc_stat_accepts_non_utf8_comm_without_changing_exit_rules() {
        for (state, threads, exited) in [
            ("Z", "1", true),
            ("Z", "2", false),
            ("Z", "", false),
            ("X", "2", true),
            ("x", "", true),
            ("S", "1", false),
        ] {
            let mut line = b"614980 (name) bad utf8\xe2)".to_vec();
            let text = stat("atmd", state, threads);
            line.extend_from_slice(text.rsplit_once(')').unwrap().1.as_bytes());
            assert_eq!(
                proc_stat_has_exited(&line),
                exited,
                "state {state}, threads {threads}"
            );
        }
    }

    #[test]
    fn incomplete_or_unknown_proc_stat_does_not_prove_exit() {
        for stat in [
            "",
            "123 (atmd Z 1 2 3",
            "123 (atmd)",
            "123 (atmd)   \n",
            "123 (atmd) unknown 1 2 3",
            "123 (atmd) Zombie 1 2 3",
            // A zombie without its thread count does not prove the group exited.
            "123 (atmd) Z 1 2 3",
        ] {
            assert!(!proc_stat_has_exited(stat.as_bytes()), "{stat:?}");
        }
    }

    #[test]
    fn pid_and_log_state_use_the_override_or_legacy_fallback() {
        for (override_dir, default_dir, expected) in [
            (Some("/candidate"), Some("/state"), "/candidate"),
            (None, Some("/state"), "/state/atm"),
            (None, None, "/tmp/atm"),
        ] {
            let state = resolve_daemon_state_dir(
                override_dir.map(PathBuf::from),
                default_dir.map(PathBuf::from),
            );
            assert_eq!(state, PathBuf::from(expected));
            assert_eq!(
                pid_file_path(&state),
                PathBuf::from(format!("{expected}/atmd.pid"))
            );
            assert_eq!(
                log_file_path(&state),
                PathBuf::from(format!("{expected}/atm.log"))
            );
        }
    }

    #[test]
    fn unset_override_keeps_legacy_state_bytes_including_non_utf8() {
        use std::os::unix::ffi::OsStringExt;
        for default in [
            dirs::state_dir(),
            Some(PathBuf::from(std::ffi::OsString::from_vec(
                b"/state/\xff".to_vec(),
            ))),
            Some(PathBuf::from("relative-home/.local/state")),
            None,
        ] {
            let legacy = default
                .clone()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join("atm");
            assert_eq!(
                resolve_daemon_state_dir(None, default).as_os_str(),
                legacy.as_os_str()
            );
        }
    }

    #[test]
    fn stop_wait_progress_once_and_exit_wins_at_deadline() {
        use StopWaitAction::*;
        for (millis, exited, reported, expected) in [
            (
                4_999,
                false,
                false,
                Wait(std::time::Duration::from_millis(1)),
            ),
            (5_000, false, false, Progress),
            (5_001, false, false, Progress),
            (5_000, false, true, Wait(std::time::Duration::from_secs(70))),
            (
                74_999,
                false,
                true,
                Wait(std::time::Duration::from_millis(1)),
            ),
            (75_000, false, true, TimedOut),
            (75_000, false, false, TimedOut),
            (0, true, false, Stopped),
            (5_000, true, false, Stopped),
            (75_000, true, true, Stopped),
            (80_000, true, true, Stopped),
        ] {
            assert_eq!(
                stop_wait_action(std::time::Duration::from_millis(millis), exited, reported),
                expected
            );
        }
    }

    #[test]
    fn stop_waits_for_exit_after_progress_without_repeating_it() {
        use std::{cell::Cell, time::Duration};
        let elapsed = Cell::new(Duration::ZERO);
        let mut progress = Vec::new();
        wait_for_exit(
            123,
            |wait| {
                elapsed.set((elapsed.get() + wait).min(Duration::from_secs(6)));
                Ok(elapsed.get() == Duration::from_secs(6))
            },
            || elapsed.get(),
            || progress.push(elapsed.get()),
        )
        .unwrap();
        assert_eq!(progress, [Duration::from_secs(5)]);
        assert_eq!(elapsed.get(), Duration::from_secs(6));
    }

    #[test]
    fn stop_timeout_names_still_live_pid_at_seventy_five_seconds() {
        use std::{cell::Cell, time::Duration};
        let elapsed = Cell::new(Duration::ZERO);
        let mut progress = Vec::new();
        let error = wait_for_exit(
            123,
            |wait| {
                elapsed.set(elapsed.get() + wait);
                Ok(false)
            },
            || elapsed.get(),
            || progress.push(elapsed.get()),
        )
        .unwrap_err();
        assert_eq!(elapsed.get(), Duration::from_secs(75));
        assert_eq!(progress, [Duration::from_secs(5)]);
        let message = error.to_string();
        assert!(message.contains("PID 123"), "{message}");
        assert!(message.contains("75 seconds"), "{message}");
        assert!(message.contains("still be shutting down"), "{message}");
    }

    #[test]
    fn failed_exit_probe_is_not_success() {
        let error = wait_for_exit(
            123,
            |_| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            || std::time::Duration::ZERO,
            || panic!("must not report progress after a failed probe"),
        )
        .unwrap_err();
        assert!(error.downcast_ref::<std::io::Error>().is_some());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pidfd_exit_observation_survives_reaping() {
        let mut child = std::process::Command::new("/bin/true").spawn().unwrap();
        let pid = child.id();
        let fd = open_pidfd(pid)
            .unwrap()
            .expect("Linux test requires pidfds");
        assert!(child.wait().unwrap().success());
        assert!(wait_for_process(pid, Some(&fd), Duration::ZERO).unwrap());
        assert!(wait_for_process(pid, None, Duration::ZERO).unwrap());
        assert!(!wait_for_process(std::process::id(), None, Duration::ZERO).unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stop_accepts_exit_before_pidfd_open_and_rejects_nonpositive_pids() {
        let absent = fs::read_to_string("/proc/sys/kernel/pid_max")
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        stop_daemon(absent).unwrap();
        assert!(stop_daemon(0).is_err());
        assert!(stop_daemon(u32::MAX).is_err());
    }

    fn socket_in(dir: &Path) -> PathBuf {
        dir.join("run").join("atmd.sock")
    }

    fn lock_file(socket: &Path) -> String {
        format!("{}.lock", socket.display())
    }

    /// Opens the log twice, as daemonize() does for stdout and stderr.
    fn write_run(log: &Path, stdout_line: &str, stderr_line: &str) {
        let mut stdout = open_log(log).expect("open stdout log");
        let mut stderr = open_log(log).expect("open stderr log");
        stdout.write_all(stdout_line.as_bytes()).expect("write");
        stderr.write_all(stderr_line.as_bytes()).expect("write");
    }

    #[test]
    fn test_log_rotation_keeps_the_previous_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("atm.log");
        assert!(rotate_log(&log).is_none(), "a first start has no log");
        write_run(&log, "old out\n", "old err\n");

        assert!(rotate_log(&log).is_none());
        write_run(&log, "new out\n", "new err\n");

        let old = fs::read_to_string(dir.path().join("atm.log.1")).expect("rotated log");
        assert_eq!(old, "old out\nold err\n");
        assert_eq!(fs::read_to_string(&log).expect("log"), "new out\nnew err\n");
    }

    /// A failed rotation must not cost the crash log: the next run
    /// appends to it instead of truncating it.
    #[test]
    fn test_failed_log_rotation_keeps_the_old_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("atm.log");
        fs::write(&log, "crash trace\n").expect("old log");
        fs::create_dir(dir.path().join("atm.log.1")).expect("blocking dir");

        assert!(rotate_log(&log).is_some(), "rename onto a directory");
        write_run(&log, "new out\n", "new err\n");

        assert_eq!(
            fs::read_to_string(&log).expect("log"),
            "crash trace\nnew out\nnew err\n"
        );
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
        fs::set_permissions(lock_file(&socket), fs::Permissions::from_mode(0o444)).expect("chmod");

        assert!(lock_daemon(&socket).expect("lock").is_some());
    }

    /// Other users must not be able to open the lock file and hold the
    /// lock while this user's daemon is down.
    #[test]
    fn test_daemon_lock_file_is_private() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = socket_in(dir.path());
        drop(lock_daemon(&socket).expect("lock"));

        let mode = fs::metadata(lock_file(&socket))
            .expect("lock file")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// The default socket dir is world-writable /tmp, so a planted
    /// symlink must not redirect the lock's O_CREAT.
    #[test]
    fn test_daemon_lock_refuses_a_symlinked_lock_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = socket_in(dir.path());
        fs::create_dir_all(dir.path().join("run")).expect("mkdir");
        let target = dir.path().join("elsewhere");
        std::os::unix::fs::symlink(&target, lock_file(&socket)).expect("symlink");

        assert!(lock_daemon(&socket).is_err(), "followed a symlinked lock");
        assert!(!target.exists(), "created the symlink's target");
    }

    /// A FIFO (or a stuck mount) at config.toml can't hang atmd's start:
    /// the read gives up after its timeout.
    #[tokio::test]
    async fn test_config_read_gives_up_on_a_fifo() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let fifo = std::ffi::CString::new(path.to_str().expect("utf-8")).expect("fifo path");
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0, "mkfifo");

        let started = std::time::Instant::now();
        let result = read_config(path, std::time::Duration::from_millis(200)).await;
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
        let err = result.expect_err("read a FIFO with no writer");
        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
    }

    #[tokio::test]
    async fn test_config_read_reports_a_missing_file_as_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("config.toml");
        let five = std::time::Duration::from_secs(5);
        assert_eq!(
            read_config(missing.clone(), five).await.expect("read"),
            None
        );
        fs::write(&missing, "[notify]\n").expect("write");
        assert_eq!(
            read_config(missing, five).await.expect("read").as_deref(),
            Some("[notify]\n")
        );
    }

    /// A blocking read-only open of a FIFO waits for a writer, which would
    /// hang `atmd start` before it ever reaches LOCK_NB.
    #[test]
    fn test_daemon_lock_refuses_a_fifo_without_blocking() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = socket_in(dir.path());
        fs::create_dir_all(dir.path().join("run")).expect("mkdir");
        let fifo = std::ffi::CString::new(lock_file(&socket)).expect("fifo path");
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0, "mkfifo");

        let (tx, rx) = std::sync::mpsc::channel();
        let path = socket.clone();
        std::thread::spawn(move || {
            let _ = tx.send(lock_daemon(&path).map(|lock| lock.is_some()));
        });
        let result = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("lock_daemon blocked on the FIFO");

        let err = result.expect_err("locked a FIFO");
        assert!(format!("{err:#}").contains(&lock_file(&socket)));
    }

    #[test]
    fn test_daemon_lock_fails_hard_when_the_lock_file_cannot_be_opened() {
        let dir = tempfile::tempdir().expect("tempdir");
        let not_a_dir = dir.path().join("file");
        fs::write(&not_a_dir, "").expect("write file");
        let socket = not_a_dir.join("atmd.sock");

        let err = lock_daemon(&socket).expect_err("lock under a file");
        assert!(format!("{err:#}").contains(&not_a_dir.display().to_string()));
    }
}
