"""Owner-run rollout of one candidate pair in one command, around atm-promote and atm-rollback.

Report, smoke, snapshot, stop, promote, start, checks, rollback on a failed check.
Steps, refusals, exit codes and limits: atm-promotion.md. Linux, Python 3.9+, tmux.
"""
import argparse
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
import subprocess
import sys
import tempfile
import threading
import time
import uuid

from atm_candidate import STOP_WAIT_TIMEOUT, identity, process_env, wait_for
from atm_candidate import Refused as TimedOut
from atm_promotion import NAMES, Refused, checked_path, directory, pair, previous, require, same_pair

SCRIPTS = Path(__file__).resolve().parent
STAND_IN = SCRIPTS.parent / 'tests/support/stand_in/harness.py'
# The variables that decide which state, config and tmux server a daemon uses.
DAEMON_ENV = ('HOME', 'ATM_STATE_DIR', 'XDG_STATE_HOME', 'XDG_CONFIG_HOME', 'ATM_TMUX_SOCKET', 'TMUX_TMPDIR')
SIDEBAR_FLAGS = ('--compact', '--pick', '-p')
WAIT_POLL = 2.0  # seconds between two looks at work in flight (--wait)
# Hang guards, in seconds, for events that normally arrive at once: the daemon's lock after
# its stop has returned, a sidebar's exit after its quit key, the sent text on the scratch pane.
LOCK_WAIT = QUIT_WAIT = SEND_WAIT = 15
COMMAND_WAIT = 5  # for a one-shot atm call (a loop's `atm list`) to end by itself
SWAP_WAIT = 300  # for atm-promote or atm-rollback; its own atm setup gives up after 60
# What `atm agent list` answers while a daemon that has just started still reconciles its
# agents (the list waits five seconds for that pass, then times out), and how long to keep asking.
STARTING = ('agent reconciliation is in progress', 'agent operation timed out', 'daemon request timed out')
AGENT_LIST_WAIT = 60
# What it answers when the organisation service is off because the log could not be replayed.
UNREADABLE_LOG = 'organisation service unavailable: events replay'
# The smoke's daemon delivers as soon as its stand-in is idle; the default policy waits ten seconds.
SMOKE_CONFIG = ('[delivery]\nstable_idle = { secs = 0, nanos = 1000000 }\n'
                'stable_empty_input = { secs = 0, nanos = 1000000 }\n')
# One command that writes a saved statusLine back into Claude's settings.json.
RESTORE = ('import json, pathlib, sys; path = pathlib.Path(sys.argv[1]); data = json.loads(path.read_text()); '
           'data["statusLine"] = json.loads(sys.argv[2]); path.write_text(json.dumps(data, indent=2) + "\\n")')
YOURS = ('Still yours: reload the pi extension in running pi sessions, trust the Codex hooks again if '
         'Codex asks, and look at any loop or new sidebar that called atm while the daemon was down.')


class CheckFailed(Exception):
    def __init__(self, name, detail):
        super().__init__(f'check {name} failed: {detail}')
        self.name, self.detail = name, detail


class NeedsYou(Exception):
    pass


class Stopped(Exception):
    """Refused before the swap, but after something was stopped and started again."""


def say(text=''):
    print(text, flush=True)


def listening(path):
    """Whether something accepts on the socket; a full backlog still means a listener."""
    with socket.socket(socket.AF_UNIX) as probe:
        probe.setblocking(False)
        try:
            probe.connect(str(path))
        except BlockingIOError:
            return True
        except OSError:
            return False
        return True


def fingerprint(folder):
    """Bytes and mode of both binaries: version text does not tell two builds apart."""
    return {name: (hashlib.sha256((folder / name).read_bytes()).hexdigest(),
                   stat.S_IMODE((folder / name).stat().st_mode)) for name in NAMES}


def is_sidebar(argv):
    rest = argv[1:]
    while rest:
        if rest[0] in SIDEBAR_FLAGS or rest[0].startswith('--tmux-session='):
            rest = rest[1:]
        elif rest[0] == '--tmux-session' and len(rest) > 1:
            rest = rest[2:]
        else:
            return False
    return True


def at_prompt(shell):
    """Whether the shell is in the foreground of its terminal: nothing it started is in front."""
    try:
        fields = Path(f'/proc/{shell}/stat').read_text().rsplit(')', 1)[1].split()
    except (OSError, IndexError):
        return False
    return len(fields) > 5 and fields[2] == fields[5]  # its process group is the terminal's


def in_flight(snapshot):
    """What a restart would break: it leaves such an agent stuck and such a message held."""
    busy = [f"agent {agent['agent_id']} is {agent['state']}" for agent in snapshot['agents'] or []
            if agent['state'] in ('starting', 'clearing')]
    busy += [f"message {message['message_id']} to {agent} is being delivered"
             for agent, messages in snapshot['messages'].items() for message in messages
             if message['delivery']['phase'] == 'delivering']
    return busy


