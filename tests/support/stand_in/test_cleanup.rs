//! Deterministic cleanup races: absence is stopped, changed identity is refused.
use super::*;

fn server(rig: &StandInRig) -> (PathBuf, u32) {
    let identity = checked(rig.tmux(&["display-message", "-p", "#{socket_path} #{pid}"]));
    let (socket, pid) = identity.rsplit_once(' ').unwrap();
    let socket = PathBuf::from(socket);
    assert!(socket.starts_with(rig.root().join("tmux").canonicalize().unwrap()));
    (socket, pid.parse().unwrap())
}

fn stop_server(rig: &StandInRig) {
    let (socket, pid) = server(rig);
    assert_eq!(server(rig), (socket.clone(), pid));
    checked(process::run(rig.command(Path::new("tmux")).args([
        "-S",
        socket.to_str().unwrap(),
        "kill-server",
    ])));
    process::wait_exit(pid, Duration::from_secs(30)).unwrap();
}

#[test]
fn stopped_and_replaced_private_servers_clean_up_without_stale_pid() {
    for replace in [false, true] {
        let mut rig = StandInRig::start();
        let old = server(&rig);
        stop_server(&rig);
        if replace {
            checked(rig.tmux(&[
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "replacement",
                "/bin/bash --noprofile --norc",
            ]));
            let current = server(&rig);
            assert_eq!(current.0, old.0);
            assert_ne!(current.1, old.1);
            // Record the result before assertion so a red run still cleans up.
            let current_env = rig.tmux_env();
            let result = rig.cleanup();
            if result.is_err() {
                stop_server(&rig);
            }
            assert!(result.is_ok(), "{result:?}");
            assert_eq!(
                current_env,
                format!("{},{},0", current.0.display(), current.1)
            );
            process::wait_exit(current.1, Duration::from_secs(30)).unwrap();
        } else {
            assert!(rig.cleanup().is_ok(), "an already-stopped server is clean");
        }
        assert!(rig.cleanup().is_ok(), "cleanup is idempotent");
    }
}

#[test]
fn vanished_or_reused_process_is_never_signalled() {
    let calls = std::cell::Cell::new(0);
    for current in [None, Some("replacement")] {
        let result = signal_if_unchanged("original", current, || {
            calls.set(calls.get() + 1);
            Ok(())
        });
        assert_eq!(result.is_ok(), current.is_none());
    }
    assert_eq!(calls.get(), 0);
    signal_if_unchanged("original", Some("original"), || {
        calls.set(calls.get() + 1);
        Ok(())
    })
    .unwrap();
    assert_eq!(calls.get(), 1);
}

#[test]
fn detached_daemon_log_provides_lost_handshake_diagnostics() {
    let mut rig = StandInRig::start();
    let log = rig.root().join("state/atm/atm.log");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, "Handshake failed: Broken pipe").unwrap();
    let diagnostics = rig.diagnostics();
    assert!(diagnostics.contains("state/atm/atm.log"));
    assert!(diagnostics.contains("Possible lost hook"));
    rig.close();
}

#[test]
fn foreign_socket_is_refused_without_affecting_either_server() {
    let mut first = StandInRig::start();
    let mut second = StandInRig::start();
    let before = server(&second);
    let original = first.tmux_identity.take();
    first.tmux_identity = second.tmux_identity.clone();
    let result = first.stop_tmux();
    first.tmux_identity = original;
    assert!(result.is_err());
    assert_eq!(server(&second), before);
    first.close();
    second.close();
}

#[test]
fn drop_retains_run_directory_when_cleanup_refuses_an_identity() {
    let mut rig = StandInRig::start();
    let mut foreign = StandInRig::start();
    let root = rig.root().to_owned();
    stop_server(&rig);
    // Force an ownership refusal without leaving an owned server alive.
    rig.tmux_identity = foreign.tmux_identity.clone();
    let before = server(&foreign);
    drop(rig);
    let retained = root.is_dir();
    assert_eq!(server(&foreign), before);
    foreign.close();
    if retained {
        std::fs::remove_dir_all(&root).unwrap();
    }
    assert!(
        retained,
        "cleanup failure must preserve its diagnostic run directory"
    );
}

#[test]
fn daemon_error_still_cleans_tmux_and_returns_first_error() {
    let mut rig = StandInRig::start();
    let (_, pid) = server(&rig);
    let result = rig.cleanup_with(|_| Err("injected daemon error".into()));
    assert_eq!(result, Err("injected daemon error".into()));
    assert!(rig.tmux_identity.is_none(), "tmux cleanup must still run");
    process::wait_exit(pid, Duration::from_secs(30)).unwrap();
    // The injected boundary did not actually stop the daemon; real cleanup owns it.
    rig.close();
}
