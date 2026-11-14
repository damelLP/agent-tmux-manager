#!/usr/bin/env python3
"""Sealed promotion contracts; Cargo supplies binaries for real setup tests."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

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

    def report(self, *refs, env=None, ok=True):
        result = self.command([sys.executable, SCRIPTS / 'atm-protected-files.py', *refs],
                              cwd=self.repo, env=env)
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


if __name__ == '__main__':
    unittest.main()
