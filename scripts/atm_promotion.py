"""Owner-run, quiescent promotion of regular binary pairs. No daemon control."""
import argparse
import fcntl
import filecmp
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tempfile

NAMES = ('atm', 'atmd')


class Refused(Exception):
    pass


def require(condition, reason):
    if not condition:
        raise Refused(reason)


def checked_path(value):
    path = Path(value)
    require(path.is_absolute() and path == path.resolve(),
            f'path must be absolute and contain no symlinks or ..: {path}')
    return path


def directory(path, private=False):
    checked_path(path)
    info = path.stat()
    require(stat.S_ISDIR(info.st_mode) and info.st_uid == os.getuid()
            and not info.st_mode & (0o077 if private else 0o022),
            f'not an owned, safely permissioned directory: {path}')


def regular(path, executable=False):
    info = path.lstat()
    require(stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid(),
            f'not an owned regular file: {path}')
    if executable:
        require(os.access(path, os.X_OK), f'not executable: {path}')


def pair(path):
    directory(path)
    for name in NAMES:
        regular(path / name, executable=True)


def ancestors():
    result = set()
    pid = os.getpid()
    while pid and pid not in result:
        result.add(pid)
        status = dict(line.split(':', 1) for line in
                      (Path('/proc') / str(pid) / 'status').read_text().splitlines() if ':' in line)
        pid = int(status.get('PPid', '0'))
    return result


def quiescent(bin_dir, socket_path, pairs):
    require(not os.path.lexists(socket_path),
            f'socket path exists; stop the daemon and investigate stale paths: {socket_path}')
    targets = {bin_dir / name for name in NAMES}
    for saved in pairs.iterdir():
        directory(saved, private=True)
        targets.update(saved / name for name in NAMES)
    inodes = {(info.st_dev, info.st_ino) for path in targets if path.exists()
              for info in (path.stat(),)}
    caller_chain = ancestors()
    for process in Path('/proc').iterdir():
        if not process.name.isdecimal() or int(process.name) == os.getpid():
            continue
        try:
            if process.stat().st_uid != os.getuid():
                continue
            executable = process / 'exe'
            path = Path(os.readlink(executable).removesuffix(' (deleted)'))
            info = executable.stat()
            require(path not in targets and (info.st_dev, info.st_ino) not in inodes,
                    f'running installed binary: pid {process.name}; close TUIs and stop daemon first')
            # Calling shells may themselves export ATM_SOCKET. Never exempt an
            # ancestor actually executing a target binary (checked above).
            if int(process.name) in caller_chain:
                continue
            environment = (process / 'environ').read_bytes().split(b'\0')
            require(os.fsencode('ATM_SOCKET=' + str(socket_path)) not in environment,
                    f'running process uses socket: pid {process.name}')
        except (FileNotFoundError, ProcessLookupError):
            continue  # exited processes and zombies have no executable
        except PermissionError as error:
            # User systemd services can hide exe/environ even from their UID.
            # Do not mistake an unrelated service for a running ATM. Ambiguous
            # ATM identities still fail closed; this is not a hostile-process sandbox.
            try:
                command = (process / 'cmdline').read_bytes().split(b'\0')
                name = (process / 'comm').read_text().strip()
            except (FileNotFoundError, ProcessLookupError):
                continue
            if name in NAMES or not any(command) or any(
                    Path(os.fsdecode(arg)) in targets or Path(os.fsdecode(arg)).name in NAMES
                    for arg in command if arg):
                raise Refused(f'cannot inspect possible ATM process {process.name}; refusing promotion') from error


def previous(metadata):
    marker = metadata / 'previous.json'
    regular(marker)
    data = json.loads(marker.read_text())
    require(isinstance(data, dict), 'invalid previous pair record')
    name = data.get('pair')
    require(isinstance(name, str) and re.fullmatch(r'pair-[a-zA-Z0-9_-]+', name),
            'invalid previous pair name')
    saved = metadata / 'pairs' / name
    pair(saved)
    return saved


def save_previous(metadata, saved):
    fd, temporary = tempfile.mkstemp(prefix='.previous-', dir=metadata)
    try:
        with os.fdopen(fd, 'w') as stream:
            json.dump({'pair': saved.name}, stream)
            stream.write('\n')
        os.replace(temporary, metadata / 'previous.json')
    finally:
        Path(temporary).unlink(missing_ok=True)


def backup(bin_dir, metadata, record=True):
    saved = Path(tempfile.mkdtemp(prefix='pair-', dir=metadata / 'pairs'))
    try:
        for name in NAMES:
            shutil.copy2(bin_dir / name, saved / name)
        if record:
            pair(saved)
            save_previous(metadata, saved)
        return saved
    except BaseException:
        shutil.rmtree(saved)  # only this call's incomplete, newly created backup
        raise


