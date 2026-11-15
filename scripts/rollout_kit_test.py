#!/usr/bin/env python3
"""Sealed rollout contracts: fake home and bin, a private daemon and a private tmux server.

Cargo supplies the binaries. Every wait is on a condition: a process, a socket, a screen.
Nothing here may reach the default socket or an installed pair: see Rig.setUp.
"""
import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import pty
import re
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
SCRIPTS = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPTS))
import atm_rollout  # noqa: E402
from atm_candidate import wait_for  # noqa: E402

NAMES = ('atm', 'atmd')
LABEL = 'rollout-test'
# A wrapper `atm`: every command goes to the real binary except those the test names in
# modes.json, which exit with the given code (a pair adds the error text), or print the
# given text and exit 0.
WRAPPER = '''#!/usr/bin/python3
import json, os, sys
modes = json.load(open(MODES))
for words, action in modes.items():
    if sys.argv[1:1 + len(words.split())] == words.split():
        if isinstance(action, int):
            action = [action, 'injected failure: ' + words]
        if isinstance(action, list) and action and isinstance(action[0], int):
            print(action[1], file=sys.stderr)
            sys.exit(action[0])
        print(action if isinstance(action, str) else json.dumps(action))
        sys.exit(0)
os.execv(REAL, [REAL, *sys.argv[1:]])
'''


