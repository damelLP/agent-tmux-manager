//! Private real daemon for control CLI tests. Never inherits live sockets or tmux.
#![allow(dead_code)]

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

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

/// Register before probing so binding between probe and wait cannot be missed.
/// Readiness means an accepting socket, not merely a filesystem entry.
pub async fn wait_for_socket(socket: &Path) {
    tokio::time::timeout(Duration::from_secs(30), async {
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        assert!(
            fd >= 0,
            "inotify_init1: {}",
            std::io::Error::last_os_error()
        );
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let parent =
            std::ffi::CString::new(socket.parent().unwrap().as_os_str().as_bytes()).unwrap();
        assert!(
            unsafe {
                libc::inotify_add_watch(
                    fd.as_raw_fd(),
                    parent.as_ptr(),
                    libc::IN_CREATE | libc::IN_MOVED_TO,
                )
            } >= 0
        );
        let fd = tokio::io::unix::AsyncFd::new(fd).unwrap();
        loop {
            match tokio::net::UnixStream::connect(socket).await {
                Ok(_) => break,
                Err(error)
                    if error.kind() == std::io::ErrorKind::ConnectionRefused && socket.exists() =>
                {
                    // bind creates the path before listen makes it connectable;
                    // listen emits no inotify event. Poll the connection condition
                    // within the outer guard, not a delay assumed to mean ready.
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    continue;
                }
                Err(_) => {}
            }
            let mut ready = fd.readable().await.unwrap();
            let _ = ready.try_io(|fd| {
                let mut events = [0_u8; 4096];
                let n =
                    unsafe { libc::read(fd.as_raw_fd(), events.as_mut_ptr().cast(), events.len()) };
                if n < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    })
    .await
    .expect("private daemon did not accept a connection");
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
        let log = std::fs::File::create(dir.join("daemon.log")).unwrap();
        let mut command = Command::new(binary);
        isolated(&mut command, dir)
            .env("PATH", path)
            .args(["start"])
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log);
        let child = command.spawn().unwrap();
        let mut daemon = Self {
            child,
            dir: dir.to_owned(),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !socket_is_ready(&dir.join("atmd.sock")) {
            assert!(
                daemon.child.try_wait().unwrap().is_none(),
                "daemon exited: {}",
                std::fs::read_to_string(dir.join("daemon.log")).unwrap()
            );
            assert!(Instant::now() < deadline, "daemon startup timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
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
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                assert_owned(self.child.id(), &self.dir);
                self.child.kill().unwrap();
                self.child.wait().unwrap();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn autostart_cleanup_does_not_panic_on_invalid_or_unowned_pid() {
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
mod socket_readiness_tests {
    use super::*;
    use std::future::{poll_fn, Future};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::task::Poll;

    #[tokio::test]
    async fn wait_for_socket_retries_bound_socket_until_listen() {
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
        let ready = wait_for_socket(&socket);
        tokio::pin!(ready);
        poll_fn(|cx| {
            assert!(ready.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        // Only listen changes now: no new filesystem event can wake the helper.
        assert_eq!(unsafe { libc::listen(fd.as_raw_fd(), 1) }, 0);
        let listener = UnixListener::from(fd);
        ready.await;
        assert!(socket_is_ready(&socket));
        listener
            .accept()
            .expect("readiness required a real connect");
    }

    #[test]
    fn synchronous_readiness_does_not_block_on_a_full_backlog() {
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

    #[tokio::test]
    async fn wait_for_socket_observes_creation_after_watch() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("atmd.sock");
        let ready = wait_for_socket(&socket);
        tokio::pin!(ready);
        poll_fn(|cx| {
            assert!(ready.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        ready.await;
        listener
            .accept()
            .expect("readiness required a real connect");
    }

    #[tokio::test]
    async fn wait_for_socket_connects_to_existing_listener() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("atmd.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        wait_for_socket(&socket).await;
        listener
            .accept()
            .expect("readiness required a real connect");
    }
}