def replace_pair(source, destination):
    staged = []
    try:
        for name in NAMES:
            fd, temporary = tempfile.mkstemp(prefix='.atm-stage-', dir=destination)
            os.close(fd)
            staged.append((Path(temporary), destination / name))
            shutil.copy2(source / name, temporary)
        for temporary, target in staged:
            os.replace(temporary, target)
    finally:
        for temporary, _ in staged:
            temporary.unlink(missing_ok=True)


def setup(bin_dir, socket_path):
    env = {key: value for key, value in os.environ.items() if key not in ('TMUX', 'TMUX_PANE')}
    env.update(ATM_SOCKET=str(socket_path), PATH=str(bin_dir) + ':' + env.get('PATH', os.defpath))
    try:
        result = subprocess.run([str(bin_dir / 'atm'), 'setup'], env=env, stdin=subprocess.DEVNULL,
                                capture_output=True, text=True, errors='replace', timeout=60)
    except subprocess.TimeoutExpired as error:
        # TimeoutExpired carries bytes even when text mode was requested.
        print((error.stdout or b'').decode(errors='replace'), end='', flush=True)
        print((error.stderr or b'').decode(errors='replace'), end='', file=sys.stderr, flush=True)
        raise
    print(result.stdout, end='', flush=True)
    print(result.stderr, end='', file=sys.stderr, flush=True)
    # Verified with atm 0.3.0: skill failures are diagnostics, not exit failures.
    require(result.returncode == 0 and 'failed:' not in (result.stdout + result.stderr).lower(),
            f'atm setup failed (exit {result.returncode}); see setup output above')


def same_pair(left, right):
    return all(filecmp.cmp(left / name, right / name, shallow=False)
               and (left / name).stat().st_mode == (right / name).stat().st_mode
               for name in NAMES)


def promote(bin_dir, source, socket_path, metadata):
    require(not any(bin_dir.glob('.atm-stage-*')),
            'interrupted promotion; run atm-rollback first')
    require(not same_pair(bin_dir, source), 'source pair already installed')
    old_previous = previous(metadata) if (metadata / 'previous.json').exists() else None
    saved = backup(bin_dir, metadata)
    print(f'Previous pair: {saved}', flush=True)
    try:
        replace_pair(source, bin_dir)
        setup(bin_dir, socket_path)
    except (Exception, KeyboardInterrupt) as error:
        try:
            replace_pair(saved, bin_dir)
            setup(bin_dir, socket_path)
            if old_previous is not None:
                save_previous(metadata, old_previous)
        except (Exception, KeyboardInterrupt) as recovery:
            raise Refused(f'promotion failed: {error}; recovery failed: {recovery}; '
                          f'previous pair retained at {saved}; do not restart yet') from error
        raise Refused(f'promotion failed: {error}; old pair restored and setup rerun') from error


def main(operation):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', required=True)
    parser.add_argument('--socket', required=True)
    if operation == 'install':
        parser.add_argument('--from-bin', required=True)
    args = parser.parse_args()
    try:
        bin_dir, socket_path = checked_path(args.bin_dir), checked_path(args.socket)
        directory(bin_dir)
        if operation == 'install':
            source = checked_path(args.from_bin)
            require(not source.is_relative_to(bin_dir) and not bin_dir.is_relative_to(source),
                    'source and destination must not overlap')
            pair(source)
            pair(bin_dir)
        else:
            # Rollback can repair a missing entry, but must never follow a link.
            for name in NAMES:
                if os.path.lexists(bin_dir / name):
                    regular(bin_dir / name)
        metadata = bin_dir / '.atm-promotion'
        metadata.mkdir(mode=0o700, exist_ok=True)
        directory(metadata, private=True)
        lock_path = metadata / 'lock'
        if os.path.lexists(lock_path):
            regular(lock_path)
        fd = os.open(lock_path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
        with os.fdopen(fd, 'r+') as lock:
            require(stat.S_ISREG(os.fstat(lock.fileno()).st_mode), 'invalid promotion lock')
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError as error:
                raise Refused('another promotion/rollback holds the lock') from error
            pairs = metadata / 'pairs'
            pairs.mkdir(mode=0o700, exist_ok=True)
            directory(pairs, private=True)
            if os.path.lexists(metadata / 'previous.json'):
                previous(metadata)  # refuse corrupted metadata, even for another install
            quiescent(bin_dir, socket_path, pairs)
            if operation == 'install':
                promote(bin_dir, source, socket_path, metadata)
            else:
                saved = previous(metadata)
                if all((bin_dir / name).exists() for name in NAMES) and not same_pair(bin_dir, saved):
                    retained = backup(bin_dir, metadata, record=False)
                    print(f'Retained replaced pair: {retained}', flush=True)
                replace_pair(saved, bin_dir)
                for staged in bin_dir.glob('.atm-stage-*'):
                    staged.unlink()
                setup(bin_dir, socket_path)
                print(f'Restored previous pair: {saved}')
            print('Pair installed and setup completed. Daemon remains stopped.')
        return 0
    except (OSError, ValueError, Refused, subprocess.SubprocessError, KeyboardInterrupt) as error:
        print(f'promotion: {error}', file=sys.stderr)
        return 1
