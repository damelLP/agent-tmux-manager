//! Private real daemon for control CLI tests. Never inherits live sockets or tmux.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[path = "role.rs"]
pub mod role;
pub use role::process;

/// Log writes and child exit are events, not evidence inferred from a quiet
/// interval. Watch the directory before launch, including daemonized log rotation.
pub struct LogEvents {
    notifications: OwnedFd,
    path: PathBuf,
    reader: Option<BufReader<std::fs::File>>,
    identity: Option<(u64, u64)>,
    partial: Vec<u8>,
    seen: String,
}

impl LogEvents {
    pub fn new(path: &Path) -> Self {
        let parent = path.parent().unwrap();
        std::fs::create_dir_all(parent).unwrap();
        let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        assert!(raw >= 0, "inotify: {}", std::io::Error::last_os_error());
        let notifications = unsafe { OwnedFd::from_raw_fd(raw) };
        let parent = std::ffi::CString::new(parent.as_os_str().as_bytes()).unwrap();
        assert!(
            unsafe {
                libc::inotify_add_watch(
                    notifications.as_raw_fd(),
                    parent.as_ptr(),
                    libc::IN_CREATE | libc::IN_MOVED_TO | libc::IN_MODIFY | libc::IN_CLOSE_WRITE,
                )
            } >= 0
        );
        let mut log = Self {
            notifications,
            path: path.to_owned(),
            reader: None,
            identity: None,
            partial: Vec::new(),
            seen: String::new(),
        };
        log.refresh();
        if let Some(reader) = &mut log.reader {
            reader.seek(SeekFrom::End(0)).unwrap();
        }
        log
    }

    fn refresh(&mut self) {
        if let Ok(file) = std::fs::File::open(&self.path) {
            let metadata = file.metadata().unwrap();
            let identity = (metadata.dev(), metadata.ino());
            if self.identity != Some(identity) {
                self.reader = Some(BufReader::new(file));
                self.identity = Some(identity);
                self.partial.clear();
                self.seen.clear();
            }
        }
    }

    fn contains(&mut self, text: &str) -> bool {
        self.refresh();
        if let Some(reader) = &mut self.reader {
            while reader.read_until(b'\n', &mut self.partial).unwrap() != 0 {
                if self.partial.ends_with(b"\n") {
                    self.seen.push_str(&String::from_utf8_lossy(&self.partial));
                    self.partial.clear();
                }
            }
        }
        self.seen.lines().any(|line| line.contains(text))
    }

    pub fn wait_for(&mut self, text: &str, pid: Option<u32>) -> Result<(), String> {
        self.wait_observed(text, pid, || {})
    }

    fn wait_observed(
        &mut self,
        text: &str,
        pid: Option<u32>,
        mut before_wait: impl FnMut(),
    ) -> Result<(), String> {
        let raw = pid.map(|pid| unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) });
        let exited = raw
            .filter(|fd| *fd >= 0)
            .map(|fd| unsafe { OwnedFd::from_raw_fd(fd as i32) });
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if self.contains(text) {
                return Ok(());
            }
            if pid.is_some() && exited.is_none() {
                return Err(format!("process exited before {text}: {}", self.seen));
            }
            before_wait();
            let mut events = [
                libc::pollfd {
                    fd: self.notifications.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: exited.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let remaining = deadline.saturating_duration_since(Instant::now());
            let ready = unsafe {
                libc::poll(
                    events.as_mut_ptr(),
                    events.len() as libc::nfds_t,
                    i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX),
                )
            };
            if ready < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
            {
                continue;
            }
            if ready <= 0 {
                return Err(format!("no log event for {text}: {}", self.seen));
            }
            if events[0].revents != 0 {
                let mut buffer = [0u8; 16384];
                let read = unsafe {
                    libc::read(
                        self.notifications.as_raw_fd(),
                        buffer.as_mut_ptr().cast(),
                        buffer.len(),
                    )
                };
                if read < 0
                    && std::io::Error::last_os_error().kind() != std::io::ErrorKind::WouldBlock
                {
                    return Err(format!(
                        "log notifications: {}",
                        std::io::Error::last_os_error()
                    ));
                }
            }
            if events[1].revents != 0 && !self.contains(text) {
                return Err(format!("process exited before {text}: {}", self.seen));
            }
        }
    }

    pub fn listening(&mut self, pid: Option<u32>) {
        self.wait_for("Daemon server listening", pid)
            .unwrap_or_else(|error| panic!("{error}"));
    }
}

