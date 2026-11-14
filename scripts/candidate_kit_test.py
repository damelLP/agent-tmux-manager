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

from atm_candidate import git_env

SCRIPT = Path(__file__).with_name('atm-candidate')


class CandidateKit(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='kit-')
        self.root = Path(self.temp.name)
        self.script = SCRIPT
        self.env = dict(os.environ)
        for key in ('TMUX', 'TMUX_PANE'):
            self.env.pop(key, None)
        for key, sub in [('ROOT', 'kit'), ('STATE_ROOT', 'state'),
                         ('BUILD_ROOT', 'build'), ('RUNTIME_ROOT', 'run')]:
            self.env['ATM_CANDIDATE_' + key] = str(self.root / sub)
        # No user configuration/plugins in repeatable tests; production keeps HOME.
        (self.root / 'home').mkdir()
        self.env.update(HOME=str(self.root / 'home'), SHELL='/bin/sh',
                        XDG_CONFIG_HOME=str(self.root / 'home/.config'),
                        XDG_DATA_HOME=str(self.root / 'home/.local/share'),
                        ATM_SOCKET=str(self.root / 'decoy.sock'),
                        ATM_STATE_DIR=str(self.root / 'decoy-state'),
                        ATM_TMUX_SOCKET='decoy', TMUX_TMPDIR=str(self.root / 'decoy-tmux'),
                        PATH='/usr/local/bin:/usr/bin:/bin')
        self.decoy = socket.socket(socket.AF_UNIX)
        self.decoy.bind(self.env['ATM_SOCKET'])
        self.decoy.listen()
        self.decoy.setblocking(False)
        self.addCleanup(self.decoy.close)
        self.bin = self.root / 'prebuilt'
        self.bin.mkdir()
        for name in ('atm', 'atmd'):
            (self.bin / name).symlink_to(os.environ['CANDIDATE_TEST_' + name.upper()])

    def tearDown(self):
        if self.script.exists():
            out = self.call('stop', ok=None)
            self.assertEqual(out.returncode, 0, out.stderr)
        try:
            connection, _ = self.decoy.accept()
        except BlockingIOError:
            pass
        else:
            connection.close()
            self.fail('candidate connected to inherited decoy socket')
        self.temp.cleanup()

    def call(self, *args, ok=True, env=None):
        out = subprocess.run(['bash', str(self.script), *map(str, args)],
                             env=env or self.env, capture_output=True, text=True, timeout=45)
        if ok is not None:
            self.assertEqual(out.returncode == 0, ok, out.stdout + out.stderr)
        return out

    def git(self, repo, *args):
        return subprocess.check_output(
            ['git', '-C', str(repo), '-c', 'user.name=Candidate kit fixture',
             '-c', 'user.email=candidate-kit@example.invalid', *args],
            env=git_env(self.env), text=True, timeout=30).strip()

    def init_repo(self, repo):
        repo.mkdir()
        # Bootstrap is explicitly pinned; rev-parse needs an initialized repo.
        self.git(repo, '--git-dir', str(repo / '.git'), '--work-tree', str(repo), 'init', '-q')
        git_dir = Path(self.git(repo, 'rev-parse', '--absolute-git-dir'))
        self.assertTrue(git_dir.is_relative_to(self.root))
        self.assertEqual(git_dir, repo / '.git')  # before staging or committing any files

    def fixture_repo(self):
        import shutil
        repo = self.root / 'repo'
        self.init_repo(repo)
        source = SCRIPT.parent.parent
        for relative in ('scripts', 'crates/atm/scripts', 'crates/atm/assets/pi-atm',
                         'crates/atm/assets/atm-skills'):
            shutil.copytree(source / relative, repo / relative)
        self.git(repo, 'add', '.')
        self.git(repo, 'commit', '-qm', 'test: first fixture revision')
        first = self.git(repo, 'rev-parse', 'HEAD')
        (repo / 'revision').write_text('second')
        self.git(repo, 'add', '.')
        self.git(repo, 'commit', '-qm', 'test: second fixture revision')
        self.script = repo / 'scripts/atm-candidate'
        return repo, first

    def decoy_repo(self):
        repo = self.root / 'decoy-repo'
        self.init_repo(repo)
        (repo / 'sentinel').write_text('committed decoy')
        self.git(repo, 'add', '.')
        self.git(repo, 'commit', '-qm', 'test: decoy revision')
        (repo / 'sentinel').write_text('staged decoy')
        self.git(repo, 'add', '.')
        return repo

    def repository_snapshot(self, repo):
        git_dir = repo / '.git'
        paths = [git_dir / name for name in ('HEAD', 'index', 'config', 'packed-refs')]
        paths.extend((git_dir / 'refs').rglob('*'))
        return {str(path.relative_to(git_dir)): path.read_bytes() for path in paths if path.is_file()}

    # Strip every Git override, including future ones, without changing routing.
    def test_git_environment_removes_all_git_overrides(self):
        import atm_candidate
        env = dict(ATM_SOCKET='private.sock', PATH='/usr/bin',
                   GIT_DIR='foreign', GIT_WORK_TREE='foreign', GIT_INDEX_FILE='foreign',
                   GIT_COMMON_DIR='foreign', GIT_OBJECT_DIRECTORY='foreign',
                   GIT_ALTERNATE_OBJECT_DIRECTORIES='foreign', GIT_FUTURE_OVERRIDE='foreign')
        self.assertEqual(atm_candidate.git_env(env), {'ATM_SOCKET': 'private.sock', 'PATH': '/usr/bin'})
        self.assertIn('GIT_DIR', env, 'do not mutate the caller environment')

    # Hook Git overrides must not redirect fixture commits/config/index to a decoy.
    def test_fixture_ignores_inherited_git_repository(self):
        from unittest.mock import patch
        decoy = self.decoy_repo()
        before = self.repository_snapshot(decoy)
        original = self.env.copy()
        injected = {'GIT_DIR': str(decoy / '.git'), 'GIT_INDEX_FILE': str(decoy / '.git/index')}
        try:
            self.env.update(injected)
            with patch.dict(os.environ, injected):
                repo, _ = self.fixture_repo()
            self.assertEqual(self.repository_snapshot(decoy), before)
            self.assertTrue((repo / '.git/HEAD').is_file())
            self.assertNotIn(b'[user]', (repo / '.git/config').read_bytes())
        finally:
            self.env = original

    # The kit's ref/assets/worktree operations must ignore hook Git overrides too.
    def test_kit_git_ignores_inherited_repository(self):
        decoy = self.decoy_repo()
        before = self.repository_snapshot(decoy)
        repo, _ = self.fixture_repo()
        env = dict(self.env, GIT_DIR=str(decoy / '.git'), GIT_WORK_TREE=str(decoy),
                   GIT_INDEX_FILE=str(decoy / '.git/index'), GIT_COMMON_DIR=str(decoy / '.git'),
                   GIT_OBJECT_DIRECTORY=str(decoy / '.git/objects'),
                   GIT_ALTERNATE_OBJECT_DIRECTORIES=str(decoy / '.git/objects'))
        out = self.call('install', 'HEAD', '--prebuilt', self.bin, env=env)
        self.assertTrue((repo / '.git/HEAD').is_file())
        self.assertEqual(self.repository_snapshot(decoy), before)
        tools_env = self.build_fault_tools('remove-error')
        tools_env.update({k: v for k, v in env.items() if k.startswith('GIT_')})
        failed = self.call('install', 'HEAD', env=tools_env, ok=False)
        self.assertIn('returned non-zero exit status 17.', failed.stderr)
        self.assertEqual(self.repository_snapshot(decoy), before)
        self.assertEqual(list((self.root / 'build').glob('source-*')), [])
        self.assertEqual(list((self.root / 'kit/installs').glob('.install-*')), [])
        self.assertEqual(out.stdout.strip(), self.git(repo, 'rev-parse', 'HEAD') + '-prebuilt')
        self.assertEqual(self.git(repo, 'worktree', 'list', '--porcelain').count('worktree '), 1)

    def install(self, ref='HEAD'):
        return self.call('install', ref, '--prebuilt', self.bin).stdout.strip()

    def status(self):
        return json.loads(self.call('status').stdout)

    def tm(self, status, *args):
        # tmux takes PATH from a new-window client's environment, not just globals.
        # Match the kit's attach/atm boundary rather than an unsealed external client.
        env = dict(self.env, PATH=str(Path(status['run']) / 'bin') + ':'
                   + status['bin'] + ':' + self.env['PATH'])
        out = subprocess.run(['tmux', '-S', status['tmux_socket'], *args],
                             env=env, capture_output=True, text=True, timeout=10)
        self.assertEqual(out.returncode, 0, out.stderr)
        return out.stdout.strip()

    # Removing install staging/immutability would overwrite the retained pair.
    def test_install_retains_versions_and_refuses_overwrite(self):
        _, previous = self.fixture_repo()
        first = self.install()
        self.call('install', 'HEAD', '--prebuilt', self.bin, ok=False)
        second = self.install(previous)
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
        foreign.mkdir(mode=0o700)
        (foreign / 'sentinel').write_text('untouched')
        link = self.root / 'link'
        link.symlink_to(foreign)
        self.call('installs', env=dict(self.env, ATM_CANDIDATE_ROOT=str(link)), ok=False)
        out = self.call('installs', env=dict(self.env, ATM_CANDIDATE_ROOT=str(foreign)), ok=False)
        self.assertIn('unowned root', out.stderr)
        self.call('start', '../../outside', ok=False)
        self.assertEqual((foreign / 'sentinel').read_text(), 'untouched')

    # Status must not autostart; wrapper must never expose setup/uninstall mutations.
    def test_read_only_status_and_refused_commands(self):
        self.assertEqual(self.status(), {'running': False})
        self.assertFalse((self.root / 'kit').exists(), 'status must not create kit state')
        self.assertEqual(json.loads(self.call('installs').stdout), [])
        self.assertFalse((self.root / 'kit').exists(), 'installs must be read-only')
        self.call('atm', 'list', ok=False)
        self.call('attach', env=dict(self.env, TMUX='/foreign,123,0'), ok=False)
        self.assertFalse((self.root / 'kit/active.json').exists())
        self.call('nonsense', ok=False)

    # Without the explicit guard, these commands reach real asset writers.
    def test_active_wrapper_refuses_asset_mutation(self):
        self.call('start', self.install())
        sentinels = {}
        for relative in ('.claude/settings.json', '.codex/hooks.json', '.pi/agent/settings.json'):
            path = self.root / 'home' / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text('{"sentinel": "unchanged"}\n')
            sentinels[path] = path.read_bytes()
        for command in ('setup', 'uninstall', 'workspace'):
            out = self.call('atm', command, ok=False)
            self.assertIn('would mutate live assets; refused', out.stderr)
            for path, content in sentinels.items():
                self.assertEqual(path.read_bytes(), content)

    # Ordinary pane commands must pass the same refusal, not the raw binary.
    def test_pane_wrapper_refuses_mutations_and_preserves_private_pane(self):
        output = self.root / 'pane-results.json'
        probe = self.root / 'commands.py'
        probe.write_text('import json,subprocess,os,shutil\n'
                         'results = {cmd: subprocess.run(["atm",cmd],capture_output=True,text=True,timeout=30) '
                         'for cmd in ("setup","uninstall","workspace","--version")}\n'
                         'results["stop"] = subprocess.run(["bash",' + repr(str(self.script))
                         + ',"stop"],capture_output=True,text=True,timeout=30)\n'
                         'open(' + repr(str(output)) + ',"w").write(json.dumps('
                         '{"atm":shutil.which("atm"),"results":'
                         '{k:[v.returncode,v.stdout,v.stderr] for k,v in results.items()}}))\n')
        command = self.root / 'commands.sh'
        command.write_text('#!/bin/sh\ntmux wait-for kit-release\npython3 ' + str(probe) + '\n'
                           'tmux wait-for -S kit-commands\nexec /bin/sh\n')
        command.chmod(0o700)
        (self.root / 'home/.tmux.conf').write_text('set -g default-command "' + str(command) + '"\n')
        self.call('start', self.install())
        status = self.status()
        self.tm(status, 'wait-for', '-S', 'kit-release')
        self.tm(status, 'wait-for', 'kit-commands')
        result = json.loads(output.read_text())
        self.assertEqual(result['atm'], str(Path(status['run']) / 'bin/atm'))
        for name in ('setup', 'uninstall', 'workspace'):
            code, _, error = result['results'][name]
            self.assertNotEqual(code, 0)
            self.assertIn('would mutate live assets; refused', error)
        self.assertEqual(result['results']['--version'][0], 0)
        self.assertNotEqual(result['results']['stop'][0], 0)
        self.assertIn('run stop outside the candidate server', result['results']['stop'][2])
        self.assertTrue(self.status()['running'])
        # A normal pane command retains private TMUX/PANE (pre-U7 spawn needs it).
        binary = Path(status['bin']) / 'atm'
        binary.rename(binary.with_name('atm-original'))
        binary.write_text('#!/bin/sh\nprintf "%s\\n%s\\n" "$TMUX" "$TMUX_PANE"\n')
        binary.chmod(0o700)
        env = dict(self.env, ATM_CANDIDATE_RUN=status['token'], TMUX_PANE='%0',
                   TMUX=status['tmux_socket'] + ',' + str(status['server']['pid']) + ',0')
        self.assertEqual(self.call('atm', env=env).stdout.splitlines(), [env['TMUX'], '%0'])

    # A stop inside the owned server must refuse before killing its own terminal.
    def test_stop_refuses_inside_candidate(self):
        self.call('start', self.install())
        status = self.status()
        env = dict(self.env, ATM_CANDIDATE_RUN=status['token'])
        out = self.call('stop', env=env, ok=False)
        self.assertIn('run stop outside the candidate server', out.stderr)
        self.assertTrue(self.status()['running'])
        self.assertEqual(self.tm(status, 'display-message', '-p', '#{pid}'),
                         str(status['server']['pid']))

    # Wrong endpoint/global env, omitted cleanup, or new state on restart breaks this.
    def test_lifecycle_environment_restart_and_state_retention(self):
        install = self.install()
        # The initial pane is created by the kit, not a test client seeding PATH.
        initial = self.root / 'initial.json'
        probe = self.root / 'probe.py'
        probe.write_text('import json,os,shutil\n'
                         'open(' + repr(str(initial)) + ',"w").write(json.dumps('
                         '{"env":dict(os.environ),"atm":shutil.which("atm")}))\n')
        command = self.root / 'pane.sh'
        command.write_text('#!/bin/sh\npython3 ' + str(probe) + '\n'
                           'tmux wait-for -S kit-initial\nexec /bin/sh\n')
        command.chmod(0o700)
        (self.root / 'home/.tmux.conf').write_text('set -g default-command "' + str(command) + '"\n')
        inherited = dict(self.env, TMUX='/foreign,123,0', TMUX_PANE='%123',
                         ATM_SOCKET=self.env['ATM_SOCKET'], ATM_TMUX_SOCKET='decoy',
                         ATM_STATE_DIR='/forbidden')
        self.call('start', install, env=inherited)
        status = self.status()
        self.assertTrue(status['running'])
        self.tm(status, 'wait-for', 'kit-initial')
        initial_env = json.loads(initial.read_text())
        wrapper_bin = str(Path(status['run']) / 'bin')
        self.assertEqual(initial_env['atm'], wrapper_bin + '/atm')
        self.assertEqual(initial_env['env']['PATH'].split(':')[0], wrapper_bin)
        self.assertTrue(status['socket'].startswith(str(self.root)))
        self.call('start', install, ok=False)
        self.call('uninstall', install, ok=False)
        variables = dict(line.split('=', 1) for line in
                         self.tm(status, 'show-environment', '-g').splitlines() if '=' in line)
        self.assertEqual(variables['ATM_SOCKET'], status['socket'])
        self.assertEqual(variables['ATM_STATE_DIR'], status['state'])
        self.assertEqual(variables['ATM_TMUX_SOCKET'], 'candidate')
        self.assertEqual(variables['PATH'].split(':')[0], wrapper_bin)
        # Event synchronization: tmux wait-for signals after the child writes.
        output = self.root / 'pane-env.json'
        code = 'import json,os; open(' + repr(str(output)) + ',"w").write(json.dumps(dict(os.environ)))'
        import shlex
        self.tm(status, 'new-window', '-d',
                'python3 -c ' + shlex.quote(code) + '; tmux wait-for -S kit-env')
        self.tm(status, 'wait-for', 'kit-env')
        pane_env = json.loads(output.read_text())
        for key in ('ATM_SOCKET', 'ATM_STATE_DIR', 'ATM_TMUX_SOCKET'):
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
        self.assertEqual(int((state / 'atmd.pid').read_text()), after['daemon']['pid'])
        self.assertIn(('ATM_STATE_DIR=' + str(state)).encode(),
                      Path(f'/proc/{after["daemon"]["pid"]}/environ').read_bytes().split(b'\0'))
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

    # Interrupted/failed atomic saves must keep the old manifest and allow retry.
    def test_save_cleans_temporary_file_and_recovers_interruption(self):
        import atm_candidate
        from unittest.mock import patch
        manifest = self.root / 'manifest.json'
        manifest.write_text('{"old": true}')
        temporary = manifest.with_suffix('.new')
        temporary.write_text('interrupted write')
        atm_candidate.save(manifest, {'new': True})
        self.assertEqual(json.loads(manifest.read_text()), {'new': True})
        for error in (OSError('replace failed'), KeyboardInterrupt()):
            with patch.object(Path, 'replace', side_effect=error):
                with self.assertRaises(type(error)):
                    atm_candidate.save(manifest, {'lost': True})
            self.assertFalse(temporary.exists())
            self.assertEqual(json.loads(manifest.read_text()), {'new': True})
        atm_candidate.save(manifest, {'retry': True})
        self.assertEqual(json.loads(manifest.read_text()), {'retry': True})

    # Malformed manifest shapes are user errors, not Python tracebacks.
    def test_malformed_manifest_reports_concise_error(self):
        self.install()
        active = self.root / 'kit/active.json'
        try:
            for value in ([], {'id': 123}):
                active.write_text(json.dumps(value))
                out = self.call('status', ok=False)
                self.assertNotIn('Traceback', out.stderr)
                self.assertIn('candidate:', out.stderr)
        finally:
            active.unlink()

    def build_fault_tools(self, mode):
        tools = self.root / 'tools'
        tools.mkdir()
        cargo = tools / 'cargo'
        cargo.write_text('#!/bin/sh\nif env | grep -q "^GIT_"; then exit 23; fi\n'
                         'if [ "$1" = --version ]; then echo test-cargo; exit; fi\n'
                         'touch "' + str(self.root / 'build-attempt') + '"\nexit 17\n')
        rustup = tools / 'rustup'
        rustup.write_text('#!/bin/sh\necho "' + str(cargo) + '"\n')
        git = tools / 'git'
        git.write_text('#!/bin/sh\n'
                       + ('case "$*" in *"worktree remove"*)\n'
                          'if [ ! -e "' + str(tools / 'removed-once') + '" ]; then\n'
                          'touch "' + str(tools / 'removed-once') + '"; exit 19; fi;; esac\n'
                          if mode == 'remove-error' else '')
                       + '/usr/bin/git "$@"\n'
                       + ('case "$*" in *"worktree add"*) kill -INT "$PPID";; esac\n'
                          if mode == 'interrupt-add' else ''))
        for path in (cargo, rustup, git):
            path.chmod(0o700)
        return dict(self.env, PATH=str(tools) + ':' + self.env['PATH'])

    # worktree add can register its tree before timeout/interrupt is reported.
    def test_interrupted_add_removes_registered_worktree(self):
        repo, _ = self.fixture_repo()
        out = self.call('install', 'HEAD', env=self.build_fault_tools('interrupt-add'), ok=False)
        self.assertNotIn('Traceback', out.stderr)
        self.assertIn('candidate:', out.stderr)
        self.assertEqual(list((self.root / 'build').glob('source-*')), [])
        self.assertEqual(list((self.root / 'kit/installs').glob('.install-*')), [])
        trees = self.git(repo, 'worktree', 'list', '--porcelain')
        self.assertEqual(trees.count('worktree '), 1)

    # A cleanup error must not mask the build error or strand staging/worktrees.
    def test_failed_build_and_cleanup_leave_no_staging(self):
        repo, _ = self.fixture_repo()
        out = self.call('install', 'HEAD', env=self.build_fault_tools('remove-error'), ok=False)
        self.assertNotIn('Traceback', out.stderr)
        self.assertIn('17', out.stderr)
        self.assertTrue((self.root / 'build-attempt').exists())
        self.assertEqual(list((self.root / 'build').glob('source-*')), [])
        self.assertEqual(list((self.root / 'kit/installs').glob('.install-*')), [])
        trees = self.git(repo, 'worktree', 'list', '--porcelain')
        self.assertEqual(trees.count('worktree '), 1)

    # Missing assets must refuse before even attempting an expensive source build.
    def test_missing_ref_assets_refuse_before_build(self):
        repo, _ = self.fixture_repo()
        asset = 'crates/atm/scripts/atm-hook'
        self.git(repo, 'rm', '-q', asset)
        self.git(repo, 'commit', '-qm', 'test: ref without hook')
        out = self.call('install', 'HEAD', env=self.build_fault_tools('build-error'), ok=False)
        self.assertIn('missing required asset at ref', out.stderr)
        self.assertIn(asset, out.stderr)
        self.assertFalse((self.root / 'build-attempt').exists())
        self.assertEqual(list((self.root / 'build').glob('source-*')), [])
        self.assertEqual(list((self.root / 'kit/installs').glob('.install-*')), [])

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
            for command in ('stop', 'status', 'installs', 'atm', 'attach', 'restart-daemon'):
                self.assertIn('holds the lock', self.call(command, ok=False).stderr)

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

    # Reused records without a listener must be discarded, not recovered/signaled.
    def test_reused_dead_records_are_gone(self):
        import atm_candidate
        import select
        self.call('start', self.install())
        status = self.status()
        kit = object.__new__(atm_candidate.Kit)
        self.assertEqual(self.tm(status, 'display-message', '-p', '#{socket_path} #{pid}'),
                         status['tmux_socket'] + ' ' + str(status['server']['pid']))
        kit.signal_process(status, status['daemon'])
        fd = os.pidfd_open(status['server']['pid'])
        try:
            self.tm(status, 'kill-server')
            self.assertTrue(select.select([fd], [], [], 30)[0])
        finally:
            os.close(fd)
        for kind in ('daemon', 'server'):
            status[kind].update(pid=os.getpid(), start='0')
        (self.root / 'kit/active.json').write_text(json.dumps(status))
        self.assertFalse(self.status()['running'])
        self.call('stop')
        os.kill(os.getpid(), 0)

    def cat_ready(self, child):
        import select
        # Popen can return while procfs still shows the pre-exec Python image.
        # An echoed line proves cat is executing before we inspect its identity.
        child.stdin.write(b'ready\n')
        child.stdin.flush()
        self.assertTrue(select.select([child.stdout], [], [], 30)[0], 'cat did not echo readiness')
        self.assertEqual(child.stdout.readline(), b'ready\n')

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
        daemon_binary = Path(status['bin']) / 'atmd'
        daemon_binary.unlink()
        shutil.copy2('/bin/false', daemon_binary)
        self.assertTrue(os.readlink(f'/proc/{status["daemon"]["pid"]}/exe').endswith(' (deleted)'))
        binary = tools / 'cat'
        shutil.copy2('/bin/cat', binary)
        child = subprocess.Popen([str(binary)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                 env=dict(self.env, ATM_SOCKET=status['socket'],
                                          TMUX_TMPDIR=str(Path(status['run']) / 't'),
                                          ATM_CANDIDATE_RUN=status['token']))
        try:
            self.cat_ready(child)
            binary.unlink()
            self.assertEqual(os.readlink(f'/proc/{child.pid}/exe'), str(binary) + ' (deleted)')
            self.assertIsNotNone(atm_candidate.identity(child.pid))
            self.call('stop')
            child.wait(timeout=30)
        finally:
            child.stdout.close()
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
        child = subprocess.Popen(['bash', str(self.script), 'attach'],
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
                                 stdout=subprocess.PIPE)
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
            self.cat_ready(child)
            kit.signal_process(status, atm_candidate.identity(child.pid))
        finally:
            child.stdout.close()
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
