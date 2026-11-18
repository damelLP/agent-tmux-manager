//! Isolated libtest children with a positive assertion that their case ran.

use std::io::{Read, Seek, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

pub(super) fn run_child(test: &str, configure: impl FnOnce(&mut Command)) {
    let mut output = tempfile::tempfile().unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test])
        .stdout(output.try_clone().unwrap())
        .stderr(output.try_clone().unwrap());
    configure(&mut command);
    let mut child = StopChild(command.spawn().unwrap());
    let status = wait_exit(&mut child.0);
    output.seek(SeekFrom::Start(0)).unwrap();
    let mut text = String::new();
    output.read_to_string(&mut text).unwrap();
    assert!(status.success(), "child {test}: {status}\n{text}");
    assert!(
        text.lines()
            .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed;")),
        "expected exactly one child test for {test}\n{text}"
    );
}

pub(crate) fn isolated(test: &str) -> bool {
    if std::env::var("ATM_TEST_EVENTLOG_CASE").as_deref() == Ok(test) {
        return false;
    }
    run_child(test, |command| {
        command.env("ATM_TEST_EVENTLOG_CASE", test);
    });
    true
}

struct StopChild(Child);

impl Drop for StopChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_exit(child: &mut Child) -> ExitStatus {
    if let Some(status) = child.try_wait().unwrap() {
        return status;
    }
    // This unreaped child cannot have a reused PID. pidfd readiness also
    // retains an exit that happens before poll registers its interest.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) };
    assert!(raw >= 0, "pidfd_open: {}", std::io::Error::last_os_error());
    let fd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut event = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        let ready = unsafe {
            libc::poll(
                &mut event,
                1,
                i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX),
            )
        };
        if ready > 0 {
            return child.wait().unwrap();
        }
        assert!(ready != 0, "child test timed out");
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::Interrupted
        );
    }
}

#[test]
fn an_exit_recorded_before_the_wait_is_not_missed() {
    let mut child = StopChild(
        Command::new("/bin/sh")
            .args(["-c", "exit 23"])
            .spawn()
            .unwrap(),
    );
    assert_eq!(child.0.wait().unwrap().code(), Some(23));
    assert_eq!(wait_exit(&mut child.0).code(), Some(23));
}

#[test]
#[should_panic(expected = "expected exactly one child test")]
fn a_missing_child_filter_cannot_pass_vacuously() {
    run_child("eventlog::nonexistent_child_case", |_| {});
}