class Rig(unittest.TestCase):
    """One temp dir per test: home, installed bin, candidate install, state, tmux, a git repo."""

    @classmethod
    def setUpClass(cls):
        # One copy of the Cargo-built pair per class; tests link to it. `new` is the same
        # build with another mode, as the promotion kit's real-setup tests have it. The
        # copies are stripped where strip exists: every run copies and hashes them often.
        cls.cache = tempfile.TemporaryDirectory(prefix='roll-bin-')
        cls.addClassCleanup(cls.cache.cleanup)
        for label in ('old', 'new'):
            folder = Path(cls.cache.name).resolve() / label
            folder.mkdir()
            for name in NAMES:
                built = os.environ.get('CANDIDATE_TEST_' + name.upper())
                assert built and Path(built).is_file(), 'Cargo-built CANDIDATE_TEST_' + name.upper() + ' required'
                try:
                    stripped = subprocess.run(['strip', '-o', str(folder / name), built]).returncode == 0
                except OSError:
                    stripped = False
                if not stripped:
                    shutil.copy2(built, folder / name)
        (Path(cls.cache.name).resolve() / 'new/atm').chmod(0o751)

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='roll-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.home, self.bin, self.candidate = self.root / 'home', self.root / 'bin', self.root / 'candidate'
        for folder in (self.home, self.bin, self.candidate / 'bin', self.root / 'tmux', self.root / 'state'):
            folder.mkdir(parents=True)
        self.socket = self.root / 'atm.sock'
        self.env = {k: v for k, v in os.environ.items()
                    if not k.startswith('GIT_') and k not in ('TMUX', 'TMUX_PANE')}
        self.env.update(HOME=str(self.home), SHELL='/bin/sh', PATH='/usr/local/bin:/usr/bin:/bin',
                        XDG_CONFIG_HOME=str(self.home / '.config'), XDG_DATA_HOME=str(self.home / '.local/share'),
                        XDG_STATE_HOME=str(self.root / 'xdg'), ATM_STATE_DIR=str(self.root / 'state'),
                        ATM_SOCKET=str(self.socket), ATM_TMUX_SOCKET=LABEL, TMUX_TMPDIR=str(self.root / 'tmux'),
                        TMPDIR=str(self.root), PYTHONDONTWRITEBYTECODE='1', LANG='C.UTF-8', TERM='xterm-256color')
        # The stand-in pi that `agent up` starts in this test's daemon (never a model CLI).
        (self.root / 'stand-in').mkdir()
        (self.root / 'stand-in/pi').symlink_to(atm_rollout.STAND_IN)
        self.env['ATM_SPAWN_BIN'] = str(self.root / 'stand-in/pi')
        self.assertNotEqual(self.socket, Path('/tmp/atm.sock'))
        self.assertTrue(self.bin.is_relative_to(self.root))
        self.install(self.bin, 'old')
        self.install(self.candidate / 'bin', 'new')
        self.repo = self.root / 'repo'
        self.before, self.after = self.commits()
        self.manifest()
        self.server = None
        self.addCleanup(self.stop_everything)

    # ---- pairs ---------------------------------------------------------

    def install(self, folder, label):
        for name in NAMES:
            (folder / name).unlink(missing_ok=True)
            source = Path(self.cache.name).resolve() / label / name
            try:
                os.link(source, folder / name)
            except OSError:
                shutil.copy2(source, folder / name)

    def wrap(self, modes, installed=False):
        """The candidate's atm (or the installed one) becomes a wrapper around the real binary."""
        label, wrapper = ('old', self.bin / 'atm') if installed else ('new', self.candidate / 'bin/atm')
        path = self.root / f'modes-{label}.json'
        path.write_text(json.dumps(modes))
        wrapper.unlink()
        wrapper.write_text(WRAPPER.replace('MODES', repr(str(path)))
                           .replace('REAL', repr(str(Path(self.cache.name).resolve() / label / 'atm'))))
        wrapper.chmod(0o755)
        self.manifest()
        return path

    def manifest(self):
        hashes = {name: hashlib.sha256((self.candidate / 'bin' / name).read_bytes()).hexdigest() for name in NAMES}
        (self.candidate / 'install.json').write_text(json.dumps(
            dict(id=self.after + '-prebuilt', sha=self.after, profile='prebuilt', hashes=hashes)))

    def pair(self, folder=None):
        folder = folder or self.bin
        return {name: (hashlib.sha256((folder / name).read_bytes()).hexdigest(),
                       stat.S_IMODE((folder / name).stat().st_mode)) for name in NAMES}

    # ---- git, for the report -------------------------------------------

    def git(self, *args):
        result = self.command(['git', '-C', self.repo, '-c', 'user.name=Rollout fixture',
                               '-c', 'user.email=rollout@example.invalid', *args])
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout.strip()

    def commits(self):
        self.repo.mkdir()
        self.git('--git-dir', self.repo / '.git', '--work-tree', self.repo, 'init', '-q')
        self.assertEqual(Path(self.git('rev-parse', '--absolute-git-dir')), self.repo / '.git')
        shas = []
        for text in ('one', 'two'):
            (self.repo / 'scripts').mkdir(exist_ok=True)
            (self.repo / 'scripts/tool').write_text(text)
            self.git('add', '-A')
            self.git('commit', '-qm', 'test: fixture revision')
            shas.append(self.git('rev-parse', 'HEAD'))
        return shas

    def report_hash(self):
        result = self.command([sys.executable, SCRIPTS / 'atm-protected-files.py', self.before, self.after],
                              cwd=self.repo)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('scripts/tool', result.stdout)
        return hashlib.sha256(result.stdout.encode()).hexdigest()

    # ---- processes -------------------------------------------------------

    def command(self, argv, cwd=None, env=None, timeout=120):
        return subprocess.run(list(map(str, argv)), cwd=cwd, env=env or self.env, stdin=subprocess.DEVNULL,
                              capture_output=True, text=True, timeout=timeout)

    def checked(self, argv, **kwargs):
        result = self.command(argv, **kwargs)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result.stdout.strip()

    def tmux(self, *args):
        return self.checked(['tmux', '-L', LABEL, *args], env=self.pane_env)

    def start_tmux(self):
        # Panes must not carry the daemon's socket in their start environment: atm-promote
        # refuses such a process. They reach the daemon through a symlink to it instead.
        (self.root / 'alias.sock').symlink_to(self.socket)
        self.pane_env = dict(self.env, ATM_SOCKET=str(self.root / 'alias.sock'), ATM_STANDIN_ROOT=str(self.root),
                             ATM_STANDIN_LOG=str(self.root / 'stand-in.jsonl'), ATM_STANDIN_ID_PREFIX='seat')
        self.tmux('-f', '/dev/null', 'new-session', '-d', '-s', 'main', '-x', '200', '-y', '101',
                  '/bin/bash --noprofile --norc')
        self.server = self.tmux('display-message', '-p', '#{socket_path} #{pid}').split()
        self.assertTrue(Path(self.server[0]).is_relative_to(self.root))
        for option, value in (('default-shell', '/bin/bash'), ('status', 'off'),
                              ('default-command', '/bin/bash --noprofile --norc')):
            self.tmux('set-option', '-g', option, value)

    def start_daemon(self):
        self.checked([self.bin / 'atmd', 'start', '-d'])
        wait_for(lambda: atm_rollout.listening(self.socket), 'the test daemon')
        return self.daemon_pid()

    def daemon_pid(self):
        path = self.root / 'state/atmd.pid'
        return int(path.read_text()) if path.exists() else None

    def pane(self):
        pane = self.tmux('new-window', '-d', '-P', '-F', '#{pane_id}', '/bin/bash --noprofile --norc')
        shell = int(self.tmux('display-message', '-p', '-t', pane, '#{pane_pid}'))
        return pane, shell

    def children(self, shell):
        return [int(pid) for pid in Path(f'/proc/{shell}/task/{shell}/children').read_text().split()]

    def sidebar(self, *flags):
        """A real TUI of the installed atm, typed into a shell pane as atm's sidebars are."""
        pane, shell = self.pane()
        self.tmux('send-keys', '-l', '-t', pane, ' '.join([str(self.bin / 'atm'), *flags]))
        self.tmux('send-keys', '-t', pane, 'Enter')
        installed = (self.bin / 'atm').stat().st_ino

        def running():
            return [pid for pid in self.children(shell) if Path(f'/proc/{pid}/exe').stat().st_ino == installed]
        return pane, shell, wait_for(running, 'the sidebar')[0]

    def agent(self, name='seat'):
        """A named agent on a stand-in pi, started by the installed build; returns its list row."""
        roles = self.root / 'project/.atm/roles'
        roles.mkdir(parents=True, exist_ok=True)
        (roles / f'{name}.md').write_text("+++\nharness='pi'\n+++\n")
        placement, _ = self.pane()
        caller = dict(self.env, TMUX=','.join([*self.server, '0']), TMUX_PANE=placement)
        up = json.loads(self.checked([self.bin / 'atm', 'agent', 'up', name, '--project-root',
                                      self.root / 'project', '--format', 'json'], env=caller))
        self.assertEqual(up['state'], 'ready', up)
        return up

    def agents(self):
        return json.loads(self.checked([self.bin / 'atm', 'agent', 'list', '--format', 'json']))

    def stop_everything(self):
        """Only what this test owns, each by an identity read first; the tmux server goes
        even when the daemon's part fails."""
        foreign = None
        try:
            pid = self.daemon_pid()
            if pid and Path(f'/proc/{pid}').exists():
                environ = set(Path(f'/proc/{pid}/environ').read_bytes().split(b'\0'))
                # A sidebar that started a daemon by itself names the socket by its alias.
                mine = {os.fsencode(f'ATM_SOCKET={path}') for path in (self.socket, self.root / 'alias.sock')}
                if mine & environ:
                    self.command([self.bin / 'atmd', 'stop'])
                else:
                    foreign = pid
        finally:
            if self.server:
                now = self.command(['tmux', '-S', self.server[0], 'display-message', '-p', '#{socket_path} #{pid}'],
                                   env=self.pane_env)
                if now.returncode == 0 and now.stdout.split() == self.server:
                    self.command(['tmux', '-S', self.server[0], 'kill-server'], env=self.pane_env)
        self.assertIsNone(foreign, 'the pid file names a process that is not this test\'s daemon')
        self.assertFalse(atm_rollout.listening(self.socket), 'a daemon outlived its test')

    # ---- the rollout -------------------------------------------------------

    def args(self, *extra, report=True, commit=True):
        return [str(self.candidate), '--bin-dir', str(self.bin), '--socket', str(self.socket),
                *(['--installed-commit', self.before if commit is True else commit] if commit else []),
                *(['--report-sha256', self.report_hash()] if report else []), *extra]

    def rollout(self, *args):
        """The command itself, as the owner runs it, from the repository."""
        return self.command([sys.executable, SCRIPTS / 'atm-rollout', *args], cwd=self.repo, timeout=300)

    def in_process(self, *args, smoke=False, patches=()):
        """The same entry point inside this process, so a test can stand between two steps.
        The smoke has its own tests; here it is skipped unless asked for."""
        out, err, here = io.StringIO(), io.StringIO(), os.getcwd()
        os.chdir(self.repo)
        try:
            with contextlib.ExitStack() as stack:
                stack.enter_context(patch.dict(os.environ, self.env, clear=True))
                # tempfile remembers its directory per process; the command is a new process.
                stack.enter_context(patch.object(tempfile, 'tempdir', str(self.root)))
                if not smoke:
                    stack.enter_context(patch.object(atm_rollout.Rollout, 'smoke'))
                for each in patches:
                    stack.enter_context(each)
                stack.enter_context(contextlib.redirect_stdout(out))
                stack.enter_context(contextlib.redirect_stderr(err))
                code = atm_rollout.main(list(map(str, args)))
        finally:
            os.chdir(here)
        return code, out.getvalue(), err.getvalue()

    def before_step(self, step, action, when=lambda *args: True):
        """A patch that runs `action(rollout)` once, just before the rollout's `step`."""
        real, done = getattr(atm_rollout.Rollout, step), []

        def stepped(rollout, *args, **kwargs):
            if not done and when(*args):
                done.append(True)
                action(rollout)
            return real(rollout, *args, **kwargs)
        return patch.object(atm_rollout.Rollout, step, stepped)

    def assert_unchanged(self, pair, daemon):
        self.assertEqual(self.pair(), pair)
        self.assertEqual(self.daemon_pid(), daemon)
        self.assertTrue(Path(f'/proc/{daemon}').exists())

    def assert_running(self, pair):
        """The given pair is installed and its own daemon answers."""
        self.assertEqual(self.pair(), pair)
        self.assertTrue(atm_rollout.listening(self.socket))
        self.assertEqual(Path(f'/proc/{self.daemon_pid()}/exe').stat().st_ino, (self.bin / 'atmd').stat().st_ino)

    def sidebar_of(self, shell):
        installed = (self.bin / 'atm').stat().st_ino
        return [pid for pid in self.children(shell) if Path(f'/proc/{pid}/exe').stat().st_ino == installed]