/// A successful, nonblocking connect: neither bind nor a full backlog is ready.
pub fn socket_is_ready(socket: &Path) -> bool {
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let address = socket_address(socket);
    unsafe {
        libc::connect(
            fd.as_raw_fd(),
            std::ptr::from_ref(&address).cast(),
            std::mem::size_of_val(&address) as libc::socklen_t,
        ) == 0
    }
}

fn socket_address(socket: &Path) -> libc::sockaddr_un {
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let path = std::ffi::CString::new(socket.as_os_str().as_bytes()).unwrap();
    assert!(path.as_bytes_with_nul().len() <= address.sun_path.len());
    for (dst, src) in address.sun_path.iter_mut().zip(path.as_bytes_with_nul()) {
        *dst = *src as libc::c_char;
    }
    address
}

/// Await the actual listening event before one connect; socket creation alone
/// cannot distinguish bind from listen. Real I/O never runs on a paused clock.
pub async fn wait_for_socket(socket: &Path, listening: impl std::future::Future<Output = ()>) {
    tokio::time::timeout(Duration::from_secs(30), async {
        listening.await;
        tokio::net::UnixStream::connect(socket)
            .await
            .expect("listening socket");
    })
    .await
    .expect("private daemon did not accept a connection");
}

/// For these in-process servers the first Pending comes from the accept loop:
/// run() has no await before binding. Poll it in its owning task, signal that
/// boundary, then keep/join the same task through shutdown.
pub async fn start_server(
    server: atmd::server::DaemonServer,
    socket: &Path,
) -> tokio::task::JoinHandle<Result<(), atmd::server::ServerError>> {
    let (send, ready) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let run = server.run();
        tokio::pin!(run);
        tokio::select! {
            biased;
            result = &mut run => panic!("server exited before listening: {result:?}"),
            _ = std::future::ready(()) => {}
        }
        let _ = send.send(());
        run.await
    });
    wait_for_socket(socket, async {
        ready.await.expect("server startup task");
    })
    .await;
    task
}
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub fn isolated<'a>(command: &'a mut Command, dir: &Path) -> &'a mut Command {
    // Stand-in tmux has no real socket, but must report a canonical identity.
    let socket = dir.join(format!("tmux-{}/control-test", unsafe { libc::getuid() }));
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    std::fs::write(&socket, "").unwrap();
    command
        .env_clear()
        .env("CONTROL_TEST_TMUX_SOCKET", socket)
        .env("HOME", dir)
        .env("ATM_SOCKET", dir.join("atmd.sock"))
        .env("ATM_STATE_DIR", dir.join("events"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("TMPDIR", dir)
        .env("TMUX_TMPDIR", dir)
        .env("ATM_TMUX_SOCKET", "control-test")
}

/// `isolated` sets `ATM_STATE_DIR`, so atmd writes its PID file there rather
/// than under the legacy `XDG_STATE_HOME/atm`.
pub fn pid_file(dir: &Path) -> PathBuf {
    dir.join("events/atmd.pid")
}

pub struct Daemon {
    child: Child,
    dir: PathBuf,
}

impl Daemon {
    pub fn start(dir: &Path, path: &Path, binary: &Path) -> Self {
        let mut events = LogEvents::new(&dir.join("daemon.log"));
        let log = std::fs::File::create(dir.join("daemon.log")).unwrap();
        let mut command = Command::new(binary);
        isolated(&mut command, dir)
            .env("PATH", path)
            .args(["start"])
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log);
        let child = command.spawn().unwrap();
        let daemon = Self {
            child,
            dir: dir.to_owned(),
        };
        events.listening(Some(daemon.child.id()));
        assert!(
            socket_is_ready(&dir.join("atmd.sock")),
            "daemon not listening"
        );
        daemon
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_some() {
            return;
        }
        assert_owned(self.child.id(), &self.dir);
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        if process::wait_exit(self.child.id(), Duration::from_secs(30)).is_err() {
            assert_owned(self.child.id(), &self.dir);
            self.child.kill().unwrap();
            process::wait_exit(self.child.id(), Duration::from_secs(30)).unwrap();
        }
        self.child.wait().unwrap();
    }
}

