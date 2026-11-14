#!/usr/bin/env python3
"""Candidate lifecycle boundary: private endpoints, JSON manifests, pidfd signals.

Only stdlib; Linux with pidfds, Python >= 3.9, tmux, git, Rust for source builds.
See atm-candidate.md. The Bash entrypoint never evaluates a stored manifest.
"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import select
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import uuid


class Refused(Exception):
    pass


def require(condition, reason):
    if not condition:
        raise Refused(reason)


def run(args, env=None, cwd=None, timeout=30):
    return subprocess.run(list(map(str, args)), env=env, cwd=cwd, check=True,
                          capture_output=True, text=True, timeout=timeout).stdout.strip()


def clean_env():
    return {k: v for k, v in os.environ.items() if k not in ('TMUX', 'TMUX_PANE')}


def checked_path(value):
    path = Path(value)
    require(path.is_absolute() and path == path.resolve(),
            'paths must be absolute, normalized, and contain no symlinks')
    return path


def private_dir(path):
    checked_path(str(path))
    info = path.stat()
    require(info.st_uid == os.getuid() and info.st_mode & 0o077 == 0 and path.is_dir(),
            f'not a private owned directory: {path}')


def owned_root(value, create=True):
    path = checked_path(value)
    require(path not in (Path('/'), Path('/tmp'), Path('/dev/shm'), Path.home())
            and len(path.parts) > 2, 'refusing dangerous root')
    marker = path / '.atm-candidate-owner'
    if path.exists():
        private_dir(path)
        require(marker.is_file() and not marker.is_symlink()
                and marker.read_text() == str(os.getuid()), f'unowned root: {path}')
    elif create:
        path.mkdir(parents=True, mode=0o700)
        marker.write_text(str(os.getuid()))
    return path


def save(path, data):
    require(not path.is_symlink(), 'manifest is a symlink')
    temporary = path.with_suffix('.new')
    with temporary.open('x') as stream:
        json.dump(data, stream, indent=2)
        stream.write('\n')
    temporary.replace(path)


def load(path):
    require(path.is_file() and not path.is_symlink(), f'missing regular manifest: {path}')
    return json.loads(path.read_text())


def identity(pid):
    try:
        root = Path('/proc') / str(pid)
        fields = (root / 'stat').read_text().rsplit(')', 1)
        require(len(fields) == 2, 'invalid proc stat')
        # Validated Linux proc stat shape before accessing state/starttime.
        values = fields[1].split()
        require(len(values) >= 20, 'short proc stat')
        if values[0] == 'Z':
            return None
        return dict(pid=int(pid), start=values[19], exe=str((root / 'exe').resolve(strict=True)))
    except (FileNotFoundError, ProcessLookupError):
        return None


def process_env(pid):
    try:
        return dict(item.split(b'=', 1) for item in
                    (Path('/proc') / str(pid) / 'environ').read_bytes().split(b'\0') if b'=' in item)
    except (FileNotFoundError, ProcessLookupError, PermissionError):
        return {}


def asset_manifest(repo, sha):
    pairs = {'.local/bin/' + name: 'crates/atm/scripts/' + name
             for name in ('atm-hook', 'atm-codex-hook')}
    for name in ('package.json', 'extensions/pi-atm.ts'):
        pairs['.pi/agent/packages/pi-atm/' + name] = 'crates/atm/assets/pi-atm/' + name
    for harness in ('.claude', '.codex', '.pi/agent'):
        for skill in ('atm-coordinate', 'atm-implement'):
            prefix = harness + '/skills/' + skill + '/'
            pairs[prefix + 'SKILL.md'] = 'crates/atm/assets/atm-skills/' + skill + '/SKILL.md'
            for name in ('references/atm-commands.md', 'scripts/atm-wait',
                         'scripts/atm-profile', 'agents/openai.yaml'):
                pairs[prefix + name] = 'crates/atm/assets/atm-skills/shared/' + name
    hashes = {}
    for source in set(pairs.values()):
        content = subprocess.check_output(['git', 'show', sha + ':' + source], cwd=repo, timeout=30)
        hashes[source] = hashlib.sha256(content).hexdigest()
    return {installed: hashes[source] for installed, source in pairs.items()}


def asset_drift(manifest):
    result = {}
    for relative, expected in manifest.items():
        path = Path.home() / relative
        result[relative] = ('missing' if not path.is_file() else
                            'same' if hashlib.sha256(path.read_bytes()).hexdigest() == expected
                            else 'different')
    return result


def wait_for(predicate, description, seconds=30):
    deadline = time.monotonic() + seconds
    while True:
        value = predicate()
        if value:
            return value
        require(time.monotonic() < deadline, 'timeout: ' + description)
        time.sleep(.05)  # bounded condition polling, never a settling delay


class Kit:
    def __init__(self, read_only=False):
        home = Path.home()
        self.root = owned_root(os.environ.get('ATM_CANDIDATE_ROOT', str(home / '.local/share/atm-candidate')), not read_only)
        self.state = owned_root(os.environ.get('ATM_CANDIDATE_STATE_ROOT', str(home / '.local/state/atm-candidate')), not read_only)
        self.build = owned_root(os.environ.get('ATM_CANDIDATE_BUILD_ROOT', str(home / '.cache/atm-candidate')), not read_only)
        roots = (self.root, self.state, self.build)
        require(all(not a.is_relative_to(b) for a in roots for b in roots if a != b)
                and len(set(roots)) == 3, 'kit roots must be separate, nonoverlapping directories')
        value = os.environ.get('ATM_CANDIDATE_RUNTIME_ROOT')
        self.runtime = owned_root(value, not read_only) if value else Path('/tmp')
        self.installs = self.root / 'installs'
        if not read_only:
            self.installs.mkdir(mode=0o700, exist_ok=True)
        if self.installs.exists():
            private_dir(self.installs)
        self.active_file = self.root / 'active.json'
        self.lock = None
        if not read_only or (self.root / 'lock').exists():
            require(not (self.root / 'lock').is_symlink(), 'lock is a symlink')
            self.lock = (self.root / 'lock').open('r' if read_only else 'a')
            try:
                fcntl.flock(self.lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError as error:
                raise Refused('another kit operation holds the lock') from error

    def installation(self, name):
        require(re.fullmatch(r'[0-9a-f]{40}-(release|debug|prebuilt)', name or '') is not None,
                'invalid installed ID')
        path = self.installs / name
        private_dir(path)
        private_dir(path / 'bin')
        data = load(path / 'install.json')
        require(data.get('id') == name, 'install identity mismatch')
        for exe in ('atm', 'atmd'):
            binary = path / 'bin' / exe
            require(binary.is_file() and not binary.is_symlink() and os.access(binary, os.X_OK),
                    f'missing installed executable: {exe}')
        return data

    def install(self, args):
        repo = Path(__file__).resolve().parent.parent
        sha = run(['git', 'rev-parse', '--verify', '--end-of-options', args.ref + '^{commit}'], cwd=repo)
        require(re.fullmatch('[0-9a-f]{40}', sha) is not None, 'not a commit SHA')
        profile = 'prebuilt' if args.prebuilt else 'debug' if args.debug else 'release'
        name = sha + '-' + profile
        destination = self.installs / name
        require(not destination.exists(), 'immutable install already exists')
        worktree = self.build / ('source-' + uuid.uuid4().hex)
        stage = Path(tempfile.mkdtemp(prefix='.install-', dir=self.installs))
        added = False
        try:
            toolchain = None
            if args.prebuilt:
                binaries = Path(args.prebuilt).resolve(strict=True)
            else:
                cargo = Path(run(['rustup', 'which', 'cargo'])).resolve(strict=True)
                env = clean_env()
                env['PATH'] = str(cargo.parent) + ':/usr/local/bin:/usr/bin:/bin'
                target = self.build / 'target'
                target.mkdir(mode=0o700, exist_ok=True)
                private_dir(target)
                env['CARGO_TARGET_DIR'] = str(target)
                run(['git', 'worktree', 'add', '--detach', worktree, sha], cwd=repo)
                added = True
                toolchain = run([cargo, '--version'], env=env)
                command = [cargo, 'build', '--locked', '--bin', 'atm', '--bin', 'atmd']
                if not args.debug:
                    command.append('--release')
                print(f'Building {sha} ({profile}) in {worktree}; target {env["CARGO_TARGET_DIR"]}', file=sys.stderr)
                run(command, env=env, cwd=worktree, timeout=1800)
                binaries = self.build / 'target' / profile
            (stage / 'bin').mkdir(mode=0o700)
            hashes = {}
            for exe in ('atm', 'atmd'):
                source = binaries / exe
                require(source.is_file() and os.access(source, os.X_OK), f'not executable: {source}')
                shutil.copy2(source, stage / 'bin' / exe)
                hashes[exe] = hashlib.sha256((stage / 'bin' / exe).read_bytes()).hexdigest()
            data = dict(id=name, sha=sha, profile=profile, toolchain=toolchain,
                        hashes=hashes, assets=asset_manifest(repo, sha),
                        prebuilt_provenance_unverified=bool(args.prebuilt))
            save(stage / 'install.json', data)
            stage.rename(destination)
            save(self.root / 'selected.json', {'id': name})
            print(name)
        finally:
            if added:
                run(['git', 'worktree', 'remove', '--force', worktree], cwd=repo)
            if stage.exists():
                shutil.rmtree(stage)

    def active(self, optional=False):
        if optional and not self.active_file.exists():
            return None
        a = load(self.active_file)
        self.installation(a.get('id'))
        path = checked_path(a.get('run', ''))
        require(path.parent == self.runtime and path.name.startswith(f'atmc-{os.getuid()}-'),
                'foreign runtime directory')
        private_dir(path)
        require(load(path / 'owner.json') == {'root': str(self.root), 'token': a.get('token')},
                'runtime owner mismatch')
        require(a.get('socket') == str(path / 'a.sock')
                and a.get('tmux_socket') == str(path / 't' / f'tmux-{os.getuid()}' / 'candidate')
                and a.get('state') == str(self.state / a['id'])
                and a.get('bin') == str(self.installs / a['id'] / 'bin'), 'foreign endpoint in manifest')
        private_dir(Path(a['state']))
        return a

    def environment(self, a):
        env = clean_env()
        env.update(ATM_SOCKET=a['socket'], ATM_STATE_DIR=a['state'], ATM_TMUX_SOCKET='candidate',
                   TMUX_TMPDIR=str(Path(a['run']) / 't'), ATM_CANDIDATE_RUN=a['token'],
                   PATH=a['bin'] + ':' + env.get('PATH', '/usr/local/bin:/usr/bin:/bin'))
        return env

    def tm(self, a, *args):
        return run(['tmux', '-L', 'candidate', *args], env=self.environment(a))

    def owns(self, a, pid):
        env = process_env(pid)
        return all(env.get(k.encode()) == v.encode() for k, v in {
            'ATM_CANDIDATE_RUN': a['token'], 'ATM_SOCKET': a['socket'],
            'TMUX_TMPDIR': str(Path(a['run']) / 't')}.items())

    def processes(self, a):
        result = []
        for path in Path('/proc').iterdir():
            if path.name.isdigit() and int(path.name) != os.getpid() and self.owns(a, path.name):
                proc = identity(path.name)
                if proc:
                    result.append(proc)
        return result

    def tmux_listening(self, a):
        # No socket was created when start rejected an overlong runtime path.
        if not Path(a['tmux_socket']).exists():
            return False
        # tmux normally leaves a socket inode after exit, including failed starts.
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(1)
            try:
                connection.connect(a['tmux_socket'])
                return True
            except (FileNotFoundError, ConnectionRefusedError):
                return False

    def verify_server(self, a):
        expected = a.get('server')
        if not expected:
            if not self.tmux_listening(a):
                return False
            # A config/client error may follow successful server creation. Recover
            # only through our private socket, with the run token from before spawn.
            observed = self.tm(a, 'display-message', '-p', '#{socket_path} #{pid}')
            fields = observed.rsplit(' ', 1)
            require(len(fields) == 2 and fields[0] == a['tmux_socket'], 'foreign partial server')
            expected = identity(int(fields[1]))
            require(expected and self.owns(a, expected['pid']), 'unowned partial server')
            a['server'] = expected
            save(self.active_file, a)
        actual = identity(expected.get('pid'))
        if actual is None:
            require(not self.tmux_listening(a), 'stale server record with a live listener')
            return False
        require(actual == expected and self.owns(a, actual['pid']), 'foreign server identity')
        observed = self.tm(a, 'display-message', '-p', '#{socket_path} #{pid}')
        require(observed == a['tmux_socket'] + ' ' + str(actual['pid']), 'tmux socket/PID mismatch')
        return True

    def signal_process(self, a, proc):
        pid = proc['pid']
        if identity(pid) is None:
            return
        try:
            fd = os.pidfd_open(pid)
        except ProcessLookupError:
            return
        try:
            current = identity(pid)
            if current is None:
                return
            # Shell descendants can exec between enumeration and signaling;
            # starttime + pidfd pin the process, the token pins this run.
            require(current['start'] == proc['start'], 'process identity changed; refusing signal')
            if not self.owns(a, pid):
                # Exit can clear environ after the stat read. Wait on the pinned
                # process, never signal without ownership or mistake exit for reuse.
                require(bool(select.select([fd], [], [], 30)[0]),
                        'process ownership changed; refusing signal')
                return
            try:
                signal.pidfd_send_signal(fd, signal.SIGTERM)
            except ProcessLookupError:
                return
            require(bool(select.select([fd], [], [], 30)[0]), f'process {pid} did not exit; retained runtime')
        finally:
            os.close(fd)

    def daemon_processes(self, a):
        expected = a.get('daemon')
        if expected and identity(expected['pid']) is not None:
            require(identity(expected['pid']) == expected and self.owns(a, expected['pid']),
                    'foreign daemon identity')
        return [p for p in self.processes(a) if p['exe'] == str(Path(a['bin']) / 'atmd')]

    def ready(self, a):
        try:
            with socket.socket(socket.AF_UNIX) as connection:
                connection.settimeout(1)
                connection.connect(a['socket'])
                connection.sendall(b'{"protocol_version":{"major":1,"minor":1},"type":"connect"}\n')
                with connection.makefile('rb') as stream:
                    if json.loads(stream.readline(65536)).get('type') != 'connected':
                        return False
                    connection.sendall(b'{"protocol_version":{"major":1,"minor":1},"type":"list_sessions"}\n')
                    response = json.loads(stream.readline(4 * 1024 * 1024))
                    return response.get('type') == 'session_list'
        except (OSError, ValueError):
            return False

    def start_daemon(self, a):
        require(not self.daemon_processes(a), 'candidate daemon already running')
        with (Path(a['state']) / 'candidate.log').open('a') as log:
            child = subprocess.Popen([str(Path(a['bin']) / 'atmd'), 'start'],
                                     env=self.environment(a), stdin=subprocess.DEVNULL,
                                     stdout=log, stderr=log, start_new_session=True)
        a['daemon'] = identity(child.pid)
        save(self.active_file, a)
        def ready():
            require(child.poll() is None, 'daemon exited; see candidate.log')
            return self.ready(a)
        wait_for(ready, 'candidate protocol round trip')

    def start(self, name):
        require(not self.active_file.exists(), 'candidate active; stop before switching')
        name = name or load(self.root / 'selected.json').get('id')
        self.installation(name)
        state = self.state / name
        state.mkdir(mode=0o700, exist_ok=True)
        private_dir(state)
        runtime = Path(tempfile.mkdtemp(prefix=f'atmc-{os.getuid()}-', dir=self.runtime))
        (runtime / 't').mkdir(mode=0o700)
        a = dict(id=name, run=str(runtime), token=uuid.uuid4().hex,
                 socket=str(runtime / 'a.sock'), state=str(state),
                 tmux_socket=str(runtime / 't' / f'tmux-{os.getuid()}' / 'candidate'),
                 bin=str(self.installs / name / 'bin'), daemon=None, server=None)
        save(runtime / 'owner.json', {'root': str(self.root), 'token': a['token']})
        save(self.active_file, a)
        try:
            require(all(len(os.fsencode(a[k])) < 104 for k in ('socket', 'tmux_socket')),
                    'runtime root too long for Unix sockets')
            # Seed routing before tmux reads user config/plugins. No HOME/XDG changes.
            observed = self.tm(a, 'new-session', '-d', '-s', 'candidate', '-P', '-F', '#{pid}')
            a['server'] = identity(int(observed))
            save(self.active_file, a)
            require(self.verify_server(a), 'candidate server exited')
            self.tm(a, 'set-option', '-g', 'update-environment', '')
            for key, value in self.environment(a).items():
                if key.startswith('ATM_') or key in ('PATH', 'TMUX_TMPDIR'):
                    self.tm(a, 'set-environment', '-g', key, value)
            self.start_daemon(a)
            print(json.dumps(a))
        except BaseException:
            self.stop()
            raise

    def stop(self):
        a = self.active(optional=True)
        if a is None:
            return
        has_server = self.verify_server(a)
        daemons = self.daemon_processes(a)  # validate everything before the first signal
        processes = self.processes(a)
        if has_server:
            fd = os.pidfd_open(a['server']['pid'])
            try:
                self.verify_server(a)  # recheck immediately before kill-server
                self.tm(a, 'kill-server')
                require(bool(select.select([fd], [], [], 30)[0]), 'tmux server did not exit')
            finally:
                os.close(fd)
        for proc in processes:
            self.signal_process(a, proc)
        for proc in daemons:
            self.signal_process(a, proc)
        require(not self.processes(a), 'candidate descendants remain; retained runtime')
        # Only the private, marker-validated directory created by start is removed.
        shutil.rmtree(a['run'])
        self.active_file.unlink()

    def dispatch(self, args):
        command = args.command
        if command == 'install':
            self.install(args)
        elif command == 'installs':
            paths = sorted(self.installs.iterdir()) if self.installs.exists() else []
            print(json.dumps([self.installation(p.name) for p in paths if not p.name.startswith('.')]))
        elif command == 'start':
            self.start(args.id)
        elif command == 'stop':
            self.stop()
        elif command == 'status':
            a = self.active(optional=True)
            if a is None:
                print(json.dumps({'running': False}))
            else:
                # No CLI call: status must never autostart a daemon.
                print(json.dumps(dict(a, running=bool(self.daemon_processes(a)),
                                      install=self.installation(a['id']),
                                      asset_drift=asset_drift(self.installation(a['id'])['assets']))))
        elif command == 'restart-daemon':
            a = self.active()
            require(self.verify_server(a), 'candidate server missing')
            for proc in self.daemon_processes(a):
                self.signal_process(a, proc)
            a['daemon'] = None
            self.start_daemon(a)
        elif command == 'uninstall':
            self.installation(args.id)
            a = self.active(optional=True)
            require(a is None or a['id'] != args.id, 'stop before uninstalling active candidate')
            shutil.rmtree(self.installs / args.id)
            if args.wipe_state and (self.state / args.id).exists():
                private_dir(self.state / args.id)
                shutil.rmtree(self.state / args.id)
        elif command in ('atm', 'attach'):
            a = self.active()
            require(self.verify_server(a), 'candidate server missing')
            env = self.environment(a)
            if command == 'attach':
                # Interactive attachment owns its terminal; not a timed subprocess.
                argv = ['tmux', '-L', 'candidate', 'attach-session', '-t', 'candidate']
            else:
                require(not any(x in ('setup', 'uninstall') for x in args.args),
                        'setup/uninstall would mutate live assets; refused')
                argv = [str(Path(a['bin']) / 'atm'), *args.args]
            # Interactive atm/attach must not hold the lifecycle lock indefinitely.
            self.lock.close()
            os.execvpe(argv[0], argv, env)


def main():
    parser = argparse.ArgumentParser(description='Isolated candidate kit; see scripts/atm-candidate.md')
    sub = parser.add_subparsers(dest='command', required=True)
    install = sub.add_parser('install')
    install.add_argument('ref')
    mode = install.add_mutually_exclusive_group()
    mode.add_argument('--debug', action='store_true')
    mode.add_argument('--prebuilt', metavar='BIN_DIR', help='copy a prebuilt atm/atmd pair (provenance unverified)')
    sub.add_parser('installs')
    sub.add_parser('start').add_argument('id', nargs='?')
    sub.add_parser('atm').add_argument('args', nargs=argparse.REMAINDER)
    for name in ('attach', 'restart-daemon', 'status', 'stop'):
        sub.add_parser(name)
    uninstall = sub.add_parser('uninstall')
    uninstall.add_argument('id')
    uninstall.add_argument('--wipe-state', action='store_true')
    argv = iter(sys.argv)
    next(argv, None)  # program name
    command = next(argv, None)
    if command == 'atm':
        args = parser.parse_args(['atm'])
        args.args = list(argv)  # preserve leading flags and argument order verbatim
    else:
        args = parser.parse_args()
    require(args.command != 'attach' or not os.environ.get('TMUX'),
            'attach from a terminal outside tmux')
    os.umask(0o077)
    Kit(read_only=args.command in ('status', 'installs')).dispatch(args)


if __name__ == '__main__':
    try:
        main()
    except (Refused, OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        print(f'candidate: {error}', file=sys.stderr)
        if isinstance(error, subprocess.CalledProcessError):
            print(error.stderr, file=sys.stderr)
        sys.exit(1)