class HappyPath(Rig):
    def test_one_command_swaps_the_pair_restarts_the_daemon_and_brings_the_sidebar_back(self):
        self.start_tmux()
        old_daemon = self.start_daemon()
        pane, shell, old_sidebar = self.sidebar('--compact')
        sessions = self.tmux('list-sessions', '-F', '#{session_name}')
        new = self.pair(self.candidate / 'bin')
        self.assertNotEqual(self.pair(), new)

        result = self.rollout(*self.args())

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.pair(), new)
        daemon = self.daemon_pid()
        self.assertNotEqual(daemon, old_daemon)
        self.assertFalse(Path(f'/proc/{old_daemon}').exists())
        self.assertEqual(Path(f'/proc/{daemon}/exe').stat().st_ino, (self.bin / 'atmd').stat().st_ino)
        # The sidebar: a new process of the new file, in the same pane, with the same line.
        self.assertFalse(Path(f'/proc/{old_sidebar}').exists())
        (sidebar,) = self.children(shell)
        self.assertEqual(Path(f'/proc/{sidebar}/exe').stat().st_ino, (self.bin / 'atm').stat().st_ino)
        self.assertEqual(Path(f'/proc/{sidebar}/cmdline').read_bytes().split(b'\0')[:-1],
                         [os.fsencode(self.bin / 'atm'), b'--compact'])
        self.assertEqual(self.tmux('list-sessions', '-F', '#{session_name}'), sessions, 'scratch session left')
        self.assertEqual(json.loads((self.bin / '.atm-promotion/rollout.json').read_text()),
                         dict(commit=self.after, previous=self.before))
        for line in ('Smoke passed', 'Checks passed: identity, list, status, agent-list, agents, send',
                     f'Sidebar back in {pane}', 'rollout: DONE'):
            self.assertIn(line, result.stdout)
        self.assertEqual(list(self.root.glob('atm-rollout-smoke-*')), [], 'the smoke left its run')

    def test_what_differs_is_told_by_pane_and_placeholder_rows_are_not_news(self):
        self.start_tmux()
        self.start_daemon()
        seat = self.agent()
        short = [row['id_short'] for row in json.loads(self.checked([self.bin / 'atm', 'list', '--format', 'json']))
                 if row['tmux_pane'] == seat['pane']]
        code, out, err = self.in_process(*self.args())
        self.assertEqual(code, 0, out + err)
        # The idle seat reports on its next event only; until then discovery shows its pane
        # under a placeholder id, which is neither a lost session nor a new one.
        differs = out[out.index('Differs from before:'):out.index('Still yours:')].splitlines()[1:]
        self.assertIn(f"  session {short[0]} on {seat['pane']}: only a placeholder row until its next event", differs)
        self.assertEqual([line for line in differs if 'pending-' in line or line.endswith(': new')], [])
        self.assertEqual([(agent['agent_id'], agent['pane']) for agent in self.agents()],
                         [(seat['agent_id'], seat['pane'])])


