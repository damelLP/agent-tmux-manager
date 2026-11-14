//! Linux process-exit notification for the stand-in's synchronous test boundary.
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Command, Output, Stdio};
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
