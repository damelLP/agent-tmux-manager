//! Real hook transport regression: stop the private daemon until the hook exits.
//! No model CLI or tmux server is started; every ordering edge is an event.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

#[test]
fn hook_event_survives_failed_handshake_reply() {
    let root = tempfile::Builder::new().prefix("t2.").tempdir().unwrap();
    for dir in ["tmp", "tmux", "state", "xdg", "config", "home"] {
        std::fs::create_dir(root.path().join(dir)).unwrap();
    }
    let output = Command::new("timeout")
        .args(["--kill-after=10s", "180s", "python3", "-I", "-u", "-c", RIG])
        .arg(root.path())
        .arg(env!("CARGO_BIN_EXE_atmd"))
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/crates/atm/scripts/atm-hook"
        ))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("BASH_ENV")
        .env_remove("ENV")
        .env("ATM_SOCKET", root.path().join("atm.sock"))
        .env("ATM_TMUX_SOCKET", "t2-hook")
        .env("TMPDIR", root.path().join("tmp"))
        .env("TMUX_TMPDIR", root.path().join("tmux"))
        .env("ATM_STATE_DIR", root.path().join("state"))
        .env("XDG_STATE_HOME", root.path().join("xdg"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("HOME", root.path().join("home"))
        .env("ATM_DEBUG", "0")
        .env("RUST_LOG", "atmd::server::connection=debug")
        .output()
        .expect("handshake regression requires timeout and python3 on PATH");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    println!("{stdout}");
}

// Python supplies waitpid(WUNTRACED/WCONTINUED), setsid, and pipe barriers without
// another test dependency. The outer timeout is only a hang guard; SIGTERM runs
// finally cleanup even when the daemon is stopped in its separate session.
const RIG: &str = r#"
import json
import os
from pathlib import Path
import queue
import shutil
import signal
import socket
import subprocess
import sys
import threading

root, binary, hook = map(Path, sys.argv[1:])
for tool in ('socat', 'jq'):
    assert shutil.which(tool), f'handshake regression requires {tool} on PATH'
assert Path('/bin/bash').is_file(), 'handshake regression requires /bin/bash'
address = os.environ['ATM_SOCKET']
assert Path(address).parent == root
assert 'TMUX' not in os.environ and 'TMUX_PANE' not in os.environ
assert Path(os.environ['TMUX_TMPDIR']).parent == root

# A timeout interrupts blocking waitpid/readline too, then runs cleanup.
def interrupted(signum, frame):
    raise RuntimeError(f'handshake regression interrupted by signal {signum}')
signal.signal(signal.SIGTERM, interrupted)

logs = []
lines = queue.Queue()
def collect(stream):
    for line in stream:
        logs.append(line.rstrip())
        lines.put(line)
    lines.put(None)

identities = {}
def executable_identity(proc):
    stat = Path(f'/proc/{proc.pid}/exe').stat()
    return stat.st_dev, stat.st_ino

def verify(proc):
    assert proc.poll() is None, f'child {proc.pid} exited early: {proc.returncode}'
    assert executable_identity(proc) == identities[proc.pid]
    environ = Path(f'/proc/{proc.pid}/environ').read_bytes().split(b'\0')
    assert f'ATM_SOCKET={address}'.encode() in environ

def send_signal(proc, sig, group=False):
    verify(proc)
    if group:
        assert os.getpgid(proc.pid) == proc.pid
        os.killpg(proc.pid, sig)
    else:
        os.kill(proc.pid, sig)

def stop_child(proc, group=False):
    if proc is None or proc.poll() is not None:
        return
    # In particular, a stopped daemon must resume before termination.
    send_signal(proc, signal.SIGCONT, group)
    send_signal(proc, signal.SIGTERM, group)
    try:
        proc.wait(timeout=30)
    except subprocess.TimeoutExpired:
        send_signal(proc, signal.SIGKILL, group)
        proc.wait(timeout=30)

def connect():
    sock = socket.socket(socket.AF_UNIX)
    sock.settimeout(60)
    sock.connect(address)
    return sock, sock.makefile('rb')

def send(sock, kind, **data):
    message = {'protocol_version': {'major': 1, 'minor': 0}, 'type': kind, **data}
    sock.sendall((json.dumps(message) + '\n').encode())

def receive(reader):
    line = reader.readline()
    assert line, 'private daemon closed connection before expected frame'
    return json.loads(line)

daemon = parent = None
subscriber = reader = None
log_thread = None
try:
    daemon = subprocess.Popen([str(binary), 'start'], start_new_session=True,
                              stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    identities[daemon.pid] = executable_identity(daemon)
    log_thread = threading.Thread(target=collect, args=(daemon.stdout,), daemon=True)
    log_thread.start()
    while True:
        line = lines.get(timeout=60)
        assert line is not None, 'daemon exited before listening'
        if 'Daemon server listening' in line:
            break
    assert os.getsid(daemon.pid) == daemon.pid, 'daemon must not share the pane session'
    subscriber, reader = connect()
    send(subscriber, 'connect', client_id='handshake-regression-observer')
    assert receive(reader)['type'] == 'connected'
    send(subscriber, 'subscribe')
    assert receive(reader)['type'] == 'session_list'
    print('subscriber acknowledged before daemon stop')

    # The actual script resolves this real shell's comm through /proc. Keep it
    # alive after hook exit until the session is observed, avoiding stale removal.
    launcher = root / 'claude'
    launcher.symlink_to('/bin/bash')
    payload = root / 'hook.json'
    session_id = 'handshake-hook-delivery'
    payload.write_text(json.dumps({'session_id': session_id,
                                  'hook_event_name': 'SessionStart', 'source': 'startup'}))
    script = '''printf 'READY %s\n' "$$"
read -r gate
/bin/bash "$1" < "$2"
rc=$?
printf 'HOOK_EXIT %s\n' "$rc"
read -r gate
exit "$rc"
'''
    parent = subprocess.Popen([str(launcher), '-c', script, '--', str(hook), str(payload)],
                              stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                              stderr=subprocess.STDOUT, text=True, start_new_session=True)
    identities[parent.pid] = executable_identity(parent)
    ready = parent.stdout.readline().strip()
    assert ready == f'READY {parent.pid}'
    assert Path(f'/proc/{parent.pid}/comm').read_text().strip() == 'claude'
    send_signal(daemon, signal.SIGSTOP)
    pid, status = os.waitpid(daemon.pid, os.WUNTRACED)
    assert pid == daemon.pid and os.WIFSTOPPED(status)
    assert os.WSTOPSIG(status) == signal.SIGSTOP
    print('daemon SIGSTOP confirmed by waitpid')

    parent.stdin.write('run\n')
    parent.stdin.flush()
    # This marker is emitted only AFTER the unchanged hook exits. Any hook
    # stdout/stderr would appear before it and fail the equality assertion.
    hook_exit = parent.stdout.readline().strip()
    assert hook_exit == 'HOOK_EXIT 0', 'hook must exit silently with status 0'
    print('real atm-hook exited while daemon remained stopped')
    send_signal(daemon, signal.SIGCONT)
    pid, status = os.waitpid(daemon.pid, os.WCONTINUED)
    assert pid == daemon.pid and os.WIFCONTINUED(status)
    print('daemon SIGCONT confirmed by waitpid')

    while True:
        try:
            event = receive(reader)
        except TimeoutError as error:
            # The observer is connection 0; any later connection before the
            # snapshot query is the hook. Processing logs carry its session id.
            hook_connected = sum('New client connected' in line for line in logs) > 1
            hook_processed = any('Processing hook event' in line and session_id in line for line in logs)
            if not hook_connected and not hook_processed:
                cause = 'the hook did not deliver within its 0.1 s transport timeout (load?), not a daemon drop'
            else:
                cause = 'the daemon dropped it'
            raise AssertionError(cause + '\n' + '\n'.join(logs)) from error
        if event['type'] == 'session_updated' and event['session']['id'] == session_id:
            row = event['session']
            assert row['harness'] == 'claude', row
            print('subscriber observed hook session:', json.dumps(row))
            break
    # Query the snapshot independently; discovery cannot substitute for the
    # unique SessionStart id, and no status-line event was sent in this rig.
    check, check_reader = connect()
    try:
        send(check, 'connect')
        assert receive(check_reader)['type'] == 'connected'
        send(check, 'list_sessions')
        snapshot = receive(check_reader)
        assert snapshot['type'] == 'session_list'
        assert any(r['id'] == session_id and r['harness'] == 'claude' for r in snapshot['sessions'])
    finally:
        check_reader.close()
        check.close()
    parent.stdin.write('done\n')
    parent.stdin.flush()
    assert parent.wait(timeout=30) == 0
    reader.close()
    subscriber.close()
    send_signal(daemon, signal.SIGTERM)
    assert daemon.wait(timeout=30) == 0
    log_thread.join(timeout=30)
    assert not log_thread.is_alive()
    assert any('Handshake reply failed; draining one-way events' in line and 'Broken pipe' in line
               for line in logs), 'real transport did not exercise the broken reply path'
    # SessionView does not expose pid. The daemon's received-payload log records
    # the real ancestry-selected pid; the subscriber/snapshot prove application.
    assert any('Processing hook event' in line and session_id in line and f'Some({parent.pid})' in line
               for line in logs), 'hook did not carry the stand-in pid'
    print('verified private daemon and stand-in reaped')
finally:
    cleanup_errors = []
    # An identity/cleanup failure for one child must not strand the other.
    for child, group in ((parent, True), (daemon, False)):
        try:
            stop_child(child, group)
        except Exception as error:
            cleanup_errors.append(f'child {child.pid}: {error}')
    if log_thread:
        log_thread.join(timeout=30)
    print('daemon log:\n' + '\n'.join(logs))
    if cleanup_errors:
        raise RuntimeError('cleanup failed: ' + '; '.join(cleanup_errors))
"#;