class Refusals(Rig):
    """Exit 1, and nothing that was read is changed."""

    def test_preflight_refuses_before_anything_is_read_or_changed(self):
        code, _, err = self.in_process(*self.args())
        self.assertEqual(code, 1, err)
        self.assertIn('refused, nothing changed: no daemon answers', err)
        daemon, old = self.start_daemon(), self.pair()

        def refused(text):
            code, out, err = self.in_process(*self.args())
            self.assertEqual(code, 1, out + err)
            self.assertIn(text, err)
            self.assert_unchanged(old, daemon)

        manifest = json.loads((self.candidate / 'install.json').read_text())
        manifest['hashes']['atmd'] = '0' * 64
        (self.candidate / 'install.json').write_text(json.dumps(manifest))
        refused('candidate binaries do not match install.json: atmd')
        self.install(self.candidate / 'bin', 'old')
        self.manifest()
        refused('candidate pair already installed')
        self.install(self.candidate / 'bin', 'new')
        self.manifest()
        (self.bin / '.atm-stage-left').write_text('x')
        refused('interrupted promotion; run atm-rollback first')
        (self.bin / '.atm-stage-left').unlink()
        # A shell whose environment would start the next daemon on other state or config.
        for key, text in (('ATM_STATE_DIR', 'atmd status names no running daemon'),
                          ('XDG_CONFIG_HOME', 'a restart from here would use other state')):
            with self.subTest(key=key), patch.dict(self.env, {key: str(self.root / 'elsewhere')}):
                refused(text)
        self.assertFalse((self.bin / '.atm-promotion').exists())

    def test_report_needs_the_exact_hash_or_a_yes_typed_on_a_terminal(self):
        daemon, old = self.start_daemon(), self.pair()
        cases = (
            (self.args(report=False) + ['--report-sha256', '0' * 64], 'protected-file report not accepted'),
            (self.args(report=False), 'the report needs your yes'),  # stdin is not a terminal
            (self.args(commit=False), "installed build's commit is unknown"),
            (self.args(commit='no-such-revision'), 'protected-file report failed'),
        )
        for args, text in cases:
            with self.subTest(text=text), patch.object(atm_rollout.sys, 'stdin', io.StringIO('yes\n')):
                code, out, err = self.in_process(*args)
                self.assertEqual(code, 1, out + err)
                self.assertIn(text, err)
                self.assertEqual('M\t"scripts/tool"' in out, 'report' in text and 'failed' not in text)
                self.assert_unchanged(old, daemon)
        # A record of the last rollout names the installed commit; a different claim is refused.
        (self.bin / '.atm-promotion').mkdir(mode=0o700)
        (self.bin / '.atm-promotion/rollout.json').write_text(json.dumps(dict(commit=self.after)))
        code, out, err = self.in_process(*self.args())
        self.assertEqual(code, 1, out + err)
        self.assertIn('--installed-commit disagrees with the recorded ' + self.after, err)
        self.assert_unchanged(old, daemon)

    def on_terminal(self, answer):
        """The command with a terminal as stdin, and `answer` typed on it."""
        master, slave = pty.openpty()
        try:
            process = subprocess.Popen([sys.executable, str(SCRIPTS / 'atm-rollout'), *self.args(report=False)],
                                       cwd=self.repo, env=self.env, stdin=slave, stdout=subprocess.PIPE,
                                       stderr=subprocess.PIPE, text=True)
            os.write(master, answer.encode() + b'\n')
            out, err = process.communicate(timeout=300)
        finally:
            os.close(master)
            os.close(slave)
        return process.returncode, out, err

    def test_a_typed_no_refuses_and_a_typed_yes_rolls_out(self):
        self.start_tmux()
        daemon, old = self.start_daemon(), self.pair()
        code, out, err = self.on_terminal('no')
        self.assertEqual(code, 1, out + err)
        self.assertIn('protected-file report not accepted', err)
        self.assert_unchanged(old, daemon)
        code, out, err = self.on_terminal('yes')
        self.assertEqual(code, 0, out + err)
        self.assert_running(self.pair(self.candidate / 'bin'))
        self.assertNotEqual(self.daemon_pid(), daemon)

    def served(self, state='ready', phase='delivered'):
        """What a wrapped installed atm answers: one agent, one unread message."""
        agent = dict(agent_id='project/seat', state=state, activity='idle', pane='%1', session_id=None,
                     harness='pi', model=None, cost_usd=None, reason=None)
        message = dict(message_id='m7', status='queued', read_at=None, sent_at='2026-11-15T13:09:17Z',
                       content=dict(to='project/seat', body='hello', kind='note', attachments=[], task=None),
                       delivery=dict(phase=phase, reason=None, attempt=1, pane='%1', session_id=None,
                                     confirmation=None, retry_at=None, blocked_by=None))
        return {'agent list': [agent],
                'inbox --for project/seat': dict(messages=[message], unread_count=1, next_after=None)}

    def test_work_in_flight_refuses_and_wait_waits_for_the_quiet_moment(self):
        modes = self.wrap(self.served(), installed=True)
        self.start_tmux()
        daemon, old = self.start_daemon(), self.pair()
        for served, text in ((self.served(state='starting'), 'agent project/seat is starting'),
                             (self.served(state='clearing'), 'agent project/seat is clearing'),
                             (self.served(phase='delivering'), 'message m7 to project/seat is being delivered')):
            with self.subTest(text=text):
                modes.write_text(json.dumps(served))
                code, out, err = self.in_process(*self.args())
                self.assertEqual(code, 1, out + err)
                self.assertIn('refused, nothing changed: ' + text, err)
                self.assert_unchanged(old, daemon)
        # Queued, waiting and blocked messages survive a restart as they are: no refusal.
        modes.write_text(json.dumps(self.served(phase='blocked')))
        code, out, err = self.in_process(*self.args(report=False), '--dry-run')
        self.assertEqual(code, 0, out + err)
        # --wait looks again instead of refusing; the work ends while it waits.
        modes.write_text(json.dumps(self.served(state='clearing')))
        real = atm_rollout.in_flight

        def look(snapshot):
            busy = real(snapshot)
            modes.write_text(json.dumps(self.served()))  # over by the next look
            return busy
        patches = [patch.object(atm_rollout, 'in_flight', look), patch.object(atm_rollout, 'WAIT_POLL', 0.01)]
        code, out, err = self.in_process(*self.args(report=False), '--wait', '--dry-run', patches=patches)
        self.assertEqual(code, 0, out + err)
        self.assertIn('Waiting for a quiet moment: agent project/seat is clearing', out)
        self.assertIn('Agent project/seat: ready, pane %1', out)
        self.assert_unchanged(old, daemon)

    def test_only_sidebars_are_quit_for_the_owner(self):
        for argv, sidebar in ((['atm'], True), (['atm', '--compact', '--tmux-session', 'main'], True),
                              (['/bin/atm', '-p', '--tmux-session=main', '--pick'], True),
                              (['atm', 'agent', 'up', 'seat'], False), (['atm', '--compact', 'list'], False),
                              (['atm', '--tmux-session'], False)):
            self.assertEqual(atm_rollout.is_sidebar(argv), sidebar, argv)
        self.start_tmux()
        daemon, old = self.start_daemon(), self.pair()
        # A command of the installed atm that is still running: not the rollout's to end.
        # (This process stands in for it: a real atm command that stays is an agent start.)
        real, me = atm_rollout.Rollout.processes, atm_rollout.identity(os.getpid())
        command = dict(pid=me['pid'], start=me['start'], name='atm', parent=1, current=True, server=None,
                       pane=None, env={}, argv=[str(self.bin / 'atm'), 'agent', 'up', 'seat'])
        busy = [patch.object(atm_rollout.Rollout, 'processes', lambda rollout: real(rollout) + [command]),
                patch.object(atm_rollout, 'COMMAND_WAIT', 0.2)]
        code, out, err = self.in_process(*self.args(), patches=busy)
        self.assertEqual(code, 1, out + err)
        self.assertIn(f'pid {me["pid"]} runs "{self.bin}/atm agent up seat", not a sidebar', err)
        self.assert_unchanged(old, daemon)
        # A loop's short call that has ended by the second look is no reason to refuse.
        command.update(pid=2 ** 22 + 1, start='0')
        code, out, err = self.in_process(*self.args(report=False), '--dry-run', patches=busy)
        self.assertEqual(code, 0, out + err)
        # A TUI that is its pane's own command would take the pane with it.
        pane = self.tmux('new-window', '-d', '-P', '-F', '#{pane_id}', f'{self.bin}/atm --compact')
        wait_for(lambda: self.tmux('display-message', '-p', '-t', pane, '#{pane_current_command}') == 'atm',
                 'the pane-command TUI')
        code, out, err = self.in_process(*self.args())
        self.assertEqual(code, 1, out + err)
        self.assertIn(f'was not started from the shell of pane {pane} and would not come back', err)
        self.assert_unchanged(old, daemon)
        self.assertEqual(self.tmux('display-message', '-p', '-t', pane, '#{pane_current_command}'), 'atm')

    def test_dry_run_reads_and_prints_and_changes_nothing(self):
        self.start_tmux()
        daemon, old = self.start_daemon(), self.pair()
        pane, shell, sidebar = self.sidebar('--compact')
        sessions = self.tmux('list-sessions', '-F', '#{session_name}')
        code, out, err = self.in_process(*self.args(report=False), '--dry-run', smoke=True)
        self.assertEqual(code, 0, out + err)
        self.assert_unchanged(old, daemon)
        self.assertEqual(self.sidebar_of(shell), [sidebar])
        self.assertEqual(self.tmux('list-sessions', '-F', '#{session_name}'), sessions)
        self.assertFalse((self.bin / '.atm-promotion').exists())
        self.assertEqual(list(self.root.glob('atm-rollout-smoke-*')), [])
        for line in ('Report sha256: ' + self.report_hash(), f'Sidebar pid {sidebar} in {pane}',
                     'Dry run: nothing was changed', 'agent up, msg send, agent clear, agent down --kill',
                     f'take {self.socket}.lock, then: {self.bin}/atmd stop',
                     f'scripts/atm-promote --from-bin {self.candidate}/bin --bin-dir {self.bin} '
                     f'--socket {self.socket}', f'{self.bin}/atmd start -d', 'scripts/atm-rollback'):
            self.assertIn(line, out)

    def test_a_sidebar_that_does_not_quit_refuses_and_the_others_come_back(self):
        self.start_tmux()
        daemon, old = self.start_daemon(), self.pair()
        first, second = self.sidebar('--compact'), self.sidebar()
        shell_of = {first[2]: first[1], second[2]: second[1]}  # sidebar pid to its pane's shell
        real, calls = atm_rollout.select.select, []

        def second_never_exits(*args):
            calls.append(args)
            return real(*args) if len(calls) == 1 else ([], [], [])
        stuck = patch.object(atm_rollout.select, 'select', second_never_exits)
        code, out, err = self.in_process(*self.args(), patches=[stuck])
        self.assertEqual(code, 1, out + err)
        self.assert_unchanged(old, daemon)
        # The one that had quit is typed back into its pane: a new process there.
        stuck = int(re.search(r'refused, nothing changed: sidebar pid (\d+) in %\d+ did not quit', err).group(1))
        (quit_first,) = shell_of.keys() - {stuck}
        self.assertIn('Sidebar back in ', out)
        (back,) = self.sidebar_of(shell_of[quit_first])
        self.assertNotEqual(back, quit_first)


