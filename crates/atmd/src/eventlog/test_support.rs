//! Isolated libtest children with a positive assertion that their case ran.

use std::io::{Read, Seek, SeekFrom};
use std::process::Command;
use std::time::{Duration, Instant};

pub(super) fn run_child(test: &str, configure: impl FnOnce(&mut Command)) {
    let mut output = tempfile::tempfile().unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test])
        .stdout(output.try_clone().unwrap())
        .stderr(output.try_clone().unwrap());
    configure(&mut command);
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("child test {test} timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
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

#[test]
#[should_panic(expected = "expected exactly one child test")]
fn a_missing_child_filter_cannot_pass_vacuously() {
    run_child("eventlog::nonexistent_child_case", |_| {});
}
