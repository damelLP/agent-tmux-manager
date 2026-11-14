#!/usr/bin/env python3
"""Sealed promotion contracts; Cargo supplies binaries for real setup tests."""
import contextlib
import fcntl
import io
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
SCRIPTS = Path(__file__).resolve().parent


class Sealed(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='prom-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.home = self.root / 'home'
        self.home.mkdir()
        self.env = {k: v for k, v in os.environ.items()
                    if not k.startswith('GIT_') and k not in ('TMUX', 'TMUX_PANE')}
        self.env.update(HOME=str(self.home), SHELL='/bin/sh',
                        PATH='/usr/local/bin:/usr/bin:/bin',
                        XDG_CONFIG_HOME=str(self.home / '.config'),
                        XDG_DATA_HOME=str(self.home / '.local/share'),
                        XDG_STATE_HOME=str(self.root / 'xdg'),
                        ATM_STATE_DIR=str(self.root / 'state'),
                        ATM_SOCKET=str(self.root / 'atm.sock'),
                        ATM_TMUX_SOCKET='promotion-test',
                        TMUX_TMPDIR=str(self.root / 'tmux'), TMPDIR=str(self.root),
                        PYTHONDONTWRITEBYTECODE='1')

    def command(self, argv, cwd=None, env=None):
        return subprocess.run(list(map(str, argv)), cwd=cwd, env=env or self.env,
                              capture_output=True, text=True, timeout=30)

    def git(self, repo, *args):
        result = self.command(['git', '-C', repo, '-c', 'user.name=Promotion fixture',
                               '-c', 'user.email=promotion@example.invalid', *args],
                              env={k: v for k, v in self.env.items() if not k.startswith('GIT_')})
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout.strip()

    def init_repo(self, name):
        repo = self.root / name
        repo.mkdir()
        self.git(repo, '--git-dir', repo / '.git', '--work-tree', repo, 'init', '-q')
        self.assertEqual(Path(self.git(repo, 'rev-parse', '--absolute-git-dir')), repo / '.git')
        return repo

    def commit(self, repo):
        self.git(repo, 'add', '-A')
        self.git(repo, 'commit', '-qm', 'test: fixture revision')
        return self.git(repo, 'rev-parse', 'HEAD')


class ProtectedFiles(Sealed):
    def setUp(self):
        super().setUp()
        self.repo = self.init_repo('repo')
        (self.repo / 'scripts').mkdir()
        self.rules = self.repo / 'scripts/protected-files.txt'
        self.rules.write_text('judge/*\nscripts/*\n')
        (self.repo / 'judge').mkdir()
        (self.repo / 'judge/old').write_text('old')
        (self.repo / 'ordinary').write_text('unchanged')
        self.before = self.commit(self.repo)
        self.git(self.repo, 'update-ref', 'refs/remotes/origin/main', self.before)

    def report(self, *refs, env=None, ok=True, cwd=None):
        result = self.command([sys.executable, SCRIPTS / 'atm-protected-files.py', *refs],
                              cwd=cwd or self.repo, env=env)
        self.assertEqual(result.returncode == 0, ok, result.stdout + result.stderr)
        changes = {}
        for line in result.stdout.splitlines():
            if '\t' in line:
                status, path = line.split('\t', 1)
                changes[json.loads(path)] = status
        return changes, result

    # Missing deletion/status handling must not hide protected changes.
    def test_defaults_report_all_change_kinds_and_escaped_names(self):
        (self.repo / 'judge/old').unlink()
        odd = 'judge/space\tand\nnewline'
        (self.repo / odd).write_text('new')
        (self.repo / 'ordinary').write_text('not protected')
        self.rules.write_text(self.rules.read_text() + '# a comment\n')
        after = self.commit(self.repo)
        changes, result = self.report()
        self.assertEqual(changes, {'judge/old': 'D', odd: 'A', 'scripts/protected-files.txt': 'M'})
        self.assertIn(self.before, result.stdout)
        self.assertIn(after, result.stdout)

    def test_clean_and_unprotected_changes_exit_successfully(self):
        self.assertEqual(self.report(self.before, self.before)[0], {})
        (self.repo / 'ordinary').write_text('new')
        self.commit(self.repo)
        changes, result = self.report()
        self.assertEqual(changes, {})
        self.assertIn('0 protected', result.stdout)

    def test_renames_out_and_into_protected_paths_are_delete_and_add(self):
        (self.repo / 'judge/old').rename(self.repo / 'moved-out')
        (self.repo / 'ordinary').rename(self.repo / 'judge/moved-in')
        self.commit(self.repo)
        self.assertEqual(self.report()[0], {'judge/old': 'D', 'judge/moved-in': 'A'})

    def test_content_and_mode_changes_are_reported(self):
        (self.repo / 'judge/mode').write_text('mode')
        self.before = self.commit(self.repo)
        (self.repo / 'judge/mode').chmod(0o755)
        (self.repo / 'judge/old').write_text('different')
        after = self.commit(self.repo)
        self.assertEqual(self.report(self.before, after)[0], {'judge/old': 'M', 'judge/mode': 'M'})

    def test_historical_rules_survive_narrowing_and_deletion(self):
        for replacement in ('scripts/*\n', None):
            with self.subTest(replacement=replacement):
                self.git(self.repo, 'reset', '--hard', self.before)
                if replacement is None:
                    self.rules.unlink()
                else:
                    self.rules.write_text(replacement)
                (self.repo / 'judge/old').write_text('changed')
                self.commit(self.repo)
                changes, _ = self.report()
                self.assertEqual(changes['judge/old'], 'M')
                self.assertIn('scripts/protected-files.txt', changes)

    def test_historical_rules_apply_from_subdirectory(self):
        self.rules.write_text('scripts/*\n')
        (self.repo / 'judge/old').write_text('changed')
        self.commit(self.repo)
        changes, _ = self.report(cwd=self.repo / 'scripts')
        self.assertEqual(changes, {'judge/old': 'M', 'scripts/protected-files.txt': 'M'})

    def test_initial_list_introduction_and_future_fixture(self):
        self.rules.unlink()
        before = self.commit(self.repo)
        self.rules.write_text('judge/*\n')
        fixture = self.repo / 'crates/future/tests/fixtures/golden.jsonl'
        fixture.parent.mkdir(parents=True)
        fixture.write_text('{}\n')
        after = self.commit(self.repo)
        changes, _ = self.report(before, after)
        self.assertEqual(changes, {'scripts/protected-files.txt': 'A',
                                  'crates/future/tests/fixtures/golden.jsonl': 'A'})

    def test_invalid_ref_and_invalid_manifest_fail(self):
        self.report('no-such-commit', ok=False)
        self.rules.write_text('../outside\n')
        self.commit(self.repo)
        self.report(ok=False)

    def test_diverged_revisions_compare_endpoints_not_merge_base(self):
        (self.repo / 'judge/old').write_text('left')
        left = self.commit(self.repo)
        self.git(self.repo, 'checkout', '--detach', self.before)
        (self.repo / 'ordinary').write_text('right')
        right = self.commit(self.repo)
        self.assertEqual(self.report(left, right)[0], {'judge/old': 'M'})

    def test_inherited_git_selectors_cannot_redirect_report_or_fixture(self):
        decoy = self.init_repo('decoy')
        (decoy / 'sentinel').write_text('keep')
        self.commit(decoy)
        before = {str(p.relative_to(decoy)): p.read_bytes()
                  for p in (decoy / '.git').rglob('*') if p.is_file()}
        poisoned = dict(self.env, GIT_DIR=str(decoy / '.git'),
                        GIT_INDEX_FILE=str(decoy / '.git/index'), GIT_WORK_TREE=str(decoy))
        self.env = poisoned
        fresh = self.init_repo('fresh')
        (fresh / 'file').write_text('new')
        self.commit(fresh)
        self.assertEqual(self.report(env=poisoned)[0], {})
        after = {str(p.relative_to(decoy)): p.read_bytes()
                 for p in (decoy / '.git').rglob('*') if p.is_file()}
        self.assertEqual(after, before)


class Promotion(Sealed):
    def setUp(self):
        super().setUp()
        self.bin = self.root / 'bin'
        self.source = self.root / 'source'
        self.bin.mkdir()
        self.source.mkdir()
        self.log = self.root / 'setup.jsonl'
        self.modes = self.root / 'modes.json'
        self.modes.write_text('{}')
        self.env.update(PROMOTION_LOG=str(self.log), PROMOTION_MODES=str(self.modes))
        self.old = self.make_pair(self.bin, 'old')
        self.new = self.make_pair(self.source, 'new')
        (self.bin / 'unrelated').write_text('keep')

    def make_pair(self, directory, label):
        atm = '''#!/usr/bin/python3
import json, os, sys
from pathlib import Path
label = LABEL
assert sys.argv[1:] == ['setup'], sys.argv
with Path(os.environ['PROMOTION_LOG']).open('a') as log:
    log.write(json.dumps(dict(label=label, sibling=Path(__file__).with_name('atmd').read_text(),
                              home=os.environ['HOME'], socket=os.environ['ATM_SOCKET'],
                              tmux=os.environ.get('TMUX'), pane=os.environ.get('TMUX_PANE'))) + '\\n')
mode = json.loads(Path(os.environ['PROMOTION_MODES']).read_text()).get(label)
if mode == 'warning': print('Installing atm skills... failed: blocked')
if mode == 'invalid':
    sys.stdout.buffer.write(bytes([255]))
    sys.exit(7)
if mode == 'failure':
    print('setup refused', file=sys.stderr)
    sys.exit(7)
'''.replace('LABEL', repr(label))
        (directory / 'atm').write_text(atm)
        (directory / 'atmd').write_text('#!/bin/sh\n# ' + label + '\nexit 0\n')
        (directory / 'atm').chmod(0o751)
        (directory / 'atmd').chmod(0o755)
        return self.pair_bytes(directory)

    def pair_bytes(self, directory):
        return {name: ((directory / name).read_bytes(), (directory / name).stat().st_mode & 0o777)
                for name in ('atm', 'atmd')}

    def args(self, operation):
        args = ['--bin-dir', str(self.bin), '--socket', self.env['ATM_SOCKET']]
        if operation == 'install':
            args += ['--from-bin', str(self.source)]
        return args

    def call(self, operation='install', ok=True, env=None, args=None):
        script = 'atm-promote' if operation == 'install' else 'atm-rollback'
        result = self.command([sys.executable, SCRIPTS / script,
                               *(self.args(operation) if args is None else args)], env=env)
        self.assertEqual(result.returncode == 0, ok, result.stdout + result.stderr)
        return result

    def records(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def previous(self):
        metadata = self.bin / '.atm-promotion'
        name = json.loads((metadata / 'previous.json').read_text())['pair']
        return metadata / 'pairs' / name

    def in_process(self, operation='install'):
        import atm_promotion
        with patch.dict(os.environ, self.env, clear=True), patch.object(sys, 'argv', ['tool', *self.args(operation)]):
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                return atm_promotion.main(operation)

    # Missing either rename or setup's exact destination invocation breaks this.
    def test_promote_and_idempotent_rollback_keep_regular_pairs_and_setup_routing(self):
        self.call(env=dict(self.env, TMUX='foreign', TMUX_PANE='%99'))
        self.assertEqual(self.pair_bytes(self.bin), self.new)
        self.assertEqual(self.pair_bytes(self.previous()), self.old)
        self.assertFalse((self.bin / 'atm').is_symlink())
        self.assertFalse((self.bin / 'atmd').is_symlink())
        self.call('rollback')
        archives = list((self.bin / '.atm-promotion/pairs').iterdir())
        self.assertIn(self.new, [self.pair_bytes(path) for path in archives])
        self.call('rollback')
        self.assertEqual(list((self.bin / '.atm-promotion/pairs').iterdir()), archives)
        self.assertEqual(self.pair_bytes(self.bin), self.old)
        records = self.records()
        self.assertEqual([r['label'] for r in records], ['new', 'old', 'old'])
        for record, pair in zip(records, (self.new, self.old, self.old)):
            self.assertEqual(record['sibling'], pair['atmd'][0].decode())
            self.assertEqual(record['home'], str(self.home))
            self.assertEqual(record['socket'], self.env['ATM_SOCKET'])
            self.assertIsNone(record['tmux'])
            self.assertIsNone(record['pane'])
        self.assertEqual((self.bin / 'unrelated').read_text(), 'keep')

    def test_repeated_promotion_refuses_without_repointing_previous(self):
        self.call()
        marker = self.bin / '.atm-promotion/previous.json'
        previous = marker.read_bytes()
        archives = list(marker.parent.joinpath('pairs').iterdir())
        result = self.call(ok=False)
        self.assertIn('already installed', result.stderr)
        self.assertEqual(marker.read_bytes(), previous)
        self.assertEqual(list(marker.parent.joinpath('pairs').iterdir()), archives)
        self.assertEqual([r['label'] for r in self.records()], ['new'])
        self.call('rollback')
        self.assertEqual(self.pair_bytes(self.bin), self.old)

    def test_mode_only_difference_is_promoted(self):
        self.call()
        (self.source / 'atm').chmod(0o755)
        self.call()
        self.assertEqual(self.pair_bytes(self.bin), self.pair_bytes(self.source))
        self.assertEqual(self.pair_bytes(self.previous()), self.new)
        self.call('rollback')
        self.assertEqual(self.pair_bytes(self.bin), self.new)

    def test_failed_second_promotion_preserves_rollback_target(self):
        self.call()
        marker = self.bin / '.atm-promotion/previous.json'
        previous = marker.read_bytes()
        self.make_pair(self.source, 'third')
        self.modes.write_text('{"third":"failure"}')
        self.call(ok=False)
        self.assertEqual(self.pair_bytes(self.bin), self.new)
        self.assertEqual(marker.read_bytes(), previous)
        self.assertEqual([r['label'] for r in self.records()], ['new', 'third', 'new'])
        self.call('rollback')
        self.assertEqual(self.pair_bytes(self.bin), self.old)

    def test_failed_second_recovery_retains_new_backup_for_rollback(self):
        self.call()
        self.make_pair(self.source, 'third')
        self.modes.write_text('{"third":"failure","new":"failure"}')
        result = self.call(ok=False)
        self.assertIn('recovery failed', result.stderr)
        self.assertEqual(self.pair_bytes(self.previous()), self.new)
        self.modes.write_text('{}')
        self.call('rollback')
        self.assertEqual(self.pair_bytes(self.bin), self.new)

    def test_second_promotion_remembers_immediate_previous_pair(self):
        self.call()
        third = self.make_pair(self.source, 'third')
        self.call()
        self.assertEqual(self.pair_bytes(self.bin), third)
        self.assertEqual(self.pair_bytes(self.previous()), self.new)
        self.call('rollback')
        self.assertEqual(self.pair_bytes(self.bin), self.new)

    def test_setup_errors_and_zero_exit_skill_failure_restore_old_pair(self):
        for mode in ('failure', 'warning'):
            with self.subTest(mode=mode):
                self.modes.write_text(json.dumps({'new': mode}))
                result = self.call(ok=False)
                self.assertIn('restored', result.stderr)
                self.assertEqual(self.pair_bytes(self.bin), self.old)
                self.assertEqual(self.pair_bytes(self.previous()), self.old)
                self.assertEqual([r['label'] for r in self.records()][-2:], ['new', 'old'])

    def test_recovery_setup_failure_keeps_old_pair_and_reports_both_outcomes(self):
        self.modes.write_text('{"new":"failure","old":"failure"}')
        result = self.call(ok=False)
        self.assertIn('recovery failed', result.stderr)
        self.assertIn('setup', result.stderr)
        self.assertEqual(self.pair_bytes(self.bin), self.old)
        self.assertEqual([r['label'] for r in self.records()], ['new', 'old'])

    def test_rollback_setup_failure_keeps_requested_old_pair(self):
        self.call()
        self.modes.write_text('{"old":"failure"}')
        self.call('rollback', ok=False)
        self.assertEqual(self.pair_bytes(self.bin), self.old)
        self.modes.write_text('{}')
        self.call('rollback')

    def test_invalid_setup_output_still_restores_pair(self):
        self.modes.write_text('{"new":"invalid"}')
        self.call(ok=False)
        self.assertEqual(self.pair_bytes(self.bin), self.old)
        self.assertEqual([r['label'] for r in self.records()], ['new', 'old'])

    def test_timeout_prints_partial_setup_output(self):
        import atm_promotion
        output, errors = io.StringIO(), io.StringIO()
        with patch.object(atm_promotion.subprocess, 'run', side_effect=subprocess.TimeoutExpired(
                ['atm', 'setup'], 60, output=b'partial output\\n', stderr=b'partial error\\n')):
            with patch.dict(os.environ, self.env, clear=True), contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
                with self.assertRaises(subprocess.TimeoutExpired):
                    atm_promotion.setup(self.bin, Path(self.env['ATM_SOCKET']))
        self.assertIn('partial output', output.getvalue())
        self.assertIn('partial error', errors.getvalue())

    def test_timeout_restores_old_pair_and_runs_old_setup(self):
        import atm_promotion
        real_run = subprocess.run
        def timeout_once(*args, **kwargs):
            if self.pair_bytes(self.bin) == self.new:
                raise subprocess.TimeoutExpired(args[0], 60)
            return real_run(*args, **kwargs)
        with patch.object(atm_promotion.subprocess, 'run', side_effect=timeout_once):
            self.assertNotEqual(self.in_process(), 0)
        self.assertEqual(self.pair_bytes(self.bin), self.old)
        self.assertEqual([r['label'] for r in self.records()], ['old'])

    def test_missing_nonexecutable_or_symlinked_input_refuses_before_setup(self):
        for directory in (self.bin, self.source):
            for name in ('atm', 'atmd'):
                for damage in ('missing', 'mode', 'symlink'):
                    with self.subTest(directory=directory.name, name=name, damage=damage):
                        path = directory / name
                        original = path.read_bytes(), path.stat().st_mode
                        if damage == 'mode':
                            path.chmod(0o600)
                        else:
                            path.unlink()
                            if damage == 'symlink':
                                path.symlink_to(self.bin / 'unrelated')
                        self.call(ok=False)
                        self.assertEqual(self.records(), [])
                        if path.is_symlink(): path.unlink()
                        path.write_bytes(original[0]); path.chmod(original[1])

    def test_required_arguments_overlap_and_noncanonical_directories_refuse(self):
        for args in ([], ['--socket', self.env['ATM_SOCKET']], ['--bin-dir', str(self.bin)],
                     ['--bin-dir', str(self.bin), '--socket', self.env['ATM_SOCKET'], '--from-bin', str(self.bin)]):
            self.call(args=args, ok=False)
        alias = self.root / 'alias'
        alias.symlink_to(self.bin, target_is_directory=True)
        self.call(args=['--bin-dir', str(alias), '--socket', self.env['ATM_SOCKET'],
                        '--from-bin', str(self.source)], ok=False)
        self.assertEqual(self.pair_bytes(self.bin), self.old)
        self.assertEqual(self.records(), [])

    def test_missing_incomplete_or_malformed_previous_refuses(self):
        self.call('rollback', ok=False)
        self.call()
        previous = self.previous()
        (previous / 'atmd').unlink()
        self.call('rollback', ok=False)
        self.assertEqual(self.pair_bytes(self.bin), self.new)
        marker = self.bin / '.atm-promotion/previous.json'
        for content in ('not json', '{"pair":"../../source"}'):
            marker.write_text(content)
            self.call('rollback', ok=False)
        self.assertEqual([r['label'] for r in self.records()], ['new'])

    def test_lock_and_metadata_symlinks_refuse_without_changes(self):
        self.call()
        meta = self.bin / '.atm-promotion'
        with (meta / 'lock').open('r') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.call('rollback', ok=False)
        for relative in ('lock', 'previous.json'):
            path = meta / relative
            data = path.read_bytes()
            path.unlink(); path.symlink_to(self.bin / 'unrelated')
            self.call('rollback', ok=False)
            path.unlink(); path.write_bytes(data); path.chmod(0o600)
        self.assertEqual(self.pair_bytes(self.bin), self.new)
        self.assertEqual((self.bin / 'unrelated').read_text(), 'keep')

    def test_existing_socket_refuses_without_connecting(self):
        with socket.socket(socket.AF_UNIX) as listener:
            listener.bind(self.env['ATM_SOCKET']); listener.listen(); listener.setblocking(False)
            self.call(ok=False)
            with self.assertRaises(BlockingIOError): listener.accept()
        self.assertEqual(self.pair_bytes(self.bin), self.old)
        self.assertEqual(self.records(), [])

    def child(self, executable, env):
        # cat blocks on a pipe, not a timing guess. /proc/exe identifies our copy.
        process = subprocess.Popen([str(executable)], env=env, stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        def cleanup():
            process.stdin.close()
            process.wait(timeout=30)
            process.stdout.close(); process.stderr.close()
        self.addCleanup(cleanup)
        process.stdin.write(b'ready\n'); process.stdin.flush()
        self.assertEqual(process.stdout.readline(), b'ready\n')
        return process

    def test_running_installed_binary_refuses_but_foreign_process_does_not(self):
        shutil.copy2('/bin/cat', self.bin / 'atmd')
        process = self.child(self.bin / 'atmd', dict(self.env, ATM_SOCKET=str(self.root / 'other.sock')))
        self.call(ok=False)
        self.assertIsNone(process.poll())
        process.stdin.close(); process.wait(timeout=30)
        (self.bin / 'atmd').write_bytes(self.old['atmd'][0]); (self.bin / 'atmd').chmod(0o755)
        foreign = self.child('/bin/cat', dict(self.env, ATM_SOCKET=str(self.root / 'foreign.sock')))
        self.call()
        self.assertIsNone(foreign.poll())

    def test_inaccessible_unrelated_process_is_ignored_but_possible_atm_refuses(self):
        import atm_promotion
        foreign = self.child('/bin/cat', dict(self.env, ATM_SOCKET=str(self.root / 'foreign.sock')))
        real_readlink = os.readlink
        denied = Path(f'/proc/{foreign.pid}/exe')
        def readlink(path, *args, **kwargs):
            if Path(path) == denied:
                raise PermissionError('procfs hidden')
            return real_readlink(path, *args, **kwargs)
        with patch.object(atm_promotion.os, 'readlink', side_effect=readlink):
            self.assertEqual(self.in_process(), 0)
        shutil.copy2('/bin/cat', self.bin / 'atmd')
        relevant = self.child(self.bin / 'atmd', dict(self.env, ATM_SOCKET=str(self.root / 'other.sock')))
        denied = Path(f'/proc/{relevant.pid}/exe')
        with patch.object(atm_promotion.os, 'readlink', side_effect=readlink):
            self.assertNotEqual(self.in_process(), 0)
        self.assertIsNone(relevant.poll())

    def test_matching_socket_environment_refuses_without_signaling_process(self):
        process = self.child('/bin/cat', self.env)
        self.call(ok=False)
        self.assertIsNone(process.poll())
        self.assertEqual(self.pair_bytes(self.bin), self.old)

    def test_copy_or_second_rename_failure_restores_both_and_reruns_setup(self):
        import atm_promotion
        for fault in ('copy', 'rename'):
            with self.subTest(fault=fault):
                real_copy, real_replace = shutil.copy2, os.replace
                fired = False
                def copy(source, destination, *args, **kwargs):
                    nonlocal fired
                    if not fired and Path(source) == self.source / 'atmd':
                        fired = True
                        raise OSError('injected source copy failure')
                    return real_copy(source, destination, *args, **kwargs)
                def replace(source, destination, *args, **kwargs):
                    nonlocal fired
                    if not fired and Path(destination) == self.bin / 'atmd':
                        fired = True
                        raise OSError('injected second rename failure')
                    return real_replace(source, destination, *args, **kwargs)
                with patch.object(atm_promotion.shutil, 'copy2', side_effect=copy if fault == 'copy' else real_copy), \
                     patch.object(atm_promotion.os, 'replace', side_effect=replace if fault == 'rename' else real_replace):
                    self.assertNotEqual(self.in_process(), 0)
                self.assertTrue(fired)
                self.assertEqual(self.pair_bytes(self.bin), self.old)
                self.assertEqual(self.pair_bytes(self.previous()), self.old)
                self.assertEqual(self.records()[-1]['label'], 'old')
                self.assertEqual(list(self.bin.glob('.atm-stage-*')), [])

    def test_failed_backup_does_not_replace_previous_or_installed_files(self):
        import atm_promotion
        self.call()
        previous = (self.bin / '.atm-promotion/previous.json').read_bytes()
        self.make_pair(self.source, 'third')
        real_copy = shutil.copy2
        def copy(source, destination, *args, **kwargs):
            if Path(source) == self.bin / 'atmd':
                raise OSError('injected backup failure')
            return real_copy(source, destination, *args, **kwargs)
        with patch.object(atm_promotion.shutil, 'copy2', side_effect=copy):
            self.assertNotEqual(self.in_process(), 0)
        self.assertEqual((self.bin / '.atm-promotion/previous.json').read_bytes(), previous)
        self.assertEqual(self.pair_bytes(self.bin), self.new)
        self.assertEqual([r['label'] for r in self.records()], ['new'])

    def test_rollback_repairs_accepted_interrupted_rename_window(self):
        self.call()
        # Model SIGKILL after only atm was replaced: previous remains complete,
        # and the staged atmd file has not yet been renamed into place.
        (self.bin / 'atmd').write_bytes(self.old['atmd'][0])
        staged = self.bin / '.atm-stage-interrupted'
        shutil.copy2(self.source / 'atmd', staged)
        marker = self.bin / '.atm-promotion/previous.json'
        previous = marker.read_bytes()
        mixed = self.pair_bytes(self.bin)
        result = self.call(ok=False)
        self.assertIn('interrupted promotion', result.stderr)
        self.assertIn('atm-rollback', result.stderr)
        self.assertEqual(marker.read_bytes(), previous)
        self.assertEqual(self.pair_bytes(self.bin), mixed)
        self.assertTrue(staged.exists())
        self.call('rollback')
        self.assertEqual(self.pair_bytes(self.bin), self.old)
        self.assertEqual(self.pair_bytes(self.previous()), self.old)
        self.assertEqual(list(self.bin.glob('.atm-stage-*')), [])


class PromotionRealSetup(Sealed):
    def setUp(self):
        super().setUp()
        self.bin = self.root / 'bin'
        self.source = self.root / 'source'
        self.bin.mkdir(); self.source.mkdir()
        for name in ('atm', 'atmd'):
            binary = os.environ.get('CANDIDATE_TEST_' + name.upper())
            self.assertTrue(binary and Path(binary).is_file(), 'Cargo-built CANDIDATE_TEST_' + name.upper() + ' required')
            for directory in (self.bin, self.source):
                shutil.copy2(binary, directory / name)
        # Same build, distinct modes: exercise real setup without a no-op promotion.
        (self.source / 'atm').chmod(0o751)
        for harness in ('.claude', '.codex', '.pi/agent'):
            (self.home / harness).mkdir(parents=True)
        (self.home / '.claude/settings.json').write_text('{"keep":"claude"}')
        (self.home / '.codex/hooks.json').write_text('{"keep":"codex"}')
        (self.home / '.pi/agent/settings.json').write_text('{"packages":["keep-package"]}')

    def call(self, rollback=False, ok=True):
        script = 'atm-rollback' if rollback else 'atm-promote'
        args = ['--bin-dir', self.bin, '--socket', self.env['ATM_SOCKET']]
        if not rollback: args += ['--from-bin', self.source]
        result = self.command([sys.executable, SCRIPTS / script, *args])
        self.assertEqual(result.returncode == 0, ok, result.stdout + result.stderr)
        return result

    def assert_assets(self):
        repo = SCRIPTS.parent
        for name in ('atm-hook', 'atm-codex-hook'):
            self.assertEqual((self.home / '.local/bin' / name).read_bytes(),
                             (repo / 'crates/atm/scripts' / name).read_bytes())
        for name in ('package.json', 'extensions/pi-atm.ts'):
            self.assertEqual((self.home / '.pi/agent/packages/pi-atm' / name).read_bytes(),
                             (repo / 'crates/atm/assets/pi-atm' / name).read_bytes())
        for harness in ('.claude', '.codex', '.pi/agent'):
            for skill in ('atm-implement', 'atm-coordinate'):
                self.assertEqual((self.home / harness / 'skills' / skill / 'SKILL.md').read_bytes(),
                                 (repo / 'crates/atm/assets/atm-skills' / skill / 'SKILL.md').read_bytes())
        self.assertEqual(json.loads((self.home / '.claude/settings.json').read_text())['keep'], 'claude')
        self.assertEqual(json.loads((self.home / '.codex/hooks.json').read_text())['keep'], 'codex')
        self.assertEqual(json.loads((self.home / '.pi/agent/settings.json').read_text())['packages'],
                         ['keep-package', 'packages/pi-atm'])

    def test_real_setup_refreshes_assets_on_promotion_and_rollback_without_connection(self):
        # Decoy is inherited, explicit promotion endpoint is a different absent path.
        with socket.socket(socket.AF_UNIX) as listener:
            listener.bind(str(self.root / 'decoy.sock')); listener.listen(); listener.setblocking(False)
            inherited = dict(self.env, ATM_SOCKET=str(self.root / 'decoy.sock'))
            for script in ('atm-promote', 'atm-rollback'):
                args = ['--bin-dir', self.bin, '--socket', self.env['ATM_SOCKET']]
                if script == 'atm-promote': args += ['--from-bin', self.source]
                result = self.command([sys.executable, SCRIPTS / script, *args], env=inherited)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assert_assets()
                # Force rollback to demonstrate asset refresh, not merely presence.
                if script == 'atm-promote':
                    (self.home / '.pi/agent/packages/pi-atm/extensions/pi-atm.ts').write_text('stale')
                with self.assertRaises(BlockingIOError): listener.accept()
        self.assertFalse(Path(self.env['ATM_SOCKET']).exists())
        self.assertFalse((self.root / 'state/atmd.pid').exists())

    def test_real_skill_failure_is_not_reported_as_success(self):
        skills = self.home / '.claude/skills'
        skills.mkdir(); (skills / '.atm-staging').write_text('block')
        result = self.call(ok=False)
        self.assertIn('failed:', result.stdout)
        self.assertIn('recovery failed', result.stderr)

    def test_running_real_installed_daemon_refuses_without_signaling(self):
        endpoint = self.root / 'running.sock'
        env = dict(self.env, ATM_SOCKET=str(endpoint))
        Path(env['TMUX_TMPDIR']).mkdir()
        with (self.root / 'daemon.log').open('w+') as log:
            process = subprocess.Popen([str(self.bin / 'atmd'), 'start'], env=env, stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 30
                while not endpoint.exists():
                    self.assertIsNone(process.poll(), 'daemon exited before ready')
                    self.assertLess(time.monotonic(), deadline, 'daemon socket readiness timed out')
                    time.sleep(.02)  # bounded readiness condition, not a settling delay
                result = self.call(ok=False)
                self.assertIn('running', result.stderr)
                self.assertIsNone(process.poll())
            finally:
                if process.poll() is None:
                    self.assertEqual(Path(f'/proc/{process.pid}/exe').resolve(), self.bin / 'atmd')
                    self.assertIn(('ATM_SOCKET=' + str(endpoint)).encode(),
                                  Path(f'/proc/{process.pid}/environ').read_bytes().split(b'\0'))
                    process.terminate()
                    process.wait(timeout=30)


if __name__ == '__main__':
    unittest.main()