class Gap(Rig):
    """Between the daemon's stop and the swap, something calls atm."""

    def test_a_caller_in_the_gap_cannot_bring_the_old_daemon_back(self):
        self.start_tmux()
        self.start_daemon()
        seen = []

        def caller(rollout):
            listed = self.command([self.bin / 'atm', 'list'])  # autostarts a daemon, normally
            seen.append((listed.returncode, listed.stderr, atm_rollout.listening(self.socket), self.daemon_pid()))
        gap = self.before_step('tool', caller)
        code, out, err = self.in_process(*self.args(), patches=[gap])
        self.assertEqual(len(seen), 1)
        returncode, stderr, listening, daemon = seen[0]
        self.assertNotEqual(returncode, 0)
        self.assertIn('Failed to start daemon', stderr)
        self.assertFalse(listening, 'a daemon started in the gap')
        self.assertIsNone(daemon)
        self.assertEqual(code, 0, out + err)
        self.assert_running(self.pair(self.candidate / 'bin'))

    def test_a_daemon_that_won_the_lock_is_left_running_and_nothing_is_swapped(self):
        self.start_tmux()
        old = self.pair()
        self.start_daemon()
        _, shell, sidebar = self.sidebar('--compact')
        back, real_flock, real_call = threading.Event(), atm_rollout.fcntl.flock, atm_rollout.Rollout.call

        def late_flock(*args):  # the rollout reaches for the lock only after the daemon is back
            self.assertTrue(back.wait(60))
            return real_flock(*args)

        def call(rollout, argv, *args, **kwargs):
            result = real_call(rollout, argv, *args, **kwargs)
            if [str(arg) for arg in argv] == [str(self.bin / 'atmd'), 'stop']:
                self.start_daemon()
                back.set()
            return result
        patches = [patch.object(atm_rollout.fcntl, 'flock', late_flock),
                   patch.object(atm_rollout.Rollout, 'call', call), patch.object(atm_rollout, 'LOCK_WAIT', 1)]
        code, out, err = self.in_process(*self.args(), patches=patches)
        self.assertEqual(code, 1, out + err)
        self.assertIn('refused, nothing changed: another daemon took the socket', err)
        self.assert_running(old)
        self.assertNotEqual(self.sidebar_of(shell), [sidebar])
        self.assertEqual(len(self.sidebar_of(shell)), 1)
        self.assertFalse((self.bin / '.atm-promotion/previous.json').exists())

    def test_a_swap_that_atm_promote_refuses_restores_the_daemon_and_the_sidebar(self):
        self.start_tmux()
        old = self.pair()
        daemon = self.start_daemon()
        _, shell, sidebar = self.sidebar('--compact')
        holders = []

        def holder(rollout):  # a process that names the socket: atm-promote refuses it
            process = subprocess.Popen(['/bin/cat'], env=self.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE)
            process.stdin.write(b'ready\n')
            process.stdin.flush()
            self.assertEqual(process.stdout.readline(), b'ready\n')
            holders.append(process)
        code, out, err = self.in_process(*self.args(), patches=[self.before_step('tool', holder)])
        for process in holders:
            process.stdin.close()
            process.stdout.close()
            self.assertEqual(process.wait(timeout=30), 0)
        self.assertEqual(code, 1, out + err)
        self.assertIn('refused, nothing changed: the swap failed and the previous pair is in place: '
                      f'promotion: running process uses socket: pid {holders[0].pid}', err)
        self.assert_running(old)
        self.assertNotEqual(self.daemon_pid(), daemon)
        self.assertEqual(len(self.sidebar_of(shell)), 1)
        self.assertNotEqual(self.sidebar_of(shell), [sidebar])


