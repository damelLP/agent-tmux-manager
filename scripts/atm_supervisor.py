"""Approved promotions, private rehearsals and bounded health probation. Linux, Python 3.9+.

Slot/report checksums detect drift, not a hostile owner rewriting every artifact.
Kit snapshots keep old slots verifiable when the checkout's kit changes.
"""
import argparse
from collections import deque
import contextlib
import copy
import datetime
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import select
import shlex
import shutil
import signal
import socket
import stat
import struct
import subprocess
import sys
import tempfile
import threading
import time
import uuid

from atm_candidate import Kit, Refused as KitRefused, clean_env, git_env, identity, load, lock_holders, process_env, run, save
from atm_promotion import NAMES, Refused, checked_path, directory, pair, previous, regular, require
from atm_rollout import NeedsYou, fingerprint, listening

SCRIPTS = Path(__file__).resolve().parent
KIT_FILES = ('atm-candidate', 'atm_candidate.py', 'atm-promote', 'atm-rollback',
             'atm_promotion.py', 'atm-rollout', 'atm_rollout.py', 'atm-supervise',
             'atm_supervisor.py', 'atm-protected-files.py', 'protected-files.txt')


def checksum(data):
    return hashlib.sha256(json.dumps(data, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def file_hash(path):
    regular(path)
    return hashlib.sha256(path.read_bytes()).hexdigest()


def pair_record(folder):
    pair(folder)
    return {name: dict(sha256=digest, mode=mode)
            for name, (digest, mode) in fingerprint(folder).items()}


def kit_hash(folder):
    directory(folder, private=True)
    return checksum({path.name: file_hash(path) for path in folder.iterdir()})


def retain_kit(metadata):
    kits = metadata / 'kits'
    kits.mkdir(mode=0o700, exist_ok=True)
    directory(kits, private=True)
    stage = Path(tempfile.mkdtemp(prefix='.kit-', dir=kits))
    try:
        for name in KIT_FILES:
            regular(SCRIPTS / name)
            shutil.copy2(SCRIPTS / name, stage / name)
        digest = kit_hash(stage)
        destination = kits / digest
        if destination.exists():
            require(kit_hash(destination) == digest, 'retained kit changed')
        else:
            stage.rename(destination)
        return 'kits/' + digest, digest
    finally:
        if stage.exists():
            shutil.rmtree(stage)  # only this call's new, unpublished kit snapshot


def save_slots(path, slots):
    """One atomic rename, including the checksum; a failed write keeps the old record."""
    save(path, dict(sha256=checksum(slots), slots=slots))


@contextlib.contextmanager
def supervisor_lock(metadata):
    directory(metadata, private=True)
    path = metadata / 'supervisor.lock'
    if os.path.lexists(path):
        regular(path)
    fd = os.open(path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
    with os.fdopen(fd, 'r+') as lock:
        require(stat.S_ISREG(os.fstat(lock.fileno()).st_mode), 'invalid supervisor lock')
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise Refused('another supervisor holds the lock: ' + lock_holders(lock)) from error
        yield


def valid_hash(value):
    return isinstance(value, str) and re.fullmatch('[0-9a-f]{64}', value) is not None


def validate_record(record):
    require(isinstance(record, dict), 'invalid slot record')
    commit = record.get('commit')
    require(isinstance(commit, str) and re.fullmatch('[0-9a-f]{40}', commit), 'invalid slot commit')
    require(record.get('install') in (commit + '-' + profile for profile in ('release', 'debug', 'prebuilt')),
            'slot install identity disagrees with commit')
    binaries = record.get('pair')
    require(isinstance(binaries, dict) and set(binaries) == set(NAMES), 'invalid slot pair')
    for binary in binaries.values():
        require(isinstance(binary, dict) and valid_hash(binary.get('sha256'))
                and type(binary.get('mode')) is int and 0 <= binary.get('mode') <= 0o7777,
                'invalid slot binary hash/mode')
    require(all(valid_hash(record.get(key)) for key in
                ('kit_sha256', 'report_sha256', 'install_manifest_sha256')), 'invalid slot artifact hash')
    require(record.get('kit') == 'kits/' + record.get('kit_sha256'), 'invalid retained kit path')
    report = record.get('report')
    require(isinstance(report, str) and re.fullmatch(r'reports/[a-z0-9_-]+\.json', report)
            and record.get('report_kind') in ('baseline', 'gate'), 'invalid slot report')


def stage_candidate(slots, record, approval):
    """Bind one unused approval to a gate report; staging never rotates stable."""
    validate_record(record)
    records = [slots.get(key) for key in ('stable', 'previous', 'candidate')]
    records.extend(slots.get('rejected', []))
    baseline = {item.get('report_sha256') for item in records
                if item is not None and item.get('report_kind') == 'baseline'}
    require(record.get('report_kind') != 'baseline' and approval not in baseline,
            'a baseline observation cannot be an approval')
    require(valid_hash(approval) and approval == record.get('report_sha256'),
            'approval must match the gate report sha256')
    require(slots.get('candidate') is None, 'a candidate is already pending')
    key = (record.get('commit'), checksum(record.get('pair')))
    for rejected in slots.get('rejected', []):
        require((rejected.get('commit'), checksum(rejected.get('pair'))) != key
                or rejected.get('report_sha256') != approval,
                'rejected candidate needs a new approved report')
    require(approval not in slots.get('used_approvals', []), 'approval hash already used')
    result = copy.deepcopy(slots)
    result.update(candidate=copy.deepcopy(record),
                  used_approvals=[*slots.get('used_approvals', []), approval])
    return result


def accept_candidate(slots):
    """The sole rotation point, called only after acceptance (no U2 CLI caller)."""
    require(slots.get('candidate') is not None, 'no candidate to accept')
    result = copy.deepcopy(slots)
    result.update(previous=result.get('stable'), stable=result.get('candidate'), candidate=None)
    return result


def reject_candidate(slots, reason, evidence):
    """Retain the last three failures; stable and previous do not move."""
    require(slots.get('candidate') is not None, 'no candidate to reject')
    require(isinstance(reason, str) and reason.strip() and isinstance(evidence, dict),
            'rejection needs a reason and evidence object')
    result = copy.deepcopy(slots)
    rejected = deque(result.get('rejected', []), maxlen=3)
    rejected.append(dict(result.get('candidate'), reason=reason, evidence=copy.deepcopy(evidence)))
    result.update(candidate=None, rejected=list(rejected))
    return result


@contextlib.contextmanager
def gate_worktree(repo, path, commit):
    """C's detached-worktree/unique-tree cleanup pattern; never prune other trees."""
    environment = git_env(os.environ)
    try:
        # Git can register the tree before add times out or is interrupted.
        run(['git', 'worktree', 'add', '--detach', path, commit], env=environment, cwd=repo)
        yield path
    finally:
        try:
            run(['git', 'worktree', 'remove', '--force', path], env=environment, cwd=repo)
        except (OSError, subprocess.SubprocessError):
            if path.exists():
                shutil.rmtree(path)  # only this gate's newly created source tree
            registered = run(['git', 'worktree', 'list', '--porcelain'], env=environment, cwd=repo)
            if 'worktree ' + str(path) in registered.splitlines():
                run(['git', 'worktree', 'remove', '--force', path], env=environment, cwd=repo)


def gate_step(steps, name, command):
    entry = dict(name=name, command=list(map(str, command)), result={})
    steps.append(entry)
    return entry


def gate_call(step, environment, cwd, timeout):
    with subprocess.Popen(step.get('command'), env=environment, cwd=cwd, stdin=subprocess.DEVNULL,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, errors='replace') as child:
        try:
            stdout, stderr = child.communicate(timeout=timeout)
        except (subprocess.TimeoutExpired, KeyboardInterrupt):
            child.kill()
            try:
                stdout, stderr = child.communicate(timeout=90)
            except subprocess.TimeoutExpired as captured:
                stdout, stderr = (value.decode(errors='replace') if isinstance(value, bytes) else value or ''
                                  for value in (captured.stdout, captured.stderr))
            step.update(result=dict(returncode=child.poll(), stdout=stdout, stderr=stderr))
            raise
        result = subprocess.CompletedProcess(step.get('command'), child.returncode, stdout, stderr)
        step.update(result=dict(returncode=result.returncode, stdout=result.stdout, stderr=result.stderr))
        return result


def check_gate_pass(text, commit, runner_label):
    """Require one complete local runner record, not an unrelated success substring."""
    require(runner_label.strip() and '\n' not in runner_label and '\r' not in runner_label,
            'gate runner label must be nonblank and single-line')
    label = re.escape(runner_label)
    outcomes = re.findall(r'^' + label + r': GATE (PASSED|FAILED)\b', text, re.M)
    require(outcomes == ['PASSED'], 'gate-pass has failed or conflicting outcome lines')
    started = re.findall(r'^gate start [^\n]+ head ([0-9a-f]{40}) run dir .+$', text, re.M)
    ended = re.findall(r'^' + label + r' exit ([0-9]+) after [0-9]+ s; log .+; head ([0-9a-f]{40})$', text, re.M)
    passed = re.findall(r'^' + label + r': GATE PASSED ([0-9a-f]{40}) run [0-9]+ \([^\n]+\)$', text, re.M)
    require(started == [commit] and ended == [('0', commit)] and passed == [commit]
            and not re.search(r'^LEFTOVER\b', text, re.M),
            'gate-pass must record one passed gate, exit 0 and matching candidate heads, without leftovers')


def rollout_evidence(text, before, after):
    """R prints the protected report followed by its hash, then its quiet snapshot."""
    pattern = (r'^(Comparing ' + re.escape(before + '..' + after) + r'\n.*?)'
               r'^Report sha256: ([0-9a-f]{64})\n(Sessions: .*?)'
               r'^Dry run: nothing was changed\. The run would:\n')
    matches = re.findall(pattern, text, re.M | re.S)
    require(len(matches) == 1, 'rollout did not return one protected report and quiet snapshot')
    # Exactly one match was validated above.
    report, digest, snapshot = next(iter(matches))
    require(hashlib.sha256(report.encode()).hexdigest() == digest, 'rollout protected report hash mismatch')
    return dict(protected_report=report, report_sha256=digest, quiet_snapshot=snapshot)


def rehearsal_frame(line, step):
    frame = json.loads(line)
    require(isinstance(frame, dict), 'invalid rehearsal frame')
    step.update(result=frame)
    require(frame.get('type') not in ('error', 'rejected'), 'rehearsal socket refused: ' + json.dumps(frame))
    return frame


def rehearsal_processes(root):
    """Positive ownership only; exited processes are not surviving resources."""
    result = []
    for path in Path('/proc').iterdir():
        if not path.name.isdecimal() or int(path.name) == os.getpid():
            continue
        process = identity(path.name)
        if process is None:
            continue
        prefix = os.fsencode(str(root) + '/')
        if (str(process.get('exe')).startswith(str(root) + '/')
                or any(prefix in value for value in process_env(path.name).values())):
            try:
                result.append(dict(process, name=(path / 'comm').read_text().strip()))
            except FileNotFoundError:
                pass
    return result


def rehearsal_signal(root, process, sig):
    """A pidfd pins exit/signals; recheck start time and private ownership first."""
    try:
        fd = os.pidfd_open(process.get('pid'))
    except ProcessLookupError:
        return
    try:
        current = next((item for item in rehearsal_processes(root)
                        if item.get('pid') == process.get('pid') and item.get('start') == process.get('start')), None)
        if current is not None:
            try:
                signal.pidfd_send_signal(fd, sig)
            except ProcessLookupError:
                pass
        exited, _, _ = select.select([fd], [], [], 90)
        require(exited, 'private rehearsal process did not exit')
    finally:
        os.close(fd)


class Rehearsal:
    """One disposable, model-free state lineage; no source of live state is accepted."""
    def __init__(self, root):
        self.root, self.steps = root, []
        self.daemon = self.reader = self.started = None
        self.logs = []
        self.server = root / 'tmux' / ('tmux-' + str(os.getuid())) / 'rehearsal'
        self.env = git_env(clean_env())
        # Spawn overrides must not select an inherited real harness.
        self.env = {key: value for key, value in self.env.items()
                    if not key.startswith(('ATM_SPAWN_', 'ATM_STANDIN_')) and key not in ('BASH_ENV', 'ENV')}
        self.env.update(HOME=str(root / 'home'), SHELL='/bin/sh', TMPDIR=str(root / 'tmp'),
                        PATH=str(root / 'bin') + ':/usr/local/bin:/usr/bin:/bin', ATM_SOCKET=str(root / 'a.sock'),
                        ATM_STATE_DIR=str(root / 'state'), ATM_TMUX_SOCKET='rehearsal',
                        TMUX_TMPDIR=str(root / 'tmux'), XDG_STATE_HOME=str(root / 'state'),
                        XDG_CONFIG_HOME=str(root / 'config'), XDG_DATA_HOME=str(root / 'data'),
                        XDG_CACHE_HOME=str(root / 'cache'), ATM_SPAWN_BIN=str(root / 'bin/pi'),
                        ATM_STANDIN_ROOT=str(root), ATM_STANDIN_LOG=str(root / 'standin.jsonl'),
                        ATM_STANDIN_ID_PREFIX='rehearsal', PYTHONDONTWRITEBYTECODE='1',
                        TERM='xterm-256color', LANG='C.UTF-8')

    def command(self, name, args, env=None):
        step = gate_step(self.steps, name, args)
        result = gate_call(step, env or self.env, self.root, 90)
        require(result.returncode == 0, f'{name}: exit {result.returncode}')
        return result.stdout.strip()

    def start(self, phase):
        install, _, _ = phase.partition('-')
        self.started = gate_step(self.steps, phase + '.start', [self.root / 'bin' / install / 'atmd', 'start'])
        self.logs = []
        ready = threading.Event()
        bound = []
        self.daemon = subprocess.Popen(self.started.get('command'), env=self.env, stdin=subprocess.DEVNULL,
                                       stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                                       errors='replace', start_new_session=True)
        def read_log():
            for line in self.daemon.stdout:
                self.logs.append(line)
                if 'Daemon server listening' in line and self.env.get('ATM_SOCKET') in line:
                    bound.append(True)
                    ready.set()
            ready.set()  # EOF is an exit event, including an old build refusing the log
        self.reader = threading.Thread(target=read_log, name='rehearsal-log', daemon=True)
        self.reader.start()
        require(ready.wait(90), 'private daemon did not report readiness')
        self.started.update(result=dict(stdout=''.join(self.logs), stderr='', returncode=self.daemon.poll()))
        require(bound, phase + ': daemon exited before listening')
        # One real handshake, never an atm invocation that could auto-start.
        with self.connection() as (_, stream):
            require(json.loads(stream.readline()).get('type') == 'connected', 'private daemon handshake failed')

    @contextlib.contextmanager
    def connection(self):
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(90)
            connection.connect(self.env.get('ATM_SOCKET'))
            connection.sendall(b'{"protocol_version":{"major":1,"minor":1},"type":"connect"}\n')
            with connection.makefile('r') as stream:
                yield connection, stream

    def stop(self):
        if self.daemon is None:
            return None
        step = gate_step(self.steps, self.started.get('name').removesuffix('.start') + '.stop',
                         ['signal', 'SIGTERM', self.daemon.pid])
        try:
            process = identity(self.daemon.pid)
            if process is not None:
                rehearsal_signal(self.root, process, signal.SIGTERM)
            code = self.daemon.wait(timeout=90)
            if self.reader is not None:
                self.reader.join(timeout=90)
                require(not self.reader.is_alive(), 'private daemon log reader did not exit')
            output = dict(returncode=code, stdout=''.join(self.logs), stderr='')
            self.started.update(result=output)
            step.update(result=output.copy())
            return code
        finally:
            if self.daemon.stdout is not None and (self.reader is None or not self.reader.is_alive()):
                self.daemon.stdout.close()
            self.daemon = None

    def sequence(self, phase, caller):
        atm = self.root / 'bin' / phase / 'atm'
        with self.connection() as (watch_socket, watch):
            require(json.loads(watch.readline()).get('type') == 'connected', 'watch handshake failed')
            watch_socket.sendall(b'{"protocol_version":{"major":1,"minor":1},"type":"watch","topics":[],"cursor":0}\n')
            up = json.loads(self.command(phase + '.up', [atm, 'agent', 'up', 'rehearsal', '--project-root',
                                self.root / 'project', '--format', 'json'], caller))
            require(up.get('state') == 'ready', 'rehearsal agent is not ready')
            agent = up.get('agent_id')
            sent = json.loads(self.command(phase + '.message', [atm, 'msg', 'send', '--to', agent,
                              '--body', 'supervisor rehearsal ' + phase, '--project-root',
                              self.root / 'project', '--format', 'json']))
            step = gate_step(self.steps, phase + '.receipt', ['watch', 'msg.delivered', sent.get('message_id')])
            while True:
                frame = rehearsal_frame(watch.readline(), step)
                if (frame.get('type') == 'event' and frame.get('kind') == 'msg.delivered'
                        and frame.get('body', {}).get('message_id') == sent.get('message_id')):
                    step.update(result=frame)
                    session = frame.get('body', {}).get('session_id')
                    break
        # Subscribe before taking the fresh snapshot; never reuse a startup Idle.
        step = gate_step(self.steps, phase + '.idle', ['subscribe', session, 'idle'])
        with self.connection() as (rows_socket, rows):
            require(json.loads(rows.readline()).get('type') == 'connected', 'subscription handshake failed')
            rows_socket.sendall(b'{"protocol_version":{"major":1,"minor":1},"type":"subscribe"}\n')
            while True:
                frame = rehearsal_frame(rows.readline(), step)
                sessions = frame.get('sessions', []) if frame.get('type') == 'session_list' else [frame.get('session', {})]
                if any(row.get('id') == session and row.get('status') == 'idle' for row in sessions):
                    step.update(result=frame)
                    break
        cleared = json.loads(self.command(phase + '.clear', [atm, 'agent', 'clear', agent, '--format', 'json']))
        require(cleared.get('session_id') and cleared.get('session_id') != session, 'clear did not replace the session')
        self.command(phase + '.down', [atm, 'agent', 'down', agent, '--kill', '--format', 'json'])
        listed = json.loads(self.command(phase + '.agents', [atm, 'agent', 'list', '--format', 'json']))
        require(any(row.get('agent_id') == agent and row.get('state') == 'down' for row in listed),
                'rehearsal agent was not recorded down')

    def cleanup(self):
        errors = []
        try:
            self.stop()
        except (OSError, ValueError, RuntimeError, Refused, subprocess.SubprocessError) as error:
            errors.append(str(error))
        processes = rehearsal_processes(self.root)
        servers = [item for item in processes if item.get('name', '').startswith('tmux')]
        if servers:
            try:
                actual = run(['tmux', '-S', self.server, 'display-message', '-p', '#{socket_path},#{pid}'], env=self.env)
                path, _, pid = actual.rpartition(',')
                require(path == str(self.server) and any(str(item.get('pid')) == pid for item in servers),
                        'private tmux socket/PID changed')
                run(['tmux', '-S', self.server, 'kill-server'], env=self.env)
            except (OSError, ValueError, Refused, KitRefused, subprocess.SubprocessError) as error:
                errors.append(str(error))
        for process in processes:
            try:
                rehearsal_signal(self.root, process, signal.SIGTERM)
            except (OSError, ValueError, Refused) as error:
                errors.append(str(error))
        remaining = rehearsal_processes(self.root)
        return dict(clean=not remaining and not errors, processes=len(remaining),
                    atmd=sum(item.get('name') == 'atmd' for item in remaining),
                    tmux=sum(item.get('name', '').startswith('tmux') for item in remaining)), errors


def rehearse(stable_bin, candidate_bin):
    root = Path(tempfile.mkdtemp(prefix='atm-rehearsal-', dir='/var/tmp'))
    runtime = Rehearsal(root)
    result = dict(runtime=str(root), compatibility='ONE-WAY', failure=None, steps=runtime.steps)
    try:
        for sub in ('home', 'tmp', 'tmux', 'state', 'config/atm', 'bin', 'project/.atm/roles'):
            (root / sub).mkdir(mode=0o700, parents=True, exist_ok=True)
        stand_in = SCRIPTS.parent / 'tests/support/stand_in/harness.py'
        regular(stand_in, executable=True)
        (root / 'bin/pi').symlink_to(stand_in)
        result.update(stand_in_sha256=file_hash(stand_in))
        for phase, source in (('stable', stable_bin), ('candidate', candidate_bin)):
            pair(source)
            destination = root / 'bin' / phase
            destination.mkdir(mode=0o700)
            for name in NAMES:
                shutil.copy2(source / name, destination / name)
        (root / 'config/atm/config.toml').write_text(
            '[delivery]\nstable_idle = { secs = 0, nanos = 1000000 }\n'
            'stable_empty_input = { secs = 0, nanos = 1000000 }\n')
        (root / 'project/.atm/roles/rehearsal.md').write_text("+++\nharness='pi'\n+++\n")
        pane = runtime.command('tmux.start', ['tmux', '-L', 'rehearsal', '-f', '/dev/null', 'new-session',
                               '-d', '-s', 'rehearsal', '-x', '200', '-y', '101', '-P', '-F', '#{pane_id}',
                               '/bin/bash --noprofile --norc'])
        server = runtime.command('tmux.identity', ['tmux', '-S', runtime.server, 'display-message', '-p', '#{socket_path},#{pid}'])
        path, _, pid = server.rpartition(',')
        require(path == str(runtime.server) and any(str(item.get('pid')) == pid
                for item in rehearsal_processes(root)), 'tmux server is not owned by the rehearsal')
        for key, value in (('default-shell', '/bin/bash'), ('status', 'off'),
                           ('default-command', '/bin/bash --noprofile --norc')):
            runtime.command('tmux.' + key, ['tmux', '-S', runtime.server, 'set-option', '-g', key, value])
        caller = dict(runtime.env, TMUX=server + ',0', TMUX_PANE=pane)
        for phase in ('stable', 'candidate'):
            runtime.start(phase)
            runtime.sequence(phase, caller)
            require(runtime.stop() == 0, phase + ': daemon failed while stopping')
        runtime.start('stable-read')
        atm = root / 'bin/stable/atm'
        json.loads(runtime.command('stable-read.list', [atm, 'list', '--format', 'json']))
        json.loads(runtime.command('stable-read.agents', [atm, 'agent', 'list', '--format', 'json']))
        require(runtime.stop() == 0, 'stable-read: daemon failed while stopping')
        result.update(compatibility='two-way')
    except (OSError, ValueError, TypeError, AttributeError, RuntimeError, Refused, KitRefused,
            subprocess.SubprocessError) as error:
        step = next(reversed(runtime.steps), {})
        output = step.get('result', {})
        result.update(failure=dict(step=step.get('name', 'prepare'), reason=str(error) or 'interrupted',
                                   output=(output.get('stdout', '') + output.get('stderr', '')
                                           or json.dumps(output, sort_keys=True))))
    finally:
        cleanup, errors = runtime.cleanup()
        result.update(cleanup=cleanup)
        if cleanup.get('clean'):
            shutil.rmtree(root)  # only this invocation's new, stopped runtime
        else:
            result.update(compatibility='ONE-WAY', cleanup_errors=errors)
    return result


def health_state(frame):
    """Validate the method envelope and complete known health shape, without I/O."""
    if not isinstance(frame, dict) or frame.get('type') != 'response' or frame.get('id') != 'health':
        return 'malformed'
    if ('result' in frame) == ('error' in frame):
        return 'malformed'
    error = frame.get('error')
    if isinstance(error, dict) and error.get('code') in ('unknown_method', 'unavailable'):
        return 'unknown' if error.get('code') == 'unknown_method' else 'unavailable'
    result = frame.get('result')
    if not isinstance(result, dict):
        return 'malformed'
    reservations, lifecycle = result.get('reservations'), result.get('lifecycle')
    if not isinstance(reservations, dict) or not isinstance(lifecycle, dict):
        return 'malformed'
    if (reservations.get('state') not in ('ok', 'full', 'failed')
            or lifecycle.get('state') not in ('ok', 'full', 'stopped', 'absent')
            or any(type(reservations.get(key)) is not int or reservations.get(key) < 0
                   for key in ('live', 'quarantined', 'capacity'))
            or type(lifecycle.get('capacity')) is not int or lifecycle.get('capacity') < 0
            or 'reason' not in reservations
            or reservations.get('reason') not in (None, 'quarantine_overflow')):
        return 'malformed'
    if lifecycle.get('state') == 'stopped':
        return 'stopped'
    if reservations.get('state') == 'failed':
        return 'failed'
    return 'full' if 'full' in (reservations.get('state'), lifecycle.get('state')) else 'ok'


def read_health(socket_path, pinned=None):
    """One bounded raw request. Peer credentials pin the daemon; no autostart CLI."""
    observation = dict(state='missing')
    if pinned is not None and identity(pinned.get('pid')) != pinned:
        return dict(state='changed' if identity(pinned.get('pid')) else 'dead')
    try:
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(10)  # method budget is 5 seconds
            connection.connect(str(socket_path))
            pid, uid, _ = struct.unpack('3i', connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
            peer = identity(pid)
            observation.update(identity=peer)
            if peer is None or uid != os.getuid() or (pinned is not None and peer != pinned):
                return dict(observation, state='changed')
            version = dict(major=1, minor=1)
            connection.sendall(json.dumps(dict(protocol_version=version, type='connect')).encode() + b'\n')
            with connection.makefile('r') as stream:
                handshake = json.loads(stream.readline(1024 * 1024))
                if not isinstance(handshake, dict) or handshake.get('type') != 'connected':
                    observation.update(state='malformed')
                else:
                    connection.sendall(json.dumps(dict(protocol_version=version, type='request', id='health',
                                                       method='daemon.health')).encode() + b'\n')
                    frame = json.loads(stream.readline(1024 * 1024))
                    observation.update(state=health_state(frame), response=frame)
    except ConnectionRefusedError as error:
        observation.update(state='refused', reason=str(error))
    except (OSError, ValueError) as error:
        observation.update(state='malformed' if isinstance(error, ValueError) else 'missing', reason=str(error))
    peer = observation.get('identity') or pinned
    if peer is not None:
        current = identity(peer.get('pid'))
        if current != peer:
            observation.update(state='changed' if current else 'dead')
    return observation


def health_verdict(reads):
    """Terminal failures need one read; invalid answers need three consecutive reads."""
    invalid, supported, pinned = 0, False, None
    verdict = 'pending'
    for observation in reads:
        state, peer = observation.get('state'), observation.get('identity')
        if state in ('failed', 'stopped', 'refused', 'changed', 'dead') or (pinned and peer and peer != pinned):
            return 'unhealthy'
        pinned = pinned or peer
        if state in ('ok', 'full'):
            supported, invalid = True, 0
            verdict = 'healthy' if state == 'ok' else 'full'
        elif state == 'unknown' and not supported:
            invalid, verdict = 0, 'unknown'
        else:
            invalid += 1
            verdict = 'pending'
            if invalid >= 3:
                return 'unhealthy'
    return verdict


def probation(read, clock, wait, duration=1800):
    """Read immediately, then on 15-second ticks, including the probation boundary."""
    started, reads = clock(), []
    while True:
        reads.append(read())
        verdict = health_verdict(reads)
        if verdict == 'unhealthy':
            return dict(result='unhealthy', reads=reads,
                        reason='unhealthy daemon: ' + next(reversed(reads), {}).get('state', 'invalid'))
        if clock() >= started + duration:
            break
        wait(max(0, min(started + 15 * len(reads), started + duration) - clock()))
    states = {item.get('state') for item in reads}
    if verdict not in ('healthy', 'unknown', 'full'):
        return dict(result='needs-you', reads=reads,
                    reason='health remained uncertain at the end of probation')
    if verdict == 'full' and 'ok' not in states:
        return dict(result='needs-you', reads=reads,
                    reason='full throughout probation' if states == {'full'} else 'health remained uncertain')
    return dict(result='accepted', reads=reads,
                note='health unknown; accepted on rollout checks' if verdict == 'unknown' else 'health probation passed')


def rollout_identity(text, bin_dir):
    """R reports the successful checks' PID; pin its starttime before probation."""
    pids = re.findall(r'^Version: [^\n]+, daemon pid ([0-9]+)$', text, re.M)
    pid = next(reversed(pids), None)
    pinned = identity(pid) if pid else None
    require(pinned is not None and pinned.get('exe') == str(bin_dir / 'atmd')
            and Path('/proc/' + str(pid) + '/exe').samefile(bin_dir / 'atmd'),
            'cannot pin the daemon checked by rollout')
    return pinned


def collect_logs(metadata, environment):
    """Only the two diagnostic logs are read; never inspect/copy persistent state."""
    evidence = metadata / 'evidence'
    evidence.mkdir(mode=0o700, exist_ok=True)
    directory(evidence, private=True)
    destination = Path(tempfile.mkdtemp(prefix='probation-', dir=evidence))
    state = Path(environment.get('ATM_STATE_DIR') or
                 str(Path(environment.get('XDG_STATE_HOME') or Path.home() / '.local/state') / 'atm'))
    files = {}
    for name in ('atm.log', 'atm.log.1'):
        try:
            source = checked_path(state / name)
            regular(source)  # refuse symlinks/FIFOs rather than reading their targets
            shutil.copyfile(source, destination / name)
            files.update({name: 'copied'})
        except (OSError, Refused) as error:
            files.update({name: str(error)})
    return dict(directory=str(destination), files=files)


def recovery_instructions(compatibility, bin_dir, socket_path, no_daemon, rollback=True):
    recovery = 'Compatibility: ' + compatibility + '\n'
    if compatibility == 'ONE-WAY':
        recovery += ('WARNING: the old build may not read the organisation log the newer build wrote; '
                     'named agents and messages may be unavailable until the newer build is back.\n')
    if no_daemon:
        recovery += (f'no daemon answers on {socket_path}; start it and '
                     + ('run again' if rollback else 'inspect stable health') + '\n'
                     + 'ATM_SOCKET=' + shlex.quote(str(socket_path)) + ' '
                     + shlex.join([str(bin_dir / 'atmd'), 'start', '-d'])
                     + '\nWait for the socket to answer before '
                     + ('running rollback.' if rollback else 'inspecting stable health.') + '\n')
    return recovery


class Supervisor:
    def __init__(self, bin_dir, kit):
        self.bin, self.kit = bin_dir, kit
        self.metadata = bin_dir / '.atm-promotion'
        self.path = self.metadata / 'slots.json'

    def installation(self, name):
        manifest = self.kit.installation(name)
        path = self.kit.installs / name
        binaries = pair_record(path / 'bin')
        require(isinstance(manifest, dict) and isinstance(manifest.get('hashes'), dict),
                f'invalid install manifest: {name}')
        require(all(manifest.get('hashes').get(exe) == binary.get('sha256')
                    for exe, binary in binaries.items()), f'install manifest hash mismatch: {name}')
        return manifest, binaries, file_hash(path / 'install.json')

    def verify_record(self, record):
        validate_record(record)
        manifest, binaries, manifest_hash = self.installation(record.get('install'))
        require(manifest.get('sha') == record.get('commit') and binaries == record.get('pair')
                and manifest_hash == record.get('install_manifest_sha256'), 'retained install changed')
        kit = checked_path(self.metadata / record.get('kit'))
        require(kit_hash(kit) == record.get('kit_sha256'), 'retained kit changed')
        report_path = checked_path(self.metadata / record.get('report'))
        require(file_hash(report_path) == record.get('report_sha256'), 'slot report changed')
        report = load(report_path)
        require(isinstance(report, dict) and all(report.get(key) == record.get(key) for key in
                ('commit', 'install', 'pair', 'kit', 'kit_sha256', 'install_manifest_sha256', 'report_kind')),
                'slot disagrees with its report')

    def read(self):
        try:
            envelope = load(self.path)
            require(isinstance(envelope, dict) and isinstance(envelope.get('slots'), dict),
                    'invalid slots envelope')
            slots = envelope.get('slots')
            require(envelope.get('sha256') == checksum(slots), 'slots checksum mismatch')
            require(slots.get('version') == 1 and slots.get('stable') is not None, 'invalid slots version/stable')
            rejected, used = slots.get('rejected'), slots.get('used_approvals')
            require(isinstance(rejected, list) and len(rejected) <= 3, 'invalid rejected slots')
            require(isinstance(used, list) and all(valid_hash(value) for value in used)
                    and len(set(used)) == len(used), 'invalid approval history')
            self.verify_record(slots.get('stable'))
            for key in ('previous', 'candidate'):
                require(key in slots, f'missing slot: {key}')
                if slots.get(key) is not None:
                    self.verify_record(slots.get(key))
            for record in rejected:
                self.verify_record(record)
                require(isinstance(record.get('reason'), str) and record.get('reason').strip()
                        and isinstance(record.get('evidence'), dict), 'invalid rejection evidence')
            installed = pair_record(self.bin)
            require(any(record is not None and installed == record.get('pair')
                        for record in (slots.get('stable'), slots.get('candidate'))),
                    'installed pair is neither stable nor candidate')
            return slots
        except (OSError, ValueError, TypeError, AttributeError, KitRefused, Refused) as error:
            raise NeedsYou(f'cannot verify slots: {error}') from error

    def gate(self, slots, name, gate_pass, socket_path, runner_label):
        """Retain evidence only: no slot writes, approvals, rehearsal or live changes."""
        kit, digest = retain_kit(self.metadata)
        steps = []
        report = dict(install=name, report_kind='gate', kit=kit, kit_sha256=digest,
                      replaces=slots.get('stable'), steps=steps)
        step = gate_step(steps, 'manifest', ['verify', self.kit.installs / name / 'install.json'])
        try:
            manifest, binaries, manifest_hash = self.installation(name)
            commit = manifest.get('sha')
            report.update(commit=commit, pair=binaries, install_manifest_sha256=manifest_hash)
            require(manifest.get('prebuilt_provenance_unverified') is False
                    and manifest.get('profile') in ('release', 'debug'), 'prebuilt provenance is unverified')
            require(name == str(commit) + '-' + manifest.get('profile'), 'install commit/profile mismatch')
            require(not any(item.get('commit') == commit and item.get('pair') == binaries
                            for item in slots.get('rejected')), 'candidate is rejected')
            step.update(result=dict(commit=commit, pair=binaries, install_manifest_sha256=manifest_hash))
            gate_pass = checked_path(gate_pass)
            pass_hash = file_hash(gate_pass)
            socket_path = checked_path(socket_path)
            stable = slots.get('stable')
            baseline = load(self.metadata / stable.get('report'))
            counts = {item.get('name'): item.get('result', {}).get('passed')
                      for item in baseline.get('steps', []) if isinstance(item, dict)
                      and isinstance(item.get('result'), dict)}
            repo = SCRIPTS.parent
            cargo = Path(run(['rustup', 'which', 'cargo'], env=git_env(clean_env()), cwd=repo)).resolve(strict=True)
            regular(cargo, executable=True)
            with tempfile.TemporaryDirectory(prefix='atm-gate-', dir='/var/tmp') as temporary:
                root = Path(temporary)
                for sub in ('home', 'tmp', 'tmux', 'state', 'xdg', 'config', 'data', 'cache', 's'):
                    (root / sub).mkdir(mode=0o700)
                environment = git_env(clean_env())
                environment.pop('ATM_RECORD_REGISTRY_REPLAY', None)  # presence alone rewrites goldens
                environment.pop('CARGO_TARGET_DIR', None)  # target stays in this gate's source tree
                environment.update(CARGO_HOME=os.environ.get('CARGO_HOME', str(Path.home() / '.cargo')),
                                   RUSTUP_HOME=os.environ.get('RUSTUP_HOME', str(Path.home() / '.rustup')),
                                   HOME=str(root / 'home'), SHELL='/bin/sh', TMPDIR=str(root / 'tmp'),
                                   ATM_SOCKET=str(root / 's/atm.sock'), ATM_TMUX_SOCKET='gate',
                                   ATM_STATE_DIR=str(root / 'state'), TMUX_TMPDIR=str(root / 'tmux'),
                                   XDG_STATE_HOME=str(root / 'xdg'), XDG_CONFIG_HOME=str(root / 'config'),
                                   XDG_DATA_HOME=str(root / 'data'), XDG_CACHE_HOME=str(root / 'cache'),
                                   PATH=str(cargo.parent) + ':/usr/local/bin:/usr/bin:/bin')
                with gate_worktree(repo, root / 'source', commit) as source:
                    require(run(['git', 'rev-parse', 'HEAD'], env=git_env(environment), cwd=source) == commit,
                            'gate worktree is not the candidate commit')
                    for name_filter in ('replay', 'frame_proof'):
                        step = gate_step(steps, name_filter,
                                         [cargo, 'test', '-p', 'atmd', '--lib', name_filter])
                        step.update(unset_env=['ATM_RECORD_REGISTRY_REPLAY'])
                        result = gate_call(step, environment, source, 1800)
                        summaries = re.findall(r'^test result: (ok|FAILED)\. ([0-9]+) passed; ([0-9]+) failed;[^\n]*$',
                                               result.stdout, re.M)
                        require(len(summaries) == 1, f'{name_filter}: missing or ambiguous test summary')
                        verdict, passed, failed = next(iter(summaries))
                        passed, failed = int(passed), int(failed)
                        count = counts.get(name_filter)
                        count = count if type(count) is int and count > 0 else None
                        step.get('result').update(passed=passed, failed=failed)
                        step.update(stable_passed=count, lower_than_stable=passed < count if count else None)
                        require(result.returncode == 0 and verdict == 'ok' and failed == 0 and passed > 0,
                                f'{name_filter}: requires exit 0, failed 0, passed > 0')
            step = gate_step(steps, 'gate-pass', ['check', '--gate-pass', gate_pass,
                                                 '--gate-runner-label', runner_label])
            text = gate_pass.read_text()
            step.update(result=dict(record=text, sha256=file_hash(gate_pass)))
            require(file_hash(gate_pass) == pass_hash, 'gate-pass record changed during gate')
            check_gate_pass(text, commit, runner_label)
            step = gate_step(steps, 'rehearsal', ['rehearse', '--stable-bin',
                             self.kit.installs / stable.get('install') / 'bin',
                             '--candidate-bin', self.kit.installs / name / 'bin'])
            rehearsal = rehearse(self.kit.installs / stable.get('install') / 'bin', self.kit.installs / name / 'bin')
            step.update(result=rehearsal)
            report.update(compatibility=rehearsal.get('compatibility'))
            require(rehearsal.get('cleanup', {}).get('clean'), 'rehearsal cleanup is uncertain; retained runtime')
            step = gate_step(steps, 'rollout', [sys.executable, SCRIPTS / 'atm-rollout',
                             self.kit.installs / name, '--dry-run', '--bin-dir', self.bin,
                             '--socket', socket_path, '--installed-commit', stable.get('commit')])
            result = gate_call(step, git_env(os.environ), repo, 300)
            require(result.returncode == 0, f'rollout dry-run refused (exit {result.returncode})')
            step.get('result').update(rollout_evidence(result.stdout, stable.get('commit'), commit))
            require(self.installation(name) == (manifest, binaries, manifest_hash), 'candidate changed during gate')
            require(file_hash(gate_pass) == pass_hash, 'gate-pass record changed during gate')
            require(all(file_hash(SCRIPTS / filename) == file_hash(self.metadata / kit / filename)
                        for filename in KIT_FILES), 'kit changed during gate')
            require(self.read() == slots, 'slots changed during gate')
            report.update(result='passed')
        except (OSError, ValueError, TypeError, AttributeError, Refused, KitRefused,
                NeedsYou, subprocess.SubprocessError, KeyboardInterrupt) as error:
            reason = str(error) or 'interrupted'
            step.get('result').update(error=reason)
            report.update(result='refused', reason=reason)
        for entry in steps:
            entry.update(sha256=checksum(entry))
        reports = self.metadata / 'reports'
        directory(reports, private=True)
        path = reports / (uuid.uuid4().hex + '.json')
        save(path, report)
        return dict(report=str(path), sha256=file_hash(path), result=report.get('result'))

    def approved_candidate(self, slots, name, approval):
        require(valid_hash(approval), 'approval must be a report sha256')
        require(approval not in slots.get('used_approvals'), 'approval hash already used')
        paths = [path for path in (self.metadata / 'reports').glob('*.json') if file_hash(path) == approval]
        require(len(paths) == 1, 'approval must name one retained gate report')
        path = next(iter(paths))  # exactly one match validated above
        report = load(path)
        require(isinstance(report, dict) and report.get('result') == 'passed'
                and report.get('report_kind') == 'gate' and report.get('install') == name,
                'approval is not a passed gate report for this install')
        require(report.get('replaces') == slots.get('stable'), 'approved stable slot changed')
        record = {key: report.get(key) for key in ('commit', 'install', 'pair', 'kit', 'kit_sha256',
                                                  'install_manifest_sha256', 'report_kind')}
        record.update(report='reports/' + path.name, report_sha256=approval)
        self.verify_record(record)
        manifest, binaries, _ = self.installation(name)
        require(manifest.get('prebuilt_provenance_unverified') is False
                and manifest.get('profile') in ('release', 'debug'), 'prebuilt provenance is unverified')
        require(not any(item.get('commit') == record.get('commit') and item.get('pair') == binaries
                        for item in slots.get('rejected')), 'candidate is rejected')
        require(report.get('compatibility') in ('two-way', 'ONE-WAY'), 'missing rehearsal compatibility')
        steps = report.get('steps')
        require(isinstance(steps, list) and all(isinstance(item, dict) for item in steps)
                and [item.get('name') for item in steps] ==
                ['manifest', 'replay', 'frame_proof', 'gate-pass', 'rehearsal', 'rollout'], 'invalid gate steps')
        require(all(item.get('sha256') == checksum({key: value for key, value in item.items() if key != 'sha256'})
                    for item in steps), 'gate step evidence changed')
        require(all(file_hash(SCRIPTS / filename) == file_hash(self.metadata / record.get('kit') / filename)
                    for filename in KIT_FILES), 'kit changed since approval')
        return record, report

    def finish_candidate(self, slots, code, result, reason, **evidence):
        try:
            save_slots(self.path, reject_candidate(slots, reason, evidence))
        except (OSError, KitRefused, Refused) as error:
            raise NeedsYou('cannot persist supervisor decision: ' + str(error)
                           + '\n' + evidence.get('manual_recovery', '')
                           + evidence.get('manual_rollback', 'candidate remains pending')
                           + ('\nRecovery evidence: ' + json.dumps(evidence) if evidence.get('rollback') else '')) from error
        return code, dict(result=result, reason=reason, **evidence)

    def finish_failed_recovery(self, slots, reason, **evidence):
        try:
            require(self.read() == slots, 'slots changed during rollback')
        except (OSError, ValueError, KitRefused, Refused, NeedsYou, KeyboardInterrupt) as error:
            return 4, dict(result='needs-you', reason=reason + '; cannot verify rollback; candidate remains pending: '
                           + (str(error) or 'interrupted'), **evidence)
        return self.finish_candidate(slots, 4, 'needs-you', reason, **evidence)

    def finish_recovery(self, slots, rollout, pinned, read, reason='rollout already rolled back', **evidence):
        try:
            observation = read(pinned)
        except (OSError, ValueError, KitRefused, KeyboardInterrupt) as error:
            if isinstance(error, KitRefused):
                reason = 'stable health uncertain; candidate remains pending: ' + str(error)
                if evidence:
                    return 4, dict(result='needs-you', reason=reason, rollout=rollout, **evidence)
                raise NeedsYou(reason) from error
            return self.finish_candidate(slots, 4, 'needs-you', 'stable health uncertain: ' + str(error),
                                         rollout=rollout, **evidence)
        verdict = health_verdict([observation])
        code = 3 if verdict in ('healthy', 'unknown') else 4
        return self.finish_candidate(slots, code, 'rolled-back' if code == 3 else 'needs-you',
                                     reason if code == 3 else 'stable unhealthy or uncertain',
                                     rollout=rollout, health=observation, **evidence)

    def promote(self, slots, name, approval, socket_path, read=None,
                clock=time.monotonic, wait=time.sleep, duration=1800):
        """Repeat manifest/dry-run; R owns swaps, including one authorized probation rollback."""
        require(slots.get('candidate') is None, 'a candidate is already pending')
        require(pair_record(self.bin) == slots.get('stable').get('pair'), 'stable is not installed')
        record, report = self.approved_candidate(slots, name, approval)
        socket_path = checked_path(socket_path)
        approved = next(reversed(report.get('steps')))
        command = list(map(str, [sys.executable, SCRIPTS / 'atm-rollout', self.kit.installs / name,
                                '--dry-run', '--bin-dir', self.bin, '--socket', socket_path,
                                '--installed-commit', slots.get('stable').get('commit')]))
        require(approved.get('command') == command, 'approved rollout target changed')
        step = gate_step([], 'rollout', command)
        result = gate_call(step, git_env(os.environ), SCRIPTS.parent, 300)
        require(result.returncode == 0, f'rollout dry-run refused (exit {result.returncode})')
        current = rollout_evidence(result.stdout, slots.get('stable').get('commit'), record.get('commit'))
        require(all(approved.get('result', {}).get(key) == value for key, value in current.items()),
                'protected report or quiet snapshot changed since approval')
        require(self.read() == slots, 'slots changed since approval')
        require(self.approved_candidate(slots, name, approval) == (record, report), 'approval inputs changed')
        slots = stage_candidate(slots, record, approval)
        save_slots(self.path, slots)  # consume once before R can change anything
        command.remove('--dry-run')
        command.extend(['--report-sha256', current.get('report_sha256')])
        step = gate_step([], 'rollout', command)
        try:
            result = gate_call(step, git_env(os.environ), SCRIPTS.parent, 900)
        except (OSError, subprocess.SubprocessError, KeyboardInterrupt) as error:
            return self.finish_candidate(slots, 4, 'needs-you', 'rollout completion uncertain: ' + str(error),
                                         rollout=step.get('result'))
        rollout = step.get('result')
        if result.returncode not in (0, 3):
            code = 1 if result.returncode == 1 else 4
            return self.finish_candidate(slots, code, 'refused' if code == 1 else 'needs-you',
                                         f'rollout exit {result.returncode}; nothing further tried', rollout=rollout)
        try:
            require(pair_record(previous(self.metadata)) == slots.get('stable').get('pair'),
                    'last-known-good hash mismatch; do not roll back')
            require(self.read() == slots, 'slots changed during rollout')
            expected = slots.get('candidate') if result.returncode == 0 else slots.get('stable')
            require(pair_record(self.bin) == expected.get('pair'), 'rollout installed an unexpected pair')
            pinned = rollout_identity(result.stdout, self.bin)
        except (ValueError, KitRefused) as error:
            raise NeedsYou('cannot verify rollout result; candidate remains pending: ' + str(error)) from error
        except (OSError, Refused, NeedsYou) as error:
            return self.finish_candidate(slots, 4, 'needs-you', str(error), rollout=rollout)
        read = read or (lambda pinned: read_health(socket_path, pinned))
        if result.returncode == 3:
            return self.finish_recovery(slots, rollout, pinned, read)
        try:
            outcome = probation(lambda: read(pinned), clock, wait, duration)
        except (OSError, ValueError, KitRefused, KeyboardInterrupt) as error:
            if isinstance(error, KitRefused):
                raise NeedsYou('probation interrupted; candidate remains pending: ' + str(error)) from error
            return self.finish_candidate(slots, 4, 'needs-you', 'probation interrupted: ' + str(error), rollout=rollout)
        if outcome.get('result') != 'accepted':
            # Retain diagnostics before the one authorized two-way recovery attempt.
            try:
                evidence = collect_logs(self.metadata, os.environ)
            except (OSError, Refused) as error:
                evidence = dict(error=str(error))
            manual = shlex.join(['scripts/atm-rollout', '--rollback', '--bin-dir', str(self.bin), '--socket', str(socket_path)])
            recovery = recovery_instructions(report.get('compatibility'), self.bin, socket_path,
                                             next(reversed(outcome.get('reads')), {}).get('state') in ('dead', 'refused'))
            details = dict(evidence=evidence, probation=outcome, manual_rollback=manual, manual_recovery=recovery,
                           installed_pair=None)
            if (outcome.get('result') == 'unhealthy' and report.get('compatibility') == 'two-way'
                    and evidence.get('files') == {'atm.log': 'copied', 'atm.log.1': 'copied'}):
                try:
                    require(self.read() == slots, 'slots changed during probation')
                    details.update(installed_pair=pair_record(self.bin))
                    require(details.get('installed_pair') == record.get('pair'), 'candidate is not installed')
                    require(pair_record(previous(self.metadata)) == slots.get('stable').get('pair'),
                            'last-known-good hash mismatch; do not roll back')
                except (OSError, ValueError, KitRefused, Refused, NeedsYou, KeyboardInterrupt) as error:
                    return 4, dict(result='needs-you', reason='cannot verify recovery; candidate remains pending: '
                                   + (str(error) or 'interrupted'), rollout=rollout, **details)
                step = gate_step([], 'rollback', [sys.executable, SCRIPTS / 'atm-rollout', '--rollback',
                                 '--bin-dir', self.bin, '--socket', socket_path])
                try:
                    result = gate_call(step, dict(git_env(os.environ), ATM_SOCKET=str(socket_path)), SCRIPTS.parent, 900)
                except (OSError, subprocess.SubprocessError, KeyboardInterrupt) as error:
                    return self.finish_failed_recovery(slots, 'rollback completion uncertain: ' + str(error),
                                                       rollout=rollout, rollback=step.get('result'), **details)
                details.update(rollback=step.get('result'))
                if result.returncode != 0:
                    return self.finish_failed_recovery(slots, f'rollback exit {result.returncode}; nothing further tried',
                                                       rollout=rollout, **details)
                details.update(installed_pair=None)
                try:
                    require(self.read() == slots, 'slots changed during rollback')
                    restored = pair_record(self.bin)
                    details.update(installed_pair=restored)
                    require(restored == slots.get('stable').get('pair'), 'rollback did not install stable')
                    require(pair_record(previous(self.metadata)) == restored, 'last-known-good changed during rollback')
                    pinned = rollout_identity(result.stdout, self.bin)
                except (OSError, ValueError, KitRefused, Refused, NeedsYou, KeyboardInterrupt) as error:
                    return 4, dict(result='needs-you', reason='cannot verify rollback; candidate remains pending: '
                                   + (str(error) or 'interrupted'), rollout=rollout, **details)
                return self.finish_recovery(slots, rollout, pinned, read, reason=outcome.get('reason'),
                                             restored_pair=restored, **details)
            return self.finish_candidate(slots, 4, 'needs-you', outcome.get('reason'), rollout=rollout, **details)
        try:
            require(self.read() == slots, 'slots changed during probation')
            save_slots(self.path, accept_candidate(slots))
        except (OSError, KitRefused, Refused, NeedsYou) as error:
            raise NeedsYou('cannot accept candidate: ' + str(error)) from error
        return 0, dict(result='accepted', probation=outcome)

    def check(self, slots, socket_path, read=None):
        if pair_record(self.bin) != slots.get('stable').get('pair'):
            return 4, dict(verdict='pending', reason='stable is not installed')
        observation = (read or (lambda pinned: read_health(checked_path(socket_path), pinned)))(None)
        peer = observation.get('identity')
        try:
            if peer and (peer.get('exe') != str(self.bin / 'atmd')
                         or not Path('/proc/' + str(peer.get('pid')) + '/exe').samefile(self.bin / 'atmd')):
                observation.update(state='changed')
        except OSError:
            observation.update(state='dead')
        verdict = health_verdict([observation])
        return (0 if verdict == 'healthy' else 2 if verdict == 'unhealthy' else 4), dict(verdict=verdict, health=observation)

    def init(self, commit, install_id, observed_at):
        require(re.fullmatch('[0-9a-f]{40}', commit) is not None, 'installed commit must be a full SHA')
        installed = pair_record(self.bin)
        names = [install_id] if install_id else sorted(path.name for path in self.kit.installs.glob(commit + '-*'))
        matches = []
        for name in names:
            manifest, binaries, manifest_hash = self.installation(name)
            if manifest.get('sha') == commit and binaries == installed:
                matches.append((name, manifest_hash))
        require(matches, f'no matching immutable kit install for {commit} and installed pair in {self.kit.installs}'
                + (f' (requested {install_id})' if install_id else ''))
        require(len(matches) == 1, 'ambiguous matching installs; use --install: '
                + ', '.join(name for name, _ in matches))
        # The checks above establish exactly one match before taking it.
        name, manifest_hash = next(iter(matches))
        kit, digest = retain_kit(self.metadata)
        record = dict(commit=commit, install=name, pair=installed, kit=kit, kit_sha256=digest,
                      install_manifest_sha256=manifest_hash, report_kind='baseline')
        reports = self.metadata / 'reports'
        reports.mkdir(mode=0o700, exist_ok=True)
        directory(reports, private=True)
        report = reports / (uuid.uuid4().hex + '.json')
        save(report, dict(record, observed_at=observed_at, installed_bin=str(self.bin),
                          description='baseline observation, not an approval'))
        record.update(report='reports/' + report.name, report_sha256=file_hash(report))
        slots = dict(version=1, stable=record, previous=None, candidate=None, rejected=[], used_approvals=[])
        save_slots(self.path, slots)
        return slots


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    init = commands.add_parser('init')
    init.add_argument('--installed-commit', required=True)
    init.add_argument('--install', help='matching immutable install ID, if more than one matches')
    status = commands.add_parser('status')
    gate = commands.add_parser('gate', help='verify one immutable install; change nothing live')
    gate.add_argument('install', help='immutable candidate-kit install ID')
    gate.add_argument('--gate-pass', required=True, help='local gate runner output for this exact commit')
    gate.add_argument('--gate-runner-label', required=True, help='exact literal label in the gate runner output')
    gate.add_argument('--socket', required=True, help='explicit daemon socket for rollout dry-run')
    promote = commands.add_parser('promote', help='promote an approved gate report and observe probation')
    promote.add_argument('install', help='immutable candidate-kit install ID')
    promote.add_argument('--approve', required=True, help='exact unused supervisor gate report sha256')
    promote.add_argument('--socket', required=True)
    check = commands.add_parser('check', help='read stable health once; never act')
    check.add_argument('--socket', required=True)
    for command in (init, status, gate, promote, check):
        command.add_argument('--bin-dir', required=True)
    args = parser.parse_args()
    os.umask(0o077)
    try:
        bin_dir = checked_path(args.bin_dir)
        directory(bin_dir)
        metadata = bin_dir / '.atm-promotion'
        metadata.mkdir(mode=0o700, exist_ok=True)
        with supervisor_lock(metadata), contextlib.ExitStack() as resources:
            try:
                kit = Kit(read_only=True)
                if kit.lock is not None:
                    resources.enter_context(kit.lock)
            except (OSError, ValueError, KitRefused) as error:
                if args.command == 'status' or os.path.lexists(metadata / 'slots.json'):
                    raise NeedsYou(f'cannot verify kit: {error}') from error
                raise
            supervisor = Supervisor(bin_dir, kit)
            if os.path.lexists(supervisor.path) or args.command != 'init':
                slots = supervisor.read()
                require(args.command != 'init', 'supervisor slots already initialized')
            else:
                slots = supervisor.init(args.installed_commit, args.install,
                                        datetime.datetime.now(datetime.timezone.utc).isoformat())
            if args.command == 'gate':
                output = supervisor.gate(slots, args.install, args.gate_pass, args.socket, args.gate_runner_label)
                print(json.dumps(output, indent=2))
                return 0 if output.get('result') == 'passed' else 1
            if args.command in ('promote', 'check'):
                code, output = (supervisor.promote(slots, args.install, args.approve, args.socket)
                                if args.command == 'promote' else supervisor.check(slots, args.socket))
                if code == 4 and 'rollback' in output:
                    try:
                        output['installed_pair'] = pair_record(bin_dir)
                        _, candidate, _ = supervisor.installation(args.install)
                        if output.get('installed_pair') != candidate:
                            output['manual_rollback'] = ''
                    except (OSError, ValueError, KitRefused, Refused) as error:
                        output.update(manual_rollback='', installed_pair_error=str(error))
                    output['manual_recovery'] = recovery_instructions('two-way', bin_dir, args.socket,
                                                                      not listening(args.socket),
                                                                      rollback=bool(output.get('manual_rollback')))
                print(json.dumps(output, indent=2))
                if code == 3 and output.get('rollback'):
                    rollback = output.get('rollback')
                    print('supervisor: automatic rollback completed\n' + rollback.get('stdout', '')
                          + rollback.get('stderr', '') + '\nRestored pair: ' + json.dumps(output.get('restored_pair')),
                          file=sys.stderr)
                if code == 4:
                    print('supervisor: NEEDS YOU: ' + output.get('reason', 'health uncertain'), file=sys.stderr)
                    if 'rollback' in output:
                        rollback = output.get('rollback') or {}
                        status = rollback.get('returncode')
                        print(f'supervisor: automatic rollback attempted, R exit {status if status is not None else "unknown"}\n'
                              + rollback.get('stderr', ''), file=sys.stderr)
                    if output.get('manual_rollback') or output.get('manual_recovery'):
                        print(output.get('manual_recovery', '') + output.get('manual_rollback', ''), file=sys.stderr)
                return code
            print(json.dumps(slots, indent=2))
        return 0
    except NeedsYou as error:
        print(f'supervisor: NEEDS YOU: {error}', file=sys.stderr)
        return 4
    except (OSError, ValueError, TypeError, AttributeError, Refused, KitRefused,
            subprocess.SubprocessError, KeyboardInterrupt) as error:
        print(f'supervisor: {error or "interrupted"}', file=sys.stderr)
        return 1