class DaemonLock:
    """atmd's own single-instance lock, <socket>.lock: while this holds it, no daemon starts.

    Waiting for it in the background before the stop makes the hand-over direct: the lock
    is ours the moment the daemon lets go, so a caller's autostart finds it taken.
    """

    def __init__(self, socket_path):
        self.fd = os.open(str(socket_path) + '.lock',
                          os.O_RDONLY | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
        self.mutex, self.held, self.abandoned = threading.Lock(), threading.Event(), False
        if not stat.S_ISREG(os.fstat(self.fd).st_mode):
            self.release()
            raise Refused('the daemon lock is not a regular file')
        threading.Thread(target=self.wait, daemon=True).start()

    def wait(self):
        fcntl.flock(self.fd, fcntl.LOCK_EX)
        self.held.set()
        if self.abandoned:
            self.release()

    def acquired(self, seconds):
        return self.held.wait(seconds)

    def release(self):
        with self.mutex:
            if self.fd is not None:
                os.close(self.fd)
                self.fd = None

    def abandon(self):
        """Give up waiting: a lock that arrives later is dropped at once."""
        self.abandoned = True
        if self.held.is_set():
            self.release()


class Rollout:
    def __init__(self, args):
        self.args = args
        self.bin, self.socket = checked_path(args.bin_dir), checked_path(args.socket)
        self.atm, self.atmd = self.bin / 'atm', self.bin / 'atmd'
        self.metadata = self.bin / '.atm-promotion'
        self.record_path = self.metadata / 'rollout.json'
        # atm calls count as the owner's, never as the agent whose pane this runs in.
        self.env = {key: value for key, value in os.environ.items() if key != 'TMUX_PANE'}
        self.env['ATM_SOCKET'] = str(self.socket)
        label = self.env.get('ATM_TMUX_SOCKET')
        self.tmux = ['tmux', '-L', label] if label else ['tmux']  # the server atm itself picks
        self.source = self.manifest = self.old = self.new = None
        # What atm setup, run by the swap, edits: it runs with this HOME.
        self.settings = Path(self.env.get('HOME', '/')) / '.claude/settings.json'
        self.stopped = []  # sidebars that were quit and are not back yet
        self.servers = set()  # the tmux servers those sidebars live on
        self.pid = None  # the daemon's, before anything was stopped
        self.tool_ran = False  # atm-promote or atm-rollback ran, and with it atm setup
        self.interrupted = False  # Ctrl-C arrived while that tool ran

    # ---- helpers -------------------------------------------------------

    def call(self, argv, timeout=30, env=None):
        return subprocess.run([str(arg) for arg in argv], env=env or self.env, stdin=subprocess.DEVNULL,
                              capture_output=True, text=True, errors='replace', timeout=timeout)

    def atm_json(self, *args, timeout=30):
        result = self.call([self.atm, *args], timeout=timeout)
        if result.returncode != 0:
            raise OSError(f"atm {' '.join(args)} failed (exit {result.returncode}): {result.stderr.strip()}")
        return json.loads(result.stdout)

    def tool(self, name, *extra):
        """atm-promote or atm-rollback, in its own session: a Ctrl-C on this terminal reaches
        only the rollout, which waits for the tool instead of ending it in the middle of a swap."""
        self.tool_ran = True
        say(f"$ {name} {' '.join(map(str, extra))} --bin-dir {self.bin} --socket {self.socket}")
        argv = [str(arg) for arg in (sys.executable, SCRIPTS / name, *extra, '--bin-dir', self.bin,
                                     '--socket', self.socket)]
        process = subprocess.Popen(argv, env=self.env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True, errors='replace', start_new_session=True)
        while True:
            try:
                out, err = process.communicate(timeout=SWAP_WAIT)
                break
            except KeyboardInterrupt:
                self.interrupted = True
                say('interrupted; waiting for the swap to finish')
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)  # hung: the tool and the atm setup it started
                process.wait()
                raise
        print(out, end='', flush=True)
        print(err, end='', file=sys.stderr, flush=True)
        return subprocess.CompletedProcess(argv, process.returncode, out, err)

    def daemon_pid(self):
        result = self.call([self.atmd, 'status'])
        match = re.search(r'\(PID (\d+)\)', result.stdout)
        return int(match.group(1)) if match else None

    def processes(self):
        """Every process of this user on the installed pair, by executable path or inode."""
        paths = {self.bin / name: name for name in NAMES}
        inodes = {(info.st_dev, info.st_ino): name for path, name in paths.items()
                  for info in (path.stat(),)}
        found = []
        for entry in Path('/proc').iterdir():
            if not entry.name.isdecimal() or int(entry.name) == os.getpid():
                continue
            try:
                if entry.stat().st_uid != os.getuid():
                    continue
                info = (entry / 'exe').stat()
                path = Path(os.readlink(entry / 'exe').removesuffix(' (deleted)'))
                name = paths.get(path) or inodes.get((info.st_dev, info.st_ino))
                who = identity(entry.name)
                if not name or who is None:
                    continue
                argv = [os.fsdecode(arg) for arg in (entry / 'cmdline').read_bytes().split(b'\0')[:-1]]
                parent = int((entry / 'stat').read_text().rsplit(')', 1)[1].split()[1])
                env = {os.fsdecode(k): os.fsdecode(v) for k, v in process_env(entry.name).items()}
            except (FileNotFoundError, ProcessLookupError, PermissionError):
                continue  # gone, or not ours to read: atm-promote's own check still refuses
            found.append(dict(pid=who['pid'], start=who['start'], name=name, argv=argv, parent=parent,
                              current=(info.st_dev, info.st_ino) in inodes,
                              server=env.get('TMUX', '').split(',')[0] or None, pane=env.get('TMUX_PANE'),
                              env={key: env.get(key) for key in DAEMON_ENV}))
        return found

    # ---- step 0: preflight ---------------------------------------------

    def preflight(self):
        directory(self.bin)
        pair(self.bin)
        require(shutil.which('tmux', path=self.env.get('PATH')), 'missing tool: tmux')
        require(not any(self.bin.glob('.atm-stage-*')), 'interrupted promotion; run atm-rollback first')
        self.old = fingerprint(self.bin)
        if self.args.rollback:
            saved = previous(self.metadata)
            require(not same_pair(self.bin, saved), 'the previous pair is already installed')
            self.new = fingerprint(saved)
        else:
            candidate = checked_path(self.args.candidate)
            self.source = candidate / 'bin'
            require(not self.source.is_relative_to(self.bin) and not self.bin.is_relative_to(self.source),
                    'candidate and destination must not overlap')
            pair(self.source)
            self.manifest = json.loads((candidate / 'install.json').read_text())
            require(isinstance(self.manifest, dict) and isinstance(self.manifest.get('hashes'), dict)
                    and re.fullmatch('[0-9a-f]{40}', str(self.manifest.get('sha'))),
                    'install.json names no commit and hashes')
            self.new = fingerprint(self.source)
            for name in NAMES:
                require(self.manifest['hashes'].get(name) == self.new[name][0],
                        f'candidate binaries do not match install.json: {name}')
            require(self.new != self.old, 'candidate pair already installed')
        require(listening(self.socket), f'no daemon answers on {self.socket}; start it and run again')
        pid = self.daemon_pid()
        require(pid, 'atmd status names no running daemon; is this shell using another state directory?')
        self.pid = pid
        daemons = [process for process in self.processes() if process['name'] == 'atmd']
        ours = [process for process in daemons if process['pid'] == pid and process['current']]
        require(ours, f'the daemon (pid {pid}) does not run {self.atmd}')
        for other in daemons:
            require(other['pid'] == pid, f"pid {other['pid']} also runs {self.atmd}; stop it first")
        for key, value in ours[0]['env'].items():
            require(value == self.env.get(key),
                    f'the daemon was started with {key}={value}, this shell has {self.env.get(key)}; '
                    'a restart from here would use other state')
        say(f"Installed: {self.old['atm'][0][:12]} atm, {self.old['atmd'][0][:12]} atmd; daemon pid {pid}")
        say(f"To install: {self.new['atm'][0][:12]} atm, {self.new['atmd'][0][:12]} atmd"
            + (' (the previous pair)' if self.args.rollback else f" (commit {self.manifest['sha']})"))

    # ---- step 1: the protected-file report ------------------------------

    def recorded_commit(self):
        if not self.record_path.exists():
            return None
        return json.loads(self.record_path.read_text()).get('commit')

    def report(self):
        recorded, given = self.recorded_commit(), self.args.installed_commit
        require(recorded or given, "installed build's commit is unknown: pass --installed-commit <rev>")
        require(not (recorded and given) or recorded == given,
                f'--installed-commit disagrees with the recorded {recorded}')
        # The report runs in the current directory, as atm-protected-files.py documents.
        result = self.call([sys.executable, SCRIPTS / 'atm-protected-files.py', recorded or given,
                            self.manifest['sha']], timeout=120)
        require(result.returncode == 0, f'protected-file report failed: {result.stderr.strip()}')
        digest = hashlib.sha256(result.stdout.encode()).hexdigest()
        say(result.stdout.rstrip('\n'))
        say(f'Report sha256: {digest}')
        if self.args.dry_run:
            return
        if self.args.report_sha256:
            require(self.args.report_sha256 == digest, 'protected-file report not accepted: '
                    '--report-sha256 is not the hash of this report')
            return
        require(sys.stdin.isatty(), 'the report needs your yes: use a terminal or --report-sha256 <hash>')
        try:
            answer = input('Type yes to accept this report: ')
        except EOFError:  # Ctrl-D is a no
            answer = ''
        require(answer.strip() == 'yes', 'protected-file report not accepted')

    # ---- step 2: the smoke, private ---------------------------------------

    def smoke(self):
        """One session through up, message, clear and down on a private socket, state and tmux."""
        real = self.args.real
        harness = real or 'pi'
        require(real or STAND_IN.is_file(), f'stand-in not found: {STAND_IN}; run from a full checkout')
        root = Path(tempfile.mkdtemp(prefix='atm-rollout-smoke-')).resolve()
        for sub in ('tmux', 'state', 'config/atm', 'home', 'bin', 'tmp', 'project/.atm/roles'):
            (root / sub).mkdir(parents=True)
        (root / 'config/atm/config.toml').write_text(SMOKE_CONFIG)
        (root / 'project/.atm/roles/smoke.md').write_text(f"+++\nharness='{harness}'\n+++\n")
        # Panes inherit the tmux server's environment; the daemon gets its own below.
        pane_env = dict(self.env, ATM_SOCKET=str(root / 'atm.sock'), ATM_TMUX_SOCKET='smoke',
                        TMUX_TMPDIR=str(root / 'tmux'), TMPDIR=str(root / 'tmp'))
        pane_env.pop('TMUX', None)
        if not real:
            os.symlink(STAND_IN, root / 'bin/pi')  # the stand-in takes its mode from this name
            pane_env.update(HOME=str(root / 'home'), PATH='/usr/local/bin:/usr/bin:/bin', TERM='xterm-256color',
                            LANG='C.UTF-8', ATM_STANDIN_ROOT=str(root), ATM_STANDIN_LOG=str(root / 'smoke.jsonl'),
                            ATM_STANDIN_ID_PREFIX='smoke', PYTHONDONTWRITEBYTECODE='1')
        env = dict(pane_env, ATM_STATE_DIR=str(root / 'state'), XDG_STATE_HOME=str(root / 'state'),
                   XDG_CONFIG_HOME=str(root / 'config'))
        if not real:
            env['ATM_SPAWN_BIN'] = str(root / 'bin/pi')
        atm, atmd, tmux = self.source / 'atm', self.source / 'atmd', ['tmux', '-L', 'smoke']
        stage, server = 'start', None
        project = str(root / 'project')

        def step(*args, timeout=90, caller=None):
            result = self.call([atm, *args, '--format', 'json'], timeout=timeout, env=caller or env)
            require(result.returncode == 0, f"smoke failed at {stage}: atm {' '.join(args)}: "
                    f'{(result.stderr or result.stdout).strip()}; run kept in {root}')
            return json.loads(result.stdout)

        def tm(*args):
            result = self.call([*tmux, *args], env=pane_env)
            require(result.returncode == 0, f'smoke failed at {stage}: tmux {args[0]}: '
                    f'{result.stderr.strip()}; run kept in {root}')
            return result.stdout.strip()

        say(f'Smoke ({harness}{"" if real else " stand-in"}) on a private socket in {root}')
        try:
            # A start splits the caller's pane: this one is born tall enough for both halves.
            placement = tm('-f', '/dev/null', 'new-session', '-d', '-s', 'smoke', '-x', '200', '-y', '101',
                           '-P', '-F', '#{pane_id}', '/bin/bash --noprofile --norc')
            server = tm('display-message', '-p', '#{socket_path},#{pid}')
            require(Path(server.split(',')[0]).is_relative_to(root), 'smoke tmux server is not private')
            for option, value in (('default-shell', '/bin/bash'), ('status', 'off'),
                                  ('default-command', '/bin/bash --noprofile --norc')):
                tm('set-option', '-g', option, value)
            started = self.call([atmd, 'start', '-d'], env=env)
            require(started.returncode == 0, f'smoke failed at start: atmd: {started.stderr.strip()}')
            wait_for(lambda: listening(root / 'atm.sock'), 'the smoke daemon to answer')
            stage = 'up'
            up = step('agent', 'up', 'smoke', '--project-root', project,
                      caller=dict(env, TMUX=server + ',0', TMUX_PANE=placement))
            require(up.get('state') == 'ready', f"smoke failed at up: the agent is {up.get('state')}; "
                    f'run kept in {root}')
            agent = up['agent_id']
            stage = 'message'
            sent = step('msg', 'send', '--to', agent, '--body', 'rollout smoke', '--project-root', project)

            def delivered():
                entry = step('inbox', '--for', agent, '--id', sent['message_id'],
                             '--project-root', project)['messages']
                return entry and entry[0]['delivery']['phase'] == 'delivered'
            wait_for(delivered, f'smoke failed at message: not delivered; run kept in {root}', 60)
            stage = 'clear'  # which needs the turn that the message started to be over

            def idle():
                return [row for row in step('agent', 'list') if row['agent_id'] == agent
                        and row['activity'] == 'idle']
            wait_for(idle, f'smoke failed at clear: the turn did not end; run kept in {root}', 60)
            step('agent', 'clear', agent)
            stage = 'down'
            step('agent', 'down', agent, '--kill')
            state = [row['state'] for row in step('agent', 'list') if row['agent_id'] == agent]
            require(state == ['down'], f'smoke failed at down: the agent is {state}; run kept in {root}')
        except (TimedOut, subprocess.SubprocessError, OSError, ValueError, KeyError) as error:
            raise Refused(f'smoke failed at {stage}: {error}; run kept in {root}') from error
        finally:
            # The private daemon by its own pid file; the tmux server by the socket path
            # checked above. Neither may keep the other from being stopped.
            ending = [([atmd, 'stop'], env)]
            if server:
                ending.append((['tmux', '-S', server.split(',')[0], 'kill-server'], pane_env))
            for argv, where in ending:
                try:
                    self.call(argv, timeout=STOP_WAIT_TIMEOUT + 15, env=where)
                except (OSError, subprocess.SubprocessError) as error:
                    say(f'Smoke cleanup: {argv[-1]} failed: {error}; look in {root}')
        shutil.rmtree(root)
        say('Smoke passed: up, message, clear, down.')

    # ---- step 3: the snapshot ---------------------------------------------

    def unread(self, agent):
        messages, after = [], None
        while True:
            page = self.atm_json('inbox', '--for', agent, '--unread', '--format', 'json',
                                 *(['--after', after] if after else []))
            messages += page['messages']
            after = page.get('next_after')
            if not after:
                return messages

    def status_line(self):
        try:
            return json.loads(self.settings.read_text()).get('statusLine')
        except (OSError, ValueError, AttributeError):
            return None

    def named_agents(self, patience=0):
        """The installed build's agent list; None for a build from before named agents.
        For `patience` seconds it asks again while the daemon answers that it is still starting."""
        deadline = time.monotonic() + patience
        while True:
            listed = self.call([self.atm, 'agent', 'list', '--format', 'json'])
            starting = listed.returncode != 0 and any(text in listed.stderr for text in STARTING)
            if not starting or time.monotonic() >= deadline:
                break
            time.sleep(.2)  # bounded poll on "it answers", not a settling delay
        if listed.returncode == 2 and 'unrecognized subcommand' in listed.stderr:
            return None
        if listed.returncode != 0:
            raise OSError(f'atm agent list exit {listed.returncode}: {listed.stderr.strip()}')
        return json.loads(listed.stdout)

    def panes(self, server=None):
        """The pane ids on a tmux server (the one atm picks, unless given); None if it is silent."""
        listed = self.call([*(['tmux', '-S', server] if server else self.tmux), 'list-panes', '-a', '-F',
                            '#{pane_id}'])
        return set(listed.stdout.split()) if listed.returncode == 0 else None

    def snapshot(self):
        sessions = self.atm_json('list', '--format', 'json')
        agents = self.named_agents()
        messages = {agent['agent_id']: self.unread(agent['agent_id']) for agent in agents or []}
        processes = self.processes()
        return dict(sessions=sessions, agents=agents, messages=messages, status_line=self.status_line(),
                    sidebars=[p for p in processes if p['name'] == 'atm' and is_sidebar(p['argv'])],
                    commands=[p for p in processes if p['name'] == 'atm' and not is_sidebar(p['argv'])])

    def lingering(self, commands):
        """The atm commands that are still running after a moment: not a loop's short call."""
        def alive():
            return [p for p in commands if (identity(p['pid']) or {}).get('start') == p['start']]
        try:
            wait_for(lambda: not alive(), 'atm commands to end', COMMAND_WAIT)
        except TimedOut:
            pass
        return alive()

    def quiet_snapshot(self):
        """The state before, taken when nothing is in flight; refuses, or waits with --wait."""
        deadline, shown = time.monotonic() + self.args.wait, None
        while True:
            snapshot = self.snapshot()
            busy = in_flight(snapshot)
            busy += [f"pid {p['pid']} runs \"{shlex.join(p['argv'])}\", not a sidebar; wait for it or stop "
                     'what calls it' for p in self.lingering(snapshot['commands'])]
            if not busy:
                break
            require(time.monotonic() < deadline, '; '.join(busy))
            if busy != shown:
                say('Waiting for a quiet moment: ' + '; '.join(busy))
                shown = busy
            time.sleep(WAIT_POLL)  # bounded poll on the condition above, not a settling delay
        for sidebar in snapshot['sidebars']:
            where = f"sidebar pid {sidebar['pid']}"
            require(sidebar['server'] and sidebar['pane'], f'{where} is not in a tmux pane; quit it yourself')
            shell = self.call(['tmux', '-S', sidebar['server'], 'display-message', '-p', '-t',
                               sidebar['pane'], '#{pane_pid}'])
            require(shell.returncode == 0 and shell.stdout.strip() == str(sidebar['parent']),
                    f"{where} was not started from the shell of pane {sidebar['pane']} and would not come "
                    'back; quit it yourself')
        working = [row for row in snapshot['sessions'] if row.get('status') == 'working']
        say(f"Sessions: {len(snapshot['sessions'])}, {len(working)} mid-turn"
            + (' (a restart does not stop them): ' + ', '.join(
                f"{row.get('id_short')} {row.get('tmux_pane') or '-'}" for row in working) if working else ''))
        panes = self.panes()
        for agent in snapshot['agents'] or []:
            say(f"Agent {agent['agent_id']}: {agent['state']}, pane {agent.get('pane') or '-'}")
            if panes is not None and agent.get('pane') and agent['pane'] not in panes:
                say(f"Warning: agent {agent['agent_id']}'s pane {agent['pane']} is gone; the restart will "
                    'record it down')
        if snapshot['agents'] is None:
            say('The installed build has no named agents.')
        for sidebar in snapshot['sidebars']:
            say(f"Sidebar pid {sidebar['pid']} in {sidebar['pane']}: {shlex.join(sidebar['argv'])}")
        return snapshot

    # ---- steps 4 and 6: stop and start ------------------------------------

    def quit_sidebars(self, sidebars):
        for sidebar in sidebars:
            where = f"sidebar pid {sidebar['pid']} in {sidebar['pane']}"
            try:
                watched = os.pidfd_open(sidebar['pid'])
            except ProcessLookupError:
                continue  # it quit by itself since the snapshot
            try:
                who = identity(sidebar['pid'])
                if who is None or who['start'] != sidebar['start']:
                    continue
                key = self.call(['tmux', '-S', sidebar['server'], 'send-keys', '-t', sidebar['pane'], 'C-c'])
                require(key.returncode == 0 and select.select([watched], [], [], QUIT_WAIT)[0],
                        f'{where} did not quit')
                self.stopped.append(sidebar)
                self.servers.add(sidebar['server'])
            finally:
                os.close(watched)

    def stop_daemon(self):
        """Stop the daemon and return its lock, held: nothing can start one until release."""
        lock = DaemonLock(self.socket)
        try:
            result = self.call([self.atmd, 'stop'], timeout=STOP_WAIT_TIMEOUT + 15)
            output = (result.stdout + result.stderr).strip()
            require(result.returncode == 0, f'the daemon did not stop: {output}')
            require(lock.acquired(LOCK_WAIT) and not os.path.lexists(self.socket),
                    f'another daemon took the socket, or its path stayed: {output}')
        except BaseException:
            lock.abandon()
            raise
        return lock

    def start_daemon(self):
        """Start the daemon and wait for its answer; one a caller started first is as good.
        atmd binds after its startup discovery, so an answer means it is ready."""
        result = self.call([self.atmd, 'start', '-d'])
        try:
            wait_for(lambda: listening(self.socket), 'the daemon to answer')
        except TimedOut as error:
            raise CheckFailed('start', f'{error}: {(result.stdout + result.stderr).strip()}') from error

    def restart_sidebars(self):
        """Type each quit sidebar's line back; those that did not come back stay in `stopped`."""
        for sidebar in list(self.stopped):
            line = shlex.join(sidebar['argv'])
            tmux = ['tmux', '-S', sidebar['server']]

            def running():
                return [p for p in self.processes() if p['parent'] == sidebar['parent'] and p['current']]
            try:
                shell = self.call([*tmux, 'display-message', '-p', '-t', sidebar['pane'], '#{pane_pid}'])
                if shell.returncode == 0 and shell.stdout.strip() == str(sidebar['parent']):
                    # The pane is still that shell; wait until it has its terminal back, then
                    # type the same line atm's own sidebar injection types.
                    wait_for(lambda: at_prompt(sidebar['parent']), 'the shell prompt', QUIT_WAIT)
                    self.call([*tmux, 'send-keys', '-l', '-t', sidebar['pane'], line])
                    self.call([*tmux, 'send-keys', '-t', sidebar['pane'], 'Enter'])
                    wait_for(running, 'the sidebar to start', 15)
                    say(f"Sidebar back in {sidebar['pane']}: {line}")
                    self.stopped.remove(sidebar)
                    continue
            except (TimedOut, OSError, subprocess.SubprocessError):
                pass
            say(f"Sidebar in {sidebar['pane']} is NOT back; type there: {line}")
        return self.stopped

    # ---- step 7: the checks -------------------------------------------------

    def check_send(self):
        """One atm send into a scratch session on the tmux server atm itself uses."""
        made = self.call([*self.tmux, 'new-session', '-d', '-s', f'atm-rollout-{os.getpid()}', '-x', '80',
                          '-y', '24', '-P', '-F', '#{session_id} #{pane_id}', 'cat'])
        if made.returncode != 0:
            raise CheckFailed('send', f'no scratch session: {made.stderr.strip()}')
        session, pane = made.stdout.split()
        token = 'rollout-check-' + uuid.uuid4().hex[:12]
        try:
            sent = self.call([self.atm, 'send', pane, token], timeout=60)
            if sent.returncode != 0:
                raise CheckFailed('send', (sent.stderr or sent.stdout).strip())
            # Typed once by the terminal and once more by cat: the Enter arrived too.
            wait_for(lambda: self.call([*self.tmux, 'capture-pane', '-p', '-t', pane]).stdout.count(token) >= 2,
                     'the sent text', SEND_WAIT)
        except TimedOut as error:
            raise CheckFailed('send', f'the text did not reach scratch pane {pane}') from error
        finally:
            self.call([*self.tmux, 'kill-session', '-t', session])

    def checks(self, before, expected, send=True):
        """Raises CheckFailed with the first check that fails. Without `send` they only read.
        Returns the agents of `before` that are now down, each with its pane of then."""
        def answers(name, *args):
            result = self.call([self.atm, *args])
            if result.returncode != 0:
                raise CheckFailed(name, f"atm {' '.join(args)} exit {result.returncode}: {result.stderr.strip()}")
            return result.stdout

        down = []
        try:
            if fingerprint(self.bin) != expected:
                raise CheckFailed('identity', 'the installed pair is not the expected one')
            pid = self.daemon_pid()
            if not [p for p in self.processes() if p['pid'] == pid and p['name'] == 'atmd' and p['current']]:
                raise CheckFailed('identity', f'the daemon (pid {pid}) does not run the installed atmd')
            say(f"Version: {answers('identity', '--version').strip()}, daemon pid {pid}")
            json.loads(answers('list', 'list', '--format', 'json'))
            answers('status', 'status')
            try:
                listed = self.named_agents(AGENT_LIST_WAIT)
            except (OSError, ValueError) as error:
                raise CheckFailed('agent-list', str(error)) from error
            if listed is None:  # the build now installed is from before named agents
                say(f"This build has no named agents; the {len(before['agents'] or [])} of before are in "
                    'the state directory, untouched, and not visible to it.')
            else:
                after = {agent['agent_id']: agent for agent in listed}
                for agent in before['agents'] or []:
                    name, pane, now = agent['agent_id'], agent.get('pane'), after.get(agent['agent_id'])
                    # Down is a state the difference prints, not this build's fault: a daemon's
                    # start records an agent whose pane is gone as down, and so would the old one.
                    if now is None or (now['state'] != 'down' and now.get('pane') != pane):
                        raise CheckFailed('agents', f"{name} was on pane {pane or '-'} and is now "
                                          + (f"on {now.get('pane') or '-'}" if now else 'missing'))
                    if now['state'] == 'down':
                        down.append((name, pane))
            if send:
                self.check_send()
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            raise CheckFailed('checks', str(error)) from error
        say('Checks passed: identity, list, status'
            + ('' if listed is None else ', agent-list, agents') + (', send.' if send else '.'))
        return down

    def warn_down(self, down, advice='a daemon misread the pane: look at it'):
        """Down on a pane that is still there is what a harness that exited leaves, and also what
        a build that misreads live panes records at its start. The checks pass both; this says it.
        A pane that step 3 warned was gone is not among them: tmux does not use a pane id twice."""
        panes = self.panes() or ()
        for name, pane in down:
            if pane in panes:
                say(f'rollout: WARNING: agent {name} is now down but its pane {pane} is still there. '
                    f'If you did not stop it, {advice}')

    # ---- step 9: what differs -----------------------------------------------

    def difference(self, before):
        try:
            after = self.snapshot()
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            say(f'Could not read the state after, to compare it with before: {error}')
            return
        panes = set()
        for server in self.servers | {None}:
            panes |= self.panes(server) or set()
        def by_pane(rows):  # a row found by discovery has a placeholder id, new with every daemon
            return {row.get('tmux_pane') or row['id']: row for row in rows
                    if row.get('tmux_pane') or not row['id'].startswith('pending-')}
        old, new, lines = by_pane(before['sessions']), by_pane(after['sessions']), []
        for key, row in old.items():
            name, now = f"session {row.get('id_short')} on {row.get('tmux_pane') or '-'}", new.get(key)
            if now is None:
                pane = row.get('tmux_pane')
                lines.append(f'{name}: no row ' + ('until its next event' if pane in panes else
                                                   '(its pane is gone)' if pane else '(it had no pane)'))
            elif now['id'] != row['id']:
                lines.append(f"{name}: " + ('only a placeholder row until its next event'
                                            if now['id'].startswith('pending-') else f"now {now.get('id_short')}"))
        lines += [f"session {row.get('id_short')} on {row.get('tmux_pane') or '-'}: new"
                  for key, row in new.items() if key not in old]
        states = {agent['agent_id']: agent['state'] for agent in after['agents'] or []}
        changed = [(agent['agent_id'], agent['state'], states.get(agent['agent_id']))
                   for agent in before['agents'] or [] if states.get(agent['agent_id']) != agent['state']]
        changed.sort(key=lambda change: change[2] not in ('stuck', 'down'))
        lines += [f'agent {name}: {was} -> {now}' for name, was, now in changed]
        say('Differs from before: ' + ('nothing' if not lines else ''))
        for line in lines:
            say('  ' + line)

    def status_line_note(self, before):
        """atm setup replaces a statusLine that is not atm's own; say so, restore nothing."""
        was, now = before['status_line'], self.status_line()
        if not self.tool_ran or was is None or was == now:
            return

        def command(entry):
            return entry.get('command') if isinstance(entry, dict) else entry
        backup = Path(self.env.get('XDG_CONFIG_HOME') or Path(self.env.get('HOME', '/')) / '.config') \
            / 'atm/claude-statusline-backup.json'
        say(f"atm setup CHANGED Claude's statusLine in {self.settings}:")
        say(f'  was: {command(was)}')
        say(f'  now: {command(now)}')
        say(f'  setup keeps the one it replaced in {backup}')
        say('  to put yours back: ' + shlex.join(['python3', '-c', RESTORE, str(self.settings), json.dumps(was)]))

    def record(self):
        commit = self.manifest['sha'] if self.manifest else None
        if self.args.rollback:  # back on the build the last rollout replaced, if it was written down
            commit = json.loads(self.record_path.read_text()).get('previous') if self.record_path.exists() else None
            if not commit:
                self.record_path.unlink(missing_ok=True)
                return
        data = dict(commit=commit, previous=None if self.args.rollback else
                    self.recorded_commit() or self.args.installed_commit)
        temporary = self.record_path.with_name('.rollout.json.tmp')
        temporary.write_text(json.dumps(data) + '\n')
        os.replace(temporary, self.record_path)

    # ---- the whole thing ----------------------------------------------------

    def state(self):
        try:
            now = fingerprint(self.bin)
        except OSError:
            now = None
        was, target = ('the newer pair', 'the previous pair') if self.args.rollback else \
            ('the previous pair', 'the new pair')
        which = was if now == self.old else target if now == self.new else 'a MIXED pair'
        back = '; '.join(f"type in {s['pane']}: {shlex.join(s['argv'])}" for s in self.stopped)
        return (f"{which} is installed; a daemon {'answers' if listening(self.socket) else 'does NOT answer'} "
                f'on {self.socket}' + (f'; sidebars not back: {back}' if back else ''))

    def plan(self):
        swap = ('atm-rollback' if self.args.rollback else f'atm-promote --from-bin {self.source}') \
            + f' --bin-dir {self.bin} --socket {self.socket}'
        say('Dry run: nothing was changed. The run would:')
        for line in ([] if self.args.rollback else [
                f'smoke: {self.source}/atm agent up, msg send, agent clear, agent down --kill on a private '
                f'socket with {self.args.real or "the pi stand-in"}']) + [
                'quit each sidebar above with C-c in its pane',
                f'take {self.socket}.lock, then: {self.atmd} stop',
                f'scripts/{swap}',
                f'release the lock, then: {self.atmd} start -d',
                'checks: identity, list, status, agent-list, agents, send (a scratch tmux session)',
                'on a failed check: stop, scripts/atm-rollback, start, the checks again' if not self.args.rollback
                else 'on a failed check: stop and tell you',
                'type each sidebar line back, print what differs']:
            say('  ' + line)

    def run(self):
        self.preflight()
        if not self.args.rollback:
            self.report()
            if not self.args.dry_run:
                self.smoke()
        before = self.quiet_snapshot()
        if self.args.dry_run:
            self.plan()
            return 0
        try:
            return self.switch(before)
        except (Refused, Stopped):
            raise  # switch() has put back what it stopped
        except (Exception, KeyboardInterrupt) as error:
            # Whatever atm setup did is said on this path too.
            self.status_line_note(before)
            words = str(error) if isinstance(error, NeedsYou) else f'{type(error).__name__}: {error}'
            raise NeedsYou(f'{words}; {self.state()}') from error

    def switch(self, before):
        lock = None
        try:
            self.quit_sidebars(before['sidebars'])
            lock = self.stop_daemon()
            swap = self.tool('atm-rollback') if self.args.rollback else \
                self.tool('atm-promote', '--from-bin', self.source)
            if swap.returncode != 0:
                words = (swap.stderr.strip().splitlines() or ['no message'])[-1]
                # atm-promote's own recovery failed: its "do not restart yet" stands.
                if 'recovery failed' in swap.stderr or 'do not restart' in swap.stderr:
                    raise NeedsYou(f'{words}; the daemon was not started')
                if 'setup rerun' in swap.stderr:
                    say('atm setup ran for the new build, then again for the previous one.')
                require(fingerprint(self.bin) != self.old,
                        f'the swap failed and the previous pair is in place: {words}')
                raise NeedsYou('the swap failed and did not restore the previous pair; the daemon is stopped')
        except Refused as error:
            self.restore(before, lock, error)
        lock.release()
        if self.interrupted and not self.args.rollback:
            # Ctrl-C during atm-promote means "not this build": the swap finished, now back.
            return self.roll_back(CheckFailed('interrupted', 'Ctrl-C during the swap'), before)
        try:
            self.start_daemon()
            down = self.checks(before, self.new)
        except CheckFailed as failure:
            if self.args.rollback:
                raise NeedsYou(f'the previous pair is installed, but {failure}') from failure
            return self.roll_back(failure, before)
        self.restart_sidebars()
        self.difference(before)
        self.status_line_note(before)
        say(YOURS)
        self.record()
        back = f'scripts/atm-rollout --rollback --bin-dir {self.bin} --socket {self.socket}'
        self.warn_down(down, *([] if self.args.rollback else [
            f'this build misread the pane: look at it, then roll back with: {back}']))
        say('rollout: DONE: the ' + ('previous' if self.args.rollback else 'new') + ' build is running and '
            'passed the checks.' + ('' if self.args.rollback else f' To go back: {back}'))
        return 0

    def restore(self, before, lock, reason):
        """Nothing is swapped: bring back what was stopped, then refuse. A restart is a change,
        so it is said, checked and compared; "nothing changed" is only for nothing stopped."""
        quit_any = bool(self.stopped)
        if lock:
            lock.release()
        if not listening(self.socket):
            self.start_daemon()
        missing = self.restart_sidebars()
        now = self.daemon_pid()
        if now == self.pid and not quit_any:
            raise Refused(reason)
        try:
            down = self.checks(before, self.old, send=False)
        except CheckFailed as failure:
            raise NeedsYou(f'refused before the swap ({reason}); the installed build was started again, '
                           f'and then {failure}') from failure
        if missing:
            raise NeedsYou(f'refused before the swap ({reason}); the installed build runs, but a sidebar '
                           'is not back')
        self.difference(before)
        self.status_line_note(before)
        self.warn_down(down)
        daemon = f'was started again (daemon pid {self.pid} -> {now})' if now != self.pid else \
            f'kept running (daemon pid {now})'
        raise Stopped(f'the installed build is unchanged and {daemon}: {reason}')

    def roll_back(self, failure, before):
        say(f'rollout: {failure}; rolling back')
        try:
            lock = self.stop_daemon()
        except Refused as error:  # the new pair is installed: this is no refusal
            raise NeedsYou(f'{failure}, and the rollback could not stop the daemon: {error}') from error
        swap = self.tool('atm-rollback')
        lock.release()
        if swap.returncode != 0:
            raise NeedsYou(f'{failure}, and atm-rollback failed; the daemon is stopped')
        try:
            self.start_daemon()
            down = self.checks(before, self.old)
        except CheckFailed as second:
            if second.name == 'agent-list' and UNREADABLE_LOG in second.detail:
                raise NeedsYou(f'{failure}; the previous build runs but cannot read the organisation log the '
                               'newer build wrote; named agents and messages are unavailable until the newer '
                               'build is back; the state directory was not touched') from second
            raise NeedsYou(f'{failure}; rolled back, and then {second}') from second
        self.restart_sidebars()
        self.status_line_note(before)
        self.warn_down(down)
        say(f'rollout: ROLLED BACK: {failure}; the previous build is running and passed the checks.')
        return 3