class FailedChecks(Rig):
    """Exit 3: the check is named, the previous pair is back and its daemon answers."""

    def rolled_back(self, name, old, patches=()):
        code, out, err = self.in_process(*self.args(), patches=patches)
        self.assertEqual(code, 3, out + err)
        self.assertIn(f'rollout: check {name} failed', out)
        self.assertRegex(out, f'rollout: ROLLED BACK: check {name} failed: .*; the previous build is running '
                         'and passed the checks')
        self.assert_running(old)
        return out

    def test_each_failing_check_rolls_back_to_the_previous_pair(self):
        self.start_tmux()
        old = self.pair()
        self.start_daemon()
        _, shell, _ = self.sidebar('--compact')
        modes = self.wrap({})
        quick = [patch.object(atm_rollout, 'SEND_WAIT', 1)]
        for name, served, patches in (
                ('list', {'list --format json': 1}, ()), ('status', {'status': 1}, ()),
                ('agent-list', {'agent list': 1}, ()), ('send', {'send': 1}, ()),
                # A send that reports success and types nothing.
                ('send', {'send': 'Sent to %0 by tmux paste'}, quick)):
            with self.subTest(check=name, served=served):
                modes.write_text(json.dumps(served))
                before = self.sidebar_of(shell)
                out = self.rolled_back(name, old, patches)
                self.assertIn('Restored previous pair', out)
                self.assertEqual(len(self.sidebar_of(shell)), 1)
                self.assertNotEqual(self.sidebar_of(shell), before)
                self.assertEqual(self.tmux('list-sessions', '-F', '#{session_name}'), 'main')
                shutil.rmtree(self.bin / '.atm-promotion')  # the kept pairs of this round

    def test_an_agent_that_is_missing_or_on_another_pane_rolls_back(self):
        self.start_tmux()
        old = self.pair()
        self.start_daemon()
        seat = self.agent()
        row = self.agents()[0]
        modes = self.wrap({})
        for served, text in (([], 'project/seat was on pane %s and is now missing' % seat['pane']),
                             ([dict(row, pane='%99')], 'project/seat was on pane %s and is now on %%99' % seat['pane'])):
            with self.subTest(text=text):
                modes.write_text(json.dumps({'agent list --format json': served}))
                self.assertIn(text, self.rolled_back('agents', old))
        self.assertEqual([(agent['agent_id'], agent['pane']) for agent in self.agents()],
                         [('project/seat', seat['pane'])])

    def test_a_daemon_that_is_not_the_installed_file_fails_identity(self):
        self.start_tmux()
        old = self.pair()
        self.start_daemon()
        (self.root / 'other').mkdir()
        os.link(Path(self.cache.name).resolve() / 'new/atmd', self.root / 'other/atmd')

        def foreign(rollout):  # some other atmd answers on the socket after the swap
            self.checked([self.root / 'other/atmd', 'start', '-d'])
            wait_for(lambda: atm_rollout.listening(self.socket), 'the foreign daemon')
        started = self.before_step('start_daemon', foreign)
        out = self.rolled_back('identity', old, [started])
        self.assertIn('does not run the installed atmd', out)