pub fn autostart_cleanup_does_not_panic_on_invalid_or_unowned_pid() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(pid_file(dir.path()).parent().unwrap()).unwrap();
    for pid in ["invalid".to_string(), std::process::id().to_string()] {
        std::fs::write(pid_file(dir.path()), pid).unwrap();
        drop(Autostarted(dir.path().to_owned()));
    }
}

pub struct Autostarted(pub PathBuf);

impl Autostarted {
    pub fn pid(&self) -> u32 {
        std::fs::read_to_string(pid_file(&self.0))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }
}

impl Drop for Autostarted {
    fn drop(&mut self) {
        let Ok(contents) = std::fs::read_to_string(pid_file(&self.0)) else {
            return;
        };
        let Ok(pid) = contents.trim().parse::<u32>() else {
            eprintln!("skipping daemon cleanup: invalid pid");
            return;
        };
        if !owned(pid, &self.0) {
            eprintln!("skipping daemon cleanup: pid {pid} is gone or not ours");
            return;
        }
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
        if fd < 0 {
            eprintln!(
                "skipping daemon cleanup: pidfd_open: {}",
                std::io::Error::last_os_error()
            );
            return;
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        if !owned(pid, &self.0) {
            return;
        }
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                libc::SIGTERM,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            );
        }
        let mut ready = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut ready, 1, 30_000) } != 1 {
            eprintln!("daemon {pid} did not stop; rechecking ownership before SIGKILL");
            if owned(pid, &self.0) {
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                }
                if unsafe { libc::poll(&mut ready, 1, 30_000) } != 1 {
                    eprintln!("daemon {pid} still not reaped after SIGKILL");
                }
            }
        }
    }
}

pub fn assert_owned(pid: u32, dir: &Path) {
    assert!(owned(pid, dir), "daemon {pid} is not owned by this test");
}

fn owned(pid: u32, dir: &Path) -> bool {
    let Some(binary) = option_env!("CARGO_BIN_EXE_atmd") else {
        return false;
    };
    let Ok(expected) = std::fs::canonicalize(binary) else {
        return false;
    };
    let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) else {
        return false;
    };
    let Ok(env) = std::fs::read(format!("/proc/{pid}/environ")) else {
        return false;
    };
    let socket = format!("ATM_SOCKET={}", dir.join("atmd.sock").display());
    exe == expected && env.split(|b| *b == 0).any(|v| v == socket.as_bytes())
}