def main(argv=None):
    parser = argparse.ArgumentParser(prog='atm-rollout', description=__doc__.splitlines()[0])
    parser.add_argument('candidate', nargs='?', help='a candidate-kit install directory (install.json, bin/)')
    parser.add_argument('--rollback', action='store_true', help='put the previous pair back instead')
    parser.add_argument('--bin-dir', required=True)
    parser.add_argument('--socket', required=True)
    parser.add_argument('--installed-commit', metavar='REV', help="the installed build's commit, the first time")
    parser.add_argument('--report-sha256', metavar='HEX', help='the hash of the report you already read')
    parser.add_argument('--wait', nargs='?', type=int, const=300, default=0, metavar='SECONDS',
                        help='wait for a quiet moment instead of refusing (--wait=SECONDS, default 300)')
    parser.add_argument('--real', metavar='HARNESS', help='smoke with this real harness: starts model turns')
    parser.add_argument('--dry-run', action='store_true', help='read and print only; change nothing')
    args = parser.parse_args(argv)
    if bool(args.candidate) == args.rollback:
        parser.error('give a candidate directory, or --rollback')
    try:
        return Rollout(args).run()
    except NeedsYou as error:
        print(f'rollout: NEEDS YOU: {error}', file=sys.stderr)
        return 4
    except Stopped as error:
        print(f'rollout: refused before the swap; {error}', file=sys.stderr)
        return 1
    except (Refused, TimedOut, OSError, ValueError, KeyError, subprocess.SubprocessError,
            KeyboardInterrupt) as error:
        print(f'rollout: refused, nothing changed: {error or "interrupted"}', file=sys.stderr)
        return 1