class NewerState(Rig):
    """Plan risk 7: the rollback lands on a state directory the newer build has written to."""

    def events(self):
        return b''.join(path.read_bytes() for path in sorted((self.root / 'state/log/events').glob('*.jsonl')))

    def newer_build_writes(self, *events):
        """Between the new daemon's start and its checks: events appended in the log's own
        line format, as a newer daemon would have written them, then the daemon again."""
        def write(rollout):
            self.checked([self.bin / 'atmd', 'stop'])
            segment = sorted((self.root / 'state/log/events').glob('*.jsonl'))[-1]
            lines = segment.read_text().splitlines()
            seq = json.loads(lines[-1])['seq'] + 1 if lines else int(segment.stem)
            with segment.open('a') as log:
                for kind, body in events:
                    log.write(json.dumps(dict(seq=seq, ts=time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime()),
                                              kind=kind, body=body), separators=(',', ':')) + '\n')
                    seq += 1
            self.written = self.events()
            self.checked([self.bin / 'atmd', 'start', '-d'])
            wait_for(lambda: atm_rollout.listening(self.socket), 'the newer daemon again')
        return self.before_step('checks', write)

    def rollout_onto(self, *events):
        self.start_tmux()
        self.old = self.pair()
        self.start_daemon()
        self.seat = self.agent()
        self.wrap({'status': 1})  # the newer build fails a check after it has written
        return self.in_process(*self.args(), patches=[self.newer_build_writes(*events)])

    def test_kinds_the_previous_build_does_not_know_are_skipped_and_its_agents_are_there(self):
        code, out, err = self.rollout_onto(('future.kind', {'anything': [1, 2]}),
                                           ('task.future', {'task_id': 't9', 'owner': 'project/seat'}))
        self.assertEqual(code, 3, out + err)
        self.assertIn('rollout: ROLLED BACK: check status failed', out)
        self.assert_running(self.old)
        self.assertEqual([(agent['agent_id'], agent['pane']) for agent in self.agents()],
                         [('project/seat', self.seat['pane'])])
        self.assertTrue(self.events().startswith(self.written), 'the rollback rewrote the log')
        self.assertIn(b'"kind":"future.kind"', self.events())

    def test_a_log_the_previous_build_cannot_replay_is_reported_in_words(self):
        # A known event about an agent that only an unknown kind created.
        code, out, err = self.rollout_onto(('agent.hired', {'agent_id': 'project/ghost'}),
                                           ('agent.pane_created', {'agent_id': 'project/ghost', 'pane': '%77'}))
        self.assertEqual(code, 4, out + err)
        self.assertIn('rollout: NEEDS YOU: check status failed', err)
        self.assertIn('the previous build runs but cannot read the organisation log the newer build wrote; '
                      'named agents and messages are unavailable until the newer build is back; '
                      'the state directory was not touched', err)
        self.assertIn('the previous pair is installed; a daemon answers', err)
        self.assert_running(self.old)
        self.assertEqual(self.command([self.bin / 'atm', 'list']).returncode, 0)
        self.assertNotEqual(self.command([self.bin / 'atm', 'agent', 'list']).returncode, 0)
        self.assertTrue(self.events().startswith(self.written), 'the rollback rewrote the log')


