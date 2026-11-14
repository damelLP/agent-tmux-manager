#!/usr/bin/env python3
"""Real-process contract tests. A failed assertion still stops owned resources."""
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import unittest
import sys

sys.dont_write_bytecode = True

SCRIPT = Path(__file__).with_name('atm-candidate')


class CandidateKit(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='kit-')
        self.root = Path(self.temp.name)
        self.env = dict(os.environ)
        for key in ('TMUX', 'TMUX_PANE'):
            self.env.pop(key, None)
        for key, sub in [('ROOT', 'kit'), ('STATE_ROOT', 'state'),
                         ('BUILD_ROOT', 'build'), ('RUNTIME_ROOT', 'run')]:
            self.env['ATM_CANDIDATE_' + key] = str(self.root / sub)
        # No user configuration/plugins in repeatable tests; production keeps HOME.
        (self.root / 'home').mkdir()
        self.env.update(HOME=str(self.root / 'home'), SHELL='/bin/sh',
                        ATM_SOCKET=str(self.root / 'decoy.sock'),
                        ATM_STATE_DIR=str(self.root / 'decoy-state'),
                        ATM_TMUX_SOCKET='decoy', TMUX_TMPDIR=str(self.root / 'decoy-tmux'),
                        PATH='/usr/local/bin:/usr/bin:/bin')
        self.bin = self.root / 'prebuilt'
        self.bin.mkdir()
        for name in ('atm', 'atmd'):
            (self.bin / name).symlink_to(os.environ['CANDIDATE_TEST_' + name.upper()])

    def tearDown(self):
        if SCRIPT.exists():
            out = self.call('stop', ok=None)
            self.assertEqual(out.returncode, 0, out.stderr)
        self.temp.cleanup()

    def call(self, *args, ok=True, env=None):
        out = subprocess.run(['bash', str(SCRIPT), *map(str, args)],
                             env=env or self.env, capture_output=True, text=True, timeout=45)
        if ok is not None:
            self.assertEqual(out.returncode == 0, ok, out.stdout + out.stderr)
        return out

    def install(self, ref='HEAD'):
        return self.call('install', ref, '--prebuilt', self.bin).stdout.strip()

    def status(self):
        return json.loads(self.call('status').stdout)

    def tm(self, status, *args):
        # tmux takes PATH from a new-window client's environment, not just globals.
        # Match the kit's attach/atm boundary rather than an unsealed external client.
        env = dict(self.env, PATH=status['bin'] + ':' + self.env['PATH'])
        out = subprocess.run(['tmux', '-S', status['tmux_socket'], *args],
                             env=env, capture_output=True, text=True, timeout=10)
        self.assertEqual(out.returncode, 0, out.stderr)
        return out.stdout.strip()

    # Removing install staging/immutability would overwrite the retained pair.
    def test_install_retains_versions_and_refuses_overwrite(self):
        first = self.install()
        self.call('install', 'HEAD', '--prebuilt', self.bin, ok=False)
        second = self.install('HEAD~1')
        self.assertNotEqual(first, second)
        installs = json.loads(self.call('installs').stdout)
        self.assertEqual({x['id'] for x in installs}, {first, second})
        self.call('start', first)
        first_state = Path(self.status()['state'])
        (first_state / 'switch-marker').write_text('first')
        self.call('stop')
        self.call('start', second)
        self.assertNotEqual(Path(self.status()['state']), first_state)
        self.assertFalse((Path(self.status()['state']) / 'switch-marker').exists())
        self.call('stop')
        self.call('start', first)
        self.assertEqual((Path(self.status()['state']) / 'switch-marker').read_text(), 'first')
        self.call('stop')
        self.call('uninstall', first)
        self.assertEqual([x['id'] for x in json.loads(self.call('installs').stdout)], [second])

    # A default fallback, symlink traversal, or permissive root adoption risks live state.
    def test_rejects_unsafe_roots_and_ids_without_mutation(self):
        for root in ('/', '/tmp', str(self.root / 'home'), 'relative'):
            env = dict(self.env, ATM_CANDIDATE_ROOT=root)
            self.call('installs', env=env, ok=False)
        foreign = self.root / 'foreign'
        foreign.mkdir()
        (foreign / 'sentinel').write_text('untouched')
        link = self.root / 'link'
        link.symlink_to(foreign)
        self.call('installs', env=dict(self.env, ATM_CANDIDATE_ROOT=str(link)), ok=False)
        self.call('installs', env=dict(self.env, ATM_CANDIDATE_ROOT=str(foreign)), ok=False)
        self.call('start', '../../outside', ok=False)
        self.assertEqual((foreign / 'sentinel').read_text(), 'untouched')

    # Status must not autostart; wrapper must never expose setup/uninstall mutations.
    def test_read_only_status_and_refused_commands(self):
        self.assertEqual(self.status(), {'running': False})
        self.assertFalse((self.root / 'kit').exists(), 'status must not create kit state')
        self.assertEqual(json.loads(self.call('installs').stdout), [])
        self.assertFalse((self.root / 'kit').exists(), 'installs must be read-only')
        self.call('atm', 'list', ok=False)
        self.call('atm', 'setup', ok=False)
        self.call('atm', 'uninstall', ok=False)
        self.call('attach', env=dict(self.env, TMUX='/foreign,123,0'), ok=False)
        self.assertFalse((self.root / 'kit/active.json').exists())
        self.call('nonsense', ok=False)

    # Wrong endpoint/global env, omitted cleanup, or new state on restart breaks this.
    def test_lifecycle_environment_restart_and_state_retention(self):
        install = self.install()
        inherited = dict(self.env, TMUX='/foreign,123,0', TMUX_PANE='%123',
                         ATM_SOCKET=self.env['ATM_SOCKET'], ATM_TMUX_SOCKET='decoy',
                         ATM_STATE_DIR='/forbidden')
        self.call('start', install, env=inherited)
        status = self.status()
        self.assertTrue(status['running'])
        self.assertTrue(status['socket'].startswith(str(self.root)))
        self.call('start', install, ok=False)
        self.call('uninstall', install, ok=False)
        variables = dict(line.split('=', 1) for line in
                         self.tm(status, 'show-environment', '-g').splitlines() if '=' in line)
        self.assertEqual(variables['ATM_SOCKET'], status['socket'])
        self.assertEqual(variables['ATM_STATE_DIR'], status['state'])
        self.assertEqual(variables['ATM_TMUX_SOCKET'], 'candidate')
        self.assertEqual(variables['PATH'].split(':')[0], status['bin'])
        # Event synchronization: tmux wait-for signals after the child writes.
        output = self.root / 'pane-env.json'
        code = 'import json,os; open(' + repr(str(output)) + ',"w").write(json.dumps(dict(os.environ)))'
        import shlex
        self.tm(status, 'new-window', '-d',
                'python3 -c ' + shlex.quote(code) + '; tmux wait-for -S kit-env')
        self.tm(status, 'wait-for', 'kit-env')
        pane_env = json.loads(output.read_text())
        for key in ('ATM_SOCKET', 'ATM_STATE_DIR', 'ATM_TMUX_SOCKET', 'PATH'):
            self.assertEqual(pane_env[key], variables[key])
        self.assertTrue(pane_env['TMUX'].startswith(status['tmux_socket'] + ','))
        self.call('atm', 'list', '-f', 'json')
        state = Path(status['state'])
        (state / 'sentinel').write_text('keep')
        self.call('restart-daemon')
        after = self.status()
        self.assertNotEqual(after['daemon']['pid'], status['daemon']['pid'])
        self.assertEqual(after['server'], status['server'])
        self.assertEqual(after['state'], status['state'])
        self.assertEqual((state / 'sentinel').read_text(), 'keep')
        self.call('stop')
        self.assertEqual(self.status(), {'running': False})
        self.assertFalse(Path(status['run']).exists())
        self.assertEqual((state / 'sentinel').read_text(), 'keep')
        self.call('stop')
        self.call('start', install)
        self.call('stop')
        self.call('uninstall', install, '--wipe-state')
        self.assertFalse(state.exists())

    # Install-time asset inventory must describe the named ref, not overwrite assets.
    def test_asset_drift_is_reported_without_installing_assets(self):
        first = self.install()
        assets = self.root / 'home/.local/bin'
        assets.mkdir(parents=True)
        hook = assets / 'atm-hook'
        hook.write_text('installed hook sentinel')
        self.call('start', first)
        drift = self.status()['asset_drift']
        self.assertEqual(drift['.local/bin/atm-hook'], 'different')
        self.assertEqual(drift['.local/bin/atm-codex-hook'], 'missing')
        self.assertEqual(hook.read_text(), 'installed hook sentinel')

    # A startup error must not leave either a daemon or a tmux server behind.
    def test_failed_daemon_start_cleans_runtime(self):
        install = self.install()
        binary = self.root / 'kit/installs' / install / 'bin/atmd'
        binary.write_text('#!/bin/sh\nexit 17\n')
        self.call('start', install, ok=False)
        self.assertEqual(self.status(), {'running': False})
        self.assertEqual(list((self.root / 'run').glob('atmc-*')), [])

    # tmux can start a server and still report a config/client error to the caller.
    def test_partial_tmux_start_is_owned_and_cleaned(self):
        install = self.install()
        tools = self.root / 'tools'
        tools.mkdir()
        tmux = tools / 'tmux'
        tmux.write_text('#!/bin/sh\n/usr/bin/tmux "$@"\ncase " $* " in *" new-session "*) exit 19;; esac\n')
        tmux.chmod(0o700)
        env = dict(self.env, PATH=str(tools) + ':' + self.env['PATH'])
        try:
            self.call('start', install, env=env, ok=False)
            self.assertEqual(self.status(), {'running': False})
        finally:
            # Test-only recovery for the expected RED path: verify actual identity
            # against this private socket and unique token before recording it.
            active = self.root / 'kit/active.json'
            if active.exists():
                a = json.loads(active.read_text())
                if a['server'] is None and Path(a['tmux_socket']).exists():
                    pid = int(self.tm(a, 'display-message', '-p', '#{pid}'))
                    fields = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
                    self.assertIn(('ATM_CANDIDATE_RUN=' + a['token']).encode(),
                                  Path(f'/proc/{pid}/environ').read_bytes().split(b'\0'))
                    a['server'] = dict(pid=pid, start=fields[19], exe=str(Path(f'/proc/{pid}/exe').resolve()))
                    active.write_text(json.dumps(a))

    # A failed/interrupted startup may leave only a stale socket, no recorded server.
    def test_unrecorded_exited_server_does_not_block_cleanup(self):
        import select
        self.call('start', self.install())
        status = self.status()
        active = self.root / 'kit/active.json'
        original = active.read_text()
        self.assertEqual(self.tm(status, 'display-message', '-p', '#{socket_path} #{pid}'),
                         status['tmux_socket'] + ' ' + str(status['server']['pid']))
        fd = os.pidfd_open(status['server']['pid'])
        try:
            self.tm(status, 'kill-server')
            self.assertTrue(select.select([fd], [], [], 30)[0])
        finally:
            os.close(fd)
        data = json.loads(original)
        data['server'] = None
        active.write_text(json.dumps(data))
        try:
            self.call('stop')
            self.assertFalse(Path(status['run']).exists())
        finally:
            if active.exists():
                active.write_text(original)

    # Reboot removes the runtime, not persistent manifests: permit recovery only
    # after a token scan proves no candidate process remains.
    def test_missing_runtime_recovers_after_processes_exit(self):
        install = self.install()
        self.call('start', install)
        active = self.root / 'kit/active.json'
        original = active.read_text()
        status = self.status()
        runtime = Path(status['run'])
        saved = runtime.with_name('saved-runtime')
        runtime.rename(saved)
        try:
            for command in ('status', 'stop'):
                self.call(command, ok=False)
            self.assertEqual(active.read_text(), original)
        finally:
            saved.rename(runtime)
        self.call('stop')  # actual exit of all owned processes, then runtime removal
        active.write_text(original)  # persistent manifest surviving a reboot
        try:
            self.assertFalse(self.status()['running'])
            self.assertEqual(active.read_text(), original, 'status must not mutate')
            self.call('stop')
            self.assertFalse(active.exists())
        finally:
            if active.exists():
                active.unlink()  # RED cleanup: all recorded processes already exited
        self.call('start', install)

    # Runtime length rejection must not leave a manifest that can never be stopped.
    def test_long_runtime_path_fails_cleanly(self):
        install = self.install()
        env = dict(self.env, ATM_CANDIDATE_RUNTIME_ROOT=str(self.root / ('x' * 90)))
        self.call('start', install, env=env, ok=False)
        self.assertFalse((self.root / 'kit/active.json').exists())

    # Lifecycle calls serialize; status must not signal or wait indefinitely on a lock.
    def test_lock_refuses_concurrent_operation(self):
        self.install()
        import fcntl
        with (self.root / 'kit/lock').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            self.call('stop', ok=False)

    # Intermediate symlinks must not turn an owned install into foreign binaries.
    def test_rejects_symlinked_bin_directory(self):
        import shutil
        install = self.install()
        binary_dir = self.root / 'kit/installs' / install / 'bin'
        foreign = self.root / 'foreign-bin'
        shutil.copytree(binary_dir, foreign)
        saved = binary_dir.with_name('saved-bin')
        binary_dir.rename(saved)
        binary_dir.symlink_to(foreign)
        try:
            self.call('start', install, ok=False)
        finally:
            binary_dir.unlink()
            saved.rename(binary_dir)

    # Initial flags belong to atm, not the kit's argument parser.
    def test_wrapper_forwards_leading_flags(self):
        self.call('start', self.install())
        self.assertTrue(self.call('atm', '--version').stdout.startswith('atm '))
        self.assertIn('Usage: atm [OPTIONS]', self.call('atm', '--help').stdout)

    # A reused PID is gone; recover the real server through its private socket.
    def test_reused_records_do_not_block_stop_or_signal_decoy(self):
        self.call('start', self.install())
        active = self.root / 'kit/active.json'
        original = active.read_text()
        value = json.loads(original)
        for kind in ('server', 'daemon'):
            value[kind].update(pid=os.getpid(), start='0')
        active.write_text(json.dumps(value))
        try:
            self.assertTrue(self.status()['running'])
            self.call('restart-daemon')
            self.call('stop')
            self.assertFalse(active.exists())
            os.kill(os.getpid(), 0)
        finally:
            if active.exists():
                active.write_text(original)

    # Deleted binaries remain identifiable and included in the token scan.
    def test_stop_finds_deleted_executables(self):
        import shutil
        import atm_candidate
        tools = self.root / 'tools'
        tools.mkdir()
        shutil.copy2('/usr/bin/tmux', tools / 'tmux')
        env = dict(self.env, PATH=str(tools) + ':' + self.env['PATH'])
        self.call('start', self.install(), env=env)
        status = self.status()
        (tools / 'tmux').unlink()
        binary = tools / 'cat'
        shutil.copy2('/bin/cat', binary)
        child = subprocess.Popen([str(binary)], stdin=subprocess.PIPE,
                                 env=dict(self.env, ATM_SOCKET=status['socket'],
                                          TMUX_TMPDIR=str(Path(status['run']) / 't'),
                                          ATM_CANDIDATE_RUN=status['token']))
        try:
            binary.unlink()
            self.assertEqual(os.readlink(f'/proc/{child.pid}/exe'), str(binary) + ' (deleted)')
            self.assertIsNotNone(atm_candidate.identity(child.pid))
            self.call('stop')
            child.wait(timeout=30)
        finally:
            child.stdin.close()
            child.wait(timeout=30)
            # RED cleanup: the old strict exe lookup cannot see this live tmux.
            if (self.root / 'kit/active.json').exists():
                self.assertEqual(self.tm(status, 'display-message', '-p', '#{socket_path} #{pid}'),
                                 status['tmux_socket'] + ' ' + str(status['server']['pid']))
                self.tm(status, 'kill-server')

    # Enumeration followed by PID reuse must not signal the replacement.
    def test_signal_treats_reused_process_as_gone(self):
        import atm_candidate
        kit = object.__new__(atm_candidate.Kit)
        proc = atm_candidate.identity(os.getpid())
        proc['start'] = '0'
        kit.signal_process({}, proc)
        os.kill(os.getpid(), 0)

    # Attach must reach this server without importing the caller's routing.
    def test_attach_on_a_real_terminal(self):
        import pty
        import time
        self.call('start', self.install())
        status = self.status()
        master, slave = pty.openpty()
        child = subprocess.Popen(['bash', str(SCRIPT), 'attach'],
                                 env=dict(self.env, TERM='xterm-256color'),
                                 stdin=slave, stdout=slave, stderr=slave,
                                 start_new_session=True)
        os.close(slave)
        try:
            deadline = time.monotonic() + 30
            while str(child.pid) not in self.tm(status, 'list-clients', '-F', '#{client_pid}').splitlines():
                self.assertIsNone(child.poll(), 'attach exited before becoming a client')
                self.assertLess(time.monotonic(), deadline, 'attach did not reach private server')
                time.sleep(.05)  # condition poll only
            self.call('stop')
            child.wait(timeout=30)
        finally:
            self.call('stop')
            child.wait(timeout=30)
            os.close(master)

    # Deterministically exit a real process between identity and environment reads.
    def test_stop_accepts_exit_during_ownership_check(self):
        import atm_candidate
        self.call('start', self.install())
        status = self.status()
        env = dict(self.env, ATM_SOCKET=status['socket'],
                   TMUX_TMPDIR=str(Path(status['run']) / 't'),
                   ATM_CANDIDATE_RUN=status['token'])
        child = subprocess.Popen(['/bin/cat'], env=env, stdin=subprocess.PIPE,
                                 stdout=subprocess.DEVNULL)
        kit = object.__new__(atm_candidate.Kit)
        original = kit.owns
        def exit_at_environment_read(active, pid):
            # A gate on actual process exit, not a fake ownership answer or delay.
            if child.poll() is None:
                child.stdin.close()
                child.wait(timeout=30)
            return original(active, pid)
        kit.owns = exit_at_environment_read
        try:
            kit.signal_process(status, atm_candidate.identity(child.pid))
        finally:
            if child.poll() is None:
                child.stdin.close()
                child.wait(timeout=30)

    # An atm-autostarted sibling is not the recorded initial foreground daemon.
    def test_stop_finds_autostarted_daemon(self):
        self.call('start', self.install())
        status = self.status()
        pid = status['daemon']['pid']
        self.assertEqual(self.tm(status, 'display-message', '-p', '#{socket_path} #{pid}'),
                         status['tmux_socket'] + ' ' + str(status['server']['pid']))
        fd = os.pidfd_open(pid)
        try:
            import select
            self.assertIn(('ATM_SOCKET=' + status['socket']).encode(),
                          Path(f'/proc/{pid}/environ').read_bytes().split(b'\0'))
            self.assertEqual(str(Path(f'/proc/{pid}/exe').resolve()), status['bin'] + '/atmd')
            signal.pidfd_send_signal(fd, signal.SIGTERM)
            self.assertTrue(select.select([fd], [], [], 30)[0])
        finally:
            os.close(fd)
        self.call('atm', 'list', '-f', 'json')
        new_pid = int((Path(status['state']) / 'atmd.pid').read_text())
        self.assertNotEqual(new_pid, pid)
        self.call('stop')
        self.assertFalse(Path(status['socket']).exists())
        try:
            comm = Path(f'/proc/{new_pid}/stat').read_text().rsplit(')', 1)[1].split()[0]
            self.assertEqual(comm, 'Z')  # exited orphan may await the host reaper
        except FileNotFoundError:
            pass


if __name__ == '__main__':
    unittest.main()