#[cfg(test)]
pub mod log_event_tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;

    pub fn listening_wait_holds_partial_line_until_pipe_release() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        let mut log = LogEvents::new(&path);
        let (output, writer) = UnixStream::pair().unwrap();
        output
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut child = Command::new("/usr/bin/python3").args([
            "-c",
            "import sys\nf=open(sys.argv[1],'wb')\nf.write(b'Daemon server listening: \\xc3'); f.flush()\nprint('partial',flush=True)\nsys.stdin.readline()\nf.write(b'\\xa9\\ncomplete log\\n'); f.flush()\nprint('complete',flush=True)\nsys.stdin.readline()",
            path.to_str().unwrap(),
        ]).stdin(Stdio::piped()).stdout(Stdio::from(OwnedFd::from(writer))).spawn().unwrap();
        let pid = child.id();
        let mut input = child.stdin.take().unwrap();
        let child = process::OwnedChild::new(child);
        let mut output = BufReader::new(output);
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        assert_eq!(line, "partial\n");
        assert!(
            !log.contains("Daemon server listening"),
            "partial line was readiness"
        );
        let (send, entered) = mpsc::channel();
        let mut send = Some(send);
        let waiter = std::thread::spawn(move || {
            log.wait_observed("Daemon server listening", Some(pid), || {
                if let Some(send) = send.take() {
                    send.send(()).unwrap();
                }
            })
        });
        entered.recv_timeout(Duration::from_secs(30)).unwrap();
        input.write_all(b"release\n").unwrap();
        waiter.join().unwrap().unwrap();
        line.clear();
        output.read_line(&mut line).unwrap();
        assert_eq!(line, "complete\n");
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "Daemon server listening: é\ncomplete log\n"
        );
        drop(input);
        process::wait_exit(pid, Duration::from_secs(30)).unwrap();
        assert!(child.wait_with_output().unwrap().status.success());
    }

    pub fn listening_event_delivered_before_wait_is_retained() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        let mut log = LogEvents::new(&path);
        std::fs::write(&path, "Daemon server listening\n").unwrap();
        log.wait_observed("Daemon server listening", None, || {
            panic!("lost event before wait")
        })
        .unwrap();
    }

    pub fn daemon_exit_before_listening_returns_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.log");
        let mut log = LogEvents::new(&path);
        let child = Command::new("/bin/sh")
            .args(["-c", "printf 'startup failed\\n'; exit 9"])
            .stdout(std::fs::File::create(&path).unwrap())
            .spawn()
            .unwrap();
        let pid = child.id();
        let child = process::OwnedChild::new(child);
        process::wait_exit(pid, Duration::from_secs(30)).unwrap();
        let mut waits = 0;
        let error = log
            .wait_observed("Daemon server listening", Some(pid), || {
                waits += 1;
                assert_eq!(waits, 1, "ignored retained child-exit event");
            })
            .unwrap_err();
        assert!(
            error.contains("process exited before Daemon server listening"),
            "{error}"
        );
        assert!(error.contains("startup failed"), "{error}");
        assert_eq!(child.wait_with_output().unwrap().status.code(), Some(9));
    }
}

#[cfg(test)]
pub mod socket_readiness_tests {
    use super::*;
    use std::future::{poll_fn, Future};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::task::Poll;

    pub async fn wait_for_socket_retries_bound_socket_until_listen() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("atmd.sock");
        let fd = unsafe {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
            )
        };
        assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let address = socket_address(&socket);
        assert_eq!(
            unsafe {
                libc::bind(
                    fd.as_raw_fd(),
                    std::ptr::from_ref(&address).cast(),
                    std::mem::size_of_val(&address) as libc::socklen_t,
                )
            },
            0,
            "bind: {}",
            std::io::Error::last_os_error()
        );
        assert_eq!(
            UnixStream::connect(&socket).unwrap_err().raw_os_error(),
            Some(libc::ECONNREFUSED)
        );

        assert!(socket.exists());
        assert!(
            !socket_is_ready(&socket),
            "synchronous startup must not mistake bind for listen"
        );
        let (send, listening) = tokio::sync::oneshot::channel();
        let ready = wait_for_socket(&socket, async {
            listening.await.unwrap();
        });
        tokio::pin!(ready);
        poll_fn(|cx| {
            assert!(ready.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        // Only listen changes now: no new filesystem event can wake the helper.
        assert_eq!(unsafe { libc::listen(fd.as_raw_fd(), 1) }, 0);
        let listener = UnixListener::from(fd);
        send.send(()).unwrap();
        ready.await;
        assert!(socket_is_ready(&socket));
        listener
            .accept()
            .expect("readiness required a real connect");
    }

    pub fn synchronous_readiness_does_not_block_on_a_full_backlog() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("atmd.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
        let _queued = UnixStream::connect(&socket).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        // A detached probe lets the outer hang guard report a blocking connect.
        std::thread::spawn(move || {
            let _ = tx.send(socket_is_ready(&socket));
        });
        assert!(!rx.recv_timeout(Duration::from_secs(30)).unwrap());
    }

    pub async fn wait_for_socket_observes_creation_after_watch() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("atmd.sock");
        let (send, listening) = tokio::sync::oneshot::channel();
        let ready = wait_for_socket(&socket, async {
            listening.await.unwrap();
        });
        tokio::pin!(ready);
        poll_fn(|cx| {
            assert!(ready.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        send.send(()).unwrap();
        ready.await;
        listener
            .accept()
            .expect("readiness required a real connect");
    }

    pub async fn wait_for_socket_connects_to_existing_listener() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("atmd.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        wait_for_socket(&socket, std::future::ready(())).await;
        listener
            .accept()
            .expect("readiness required a real connect");
    }
}