class Smoke(Rig):
    def test_a_candidate_that_cannot_start_an_agent_is_refused_before_anything_stops(self):
        self.start_tmux()
        daemon, old = self.start_daemon(), self.pair()
        _, shell, sidebar = self.sidebar('--compact')
        self.wrap({'agent up': 1})
        code, out, err = self.in_process(*self.args(), smoke=True)
        self.assertEqual(code, 1, out + err)
        self.assertIn('refused, nothing changed: smoke failed at up: atm agent up smoke', err)
        self.assert_unchanged(old, daemon)
        self.assertEqual(self.sidebar_of(shell), [sidebar])
        # The private run is kept for reading, with nothing of it still running.
        (kept,) = self.root.glob('atm-rollout-smoke-*')
        self.assertIn(f'run kept in {kept}', err)
        self.assertFalse(atm_rollout.listening(kept / 'atm.sock'))
        (server,) = (kept / 'tmux').glob('tmux-*/smoke')
        self.assertNotEqual(self.command(['tmux', '-S', server, 'list-sessions']).returncode, 0)

    def test_real_runs_the_same_smoke_with_the_harness_found_on_path(self):
        # The stand-in under the harness's name: no model CLI, and no spawn override either.
        self.start_tmux()
        (self.root / 'harness').mkdir()
        (self.root / 'harness/pi').symlink_to(atm_rollout.STAND_IN)
        del self.env['ATM_SPAWN_BIN']
        self.env.update(PATH=f"{self.root / 'harness'}:{self.env['PATH']}", ATM_STANDIN_ROOT=str(self.root),
                        ATM_STANDIN_LOG=str(self.root / 'real.jsonl'), ATM_STANDIN_ID_PREFIX='real')
        self.start_daemon()
        code, out, err = self.in_process(*self.args(), '--real', 'pi', smoke=True)
        self.assertEqual(code, 0, out + err)
        self.assertIn('Smoke (pi) on a private socket', out)
        self.assertIn('Smoke passed: up, message, clear, down.', out)
        self.assertIn('"kind": "identity"', (self.root / 'real.jsonl').read_text().replace('":"', '": "'))


class AfterTheSwap(Rig):
    def test_a_status_line_that_setup_replaced_is_reported_with_the_command_that_restores_it(self):
        settings = self.home / '.claude/settings.json'
        settings.parent.mkdir()
        mine = dict(type='command', command=str(self.home / 'bin/my-statusline'), padding=0)
        settings.write_text(json.dumps(dict(statusLine=mine, keep='this')))
        self.start_tmux()
        self.start_daemon()
        code, out, err = self.in_process(*self.args())
        self.assertEqual(code, 0, out + err)
        now = json.loads(settings.read_text())['statusLine']
        self.assertNotEqual(now, mine)
        backup = self.home / '.config/atm/claude-statusline-backup.json'
        self.assertTrue(backup.is_file())
        for line in (f"atm setup CHANGED Claude's statusLine in {settings}:", f"  was: {mine['command']}",
                     f"  now: {now['command']}", f'  setup keeps the one it replaced in {backup}'):
            self.assertIn(line, out)
        (restore,) = [line.split(': ', 1)[1] for line in out.splitlines() if line.startswith('  to put yours back: ')]
        self.checked(['/bin/sh', '-c', restore])
        self.assertEqual(json.loads(settings.read_text()), dict(json.loads(settings.read_text()), statusLine=mine))
        self.assertEqual(json.loads(settings.read_text())['keep'], 'this')
        self.assertEqual(json.loads(settings.read_text())['statusLine'], mine)

    def test_rollback_is_the_same_command_the_other_way(self):
        self.start_tmux()
        old, new = self.pair(), self.pair(self.candidate / 'bin')
        self.start_daemon()
        _, shell, _ = self.sidebar('--compact')
        target = ['--bin-dir', self.bin, '--socket', self.socket]
        code, out, err = self.in_process('--rollback', *target)
        self.assertEqual(code, 1, out + err)  # nothing was ever promoted here
        self.assert_running(old)
        code, out, err = self.in_process(*self.args())
        self.assertEqual(code, 0, out + err)
        self.assertIn(f'To go back: scripts/atm-rollout --rollback --bin-dir {self.bin} --socket {self.socket}', out)
        self.assert_running(new)
        daemon, sidebar = self.daemon_pid(), self.sidebar_of(shell)
        code, out, err = self.in_process('--rollback', *target)
        self.assertEqual(code, 0, out + err)
        self.assertIn('rollout: DONE: the previous build is running and passed the checks.', out)
        self.assert_running(old)
        self.assertNotEqual(self.daemon_pid(), daemon)
        self.assertEqual(len(self.sidebar_of(shell)), 1)
        self.assertNotEqual(self.sidebar_of(shell), sidebar)
        self.assertEqual(json.loads((self.bin / '.atm-promotion/rollout.json').read_text()),
                         dict(commit=self.before, previous=None))
        code, out, err = self.in_process('--rollback', *target)
        self.assertEqual(code, 1, out + err)
        self.assertIn('refused, nothing changed: the previous pair is already installed', err)
        self.assert_running(old)

    def test_a_previous_build_without_named_agents_is_rolled_back_to_and_says_so(self):
        # As 0.3.0: its atm has no `agent` and no `inbox` command at all.
        unknown = lambda word: [2, f"error: unrecognized subcommand '{word}'"]  # noqa: E731
        self.wrap({'agent': unknown('agent'), 'inbox': unknown('inbox')}, installed=True)
        self.start_tmux()
        old, new = self.pair(), self.pair(self.candidate / 'bin')
        self.start_daemon()
        code, out, err = self.in_process(*self.args())
        self.assertEqual(code, 0, out + err)
        self.assertIn('The installed build has no named agents.', out)
        self.assertIn('Checks passed: identity, list, status, agent-list, agents, send.', out)
        self.assert_running(new)
        self.agent()  # which the newer build then has
        code, out, err = self.in_process('--rollback', '--bin-dir', self.bin, '--socket', self.socket)
        self.assertEqual(code, 0, out + err)
        self.assertIn('This build has no named agents; the 1 of before are in the state directory, '
                      'untouched, and not visible to it.', out)
        self.assertIn('Checks passed: identity, list, status, send.', out)
        self.assert_running(old)


if __name__ == '__main__':
    unittest.main()
