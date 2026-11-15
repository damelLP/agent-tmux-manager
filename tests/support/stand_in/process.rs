//! Linux process-exit notification for the stand-in's synchronous test boundary.
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const GUARD: Duration = Duration::from_secs(30);

pub fn wait_exit(pid: u32, timeout: Duration) -> std::io::Result<()> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd == -1 {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        };
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    let deadline = Instant::now() + timeout;
    loop {
        let mut event = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let milliseconds = deadline
            .saturating_duration_since(Instant::now())
            .as_millis();
        let result = unsafe {
            libc::poll(
                &mut event,
                1,
                i32::try_from(milliseconds).unwrap_or(i32::MAX),
            )
        };
        if result > 0 {
            return Ok(());
        }
        if result == 0 {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Own a background command until its output is collected or its caller unwinds.
pub struct OwnedChild(Option<Child>);

impl OwnedChild {
    pub fn new(child: Child) -> Self {
        Self(Some(child))
    }

    pub fn wait_with_output(mut self) -> std::io::Result<Output> {
        self.0.take().unwrap().wait_with_output()
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            // Still unreaped and owned by us: this PID cannot name a replacement.
            let _ = child.kill();
            if wait_exit(child.id(), GUARD).is_ok() {
                let _ = child.wait();
            }
        }
    }
}

pub fn run(command: &mut Command) -> Output {
    // Files retain arbitrarily large output without blocking the child on pipes.
    let mut stdout = tempfile::tempfile().unwrap();
    let mut stderr = tempfile::tempfile().unwrap();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(stdout.try_clone().unwrap())
        .stderr(stderr.try_clone().unwrap())
        .spawn()
        .expect("spawn stand-in command");
    if let Err(error) = wait_exit(child.id(), GUARD) {
        // This unreaped child cannot have a reused PID.
        let _ = child.kill();
        let _ = child.wait();
        panic!("stand-in command failed to exit: {command:?}: {error}");
    }
    let mut output = Output {
        status: child.wait().unwrap(),
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    stdout.seek(SeekFrom::Start(0)).unwrap();
    stderr.seek(SeekFrom::Start(0)).unwrap();
    stdout.read_to_end(&mut output.stdout).unwrap();
    stderr.read_to_end(&mut output.stderr).unwrap();
    output
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn owned_child_drop_reaps_on_unwind() {
        use std::io::{BufRead, BufReader};

        // A pinned fallback also cleans up a mutant that omits OwnedChild's Drop.
        struct Fallback(OwnedFd);
        impl Drop for Fallback {
            fn drop(&mut self) {
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        self.0.as_raw_fd(),
                        libc::SIGKILL,
                        0,
                        0,
                    );
                    let mut event = libc::pollfd {
                        fd: self.0.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    if libc::poll(&mut event, 1, 30_000) > 0 {
                        let mut status = std::mem::zeroed();
                        libc::waitid(
                            libc::P_PIDFD,
                            self.0.as_raw_fd() as u32,
                            &mut status,
                            libc::WEXITED | libc::WNOHANG,
                        );
                    }
                }
            }
        }

        let mut child = Command::new("/bin/sh")
            .args(["-c", "printf 'ready\\n'; read -r finish"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        // Keep stdin open independently: simply dropping Child must not release it.
        let _input = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) };
        assert!(raw >= 0);
        let fallback = Fallback(unsafe { OwnedFd::from_raw_fd(raw as i32) });
        let mut ready = String::new();
        BufReader::new(stdout).read_line(&mut ready).unwrap();
        assert_eq!(ready, "ready\n");
        let result = std::panic::catch_unwind(|| {
            let _child = OwnedChild::new(child);
            panic!("unwind before the background command connects");
        });
        assert!(result.is_err());
        // Drop promises completion: inspect its exit event, do not wait for a race.
        let mut event = libc::pollfd {
            fd: fallback.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert!(
            unsafe { libc::poll(&mut event, 1, 0) } > 0,
            "Drop left the child running"
        );
        let mut status = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe {
                libc::waitid(
                    libc::P_PIDFD,
                    fallback.0.as_raw_fd() as u32,
                    &mut status,
                    libc::WEXITED | libc::WNOHANG,
                )
            },
            -1,
            "Drop did not reap the child"
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    pub fn command_exit_returns_complete_large_output_without_pipe_deadlock() {
        let output = run(Command::new("/usr/bin/python3").args([
            "-c",
            "import sys; print('x'*131072); print('error text', file=sys.stderr)",
        ]));
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 131073);
        assert_eq!(output.stderr, b"error text\n");
    }
}
