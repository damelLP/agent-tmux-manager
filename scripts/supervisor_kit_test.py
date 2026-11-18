#!/usr/bin/env python3
"""Supervisor contracts: private roots and model-free daemon/tmux rehearsals."""
import contextlib
import copy
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import textwrap
import threading
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
SCRIPTS = Path(__file__).resolve().parent
SCRIPT = SCRIPTS / 'atm-supervise'
NAMES = ('atm', 'atmd')


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def final_stable_failure(real):
    # Synthetic: only stable's reverse-read incarnation exits unsuccessfully.
    return textwrap.dedent('''\
        #!/usr/bin/env python3
        import os, signal, subprocess, sys
        from pathlib import Path
        counter = Path(os.environ['ATM_STATE_DIR']).parent / 'stable-launch-count'
        count = int(counter.read_text()) + 1 if counter.exists() else 1
        counter.write_text(str(count))
        child = subprocess.Popen([REAL, *sys.argv[1:]])
        stopping = False
        def stop(*_):
            global stopping
            stopping = True
            child.send_signal(signal.SIGTERM)
        signal.signal(signal.SIGTERM, stop)
        code = child.wait(timeout=90)
        if count == 2 and stopping:
            print('stable reverse-read daemon failed while stopping', file=sys.stderr)
            sys.exit(44)
        sys.exit(code)
        ''').replace('REAL', repr(str(real)))


class SupervisorKit(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='supervisor-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.home = self.root / 'home'
        self.home.mkdir()
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        self.metadata = self.bin / '.atm-promotion'
        self.slots_path = self.metadata / 'slots.json'
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith('GIT_') and key not in ('TMUX', 'TMUX_PANE', 'ATM_AGENT')}
        self.env.update(HOME=str(self.home), SHELL='/bin/sh',
                        PATH='/usr/local/bin:/usr/bin:/bin',
                        XDG_CONFIG_HOME=str(self.home / '.config'),
                        XDG_DATA_HOME=str(self.home / '.local/share'),
                        XDG_STATE_HOME=str(self.root / 'xdg'),
                        ATM_STATE_DIR=str(self.root / 'state'),
                        ATM_SOCKET=str(self.root / 'decoy.sock'),
                        ATM_TMUX_SOCKET='supervisor-test',
                        TMUX_TMPDIR=str(self.root / 'tmux'), TMPDIR=str(self.root),
                        PYTHONDONTWRITEBYTECODE='1')
        for key, folder in (('ROOT', 'kit'), ('STATE_ROOT', 'kit-state'),
                            ('BUILD_ROOT', 'kit-build'), ('RUNTIME_ROOT', 'kit-run')):
            self.env['ATM_CANDIDATE_' + key] = str(self.root / folder)
        kit_root = self.root / 'kit'
        kit_root.mkdir(mode=0o700)
        (kit_root / '.atm-candidate-owner').write_text(str(os.getuid()))
        self.installs = kit_root / 'installs'
        self.installs.mkdir(mode=0o700)
        self.commit = 'a' * 40
        self.install_id = self.make_install(self.commit)
        for name in NAMES:
            shutil.copy2(self.installs / self.install_id / 'bin' / name, self.bin / name)
        self.decoy = socket.socket(socket.AF_UNIX)
        self.decoy.bind(self.env['ATM_SOCKET'])
        self.decoy.listen()
        self.decoy.setblocking(False)
        self.addCleanup(self.check_decoy)

    def check_decoy(self):
        try:
            try:
                connection, _ = self.decoy.accept()
            except BlockingIOError:
                return
            connection.close()
            self.fail('supervisor contacted a daemon socket')
        finally:
            self.decoy.close()

    def make_install(self, commit, profile='release'):
        name = commit + '-' + profile
        install = self.installs / name
        install.mkdir(mode=0o700)
        (install / 'bin').mkdir(mode=0o700)
        for exe in NAMES:
            binary = install / 'bin' / exe
            binary.write_text('#!/bin/sh\nexit 99\n# ' + commit + ' ' + exe + '\n')
            binary.chmod(0o755)
        manifest = dict(id=name, sha=commit, profile=profile, toolchain='fixture',
                        hashes={exe: digest(install / 'bin' / exe) for exe in NAMES},
                        assets={}, prebuilt_provenance_unverified=False)
        (install / 'install.json').write_text(json.dumps(manifest) + '\n')
        return name

    def call(self, command, *args, code=0, script=SCRIPT):
        result = subprocess.run([sys.executable, script, command, '--bin-dir', self.bin,
                                 *map(str, args)], env=self.env,
                                capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, code, result.stdout + result.stderr)
        self.assertNotIn('Traceback', result.stderr)
        return result

    def init(self, *extra, code=0):
        return self.call('init', '--installed-commit', self.commit, *extra, code=code)

    def slots(self):
        return json.loads(self.slots_path.read_text())['slots']

    def write_slots(self, slots):
        payload = json.dumps(slots, sort_keys=True, separators=(',', ':')).encode()
        self.slots_path.write_text(json.dumps(dict(slots=slots, sha256=hashlib.sha256(payload).hexdigest())))

    def another_slot(self):
        install_id = self.make_install('b' * 40)
        folder = self.installs / install_id
        stable = self.slots()['stable']
        record = dict(commit='b' * 40, install=install_id,
                      pair={exe: {'sha256': digest(folder / 'bin' / exe), 'mode': 0o755} for exe in NAMES},
                      kit=stable['kit'], kit_sha256=stable['kit_sha256'],
                      install_manifest_sha256=digest(folder / 'install.json'), report_kind='gate')
        path = self.metadata / 'reports/candidate.json'
        path.write_text(json.dumps(record))
        return dict(record, report='reports/candidate.json', report_sha256=digest(path))

    # Missing provenance, incorrect selection, or executable invocation breaks init.
    def test_init_records_existing_install_and_status_only_observes(self):
        self.init()
        before = self.slots_path.read_bytes()
        result = self.call('status')
        slots = json.loads(result.stdout)
        self.assertEqual(slots, self.slots())
        self.assertIsNone(slots['previous'])
        self.assertIsNone(slots['candidate'])
        self.assertEqual(slots['rejected'], [])
        self.assertEqual(slots['used_approvals'], [])
        stable = slots['stable']
        self.assertEqual(stable['commit'], self.commit)
        self.assertEqual(stable['install'], self.install_id)
        for name in NAMES:
            self.assertEqual(stable['pair'][name],
                             {'sha256': digest(self.bin / name), 'mode': 0o755})
        report = self.metadata / stable['report']
        self.assertEqual(stable['report_sha256'], digest(report))
        observation = json.loads(report.read_text())
        self.assertIn('baseline observation, not an approval', observation['description'])
        self.assertEqual(observation['install'], self.install_id)
        self.assertTrue(observation['observed_at'])
        self.assertEqual(stable['report_kind'], 'baseline')
        self.assertEqual(self.slots_path.read_bytes(), before)

    def test_absent_matching_install_refuses_without_recording(self):
        shutil.rmtree(self.installs / self.install_id)
        result = self.init(code=1)
        self.assertIn(self.commit, result.stderr)
        self.assertIn('matching', result.stderr)
        self.assertFalse(self.slots_path.exists())

    def test_ambiguous_matching_installs_are_listed_and_explicit_id_selects(self):
        other = self.make_install(self.commit, 'debug')
        result = self.init(code=1)
        self.assertIn(self.install_id, result.stderr)
        self.assertIn(other, result.stderr)
        self.assertFalse(self.slots_path.exists())
        self.init('--install', other)
        self.assertEqual(self.slots()['stable']['install'], other)

    def test_explicit_install_still_requires_matching_commit_and_pair(self):
        other = self.make_install('b' * 40)
        self.init('--install', other, code=1)
        self.assertFalse(self.slots_path.exists())
        (self.installs / self.install_id / 'bin/atmd').chmod(0o700)
        self.init('--install', self.install_id, code=1)
        self.assertFalse(self.slots_path.exists())

    def test_init_refuses_a_mode_mismatch(self):
        (self.bin / 'atm').chmod(0o700)
        self.init(code=1)
        self.assertFalse(self.slots_path.exists())

    def test_init_refuses_manifest_hash_mismatch(self):
        manifest_path = self.installs / self.install_id / 'install.json'
        manifest = json.loads(manifest_path.read_text())
        manifest['hashes']['atmd'] = '0' * 64
        manifest_path.write_text(json.dumps(manifest))
        self.init(code=1)
        self.assertFalse(self.slots_path.exists())

    def test_init_twice_refuses_without_overwriting_the_baseline(self):
        self.init()
        before = {str(path.relative_to(self.metadata)): path.read_bytes()
                  for path in self.metadata.rglob('*') if path.is_file()}
        self.init(code=1)
        after = {str(path.relative_to(self.metadata)): path.read_bytes()
                 for path in self.metadata.rglob('*') if path.is_file()}
        self.assertEqual(after, before)

    # Neither binary's content or executable mode may drift unnoticed.
    def test_tamper_of_either_installed_binary_needs_you(self):
        self.init()
        before = self.slots_path.read_bytes()
        for name in NAMES:
            path = self.bin / name
            original = path.read_bytes()
            for change in ('bytes', 'mode'):
                with self.subTest(binary=name, change=change):
                    if change == 'bytes':
                        path.write_bytes(original + b'# tampered\n')
                    else:
                        path.chmod(0o700)
                    result = self.call('status', code=4)
                    self.assertIn('NEEDS YOU', result.stderr)
                    self.assertEqual(self.slots_path.read_bytes(), before)
                    path.write_bytes(original)
                    path.chmod(0o755)
        self.call('status')

    def test_tamper_of_either_retained_install_binary_needs_you(self):
        self.init()
        for name in NAMES:
            path = self.installs / self.install_id / 'bin' / name
            original = path.read_bytes()
            for change in ('bytes', 'mode'):
                with self.subTest(binary=name, change=change):
                    if change == 'bytes':
                        path.write_bytes(original + b'# tampered\n')
                    else:
                        path.chmod(0o700)
                    self.call('status', code=4)
                    path.write_bytes(original)
                    path.chmod(0o755)
        self.call('status')

    def test_tamper_of_slots_record_needs_you_even_on_reinit(self):
        self.init()
        original = self.slots_path.read_bytes()
        record = json.loads(original)
        record['slots']['stable']['commit'] = 'b' * 40
        for changed in (json.dumps(record).encode(), b'not json', b'[]'):
            with self.subTest(changed=changed):
                self.slots_path.write_bytes(changed)
                self.call('status', code=4)
                self.init(code=4)
        self.slots_path.write_bytes(original)
        self.call('status')

    def test_missing_slots_record_needs_you(self):
        self.call('status', code=4)

    def test_tampered_install_manifest_needs_you(self):
        self.init()
        path = self.installs / self.install_id / 'install.json'
        manifest = json.loads(path.read_text())
        manifest['prebuilt_provenance_unverified'] = True
        path.write_text(json.dumps(manifest))
        self.call('status', code=4)

    def test_tampered_report_needs_you(self):
        self.init()
        path = self.metadata / self.slots()['stable']['report']
        path.write_bytes(path.read_bytes() + b'\n')
        self.call('status', code=4)

    def test_tampered_retained_kit_needs_you(self):
        self.init()
        path = self.metadata / self.slots()['stable']['kit'] / 'atm_rollout.py'
        path.write_bytes(path.read_bytes() + b'\n# tampered\n')
        self.call('status', code=4)

    def test_symlinked_slots_are_not_followed(self):
        self.init()
        outside = self.root / 'outside.json'
        outside.write_bytes(self.slots_path.read_bytes())
        before = outside.read_bytes()
        self.slots_path.unlink()
        self.slots_path.symlink_to(outside)
        self.call('status', code=4)
        self.init(code=4)
        self.assertEqual(outside.read_bytes(), before)


    def test_previous_candidate_and_rejected_installs_are_all_rehashed(self):
        self.init()
        original_slots = self.slots()
        record = self.another_slot()
        binary = self.installs / record['install'] / 'bin/atmd'
        original_binary = binary.read_bytes()
        for slot in ('previous', 'candidate', 'rejected'):
            with self.subTest(slot=slot):
                slots = copy.deepcopy(original_slots)
                slots[slot] = ([dict(record, reason='failed', evidence={})]
                               if slot == 'rejected' else record)
                self.write_slots(slots)
                self.call('status')
                binary.write_bytes(original_binary + b'# tampered\n')
                self.call('status', code=4)
                binary.write_bytes(original_binary)
                self.call('status')
        self.write_slots(original_slots)

    def test_installed_pending_candidate_is_verified_without_rotating(self):
        self.init()
        record = self.another_slot()
        slots = self.slots()
        slots.update(candidate=record, used_approvals=[record['report_sha256']])
        self.write_slots(slots)
        before = self.slots_path.read_bytes()
        for name in NAMES:
            shutil.copy2(self.installs / record['install'] / 'bin' / name, self.bin / name)
        result = self.call('status')
        self.assertEqual(json.loads(result.stdout), slots)
        self.assertEqual(self.slots_path.read_bytes(), before)
        (self.bin / 'atm').write_bytes((self.bin / 'atm').read_bytes() + b'# unknown pair\n')
        self.call('status', code=4)

    def test_old_kit_snapshot_survives_new_checkout_file_list(self):
        import atm_supervisor as supervisor
        self.init()
        slots = self.slots()
        with (patch.dict(os.environ, self.env, clear=True),
              patch.object(supervisor, 'KIT_FILES', (*supervisor.KIT_FILES, 'future.py'))):
            kit = supervisor.Kit(read_only=True)
            try:
                observed = supervisor.Supervisor(self.bin, kit).read()
            except supervisor.NeedsYou as error:
                self.fail(f'old retained kit must stay verifiable: {error}')
        self.assertEqual(observed, slots)

    def test_recomputed_checksum_cannot_hide_a_slot_disagreeing_with_its_report(self):
        self.init()
        original = self.slots()
        for change in ('commit', 'pair', 'report_kind'):
            with self.subTest(change=change):
                slots = copy.deepcopy(original)
                if change == 'commit':
                    slots['stable'].update(commit='b' * 40, install='b' * 40 + '-release')
                elif change == 'pair':
                    slots['stable']['pair']['atm']['mode'] = 0o700
                else:
                    slots['stable']['report_kind'] = 'gate'
                self.write_slots(slots)
                self.call('status', code=4)
        self.write_slots(original)
        self.call('status')

    def test_malformed_payloads_refuse_without_a_traceback(self):
        self.init()
        original = self.slots()
        for key, value in (('rejected', {}), ('rejected', [None]),
                           ('used_approvals', [None]), ('candidate', []),
                           ('stable', None), ('version', 9)):
            with self.subTest(key=key, value=value):
                slots = copy.deepcopy(original)
                slots[key] = value
                self.write_slots(slots)
                self.call('status', code=4)
        self.write_slots(original)
        self.call('status')

    def test_symlinked_kit_and_report_artifacts_are_not_followed(self):
        self.init()
        stable = self.slots()['stable']
        for path in (self.metadata / stable['kit'] / 'atm_candidate.py',
                     self.metadata / stable['report']):
            with self.subTest(path=path.name):
                original = path.read_bytes()
                outside = self.root / ('outside-' + path.name)
                outside.write_bytes(original)
                path.unlink()
                path.symlink_to(outside)
                self.call('status', code=4)
                self.assertEqual(outside.read_bytes(), original)
                path.unlink()
                path.write_bytes(original)
        self.call('status')

    def test_launcher_does_not_write_bytecode_in_the_kit(self):
        kit = self.root / 'launchers'
        kit.mkdir()
        for name in ('atm-supervise', 'atm_supervisor.py', 'atm_candidate.py',
                     'atm_promotion.py', 'atm_rollout.py'):
            shutil.copy2(SCRIPTS / name, kit / name)
        result = subprocess.run([sys.executable, kit / 'atm-supervise', '--help'],
                                env={key: value for key, value in self.env.items()
                                     if key != 'PYTHONDONTWRITEBYTECODE'},
                                capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(list(kit.rglob('*.pyc')), [])

    def test_second_supervisor_lock_holder_refuses_and_names_first(self):
        import atm_supervisor as supervisor
        self.init()
        before = self.slots_path.read_bytes()
        with supervisor.supervisor_lock(self.metadata):
            result = self.call('status', code=1)
            self.assertIn('holds the lock', result.stderr)
            self.assertIn(str(os.getpid()), result.stderr)
            self.init(code=1)
        self.assertEqual(self.slots_path.read_bytes(), before)
        self.call('status')

    def test_failed_write_keeps_old_slots_and_removes_temporary(self):
        import atm_candidate
        import atm_supervisor as supervisor
        self.init()
        before = self.slots_path.read_bytes()
        slots = self.slots()
        slots['used_approvals'] = ['1' * 64]
        def fail_mid_write(data, stream, **kwargs):
            stream.write('{"partial":')
            raise OSError('injected full disk')
        with patch.object(atm_candidate.json, 'dump', fail_mid_write):
            with self.assertRaises(OSError):
                supervisor.save_slots(self.slots_path, slots)
        self.assertEqual(self.slots_path.read_bytes(), before)
        self.assertFalse(self.slots_path.with_suffix('.new').exists())
        self.call('status')

    def test_failed_replace_keeps_old_slots_and_removes_temporary(self):
        import atm_supervisor as supervisor
        self.init()
        before = self.slots_path.read_bytes()
        slots = self.slots()
        slots['used_approvals'] = ['1' * 64]
        with patch.object(Path, 'replace', side_effect=OSError('injected rename failure')):
            with self.assertRaises(OSError):
                supervisor.save_slots(self.slots_path, slots)
        self.assertEqual(self.slots_path.read_bytes(), before)
        self.assertFalse(self.slots_path.with_suffix('.new').exists())
        self.call('status')


    def gate_setup(self):
        self.init()
        self.repo = self.root / 'source'
        self.repo.mkdir()
        self.git('init')
        git_dir = Path(self.git('rev-parse', '--absolute-git-dir'))
        self.assertTrue(git_dir.is_relative_to(self.root))
        (self.repo / 'proof').write_text('committed\n')
        self.git('add', 'proof')
        self.git('-c', 'user.name=Kit', '-c', 'user.email=kit@example.invalid',
                 'commit', '-m', 'fixture')
        self.candidate_commit = self.git('rev-parse', 'HEAD')
        (self.repo / 'proof').write_text('dirty checkout\n')
        self.candidate_id = self.make_install(self.candidate_commit)
        self.gate_scripts = self.repo / 'scripts'
        self.gate_scripts.mkdir()
        import atm_supervisor
        for name in atm_supervisor.KIT_FILES:
            shutil.copy2(SCRIPTS / name, self.gate_scripts / name)
        self.gate_script = self.gate_scripts / 'atm-supervise'
        stand_in = self.repo / 'tests/support/stand_in/harness.py'
        stand_in.parent.mkdir(parents=True)
        shutil.copy2(SCRIPTS.parent / 'tests/support/stand_in/harness.py', stand_in)
        self.trace = self.root / 'trace.jsonl'
        tools = self.root / 'tools'
        tools.mkdir()
        cargo = tools / 'cargo'
        cargo.write_text(textwrap.dedent('''\
            #!/usr/bin/env python3
            import json, os
            from pathlib import Path
            import subprocess, sys
            if sys.argv[1:-1] != ['test', '-p', 'atmd', '--lib'] or sys.argv[-1] not in ('replay', 'frame_proof'):
                sys.exit(22)
            head = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
            with open(os.environ['GATE_TRACE'], 'a') as trace:
                trace.write(json.dumps(dict(filter=sys.argv[-1], head=head, cwd=os.getcwd(),
                    proof=Path('proof').read_text(), env=dict(os.environ))) + '\\n')
            mutation = os.environ.get('GATE_MUTATE')
            if mutation:
                with open(mutation, 'ab') as stream:
                    stream.write(b'# changed during gate\\n')
            outcome = os.environ.get('GATE_CARGO_OUTCOME', 'pass')
            passed = 26 if sys.argv[-1] == 'replay' else 6
            failed = 1 if outcome == 'failed-count' else 0
            if outcome == 'zero':
                passed = 0
            if outcome != 'no-summary':
                print(f'test result: ok. {passed} passed; {failed} failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s')
            sys.exit(1 if outcome == 'exit-fail' else 0)
            '''))
        cargo.chmod(0o755)
        rustup = tools / 'rustup'
        rustup.write_text('#!/bin/sh\n[ "$*" = "which cargo" ] || exit 23\nprintf "%s\\n" '
                          + str(cargo) + '\n')
        rustup.chmod(0o755)
        rollout = self.gate_scripts / 'atm-rollout'
        rollout.write_text(textwrap.dedent('''\
            #!/usr/bin/env python3
            import argparse, hashlib, json, os
            import sys
            parser = argparse.ArgumentParser()
            parser.add_argument('candidate')
            parser.add_argument('--dry-run', action='store_true', required=True)
            parser.add_argument('--bin-dir', required=True)
            parser.add_argument('--socket', required=True)
            parser.add_argument('--installed-commit', required=True)
            args = parser.parse_args()
            from pathlib import Path
            commit = json.loads((Path(args.candidate) / 'install.json').read_text())['sha']
            report = f'Comparing {args.installed_commit}..{commit}\\n0 protected change(s)\\n'
            with open(os.environ['GATE_TRACE'], 'a') as trace:
                trace.write(json.dumps(dict(rollout=vars(args))) + '\\n')
            print('Installed: fixture')
            print('To install: fixture')
            print(report, end='')
            print('Report sha256: ' + hashlib.sha256(report.encode()).hexdigest())
            print('Sessions: 0, 0 mid-turn')
            print('Dry run: nothing was changed. The run would:')
            print('  stop, swap, start (not executed)')
            sys.exit(int(os.environ.get('GATE_ROLLOUT_EXIT', '0')))
            '''))
        self.gate_pass = self.root / 'gate-pass.out'
        self.gate_pass.write_text(
            f'gate start 01:02:03 head {self.candidate_commit} run dir private-run\n'
            f'gate-runner exit 0 after 123 s; log private-run/gate.log; head {self.candidate_commit}\n'
            f'gate-runner: GATE PASSED {self.candidate_commit} run 42 (local-run)\n')
        self.env.update(PATH=str(tools) + ':' + self.env['PATH'], GATE_TRACE=str(self.trace))

    def git(self, *args, cwd=None):
        environment = {key: value for key, value in self.env.items() if not key.startswith('GIT_')}
        result = subprocess.run(['git', *map(str, args)], cwd=cwd or self.repo, env=environment,
                                capture_output=True, text=True, timeout=30, check=True)
        return result.stdout.strip()

    def gate(self, code=0, runner_label='gate-runner'):
        slots_before = self.slots_path.read_bytes()
        binaries_before = {name: (self.bin / name).read_bytes() for name in NAMES}
        result = self.call('gate', self.candidate_id, '--gate-pass', self.gate_pass,
                           '--gate-runner-label', runner_label,
                           '--socket', self.env['ATM_SOCKET'], code=code, script=self.gate_script)
        self.assertEqual(self.slots_path.read_bytes(), slots_before)
        self.assertEqual({name: (self.bin / name).read_bytes() for name in NAMES}, binaries_before)
        output = json.loads(result.stdout)
        path = Path(output['report'])
        self.assertTrue(path.is_relative_to(self.metadata / 'reports'))
        self.assertEqual(output['sha256'], digest(path))
        report = json.loads(path.read_text())
        self.assertEqual(output['result'], report['result'])
        self.assertEqual(report['result'], 'passed' if code == 0 else 'refused')
        registered = self.git('worktree', 'list', '--porcelain')
        self.assertEqual(registered.count('worktree '), 1, registered)
        self.assertEqual((self.repo / 'proof').read_text(), 'dirty checkout\n')
        return output, report

    # Ignoring the selected runner, interpreting it as a regex, or guessing a
    # missing label must not produce usable approval evidence.
    def test_gate_accepts_configured_runner_label(self):
        self.gate_setup()
        self.gate_pass.write_text(GatePassContracts.record('build-gate', self.candidate_commit))
        _, report = self.gate(runner_label='build-gate')
        step = report['steps'][3]
        self.assertEqual(step['command'], ['check', '--gate-pass', str(self.gate_pass),
                                           '--gate-runner-label', 'build-gate'])
        self.assertEqual(step['result']['record'], self.gate_pass.read_text())

    def test_gate_refuses_record_from_another_runner_label(self):
        self.gate_setup()
        self.gate_pass.write_text(GatePassContracts.record('gate-runner', self.candidate_commit))
        _, report = self.gate(code=1, runner_label='build-gate')
        self.assertEqual(report['steps'][-1]['name'], 'gate-pass')
        calls = [json.loads(line) for line in self.trace.read_text().splitlines()]
        self.assertEqual([call.get('filter', 'rollout') for call in calls], ['replay', 'frame_proof'])

    def test_gate_runner_label_treats_regex_metacharacters_literally(self):
        self.gate_setup()
        label = 'gate.runner+[local]'
        self.gate_pass.write_text(GatePassContracts.record(label, self.candidate_commit))
        _, report = self.gate(runner_label=label)
        self.assertEqual(report['steps'][3]['command'][-2:], ['--gate-runner-label', label])

    def test_gate_requires_explicit_runner_label(self):
        self.gate_setup()
        before = {str(path.relative_to(self.metadata)): path.read_bytes()
                  for path in self.metadata.rglob('*') if path.is_file()}
        result = self.call('gate', self.candidate_id, '--gate-pass', self.gate_pass,
                           '--socket', self.env['ATM_SOCKET'], code=2, script=self.gate_script)
        self.assertIn('--gate-runner-label', result.stderr)
        self.assertIn('required', result.stderr)
        self.assertFalse(self.trace.exists())
        after = {str(path.relative_to(self.metadata)): path.read_bytes()
                 for path in self.metadata.rglob('*') if path.is_file()}
        self.assertEqual(after, before)

    # A gate that uses the dirty checkout, records goldens, trusts zero tests,
    # rotates slots, omits evidence or calls non-dry rollout must fail these.
    def test_gate_pass_binds_all_evidence_and_runs_exact_commit_in_a_seal(self):
        self.gate_setup()
        decoy = self.root / 'decoy'
        decoy.mkdir()
        self.git('init', cwd=decoy)
        self.assertTrue(Path(self.git('rev-parse', '--absolute-git-dir', cwd=decoy)).is_relative_to(decoy))
        (decoy / 'sentinel').write_text('untouched\n')
        self.git('add', 'sentinel', cwd=decoy)
        self.git('-c', 'user.name=Kit', '-c', 'user.email=kit@example.invalid',
                 'commit', '-m', 'decoy', cwd=decoy)
        decoy_before = {name: (decoy / '.git' / name).read_bytes() for name in ('HEAD', 'index', 'config')}
        self.env.update(ATM_RECORD_REGISTRY_REPLAY='even-empty-is-unsafe',
                        GIT_DIR=str(decoy / '.git'), GIT_INDEX_FILE=str(decoy / '.git/index'))
        output, report = self.gate()
        self.assertEqual(report['commit'], self.candidate_commit)
        self.assertEqual(report['replaces'], self.slots()['stable'])
        self.assertEqual(report['report_kind'], 'gate')
        self.assertEqual(report['install'], self.candidate_id)
        manifest = self.installs / self.candidate_id / 'install.json'
        self.assertEqual(report['install_manifest_sha256'], digest(manifest))
        self.assertEqual(report['pair']['atmd']['sha256'], digest(manifest.parent / 'bin/atmd'))
        self.assertEqual(digest(self.metadata / report['kit'] / 'atm_supervisor.py'),
                         digest(self.gate_scripts / 'atm_supervisor.py'))
        steps = report['steps']
        self.assertEqual([step['name'] for step in steps],
                         ['manifest', 'replay', 'frame_proof', 'gate-pass', 'rehearsal', 'rollout'])
        for step in steps:
            content = {key: value for key, value in step.items() if key != 'sha256'}
            payload = json.dumps(content, sort_keys=True, separators=(',', ':')).encode()
            self.assertEqual(step['sha256'], hashlib.sha256(payload).hexdigest())
        self.assertEqual(steps[1]['result']['passed'], 26)
        self.assertEqual(steps[2]['result']['passed'], 6)
        self.assertIsNone(steps[1]['stable_passed'])
        self.assertIsNone(steps[1]['lower_than_stable'])
        self.assertEqual(steps[3]['result']['sha256'], digest(self.gate_pass))
        self.assertEqual(steps[3]['result']['record'], self.gate_pass.read_text())
        self.assertEqual(report['compatibility'], 'ONE-WAY')  # fixture atmd exits 99
        self.assertEqual(steps[4]['result']['failure']['step'], 'stable.start')
        self.assertTrue(steps[4]['result']['cleanup']['clean'])
        rollout = steps[5]['result']
        expected_report = f'Comparing {self.commit}..{self.candidate_commit}\n0 protected change(s)\n'
        self.assertEqual(rollout['protected_report'], expected_report)
        self.assertEqual(rollout['report_sha256'], hashlib.sha256(expected_report.encode()).hexdigest())
        self.assertEqual(rollout['quiet_snapshot'], 'Sessions: 0, 0 mid-turn\n')
        calls = [json.loads(line) for line in self.trace.read_text().splitlines()]
        self.assertEqual([call.get('filter', 'rollout') for call in calls], ['replay', 'frame_proof', 'rollout'])
        for call in calls[:2]:
            self.assertEqual(call['head'], self.candidate_commit)
            self.assertEqual(call['proof'], 'committed\n')
            environment = call['env']
            self.assertNotIn('ATM_RECORD_REGISTRY_REPLAY', environment)
            self.assertNotIn('TMUX', environment)
            self.assertNotIn('TMUX_PANE', environment)
            self.assertFalse(any(key.startswith('GIT_') for key in environment))
            self.assertTrue(Path(call['cwd']).is_relative_to(Path('/var/tmp')))
            self.assertNotEqual(environment['HOME'], str(self.home))
            self.assertTrue(Path(environment['ATM_SOCKET']).is_relative_to(Path(call['cwd']).parent))
            self.assertEqual(environment['PATH'].split(':')[0], str(self.root / 'tools'))
        self.assertEqual({name: (decoy / '.git' / name).read_bytes() for name in decoy_before}, decoy_before)

    def test_gate_refuses_exit_failure_failed_count_zero_tests_or_no_summary(self):
        self.gate_setup()
        for outcome in ('exit-fail', 'failed-count', 'zero', 'no-summary'):
            with self.subTest(outcome=outcome):
                self.env['GATE_CARGO_OUTCOME'] = outcome
                self.trace.unlink(missing_ok=True)
                _, report = self.gate(code=1)
                self.assertEqual(report['steps'][-1]['name'], 'replay')
                self.assertTrue(report['reason'])
                self.assertEqual(len(self.trace.read_text().splitlines()), 1)

    def test_gate_marks_a_lower_count_beside_stables_recorded_count(self):
        self.gate_setup()
        slots = self.slots()
        path = self.metadata / slots['stable']['report']
        baseline = json.loads(path.read_text())
        baseline['steps'] = [dict(name='replay', result={'passed': 27}),
                             dict(name='frame_proof', result={'passed': 6})]
        path.write_text(json.dumps(baseline))
        slots['stable']['report_sha256'] = digest(path)
        self.write_slots(slots)
        _, report = self.gate()
        replay, frame = report['steps'][1:3]
        self.assertEqual(replay['stable_passed'], 27)
        self.assertTrue(replay['lower_than_stable'])
        self.assertEqual(frame['stable_passed'], 6)
        self.assertFalse(frame['lower_than_stable'])

    def test_gate_refuses_prebuilt_provenance_before_any_child(self):
        self.gate_setup()
        path = self.installs / self.candidate_id / 'install.json'
        manifest = json.loads(path.read_text())
        manifest['prebuilt_provenance_unverified'] = True
        path.write_text(json.dumps(manifest))
        _, report = self.gate(code=1)
        self.assertIn('prebuilt', report['reason'])
        self.assertFalse(self.trace.exists())

    def test_gate_refuses_rejected_candidate_before_any_child(self):
        self.gate_setup()
        record = self.another_slot()
        slots = self.slots()
        slots['rejected'] = [dict(record, reason='failed', evidence={})]
        self.write_slots(slots)
        self.candidate_id = record['install']
        _, report = self.gate(code=1)
        self.assertIn('rejected', report['reason'])
        self.assertFalse(self.trace.exists())

    def test_gate_refuses_manifest_hash_mismatch(self):
        self.gate_setup()
        binary = self.installs / self.candidate_id / 'bin/atm'
        binary.write_bytes(binary.read_bytes() + b'# tampered\n')
        _, report = self.gate(code=1)
        self.assertIn('manifest', report['reason'])
        self.assertFalse(self.trace.exists())

    def test_gate_refuses_conflicting_outcomes_in_the_full_record(self):
        self.gate_setup()
        original = self.gate_pass.read_text()
        # Exact reviewed shape: valid start/exit/pass triple plus a failed retry.
        outcomes = (f'gate-runner: GATE FAILED {self.candidate_commit} run 43 (local-run)\n',
                    f'gate-runner: GATE FAILED {"f" * 40} run 43 (local-run)\n',
                    f'gate-runner: GATE PASSED {self.candidate_commit} run 43 (local-run)\n')
        for outcome in outcomes:
            with self.subTest(outcome=outcome):
                self.gate_pass.write_text(original + outcome)
                self.trace.unlink(missing_ok=True)
                _, report = self.gate(code=1)
                self.assertIn('conflict', report['reason'])
                self.assertEqual(report['steps'][-1]['name'], 'gate-pass')
                self.assertFalse(any('rollout' in json.loads(line)
                                     for line in self.trace.read_text().splitlines()))

    def test_gate_pass_record_requires_matching_heads_exit_zero_and_pass_marker(self):
        self.gate_setup()
        original = self.gate_pass.read_text()
        lines = original.splitlines(keepends=True)
        for changed in (original.replace(self.candidate_commit, 'f' * 40),
                        original.replace('exit 0', 'exit 1'),
                        original.replace('GATE PASSED', 'GATE FAILED'),
                        original.replace('head ' + self.candidate_commit, 'head ' + 'e' * 40, 1),
                        original.replace('; head ' + self.candidate_commit, '; head ' + 'e' * 40),
                        original.replace('PASSED ' + self.candidate_commit, 'PASSED ' + 'e' * 40),
                        ''.join(lines[1:]), ''.join(lines[:1] + lines[2:]), ''.join(lines[:2]),
                        original + 'LEFTOVER pid 123: fixture\n', original + original):
            with self.subTest(changed=changed):
                self.gate_pass.write_text(changed)
                self.trace.unlink(missing_ok=True)
                _, report = self.gate(code=1)
                self.assertEqual(report['steps'][-1]['name'], 'gate-pass')
                self.assertFalse(any('rollout' in json.loads(line)
                                     for line in self.trace.read_text().splitlines()))
        self.gate_pass.write_text(original)

    def test_gate_reports_rollout_refusals_and_never_attempts_recovery(self):
        self.gate_setup()
        for exit_code in ('1', '3', '4'):
            with self.subTest(exit_code=exit_code):
                self.env['GATE_ROLLOUT_EXIT'] = exit_code
                self.trace.unlink(missing_ok=True)
                _, report = self.gate(code=1)
                self.assertEqual(report['steps'][-1]['result']['returncode'], int(exit_code))
                self.assertEqual(len(self.trace.read_text().splitlines()), 3)

    def test_gate_changed_record_is_hashed_into_a_new_report(self):
        self.gate_setup()
        first, _ = self.gate()
        self.gate_pass.write_text(self.gate_pass.read_text().replace('run 42', 'run 43'))
        second, report = self.gate()
        self.assertNotEqual(first['sha256'], second['sha256'])
        self.assertEqual(report['steps'][3]['result']['sha256'], digest(self.gate_pass))
        self.assertIn('run 43', report['steps'][3]['result']['record'])

    def test_gate_marks_a_failed_final_stable_stop_one_way_and_still_passes(self):
        self.gate_setup()
        real = self.root / 'real-pair'
        real.mkdir()
        for name in NAMES:
            source = os.environ['CANDIDATE_TEST_' + name.upper()]
            subprocess.run(['strip', '-o', real / name, source], check=True, timeout=90)
        shutil.rmtree(self.metadata)  # only this fixture's old, synthetic baseline
        for install_id in (self.install_id, self.candidate_id):
            folder = self.installs / install_id
            for name in NAMES:
                shutil.copy2(real / name, folder / 'bin' / name)
        old = self.installs / self.install_id / 'bin/atmd'
        old.write_text(final_stable_failure(real / 'atmd'))
        for install_id in (self.install_id, self.candidate_id):
            folder = self.installs / install_id
            path = folder / 'install.json'
            manifest = json.loads(path.read_text())
            manifest['hashes'] = {name: digest(folder / 'bin' / name) for name in NAMES}
            path.write_text(json.dumps(manifest) + '\n')
        for name in NAMES:
            shutil.copy2(self.installs / self.install_id / 'bin' / name, self.bin / name)
        support = self.repo / 'tests/support/stand_in'
        shutil.rmtree(support)  # only this fixture's incomplete stand-in copy
        support.symlink_to(SCRIPTS.parent / 'tests/support/stand_in', target_is_directory=True)
        self.init()
        _, report = self.gate()
        rehearsal = next(step for step in report['steps'] if step['name'] == 'rehearsal')['result']
        stops = {step['name']: step['result'] for step in rehearsal['steps'] if step['name'].endswith('.stop')}
        self.assertEqual(stops['stable.stop']['returncode'], 0)
        self.assertEqual(stops['candidate.stop']['returncode'], 0)
        self.assertEqual(stops['stable-read.stop']['returncode'], 44)
        self.assertEqual(report['compatibility'], 'ONE-WAY')
        self.assertEqual(rehearsal['failure']['step'], 'stable-read.stop')
        self.assertIn('stable reverse-read daemon failed', rehearsal['failure']['output'])
        self.assertEqual(rehearsal['cleanup'], dict(clean=True, processes=0, atmd=0, tmux=0))
        self.assertFalse(Path(rehearsal['runtime']).exists())

    def test_gate_detects_install_or_pass_record_changes_while_replaying(self):
        self.gate_setup()
        for path, reason in ((self.installs / self.candidate_id / 'bin/atmd', 'manifest'),
                             (self.gate_pass, 'changed')):
            with self.subTest(path=path.name):
                original = path.read_bytes()
                self.env['GATE_MUTATE'] = str(path)
                try:
                    _, report = self.gate(code=1)
                    self.assertIn(reason, report['reason'])
                finally:
                    path.write_bytes(original)
        self.env.pop('GATE_MUTATE')


    def promote_setup(self, compatibility='two-way'):
        import atm_supervisor as supervisor
        self.gate_setup()
        rollout = self.gate_scripts / 'atm-rollout'
        text = rollout.read_text().replace("action='store_true', required=True", "action='store_true'")
        text = text.replace("parser.add_argument('candidate')", "parser.add_argument('candidate', nargs='?')")
        text = text.replace("parser.add_argument('--installed-commit', required=True)",
                            "parser.add_argument('--installed-commit')")
        text = text.replace("commit = json.loads((Path(args.candidate) / 'install.json').read_text())['sha']",
                            "commit = json.loads((Path(args.candidate) / 'install.json').read_text())['sha'] if args.candidate else ''")
        text = text.replace('dict(rollout=vars(args))',
                            "dict(rollout=dict(vars(args), environment_socket=os.environ['ATM_SOCKET'], "
                            "logs_copied=sorted(path.name for path in "
                            "(Path(args.bin_dir) / '.atm-promotion/evidence').glob('probation-*/*'))))")
        text = text.replace("args = parser.parse_args()", """parser.add_argument('--report-sha256')
parser.add_argument('--rollback', action='store_true')
args = parser.parse_args()""")
        text = text.replace("print('Installed: fixture')", """if args.rollback:
    import shutil
    root = Path(args.bin_dir)
    code = int(os.environ.get('PROMOTE_ROLLBACK_EXIT', '0'))
    if code == 0 or os.environ.get('PROMOTE_ROLLBACK_SWAP'):
        for name in ('atm', 'atmd'):
            shutil.copy2(root / '.atm-promotion/pairs/pair-fixture' / name, root / name)
        print('Version: fixture, daemon pid ' + os.environ['PROMOTE_PID'])
    if code == 0:
        print('rollout: DONE: the previous build is running and passed the checks.')
    else:
        print(os.environ.get('PROMOTE_ROLLBACK_STDERR',
                             'rollout: NEEDS YOU: candidate installed; daemon does NOT answer'), file=sys.stderr)
    sys.exit(code)
if not args.dry_run:
    if os.environ.get('PROMOTE_ROLLOUT_EXIT') == '1':
        sys.exit(1)  # R's refusal contract leaves the installed pair unchanged
    import shutil
    root = Path(args.bin_dir)
    metadata = root / '.atm-promotion'
    saved = metadata / 'pairs/pair-fixture'
    saved.mkdir(parents=True)
    for name in ('atm', 'atmd'):
        shutil.copy2(root / name, saved / name)
        if os.environ.get('PROMOTE_ROLLOUT_EXIT', '0') != '3':
            shutil.copy2(Path(args.candidate) / 'bin' / name, root / name)
    (metadata / 'previous.json').write_text(json.dumps(dict(pair='pair-fixture')))
    if os.environ.get('PROMOTE_CORRUPT_PREVIOUS'):
        (metadata / 'previous.json').write_text('{broken')
    if os.environ.get('PROMOTE_BAD_PREVIOUS'):
        (saved / 'atm').write_text('not last known good')
    log = Path(os.environ['ATM_STATE_DIR']) / 'log/events/00001.jsonl'
    log.parent.mkdir(parents=True, exist_ok=True)
    with log.open('ab') as stream:
        stream.write(b'candidate appended\\n')
    print('Version: fixture, daemon pid ' + os.environ['PROMOTE_PID'])
    sys.exit(int(os.environ.get('PROMOTE_ROLLOUT_EXIT', '0')))
print('Installed: fixture')""")
        text = text.replace("print('Sessions: 0, 0 mid-turn')", "print(os.environ.get('PROMOTE_QUIET', 'Sessions: 0, 0 mid-turn'))")
        rollout.write_text(text)
        clean = dict(compatibility=compatibility, cleanup=dict(clean=True), steps=[])
        with patch.dict(os.environ, self.env, clear=True), patch.object(supervisor, 'SCRIPTS', self.gate_scripts), \
                patch.object(supervisor, 'rehearse', return_value=clean):
            kit = supervisor.Kit(read_only=True)
            manager = supervisor.Supervisor(self.bin, kit)
            self.approval_output = manager.gate(manager.read(), self.candidate_id, self.gate_pass,
                                                self.env['ATM_SOCKET'], 'gate-runner')
        self.assertEqual(self.approval_output['result'], 'passed')
        self.approval = self.approval_output['sha256']
        self.gate_report = Path(self.approval_output['report'])
        self.env['PROMOTE_PID'] = str(os.getpid())
        state = Path(self.env['ATM_STATE_DIR'])
        (state / 'log/events').mkdir(parents=True, exist_ok=True)
        self.log = state / 'log/events/00001.jsonl'
        self.log.write_bytes(b'stable history\n')
        (state / 'atm.log').write_bytes(b'candidate diagnostic\x00\xff')
        (state / 'atm.log.1').write_bytes(b'old diagnostic\n')
        (state / 'private-state').write_bytes(b'must not be copied')
        self.trace.unlink()
        return self.slots()

    def promote_in_process(self, states=('ok', 'ok', 'ok'), pins=None):
        import atm_supervisor as supervisor
        clock, calls = FakeClock(), []
        states = iter(states)
        def read(pinned):
            calls.append(pinned)
            return dict(state=next(states))
        with patch.dict(os.environ, self.env, clear=True), patch.object(supervisor, 'SCRIPTS', self.gate_scripts), \
                patch.object(supervisor, 'rollout_identity', side_effect=pins,
                             return_value=supervisor.identity(os.getpid())):
            manager = supervisor.Supervisor(self.bin, supervisor.Kit(read_only=True))
            outcome = manager.promote(manager.read(), self.candidate_id, self.approval,
                                      self.env['ATM_SOCKET'], read=read,
                                      clock=clock.clock, wait=clock.wait, duration=30)
        return outcome, calls, clock

    def rollout_trace(self):
        return [json.loads(line)['rollout'] for line in self.trace.read_text().splitlines()]

    # Omitting acceptance, binding the wrong approval, or acting on a boundary
    # or ONE-WAY verdict must break these end-to-end checks.
    def test_promote_accepts_and_rotates_only_after_probation(self):
        before = self.promote_setup()
        prefix = self.log.read_bytes()
        (code, output), reads, clock = self.promote_in_process()
        self.assertEqual(code, 0)
        self.assertEqual(output['result'], 'accepted')
        after = self.slots()
        self.assertEqual(after['previous'], before['stable'])
        self.assertEqual(after['stable']['install'], self.candidate_id)
        self.assertIsNone(after['candidate'])
        self.assertEqual(after['rejected'], [])
        self.assertEqual(after['used_approvals'], [self.approval])
        self.assertEqual(len(reads), 3)
        self.assertEqual(clock.waits, [15, 15])
        trace = self.rollout_trace()
        self.assertEqual([row['dry_run'] for row in trace], [True, False])
        self.assertTrue(all(not row['rollback'] for row in trace))
        protected = json.loads(self.gate_report.read_text())['steps'][-1]['result']['report_sha256']
        self.assertEqual(trace[-1]['report_sha256'], protected)
        self.assertNotEqual(protected, self.approval)
        self.assertTrue(self.log.read_bytes().startswith(prefix))
        result = self.call('promote', self.candidate_id, '--approve', self.approval,
                           '--socket', self.env['ATM_SOCKET'], code=1, script=self.gate_script)
        self.assertIn('already used', result.stderr)
        self.assertEqual(len(self.rollout_trace()), 2)

    def test_unhealthy_probation_retains_only_logs_and_never_calls_rollback(self):
        import shlex
        for compatibility in ('ONE-WAY',):
            for state in ('failed', 'stopped', 'changed', 'refused'):
                with self.subTest(compatibility=compatibility, state=state):
                    # Independent private roots, even for each table row.
                    rig = SupervisorKit('test_init_records_existing_install_and_status_only_observes')
                    rig.setUp()
                    try:
                        before = rig.promote_setup(compatibility)
                        prefix = rig.log.read_bytes()
                        (code, output), reads, clock = rig.promote_in_process((state,))
                        self.assertEqual(code, 4)
                        self.assertEqual(output['result'], 'needs-you')
                        self.assertEqual(len(reads), 1)
                        self.assertEqual(clock.waits, [])
                        expected = shlex.join(['scripts/atm-rollout', '--rollback', '--bin-dir', str(rig.bin),
                                               '--socket', rig.env['ATM_SOCKET']])
                        self.assertEqual(output['manual_rollback'], expected)
                        after = rig.slots()
                        self.assertEqual(after['stable'], before['stable'])
                        self.assertEqual(after['previous'], before['previous'])
                        self.assertIsNone(after['candidate'])
                        self.assertEqual(after['rejected'][0]['install'], rig.candidate_id)
                        folder = Path(output['evidence']['directory'])
                        self.assertEqual({p.name for p in folder.iterdir()}, {'atm.log', 'atm.log.1'})
                        for name in ('atm.log', 'atm.log.1'):
                            self.assertEqual((folder / name).read_bytes(),
                                             (Path(rig.env['ATM_STATE_DIR']) / name).read_bytes())
                        self.assertTrue(rig.log.read_bytes().startswith(prefix))
                        self.assertEqual([row['dry_run'] for row in rig.rollout_trace()], [True, False])
                        self.assertTrue(all(not row['rollback'] for row in rig.rollout_trace()))
                    finally:
                        rig.doCleanups()

    def test_promote_full_all_probation_and_unknown_acceptance(self):
        self.promote_setup()
        (code, output), _, _ = self.promote_in_process(('unknown',) * 3)
        self.assertEqual(code, 0)
        self.assertIn('unknown', output['probation']['note'])
        self.assertIn('rollout', output['probation']['note'])

    def test_unavailable_wire_responses_reject_without_rotating_or_acting(self):
        import atm_supervisor as supervisor
        before = self.promote_setup('ONE-WAY')
        frame = dict(type='response', id='health', error=dict(code='unavailable', message='control stopped'))
        state = supervisor.health_state(frame)
        (code, output), reads, _ = self.promote_in_process((state,) * 3)
        self.assertEqual(code, 4)
        self.assertEqual(output['probation']['result'], 'unhealthy')
        self.assertEqual(len(reads), 3)
        self.assertEqual(self.slots()['stable'], before['stable'])
        self.assertEqual(len(self.slots()['rejected']), 1)
        self.assertEqual(len(self.rollout_trace()), 2)
        self.assertTrue(all(not row['rollback'] for row in self.rollout_trace()))
        self.assertEqual(set(output['evidence']['files']), {'atm.log', 'atm.log.1'})

    def test_manual_recovery_is_executable_explicit_and_never_automatic(self):
        import atm_supervisor as supervisor
        import shlex
        for compatibility, state in (('ONE-WAY', 'dead'), ('ONE-WAY', 'refused'), ('ONE-WAY', 'stopped')):
            with self.subTest(compatibility=compatibility, state=state):
                rig = SupervisorKit('test_init_records_existing_install_and_status_only_observes')
                rig.setUp()
                try:
                    rig.promote_setup(compatibility)
                    stdout, stderr = io.StringIO(), io.StringIO()
                    argv = [str(rig.gate_script), 'promote', rig.candidate_id, '--approve', rig.approval,
                            '--socket', rig.env['ATM_SOCKET'], '--bin-dir', str(rig.bin)]
                    with patch.dict(os.environ, rig.env, clear=True), \
                            patch.object(supervisor, 'SCRIPTS', rig.gate_scripts), patch.object(sys, 'argv', argv), \
                            patch.object(supervisor, 'rollout_identity', return_value=supervisor.identity(os.getpid())), \
                            patch.object(supervisor, 'read_health', return_value=dict(state=state)), \
                            contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                        self.assertEqual(supervisor.main(), 4)
                    command = shlex.join(['scripts/atm-rollout', '--rollback', '--bin-dir', str(rig.bin),
                                          '--socket', rig.env['ATM_SOCKET']])
                    self.assertEqual(json.loads(stdout.getvalue())['manual_rollback'], command)
                    self.assertIn('Compatibility: ' + compatibility, stderr.getvalue())
                    self.assertIn(command, stderr.getvalue())
                    if compatibility == 'ONE-WAY':
                        self.assertIn('old build may not read the organisation log the newer build wrote', stderr.getvalue())
                    if state in ('dead', 'refused'):
                        self.assertIn('no daemon answers on ' + rig.env['ATM_SOCKET'] + '; start it and run again',
                                      stderr.getvalue())
                        self.assertIn(shlex.join([str(rig.bin / 'atmd'), 'start', '-d']), stderr.getvalue())
                    else:
                        self.assertNotIn('start it and run again', stderr.getvalue())
                    self.assertEqual(len(rig.rollout_trace()), 2)
                    self.assertTrue(all(not row['rollback'] for row in rig.rollout_trace()))
                finally:
                    rig.doCleanups()

    def test_manual_start_overrides_the_environment_with_its_quoted_socket(self):
        # A missing assignment or quoting the whole assignment starts elsewhere
        # (or fails as a command); run the exact printed line, not a reconstruction.
        self.env['ATM_SOCKET'] = str(self.root / "rollback socket's.sock")
        self.promote_setup('ONE-WAY')
        (code, output), _, _ = self.promote_in_process(('dead',))
        self.assertEqual(code, 4)
        lines = output['manual_recovery'].splitlines()
        start = next((line for line in lines if ' start -d' in line), '')
        self.assertTrue(start)
        # Replace only this finished fixture's executable with an environment
        # witness; it must never start a real daemon in this unit-level test.
        (self.bin / 'atmd').write_text('#!/bin/sh\nprintf \'%s\\n\' "$ATM_SOCKET"\n')
        result = subprocess.run(['/bin/sh', '-c', start], env=dict(self.env, ATM_SOCKET=str(self.root / 'wrong.sock')),
                                capture_output=True, text=True, timeout=90)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), self.env['ATM_SOCKET'])
        self.assertEqual(len(self.rollout_trace()), 2)
        self.assertTrue(all(not row['rollback'] for row in self.rollout_trace()))

    def test_boundary_needs_you_copies_only_both_diagnostic_logs(self):
        for states, reason in ((('ok', 'missing', 'missing'), 'health remained uncertain at the end of probation'),
                               (('full',) * 3, 'full throughout probation')):
            with self.subTest(states=states):
                rig = SupervisorKit('test_init_records_existing_install_and_status_only_observes')
                rig.setUp()
                try:
                    before = rig.promote_setup()
                    prefix = rig.log.read_bytes()
                    (code, output), _, _ = rig.promote_in_process(states)
                    self.assertEqual(code, 4)
                    self.assertEqual(output['reason'], reason)
                    self.assertIn('directory', output['evidence'])
                    evidence = Path(output['evidence']['directory'])
                    self.assertEqual({path.name for path in evidence.iterdir()}, {'atm.log', 'atm.log.1'})
                    for name in ('atm.log', 'atm.log.1'):
                        self.assertEqual((evidence / name).read_bytes(),
                                         (Path(rig.env['ATM_STATE_DIR']) / name).read_bytes())
                    self.assertEqual(rig.slots()['stable'], before['stable'])
                    self.assertEqual(rig.slots()['previous'], before['previous'])
                    self.assertTrue(rig.log.read_bytes().startswith(prefix))
                    self.assertEqual(len(rig.rollout_trace()), 2)
                    self.assertTrue(all(not row['rollback'] for row in rig.rollout_trace()))
                finally:
                    rig.doCleanups()

    def test_full_all_probation_needs_you_without_an_action(self):
        before = self.promote_setup()
        (code, output), _, _ = self.promote_in_process(('full',) * 3)
        self.assertEqual(code, 4)
        self.assertIn('full throughout', output['reason'])
        self.assertEqual(self.slots()['stable'], before['stable'])
        self.assertEqual(len(self.rollout_trace()), 2)
        self.assertTrue(all(not row['rollback'] for row in self.rollout_trace()))

    def test_promote_refuses_input_drift_before_swap_or_approval_consumption(self):
        self.promote_setup()
        before = self.slots_path.read_bytes()
        binary = self.installs / self.candidate_id / 'bin/atm'
        original = binary.read_bytes()
        for mutation in ('quiet', 'candidate', 'kit', 'approval'):
            with self.subTest(mutation=mutation):
                kit_file = self.gate_scripts / 'atm-supervise'
                kit_bytes = kit_file.read_bytes()
                approval = self.approval
                try:
                    if mutation == 'quiet':
                        self.env['PROMOTE_QUIET'] = 'Sessions: 1, 0 mid-turn'
                    elif mutation == 'candidate':
                        binary.write_bytes(original + b'# drift\n')
                    elif mutation == 'kit':
                        kit_file.write_bytes(kit_bytes + b'# drift\n')
                    else:
                        self.approval = 'f' * 64
                    import atm_supervisor as supervisor
                    with self.assertRaises((supervisor.Refused, supervisor.NeedsYou)):
                        self.promote_in_process()
                    self.assertEqual(self.slots_path.read_bytes(), before)
                    if self.trace.exists():
                        self.assertTrue(all(row['dry_run'] for row in self.rollout_trace()))
                finally:
                    self.env.pop('PROMOTE_QUIET', None)
                    binary.write_bytes(original)
                    kit_file.write_bytes(kit_bytes)
                    self.approval = approval

    def test_rollout_exit_four_needs_you_and_never_reads_health_or_acts_again(self):
        before = self.promote_setup()
        self.env['PROMOTE_ROLLOUT_EXIT'] = '4'
        (code, output), reads, _ = self.promote_in_process(())
        self.assertEqual(code, 4)
        self.assertIn('rollout exit 4', output['reason'])
        self.assertEqual(reads, [])
        self.assertEqual(self.slots()['stable'], before['stable'])
        self.assertEqual(len(self.rollout_trace()), 2)

    def test_last_known_good_mismatch_needs_you_without_a_health_read_or_rollback(self):
        self.promote_setup()
        self.env['PROMOTE_BAD_PREVIOUS'] = '1'
        (code, output), reads, _ = self.promote_in_process(())
        self.assertEqual(code, 4)
        self.assertIn('last-known-good', output['reason'])
        self.assertEqual(reads, [])
        self.assertEqual(len(self.rollout_trace()), 2)

    def test_rollout_already_recovered_is_observed_once_never_retried(self):
        self.promote_setup()
        self.env['PROMOTE_ROLLOUT_EXIT'] = '3'
        (code, output), reads, _ = self.promote_in_process(('ok',))
        self.assertEqual(code, 3)
        self.assertEqual(output['result'], 'rolled-back')
        self.assertEqual(len(reads), 1)
        self.assertEqual(len(self.rollout_trace()), 2)
        self.assertIsNone(self.slots()['candidate'])

    def test_stable_unhealthy_after_rollout_recovery_needs_you(self):
        self.promote_setup()
        self.env['PROMOTE_ROLLOUT_EXIT'] = '3'
        (code, output), reads, _ = self.promote_in_process(('failed',))
        self.assertEqual(code, 4)
        self.assertIn('stable unhealthy', output['reason'])
        self.assertEqual(len(reads), 1)
        self.assertEqual(len(self.rollout_trace()), 2)

    def test_promote_and_check_refuse_unreadable_slots_and_an_unknown_installed_pair(self):
        self.promote_setup()
        original = self.slots_path.read_bytes()
        binary = self.bin / 'atm'
        binary_bytes = binary.read_bytes()
        for corruption in ('slots', 'installed'):
            for command in ('promote', 'check'):
                with self.subTest(corruption=corruption, command=command):
                    if corruption == 'slots':
                        self.slots_path.write_bytes(b'{broken')
                    else:
                        binary.write_bytes(b'unknown installed pair')
                    try:
                        extra = [self.candidate_id, '--approve', self.approval] if command == 'promote' else []
                        result = self.call(command, *extra, '--socket', self.env['ATM_SOCKET'], code=4,
                                           script=self.gate_script)
                        self.assertIn('cannot verify slots', result.stderr)
                        self.assertFalse(self.trace.exists())
                    finally:
                        self.slots_path.write_bytes(original)
                        binary.write_bytes(binary_bytes)

    def test_promote_refuses_a_rejected_candidate_and_a_nonpassing_report(self):
        self.promote_setup()
        report = json.loads(self.gate_report.read_text())
        record = {key: report[key] for key in ('commit', 'install', 'pair', 'kit', 'kit_sha256',
                                              'install_manifest_sha256', 'report_kind')}
        record.update(report='reports/' + self.gate_report.name, report_sha256=self.approval,
                      reason='earlier rejection', evidence={})
        slots = self.slots()
        slots['rejected'] = [record]
        self.write_slots(slots)
        result = self.call('promote', self.candidate_id, '--approve', self.approval,
                           '--socket', self.env['ATM_SOCKET'], code=1, script=self.gate_script)
        self.assertIn('rejected', result.stderr)
        self.assertFalse(self.trace.exists())
        slots['rejected'] = []
        self.write_slots(slots)
        report['result'] = 'refused'
        self.gate_report.write_text(json.dumps(report))
        result = self.call('promote', self.candidate_id, '--approve', digest(self.gate_report),
                           '--socket', self.env['ATM_SOCKET'], code=1, script=self.gate_script)
        self.assertIn('not a passed gate', result.stderr)
        self.assertFalse(self.trace.exists())

    def test_failure_evidence_never_follows_a_log_symlink_or_reads_other_state(self):
        self.promote_setup()
        state = Path(self.env['ATM_STATE_DIR'])
        (state / 'atm.log').unlink()
        (state / 'atm.log').symlink_to(state / 'private-state')
        opens, active = [], [True]
        def audit(event, args):
            if event == 'open' and active[0] and isinstance(args[0], str) and args[0].startswith(str(state) + '/'):
                opens.append(Path(args[0]).name)
        sys.addaudithook(audit)
        try:
            (code, output), _, _ = self.promote_in_process(('failed',))
        finally:
            active[0] = False
        self.assertEqual(code, 4)
        folder = Path(output['evidence']['directory'])
        self.assertEqual({p.name for p in folder.iterdir()}, {'atm.log.1'})
        self.assertEqual(opens, ['atm.log.1'])
        self.assertEqual((state / 'private-state').read_bytes(), b'must not be copied')

    def test_evidence_copy_failure_still_stops_needs_you_and_never_acts(self):
        import atm_supervisor as supervisor
        self.promote_setup()
        with patch.object(supervisor, 'collect_logs', side_effect=OSError('evidence disk unavailable')):
            (code, output), _, _ = self.promote_in_process(('failed',))
        self.assertEqual(code, 4)
        self.assertIn('evidence disk unavailable', output['evidence']['error'])
        self.assertEqual(len(self.rollout_trace()), 2)
        self.assertEqual(len(self.slots()['rejected']), 1)

    def test_check_peer_that_exited_is_unhealthy_not_a_filesystem_error(self):
        import atm_supervisor as supervisor
        self.init()
        with patch.dict(os.environ, self.env, clear=True):
            manager = supervisor.Supervisor(self.bin, supervisor.Kit(read_only=True))
            code, output = manager.check(manager.read(), self.env['ATM_SOCKET'], read=lambda _: dict(
                state='dead', identity=dict(pid=999999999, start='gone', exe=str(self.bin / 'atmd'))))
        self.assertEqual(code, 2)
        self.assertEqual(output['verdict'], 'unhealthy')

    def test_rollout_refusal_rejects_and_does_not_read_health(self):
        before = self.promote_setup()
        history = self.log.read_bytes()
        self.env['PROMOTE_ROLLOUT_EXIT'] = '1'
        (code, output), reads, _ = self.promote_in_process(())
        self.assertEqual(code, 1)
        self.assertEqual(output['result'], 'refused')
        self.assertEqual(reads, [])
        self.assertEqual(len(self.rollout_trace()), 2)
        self.assertEqual(len(self.slots()['rejected']), 1)
        self.assertEqual(self.log.read_bytes(), history)
        import atm_supervisor as supervisor
        self.assertEqual(supervisor.pair_record(self.bin), before['stable']['pair'])
        self.call('status', script=self.gate_script)

    def test_rollout_pid_is_pinned_to_the_installed_executable_not_an_arbitrary_process(self):
        import atm_supervisor as supervisor
        with self.assertRaises(supervisor.Refused):
            supervisor.rollout_identity(f'Version: fixture, daemon pid {os.getpid()}\n', self.bin)
        with self.assertRaises(supervisor.Refused):
            supervisor.rollout_identity('rollout returned no checked PID\n', self.bin)

    def test_dry_run_timeout_refuses_without_consuming_approval(self):
        import atm_supervisor as supervisor
        self.promote_setup()
        before, errors = self.slots_path.read_bytes(), io.StringIO()
        argv = [str(self.gate_script), 'promote', self.candidate_id, '--approve', self.approval,
                '--socket', self.env['ATM_SOCKET'], '--bin-dir', str(self.bin)]
        with patch.dict(os.environ, self.env, clear=True), patch.object(supervisor, 'SCRIPTS', self.gate_scripts), \
                patch.object(sys, 'argv', argv), contextlib.redirect_stderr(errors), \
                patch.object(supervisor, 'gate_call', side_effect=subprocess.TimeoutExpired('atm-rollout', 300)):
            self.assertEqual(supervisor.main(), 1)
        self.assertIn('timed out', errors.getvalue())
        self.assertEqual(self.slots_path.read_bytes(), before)
        self.assertFalse(self.trace.exists())

    def test_corrupt_previous_json_after_swap_is_needs_you(self):
        self.promote_setup()
        self.env['PROMOTE_CORRUPT_PREVIOUS'] = '1'
        result = self.call('promote', self.candidate_id, '--approve', self.approval,
                           '--socket', self.env['ATM_SOCKET'], code=4, script=self.gate_script)
        self.assertIn('NEEDS YOU', result.stderr)
        self.assertIn('candidate remains pending', result.stderr)
        after = self.slots()
        self.assertEqual(after['candidate']['install'], self.candidate_id)
        self.assertEqual(after['used_approvals'], [self.approval])
        import atm_supervisor as supervisor
        self.assertEqual(supervisor.pair_record(self.bin), after['candidate']['pair'])
        self.assertEqual(len(self.rollout_trace()), 2)
        self.assertTrue(all(not row['rollback'] for row in self.rollout_trace()))

    def test_post_swap_manifest_or_identity_errors_leave_pending_needs_you(self):
        import atm_supervisor as supervisor
        for error in (ValueError('invalid previous JSON'), supervisor.KitRefused('invalid proc stat')):
            with self.subTest(error=type(error).__name__):
                rig = SupervisorKit('test_init_records_existing_install_and_status_only_observes')
                rig.setUp()
                try:
                    rig.promote_setup()
                    stdout, stderr = io.StringIO(), io.StringIO()
                    argv = [str(rig.gate_script), 'promote', rig.candidate_id, '--approve', rig.approval,
                            '--socket', rig.env['ATM_SOCKET'], '--bin-dir', str(rig.bin)]
                    with patch.dict(os.environ, rig.env, clear=True), \
                            patch.object(supervisor, 'SCRIPTS', rig.gate_scripts), patch.object(sys, 'argv', argv), \
                            patch.object(supervisor, 'previous', side_effect=error), \
                            contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                        self.assertEqual(supervisor.main(), 4)
                    self.assertIn('NEEDS YOU', stderr.getvalue())
                    self.assertIsNotNone(rig.slots()['candidate'])
                    self.assertEqual(len(rig.rollout_trace()), 2)
                    self.assertTrue(all(not row['rollback'] for row in rig.rollout_trace()))
                finally:
                    rig.doCleanups()

    def test_rollout_timeout_keeps_captured_output_and_state_line(self):
        import atm_supervisor as supervisor
        self.promote_setup()
        original = supervisor.gate_call
        line = b'rollout: NEEDS YOU: daemon stopped; candidate installed\n'
        def timeout_after_dry_run(step, *args):
            if '--dry-run' in step['command']:
                return original(step, *args)
            # Mirror gate_call's retained result at its exception boundary; the
            # real-child test below proves capture, this case proves propagation.
            step['result'] = dict(returncode=-9, stdout='rollout progress\n', stderr=line.decode())
            raise subprocess.TimeoutExpired(step['command'], 900,
                                            output=b'rollout progress\n', stderr=line)
        with patch.object(supervisor, 'gate_call', side_effect=timeout_after_dry_run):
            (code, output), reads, _ = self.promote_in_process(())
        self.assertEqual(code, 4)
        self.assertIn('rollout', output)
        self.assertEqual(output['rollout']['stdout'], 'rollout progress\n')
        self.assertEqual(output['rollout']['stderr'], line.decode())
        self.assertEqual(reads, [])
        self.assertEqual(len(self.rollout_trace()), 1)

    def test_gate_call_keeps_real_child_output_on_timeout_and_interrupt(self):
        import atm_supervisor as supervisor
        for failure in (subprocess.TimeoutExpired('rollout', 900), KeyboardInterrupt()):
            with self.subTest(failure=type(failure).__name__):
                endpoint = self.root / 'interrupted.sock'
                endpoint.unlink(missing_ok=True)
                with socket.socket(socket.AF_UNIX) as server:
                    server.settimeout(90)
                    server.bind(str(endpoint))
                    server.listen()
                    command = [sys.executable, '-c',
                               "import socket,sys; print('rollout progress',flush=True); "
                               "print('rollout: NEEDS YOU: daemon stopped',file=sys.stderr,flush=True); "
                               "s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); "
                               "s.sendall(b'ready'); s.recv(1)", str(endpoint)]
                    step = supervisor.gate_step([], 'rollout', command)
                    communicate, connections = subprocess.Popen.communicate, []
                    def interrupted(child, *args, **kwargs):
                        if not connections:
                            connection, _ = server.accept()
                            connections.append(connection)
                            self.assertEqual(connection.recv(5), b'ready')
                            raise failure
                        return communicate(child, *args, **kwargs)
                    try:
                        with patch.object(subprocess.Popen, 'communicate', interrupted):
                            with self.assertRaises(type(failure)):
                                supervisor.gate_call(step, self.env, self.root, 900)
                        self.assertIn('stdout', step['result'])
                        self.assertEqual(step['result']['stdout'], 'rollout progress\n')
                        self.assertEqual(step['result']['stderr'], 'rollout: NEEDS YOU: daemon stopped\n')
                        self.assertNotEqual(step['result']['returncode'], 0)
                    finally:
                        for connection in connections:
                            connection.close()

    def test_identity_read_kit_refusal_stays_pending_needs_you_in_both_paths(self):
        import atm_supervisor as supervisor
        for rollout_exit in ('0', '3'):
            with self.subTest(rollout_exit=rollout_exit):
                rig = SupervisorKit('test_init_records_existing_install_and_status_only_observes')
                rig.setUp()
                try:
                    before = rig.promote_setup()
                    rig.env['PROMOTE_ROLLOUT_EXIT'] = rollout_exit
                    stdout, stderr = io.StringIO(), io.StringIO()
                    argv = [str(rig.gate_script), 'promote', rig.candidate_id, '--approve', rig.approval,
                            '--socket', rig.env['ATM_SOCKET'], '--bin-dir', str(rig.bin)]
                    with patch.dict(os.environ, rig.env, clear=True), \
                            patch.object(supervisor, 'SCRIPTS', rig.gate_scripts), patch.object(sys, 'argv', argv), \
                            patch.object(supervisor, 'rollout_identity', return_value=supervisor.identity(os.getpid())), \
                            patch.object(supervisor, 'read_health', side_effect=supervisor.KitRefused('invalid proc stat')), \
                            contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                        self.assertEqual(supervisor.main(), 4)
                    self.assertIn('NEEDS YOU', stderr.getvalue())
                    self.assertIn('candidate remains pending', stderr.getvalue())
                    self.assertEqual(rig.slots()['candidate']['install'], rig.candidate_id)
                    self.assertEqual(rig.slots()['stable'], before['stable'])
                    self.assertEqual(rig.slots()['previous'], before['previous'])
                    self.assertEqual(rig.slots()['used_approvals'], [rig.approval])
                    self.assertEqual(len(rig.rollout_trace()), 2)
                    self.assertTrue(all(not row['rollback'] for row in rig.rollout_trace()))
                finally:
                    rig.doCleanups()

    def test_interrupted_recovery_health_stops_needs_you_without_a_retry(self):
        self.promote_setup()
        self.env['PROMOTE_ROLLOUT_EXIT'] = '3'
        def interrupted():
            raise KeyboardInterrupt()
            yield  # generator raises only when the health read is made
        (code, output), reads, _ = self.promote_in_process(interrupted())
        self.assertEqual(code, 4)
        self.assertIn('stable health uncertain', output['reason'])
        self.assertEqual(len(reads), 1)
        self.assertEqual(len(self.rollout_trace()), 2)

    def test_a_post_swap_slots_write_failure_is_needs_you_not_a_refusal(self):
        import atm_supervisor as supervisor
        for states in (('failed',), ('ok',) * 3):
            with self.subTest(states=states):
                rig = SupervisorKit('test_init_records_existing_install_and_status_only_observes')
                rig.setUp()
                try:
                    rig.promote_setup('ONE-WAY')
                    writes = []
                    save = supervisor.save_slots
                    def fail_decision(path, slots):
                        writes.append(slots)
                        if len(writes) == 2:
                            raise OSError('decision disk unavailable')
                        return save(path, slots)
                    with patch.object(supervisor, 'save_slots', side_effect=fail_decision):
                        with self.assertRaises(supervisor.NeedsYou) as error:
                            rig.promote_in_process(states)
                    self.assertIn('decision disk unavailable', str(error.exception))
                    if states == ('failed',):
                        self.assertIn('atm-rollout --rollback', str(error.exception))
                    self.assertIsNotNone(rig.slots()['candidate'])
                    self.assertEqual(len(rig.rollout_trace()), 2)
                finally:
                    rig.doCleanups()

    def test_check_with_a_pending_installed_candidate_needs_you_without_a_read(self):
        import atm_supervisor as supervisor
        self.promote_setup()
        with patch.dict(os.environ, self.env, clear=True), patch.object(supervisor, 'SCRIPTS', self.gate_scripts):
            manager = supervisor.Supervisor(self.bin, supervisor.Kit(read_only=True))
            record, _ = manager.approved_candidate(manager.read(), self.candidate_id, self.approval)
            supervisor.save_slots(self.slots_path, supervisor.stage_candidate(manager.read(), record, self.approval))
            for name in NAMES:
                shutil.copy2(self.installs / self.candidate_id / 'bin' / name, self.bin / name)
            def read(_):
                self.fail('check read a candidate as though it were stable')
            code, output = manager.check(manager.read(), self.env['ATM_SOCKET'], read=read)
        self.assertEqual(code, 4)
        self.assertIn('stable is not installed', output['reason'])

    def test_check_reports_once_and_never_changes_slots_or_calls_rollout(self):
        import atm_supervisor as supervisor
        self.init()
        before = self.slots_path.read_bytes()
        for state, expected in (('ok', 0), ('failed', 2), ('stopped', 2), ('refused', 2),
                                ('full', 4), ('unknown', 4), ('malformed', 4)):
            with self.subTest(state=state), patch.dict(os.environ, self.env, clear=True):
                manager = supervisor.Supervisor(self.bin, supervisor.Kit(read_only=True))
                calls = []
                def read(pinned):
                    calls.append(pinned)
                    return dict(state=state)
                code, _ = manager.check(manager.read(), self.env['ATM_SOCKET'], read=read)
                self.assertEqual(code, expected)
                self.assertEqual(len(calls), 1)
                self.assertEqual(self.slots_path.read_bytes(), before)


    @contextlib.contextmanager
    def recovery_case(self, compatibility='two-way'):
        rig = SupervisorKit('test_init_records_existing_install_and_status_only_observes')
        rig.setUp()
        try:
            before = rig.promote_setup(compatibility)
            yield rig, before
        finally:
            rig.doCleanups()

    # Removing the authorized call, repeating it, rotating on failure, copying
    # state, or observing the old candidate pin instead of R's new pin is a bug.
    def test_two_way_unhealthy_rolls_back_once(self):
        for states in (('failed',), ('stopped',), ('changed',), ('missing',) * 3,
                       ('malformed',) * 3, ('unavailable',) * 3):
            for stable in ('ok', 'unknown'):
                with self.subTest(states=states, stable=stable), self.recovery_case() as (rig, before):
                    prefix = rig.log.read_bytes()
                    candidate_pin = dict(pid=11, start='candidate', exe=str(rig.bin / 'atmd'))
                    stable_pin = dict(pid=22, start='restored', exe=str(rig.bin / 'atmd'))
                    (code, output), reads, _ = rig.promote_in_process((*states, stable),
                                                                      pins=[candidate_pin, stable_pin])
                    self.assertEqual(code, 3, output)
                    self.assertEqual(output['result'], 'rolled-back')
                    self.assertEqual(reads, [candidate_pin] * len(states) + [stable_pin])
                    trace = rig.rollout_trace()
                    self.assertEqual([(row['dry_run'], row['rollback']) for row in trace],
                                     [(True, False), (False, False), (False, True)])
                    self.assertIsNone(trace[-1]['candidate'])
                    self.assertIsNone(trace[-1]['installed_commit'])
                    self.assertIsNone(trace[-1]['report_sha256'])
                    self.assertEqual(trace[-1]['environment_socket'], rig.env['ATM_SOCKET'])
                    self.assertEqual(trace[-1]['logs_copied'], ['atm.log', 'atm.log.1'])
                    import atm_supervisor as supervisor
                    self.assertEqual(supervisor.pair_record(rig.bin), before['stable']['pair'])
                    self.assertEqual(output['restored_pair'], before['stable']['pair'])
                    self.assertEqual(output['rollback']['returncode'], 0)
                    self.assertIn('previous build is running', output['rollback']['stdout'])
                    self.assertTrue(rig.log.read_bytes().startswith(prefix))
                    after = rig.slots()
                    self.assertEqual(after['stable'], before['stable'])
                    self.assertEqual(after['previous'], before['previous'])
                    self.assertIsNone(after['candidate'])
                    self.assertEqual(after['used_approvals'], [rig.approval])
                    rejected = after['rejected'][0]
                    self.assertEqual(rejected['install'], rig.candidate_id)
                    self.assertIn('unhealthy daemon', rejected['reason'])
                    for key in ('evidence', 'probation', 'rollout', 'rollback', 'restored_pair'):
                        self.assertEqual(rejected['evidence'][key], output[key])
                    rig.call('status', script=rig.gate_script)
                    result = rig.call('promote', rig.candidate_id, '--approve', rig.approval,
                                      '--socket', rig.env['ATM_SOCKET'], code=1, script=rig.gate_script)
                    self.assertIn('already used', result.stderr)
                    self.assertEqual(len(rig.rollout_trace()), 3)

    def test_rollback_socket_overrides_a_different_shell_endpoint(self):
        import atm_supervisor as supervisor
        self.promote_setup()
        endpoint = self.env['ATM_SOCKET']
        environment = dict(self.env, ATM_SOCKET=str(self.root / 'wrong.sock'))
        clock, states = FakeClock(), iter(('failed', 'ok'))
        with patch.dict(os.environ, environment, clear=True), patch.object(supervisor, 'SCRIPTS', self.gate_scripts), \
                patch.object(supervisor, 'rollout_identity', return_value=supervisor.identity(os.getpid())):
            manager = supervisor.Supervisor(self.bin, supervisor.Kit(read_only=True))
            code, output = manager.promote(manager.read(), self.candidate_id, self.approval, endpoint,
                                          read=lambda _: dict(state=next(states)),
                                          clock=clock.clock, wait=clock.wait, duration=30)
        self.assertEqual(code, 3, output)
        self.assertEqual(self.rollout_trace()[-1]['environment_socket'], endpoint)

    def test_one_way_and_boundary_never_rollback(self):
        for compatibility, states in (('ONE-WAY', ('failed',)), ('ONE-WAY', ('stopped',)),
                                      ('ONE-WAY', ('dead',)), ('ONE-WAY', ('unavailable',) * 3),
                                      ('two-way', ('full',) * 3), ('two-way', ('ok', 'missing', 'missing'))):
            with self.subTest(compatibility=compatibility, states=states), self.recovery_case(compatibility) as (rig, before):
                (code, output), _, _ = rig.promote_in_process(states)
                self.assertEqual(code, 4)
                self.assertIn('scripts/atm-rollout --rollback', output['manual_rollback'])
                self.assertIn('Compatibility: ' + compatibility, output['manual_recovery'])
                if compatibility == 'ONE-WAY':
                    self.assertIn('old build may not read', output['manual_recovery'])
                self.assertEqual(len(rig.rollout_trace()), 2)
                self.assertEqual(rig.slots()['stable'], before['stable'])
                self.assertEqual(set(output['evidence']['files'].values()), {'copied'})

    def test_recovery_rechecks_probation_drift(self):
        import atm_supervisor as supervisor
        for mutation in ('slots', 'checksum', 'retained', 'previous-bytes', 'previous-mode', 'mixed-pair', 'stable-pair'):
            with self.subTest(mutation=mutation), self.recovery_case() as (rig, _):
                changed = []
                def states():
                    if mutation == 'slots':
                        slots = rig.slots()
                        slots['used_approvals'].append('f' * 64)
                        rig.write_slots(slots)
                    elif mutation == 'checksum':
                        rig.slots_path.write_bytes(b'{broken')
                    elif mutation == 'retained':
                        path = rig.installs / rig.candidate_id / 'bin/atmd'
                        path.write_bytes(path.read_bytes() + b'drift')
                    elif mutation == 'stable-pair':
                        for name in NAMES:
                            shutil.copy2(rig.metadata / 'pairs/pair-fixture' / name, rig.bin / name)
                    elif mutation.startswith('previous-'):
                        path = rig.metadata / 'pairs/pair-fixture/atm'
                        if mutation == 'previous-bytes':
                            path.write_bytes(path.read_bytes() + b'drift')
                        else:
                            path.chmod(0o751)
                    else:
                        (rig.bin / 'atm').write_bytes(b'mixed installed pair')
                    changed.append(rig.slots_path.read_bytes())
                    yield 'failed'
                    yield 'ok'
                (code, output), reads, _ = rig.promote_in_process(states())
                self.assertEqual(code, 4, output)
                self.assertEqual(len(reads), 1)
                self.assertEqual(len(rig.rollout_trace()), 2)
                self.assertEqual(rig.slots_path.read_bytes(), changed[0])
                self.assertIn('manual_rollback', output)
                if mutation != 'checksum':
                    self.assertIn(rig.approval, rig.slots()['used_approvals'])

    def test_rollback_refusal_or_uncertainty_is_terminal(self):
        import atm_supervisor as supervisor
        # Synthetic: --rollback never returns 3 today; even an already-restored
        # pair must not turn that nonzero result into permission to read health.
        for state, exit_code, swapped in (('dead', 1, False), ('refused', 1, False), ('failed', 3, False),
                                           ('stopped', 4, False), ('failed', 3, True)):
            with self.subTest(state=state, exit=exit_code, swapped=swapped), self.recovery_case() as (rig, before):
                rig.env['PROMOTE_ROLLBACK_EXIT'] = str(exit_code)
                if swapped:
                    rig.env['PROMOTE_ROLLBACK_SWAP'] = '1'
                (code, output), reads, _ = rig.promote_in_process((state, 'ok'))
                self.assertEqual(code, 4)
                self.assertEqual(len(reads), 1)
                self.assertEqual(len(rig.rollout_trace()), 3)
                self.assertEqual(output['rollback']['returncode'], exit_code)
                self.assertIn('daemon does NOT answer', output['rollback']['stderr'])
                self.assertIn('manual_rollback', output)
                if state in ('dead', 'refused'):
                    self.assertIn(' start -d', output['manual_recovery'])
                self.assertEqual(rig.slots()['stable'], before['stable'])
        for error in (OSError('cannot launch R'), subprocess.TimeoutExpired('R', 900), KeyboardInterrupt()):
            with self.subTest(error=type(error).__name__), self.recovery_case() as (rig, _):
                original, attempts = supervisor.gate_call, []
                def uncertain(step, *args):
                    if '--rollback' not in step['command']:
                        return original(step, *args)
                    attempts.append(step['command'])
                    step['result'] = dict(returncode=-9, stdout='rollback progress\n',
                                          stderr='rollout: NEEDS YOU: daemon stopped\n')
                    raise error
                with patch.object(supervisor, 'gate_call', uncertain):
                    (code, output), reads, _ = rig.promote_in_process(('failed',))
                self.assertEqual(code, 4)
                self.assertEqual(len(attempts), 1)
                self.assertEqual(len(reads), 1)
                self.assertEqual(len(rig.rollout_trace()), 2)
                self.assertEqual(output['rollback']['stdout'], 'rollback progress\n')
                self.assertIn('daemon stopped', output['rollback']['stderr'])
                self.assertIn('manual_rollback', output)

    def test_recovery_checks_installed_pair_after_slot_verification(self):
        import atm_supervisor as supervisor
        with self.recovery_case() as (rig, before):
            original, reads, changed = supervisor.Supervisor.read, [], []
            def read_slots(manager):
                slots = original(manager)
                reads.append(slots)
                if len(reads) == 4:  # recovery's slot check, after forward rollout
                    for name in NAMES:
                        shutil.copy2(rig.metadata / 'pairs/pair-fixture' / name, rig.bin / name)
                    changed.append(rig.slots_path.read_bytes())
                return slots
            with patch.object(supervisor.Supervisor, 'read', read_slots):
                (code, output), health, _ = rig.promote_in_process(('failed', 'ok'))
            self.assertEqual(code, 4, output)
            self.assertIn('candidate is not installed', output['reason'])
            self.assertEqual(len(health), 1)
            self.assertEqual(len(rig.rollout_trace()), 2)
            self.assertEqual(supervisor.pair_record(rig.bin), before['stable']['pair'])
            self.assertEqual(rig.slots_path.read_bytes(), changed[0])
            self.assertIsNotNone(rig.slots()['candidate'])

    # Failed R completion must not overwrite another observed slot lineage or
    # "repair" unreadable metadata with the pre-R snapshot.
    def test_failed_rollback_preserves_changed_or_unreadable_slots(self):
        import atm_supervisor as supervisor
        for mutation in ('approval', 'invalid-json'):
            for outcome in (1, 4, 'timeout'):
                with self.subTest(mutation=mutation, outcome=outcome), self.recovery_case() as (rig, before):
                    rig.env['PROMOTE_ROLLBACK_EXIT'] = str(1 if outcome == 'timeout' else outcome)
                    original, changed = supervisor.gate_call, []
                    def drift(step, *args):
                        result = original(step, *args)
                        if '--rollback' in step['command']:
                            if mutation == 'approval':
                                slots = rig.slots()
                                slots['used_approvals'].append('f' * 64)
                                rig.write_slots(slots)
                            else:
                                rig.slots_path.write_bytes(b'{invalid slots during R')
                            changed.append(rig.slots_path.read_bytes())
                            if outcome == 'timeout':
                                step['result'] = dict(returncode=-9, stdout='rollback progress\n',
                                                      stderr='rollout: NEEDS YOU: daemon stopped\n')
                                raise subprocess.TimeoutExpired('R', 900)
                        return result
                    with patch.object(supervisor, 'gate_call', drift):
                        (code, output), reads, _ = rig.promote_in_process(('failed', 'ok'))
                    self.assertEqual(code, 4, output)
                    self.assertEqual(len(reads), 1)
                    self.assertEqual(len(rig.rollout_trace()), 3)
                    self.assertEqual(output['rollback']['returncode'], -9 if outcome == 'timeout' else outcome)
                    self.assertIn('daemon stopped' if outcome == 'timeout' else 'daemon does NOT answer',
                                  output['rollback']['stderr'])
                    self.assertIn('Version: fixture', output['rollout']['stdout'])
                    evidence = Path(output['evidence']['directory'])
                    self.assertEqual(sorted(path.name for path in evidence.iterdir()), ['atm.log', 'atm.log.1'])
                    self.assertIn('scripts/atm-rollout --rollback', output['manual_rollback'])
                    self.assertEqual(rig.slots_path.read_bytes(), changed[0], output)
                    self.assertIn('candidate remains pending', output['reason'])
                    if mutation == 'approval':
                        after = rig.slots()
                        self.assertIsNotNone(after['candidate'])
                        self.assertEqual(after['rejected'], [])
                        self.assertEqual(after['stable'], before['stable'])
                        self.assertEqual(after['previous'], before['previous'])
                        self.assertEqual(after['used_approvals'], [rig.approval, 'f' * 64])

    # A valid but changed slot envelope, saved hash/mode drift, or the still-
    # installed candidate after R must leave the decision pending, untouched.
    def test_recovery_rechecks_rollback_drift(self):
        import atm_supervisor as supervisor
        for mutation in ('slots', 'previous-bytes', 'previous-mode', 'candidate-pair', 'candidate-and-saved-pair'):
            with self.subTest(mutation=mutation), self.recovery_case() as (rig, before):
                original, changed = supervisor.gate_call, []
                def drift(step, *args):
                    result = original(step, *args)
                    if '--rollback' in step['command']:
                        if mutation == 'slots':
                            slots = rig.slots()
                            slots['used_approvals'].append('f' * 64)
                            rig.write_slots(slots)
                        elif mutation.startswith('candidate-'):
                            for name in NAMES:
                                source = rig.installs / rig.candidate_id / 'bin' / name
                                shutil.copy2(source, rig.bin / name)
                                if mutation == 'candidate-and-saved-pair':
                                    # Both pairs agreeing is not proof that the
                                    # restored bytes are the approved stable.
                                    shutil.copy2(source, rig.metadata / 'pairs/pair-fixture' / name)
                        else:
                            path = rig.metadata / 'pairs/pair-fixture/atm'
                            if mutation == 'previous-bytes':
                                path.write_bytes(path.read_bytes() + b'drift during rollback')
                            else:
                                path.chmod(0o751)
                        changed.append(rig.slots_path.read_bytes())
                    return result
                with patch.object(supervisor, 'gate_call', drift):
                    (code, output), reads, _ = rig.promote_in_process(('failed', 'ok'))
                self.assertEqual(code, 4, output)
                self.assertIn('cannot verify rollback; candidate remains pending', output['reason'])
                self.assertEqual(len(reads), 1)
                self.assertEqual(len(rig.rollout_trace()), 3)
                self.assertEqual(output['rollback']['returncode'], 0)
                self.assertEqual(rig.slots_path.read_bytes(), changed[0])
                after = rig.slots()
                self.assertIsNotNone(after['candidate'])
                self.assertEqual(after['rejected'], [])
                self.assertEqual(after['stable'], before['stable'])
                self.assertEqual(after['previous'], before['previous'])
                self.assertIn(rig.approval, after['used_approvals'])

    def test_restored_health_is_read_once(self):
        import atm_supervisor as supervisor
        for state in ('failed', 'stopped', 'dead', 'changed', 'full', 'missing', 'malformed', 'unavailable'):
            with self.subTest(state=state), self.recovery_case() as (rig, before):
                (code, output), reads, _ = rig.promote_in_process(('failed', state))
                self.assertEqual(code, 4)
                self.assertEqual(len(reads), 2)
                self.assertEqual(len(rig.rollout_trace()), 3)
                self.assertEqual(rig.slots()['stable'], before['stable'])
                self.assertIn('manual_rollback', output)
        for error in (OSError('health read failed'), ValueError('bad reply'),
                      supervisor.KitRefused('bad identity'), KeyboardInterrupt()):
            with self.subTest(error=type(error).__name__), self.recovery_case() as (rig, _):
                def states():
                    yield 'failed'
                    raise error
                (code, output), reads, _ = rig.promote_in_process(states())
                self.assertEqual(code, 4)
                self.assertEqual(len(reads), 2)
                self.assertEqual(len(rig.rollout_trace()), 3)
                self.assertIn('manual_rollback', output)
        for mutation in ('pair', 'pin'):
            with self.subTest(mutation=mutation), self.recovery_case() as (rig, _):
                original = supervisor.gate_call
                def corrupt(step, *args):
                    result = original(step, *args)
                    if '--rollback' in step['command'] and mutation == 'pair':
                        (rig.bin / 'atm').write_bytes(b'wrong restored pair')
                    return result
                pin = supervisor.identity(os.getpid())
                with patch.object(supervisor, 'gate_call', corrupt):
                    (code, output), reads, _ = rig.promote_in_process(('failed',),
                        pins=[pin, supervisor.Refused('cannot pin restored daemon')] if mutation == 'pin' else None)
                self.assertEqual(code, 4)
                self.assertEqual(len(reads), 1)
                self.assertEqual(len(rig.rollout_trace()), 3)
                self.assertIn('manual_rollback', output)

    def test_recovery_evidence_and_persistence_fail_closed(self):
        import atm_supervisor as supervisor
        cases = [(name, mutation) for name in ('atm.log', 'atm.log.1')
                 for mutation in ('symlink', 'fifo', 'missing', 'copy-error')]
        for name, mutation in (*cases, ('atm.log', 'collect-error')):
            with self.subTest(log=name, mutation=mutation), self.recovery_case() as (rig, _):
                state = Path(rig.env['ATM_STATE_DIR'])
                path = state / name
                with contextlib.ExitStack() as stack:
                    if mutation in ('symlink', 'fifo', 'missing'):
                        path.unlink()
                        if mutation == 'symlink':
                            path.symlink_to(state / 'private-state')
                        elif mutation == 'fifo':
                            os.mkfifo(path)
                    elif mutation == 'copy-error':
                        original = supervisor.shutil.copyfile
                        def copy(source, *args, **kwargs):
                            if Path(source) == path:
                                raise OSError('cannot retain log')
                            return original(source, *args, **kwargs)
                        stack.enter_context(patch.object(supervisor.shutil, 'copyfile', copy))
                    else:
                        stack.enter_context(patch.object(supervisor, 'collect_logs', side_effect=OSError('disk failure')))
                    (code, output), reads, _ = rig.promote_in_process(('failed', 'ok'))
                self.assertEqual(code, 4)
                self.assertEqual(len(reads), 1)
                self.assertEqual(len(rig.rollout_trace()), 2)
                if mutation != 'collect-error':
                    other = 'atm.log.1' if name == 'atm.log' else 'atm.log'
                    self.assertEqual(output['evidence']['files'][other], 'copied')
                    self.assertNotEqual(output['evidence']['files'][name], 'copied')
                self.assertEqual((state / 'private-state').read_bytes(), b'must not be copied')
                self.assertIn('manual_rollback', output)
        with self.recovery_case() as (rig, _):
            original, writes = supervisor.save_slots, []
            def fail_decision(path, slots):
                writes.append(slots)
                if len(writes) == 2:
                    raise OSError('decision disk unavailable')
                return original(path, slots)
            with patch.object(supervisor, 'save_slots', fail_decision):
                with self.assertRaises(supervisor.NeedsYou) as failure:
                    rig.promote_in_process(('failed', 'ok'))
            self.assertIn('decision disk unavailable', str(failure.exception))
            self.assertIn('previous build is running', str(failure.exception))
            self.assertIn('scripts/atm-rollout --rollback', str(failure.exception))
            self.assertIsNotNone(rig.slots()['candidate'])
            self.assertEqual(rig.slots()['used_approvals'], [rig.approval])
            self.assertEqual(len(rig.rollout_trace()), 3)

    # Stale probation state or unconditional manual advice must not suggest
    # rolling back an already-restored pair or starting a second daemon.
    def test_automatic_recovery_exit_four_reports_attempt_and_current_manual_steps(self):
        import atm_supervisor as supervisor
        cases = (
            ('stable-unhealthy', 'stopped', 0, True, True, 'failed', ''),
            ('probation-refused', 'refused', 0, True, True, 'missing', ''),
            ('probation-dead', 'dead', 0, True, True, 'stopped', ''),
            ('restored-check-failed', 'stopped', 4, True, True, None,
             'rollout: the previous pair is installed, but check agent-list failed'),
            ('restored-daemon-stopped', 'stopped', 4, True, False, None,
             'rollout: the previous pair is installed, but check daemon-start failed'),
            ('swap-setup-failed-after-replace', 'stopped', 4, True, False, None,
             'rollout: the swap failed and did not restore the previous pair; the daemon is stopped'),
            ('refused-still-candidate-running', 'dead', 1, False, True, None,
             'rollout: refused before the swap; the installed build is unchanged and was started again'),
            ('refused-still-candidate-stopped', 'stopped', 1, False, False, None,
             'rollout: no daemon answers; start it and run again'),
        )
        for label, state, exit_code, swapped, answers, stable, own_stderr in cases:
            with self.subTest(case=label), self.recovery_case() as (rig, before), contextlib.ExitStack() as stack:
                rig.env.update(PROMOTE_ROLLBACK_EXIT=str(exit_code), PROMOTE_ROLLBACK_STDERR=own_stderr)
                if swapped:
                    rig.env['PROMOTE_ROLLBACK_SWAP'] = '1'
                # Real private socket acceptance, independent of the old health
                # verdict. The decoy socket must still never be contacted.
                endpoint = rig.root / 'current.sock'
                rig.env['ATM_SOCKET'] = str(endpoint)
                if answers:
                    listener = stack.enter_context(socket.socket(socket.AF_UNIX))
                    listener.bind(str(endpoint))
                    listener.listen()
                stdout, stderr = io.StringIO(), io.StringIO()
                argv = [str(rig.gate_script), 'promote', rig.candidate_id, '--approve', rig.approval,
                        '--socket', str(endpoint), '--bin-dir', str(rig.bin)]
                # Rebind the approved dry-run to this explicit endpoint, before
                # approval is used, just as promote_setup does for the decoy.
                report = json.loads(rig.gate_report.read_text())
                step = report['steps'][-1]
                step['command'][step['command'].index('--socket') + 1] = str(endpoint)
                step['sha256'] = supervisor.checksum({key: value for key, value in step.items() if key != 'sha256'})
                rig.gate_report.write_text(json.dumps(report))
                argv[argv.index('--approve') + 1] = digest(rig.gate_report)
                states = [dict(state=state)] + ([dict(state=stable)] if stable else [])
                with patch.dict(os.environ, rig.env, clear=True), \
                        patch.object(supervisor, 'SCRIPTS', rig.gate_scripts), patch.object(sys, 'argv', argv), \
                        patch.object(supervisor, 'rollout_identity', return_value=supervisor.identity(os.getpid())), \
                        patch.object(supervisor, 'read_health', side_effect=states), \
                        contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                    self.assertEqual(supervisor.main(), 4)
                output, text = json.loads(stdout.getvalue()), stderr.getvalue()
                self.assertIn(f'automatic rollback attempted, R exit {exit_code}', text, text)
                if own_stderr:
                    self.assertIn(own_stderr, text)
                self.assertEqual('scripts/atm-rollout --rollback' in text, not swapped, text)
                self.assertEqual(' start -d' in text, not answers, text)
                self.assertEqual('start it and run again' in text, not answers and not swapped, text)
                if swapped:
                    self.assertNotIn('before running rollback', text)
                self.assertEqual(bool(output['manual_rollback']), not swapped)
                self.assertEqual(' start -d' in output['manual_recovery'], not answers)
                installed = supervisor.pair_record(rig.bin)
                self.assertEqual(installed == before['stable']['pair'], swapped)
                self.assertEqual(len(rig.rollout_trace()), 3)
                if answers:
                    listener.settimeout(90)
                    connection, _ = listener.accept()
                    connection.close()

    # Interrupts in either verification are post-swap NEEDS YOU, not a
    # preflight refusal that loses the pending decision and recovery evidence.
    def test_recovery_verification_interrupts_keep_pending_state_and_details(self):
        import atm_supervisor as supervisor
        for phase, interrupt_at, expected_calls, restored in (('before-R', 2, 2, False), ('after-R', 3, 3, True)):
            with self.subTest(phase=phase), self.recovery_case() as (rig, before):
                original, verifications = supervisor.previous, []
                def previous(metadata):
                    verifications.append(metadata)
                    if len(verifications) == interrupt_at:
                        raise KeyboardInterrupt()
                    return original(metadata)
                stdout, stderr = io.StringIO(), io.StringIO()
                argv = [str(rig.gate_script), 'promote', rig.candidate_id, '--approve', rig.approval,
                        '--socket', rig.env['ATM_SOCKET'], '--bin-dir', str(rig.bin)]
                with patch.dict(os.environ, rig.env, clear=True), \
                        patch.object(supervisor, 'SCRIPTS', rig.gate_scripts), patch.object(sys, 'argv', argv), \
                        patch.object(supervisor, 'previous', previous), \
                        patch.object(supervisor, 'rollout_identity', return_value=supervisor.identity(os.getpid())), \
                        patch.object(supervisor, 'listening', return_value=True), \
                        patch.object(supervisor, 'read_health', return_value=dict(state='stopped')) as health, \
                        contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                    self.assertEqual(supervisor.main(), 4, stderr.getvalue())
                output = json.loads(stdout.getvalue())
                self.assertIn('candidate remains pending: interrupted', output['reason'])
                self.assertEqual(output['installed_pair'], supervisor.pair_record(rig.bin))
                self.assertEqual(output['installed_pair'] == before['stable']['pair'], restored)
                self.assertIn('Version: fixture', output['rollout']['stdout'])
                evidence = Path(output['evidence']['directory'])
                self.assertEqual(sorted(path.name for path in evidence.iterdir()), ['atm.log', 'atm.log.1'])
                if restored:
                    self.assertEqual(output['rollback']['returncode'], 0)
                    self.assertIn('previous build is running', output['rollback']['stdout'])
                    self.assertIn('stderr', output['rollback'])
                    self.assertIn('automatic rollback attempted, R exit 0', stderr.getvalue())
                else:
                    self.assertNotIn('rollback', output)
                    self.assertIn('scripts/atm-rollout --rollback', stderr.getvalue())
                after = rig.slots()
                self.assertIsNotNone(after['candidate'])
                self.assertEqual(after['rejected'], [])
                self.assertEqual(after['used_approvals'], [rig.approval])
                self.assertEqual(after['stable'], before['stable'])
                self.assertEqual(after['previous'], before['previous'])
                self.assertEqual(len(rig.rollout_trace()), expected_calls)
                self.assertEqual(health.call_count, 1)

    def test_automatic_recovery_native_output_names_R_result_and_restored_pair(self):
        import atm_supervisor as supervisor
        self.promote_setup()
        stdout, stderr = io.StringIO(), io.StringIO()
        argv = [str(self.gate_script), 'promote', self.candidate_id, '--approve', self.approval,
                '--socket', self.env['ATM_SOCKET'], '--bin-dir', str(self.bin)]
        with patch.dict(os.environ, self.env, clear=True), \
                patch.object(supervisor, 'SCRIPTS', self.gate_scripts), patch.object(sys, 'argv', argv), \
                patch.object(supervisor, 'rollout_identity', return_value=supervisor.identity(os.getpid())), \
                patch.object(supervisor, 'read_health', side_effect=[dict(state='stopped'), dict(state='unknown')]), \
                contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            self.assertEqual(supervisor.main(), 3)
        output = json.loads(stdout.getvalue())
        self.assertIn('previous build is running', stderr.getvalue())
        self.assertIn('Restored pair:', stderr.getvalue())
        self.assertIn(output['restored_pair']['atmd']['sha256'], stderr.getvalue())


class GatePassContracts(unittest.TestCase):
    @staticmethod
    def record(label, commit):
        # Synthetic: complete runner grammar with selectable literal label/head.
        return (f'gate start 01:02:03 head {commit} run dir sealed-run\n'
                f'{label} exit 0 after 123 s; log sealed-run/gate.log; head {commit}\n'
                f'{label}: GATE PASSED {commit} run 42 (sealed-run)\n')

    def test_gate_runner_label_refuses_regex_lookalikes(self):
        import atm_supervisor as supervisor
        # Omitting re.escape would accept this regex-lookalike runner.
        commit, label = 'a' * 40, 'gate.runner+[local]'
        with self.assertRaises(supervisor.Refused):
            supervisor.check_gate_pass(self.record('gateXrunnerrl', commit), commit, label)
        supervisor.check_gate_pass(self.record(label, commit), commit, label)

    def test_gate_runner_label_refuses_mixed_exit_and_pass_labels(self):
        import atm_supervisor as supervisor
        # Matching either label generically would combine different runners.
        commit, label = 'a' * 40, 'gate-runner'
        original = self.record(label, commit)
        for prefix in ('gate-runner exit', 'gate-runner: GATE'):
            with self.subTest(prefix=prefix), self.assertRaises(supervisor.Refused):
                changed = original.replace(prefix, prefix.replace(label, 'other-gate'))
                supervisor.check_gate_pass(changed, commit, label)

    def test_gate_runner_label_rejects_blank_or_multiline_values(self):
        import atm_supervisor as supervisor
        # Without validation a blank/multiline prefix can match a full record.
        commit = 'a' * 40
        for label in ('', ' \t', 'gate\nrunner', 'gate\rrunner'):
            with self.subTest(label=label), self.assertRaises(supervisor.Refused):
                supervisor.check_gate_pass(self.record(label, commit), commit, label)


class FakeClock:
    def __init__(self):
        self.now, self.waits = 0, []

    def clock(self):
        return self.now

    def wait(self, seconds):
        self.waits.append(seconds)
        self.now += seconds


class HealthContracts(unittest.TestCase):
    # Missing terminal branches, a wrong debounce or sleeping instead of injection
    # must change these observable verdicts/read times.
    def test_terminal_health_and_identity_fail_on_the_first_read(self):
        import atm_supervisor as supervisor
        for state in ('failed', 'stopped', 'refused', 'changed', 'dead'):
            with self.subTest(state=state):
                self.assertEqual(supervisor.health_verdict([dict(state=state)]), 'unhealthy')
        self.assertEqual(supervisor.health_verdict([
            dict(state='ok', identity=dict(pid=1, start='a')),
            dict(state='ok', identity=dict(pid=1, start='b'))]), 'unhealthy')

    def test_three_consecutive_invalid_reads_but_a_valid_read_resets_the_streak(self):
        import atm_supervisor as supervisor
        for state in ('missing', 'malformed'):
            with self.subTest(state=state):
                self.assertEqual(supervisor.health_verdict([dict(state=state)] * 2), 'pending')
                self.assertEqual(supervisor.health_verdict([dict(state=state)] * 3), 'unhealthy')
                self.assertEqual(supervisor.health_verdict([
                    dict(state=state), dict(state=state), dict(state='ok'),
                    dict(state=state), dict(state=state)]), 'pending')
        self.assertEqual(supervisor.health_verdict([dict(state='full')]), 'full')
        self.assertEqual(supervisor.health_verdict([dict(state='unknown')] * 4), 'unknown')
        self.assertEqual(supervisor.health_verdict([
            dict(state='ok'), *[dict(state='unknown')] * 3]), 'unhealthy')

    def test_probation_reads_immediately_then_every_15_seconds_including_the_end(self):
        import atm_supervisor as supervisor
        clock, times = FakeClock(), []
        def read():
            times.append(clock.now)
            return dict(state='ok')
        outcome = supervisor.probation(read, clock.clock, clock.wait, duration=30)
        self.assertEqual(outcome['result'], 'accepted')
        self.assertEqual(times, [0, 15, 30])
        self.assertEqual(clock.waits, [15, 15])
        self.assertEqual(len(outcome['reads']), 3)

    def test_full_all_probation_needs_you_but_transient_full_is_not_unhealthy(self):
        import atm_supervisor as supervisor
        for states, expected in ((['full'] * 3, 'needs-you'),
                                 (['full', 'ok', 'full'], 'accepted'),
                                 (['malformed'] * 3, 'unhealthy'),
                                 (['missing'] * 2, 'needs-you')):
            with self.subTest(states=states):
                clock = FakeClock()
                reads = iter(dict(state=state) for state in states)
                outcome = supervisor.probation(lambda: next(reads), clock.clock, clock.wait,
                                                duration=15 * (len(states) - 1))
                self.assertEqual(outcome['result'], expected)
        clock = FakeClock()
        result = supervisor.probation(lambda: dict(state='unknown'), clock.clock, clock.wait, duration=30)
        self.assertEqual(result['result'], 'accepted')
        self.assertIn('unknown', result['note'].lower())
        self.assertIn('rollout', result['note'].lower())

    @staticmethod
    def health(reservations='ok', lifecycle='ok'):
        return dict(type='response', id='health', result=dict(
            reservations=dict(state=reservations, reason='quarantine_overflow' if reservations == 'failed' else None,
                              live=0, quarantined=0, capacity=32),
            lifecycle=dict(state=lifecycle, capacity=32)))

    def test_unavailable_for_a_full_probation_is_not_legacy_unknown(self):
        import atm_supervisor as supervisor
        for code, expected in (('unavailable', 'unhealthy'), ('unknown_method', 'accepted')):
            with self.subTest(code=code):
                clock = FakeClock()
                frame = dict(type='response', id='health', error=dict(code=code, message='fixture'))
                outcome = supervisor.probation(lambda: dict(state=supervisor.health_state(frame)),
                                               clock.clock, clock.wait)
                self.assertEqual(outcome['result'], expected)
                self.assertEqual(len(outcome['reads']), 3 if code == 'unavailable' else 121)
                if code == 'unknown_method':
                    self.assertIn('unknown; accepted on rollout checks', outcome['note'])

    def test_trailing_invalid_reads_cannot_accept_an_earlier_healthy_probation(self):
        import atm_supervisor as supervisor
        for state in ('missing', 'malformed', 'unavailable'):
            with self.subTest(state=state):
                clock = FakeClock()
                reads = iter([dict(state='ok')] * 119 + [dict(state=state)] * 2)
                outcome = supervisor.probation(lambda: next(reads), clock.clock, clock.wait)
                self.assertEqual(outcome['result'], 'needs-you')
                self.assertEqual(outcome['reason'], 'health remained uncertain at the end of probation')
                self.assertEqual(len(outcome['reads']), 121)

    def test_health_wire_validation_distinguishes_legacy_unknown_from_unavailable(self):
        import atm_supervisor as supervisor
        for reservations, lifecycle, expected in (
                ('ok', 'ok', 'ok'), ('ok', 'absent', 'ok'), ('full', 'ok', 'full'),
                ('ok', 'full', 'full'), ('failed', 'full', 'failed'), ('full', 'stopped', 'stopped')):
            with self.subTest(reservations=reservations, lifecycle=lifecycle):
                self.assertEqual(supervisor.health_state(self.health(reservations, lifecycle)), expected)
        for code, expected in (('unknown_method', 'unknown'), ('unavailable', 'unavailable')):
            self.assertEqual(supervisor.health_state(dict(type='response', id='health',
                                                         error=dict(code=code, message='fixture'))), expected)
        for bad in (None, [], {}, dict(type='response', id='other', result={}),
                    dict(type='response', id='health', result=None),
                    dict(self.health(), error=dict(code='unavailable')),
                    self.health(lifecycle='future-state')):
            with self.subTest(bad=bad):
                self.assertEqual(supervisor.health_state(bad), 'malformed')
        bad = self.health()
        del bad['result']['reservations']['capacity']
        self.assertEqual(supervisor.health_state(bad), 'malformed')

    def test_legacy_unknown_method_socket_is_accepted_on_rollout_checks(self):
        import atm_supervisor as supervisor
        # Synthetic old-daemon stand-in: its method router has no daemon.health.
        with tempfile.TemporaryDirectory(prefix='health-') as temporary:
            path = Path(temporary) / 'old.sock'
            with socket.socket(socket.AF_UNIX) as server:
                server.bind(str(path))
                server.listen()
                server.settimeout(90)
                errors, requests = [], []
                def serve():
                    try:
                        for _ in range(3):
                            with server.accept()[0] as connection, connection.makefile('r') as stream:
                                connection.settimeout(90)
                                json.loads(stream.readline())
                                connection.sendall(b'{"type":"connected"}\n')
                                requests.append(json.loads(stream.readline()))
                                frame = dict(type='response', id='health', error=dict(
                                    code='unknown_method', message='unknown method: daemon.health'))
                                connection.sendall(json.dumps(frame).encode() + b'\n')
                    except BaseException as error:
                        errors.append(error)
                thread = threading.Thread(target=serve)
                thread.start()
                clock = FakeClock()
                try:
                    outcome = supervisor.probation(lambda: supervisor.read_health(path),
                                                   clock.clock, clock.wait, duration=30)
                finally:
                    thread.join(timeout=90)
                self.assertFalse(thread.is_alive())
                self.assertEqual(errors, [])
                self.assertEqual(outcome['result'], 'accepted')
                self.assertEqual(outcome['note'], 'health unknown; accepted on rollout checks')
                self.assertEqual([item['state'] for item in outcome['reads']], ['unknown'] * 3)
                self.assertEqual([item['method'] for item in requests], ['daemon.health'] * 3)

    def test_daemon_dying_during_a_missing_answer_is_unhealthy_on_that_read(self):
        import atm_supervisor as supervisor
        with tempfile.TemporaryDirectory(prefix='health-') as temporary:
            path = Path(temporary) / 'a.sock'
            with socket.socket(socket.AF_UNIX) as server:
                server.bind(str(path))
                server.listen()
                server.settimeout(30)
                errors = []
                def serve():
                    try:
                        with server.accept()[0] as connection, connection.makefile('r') as stream:
                            connection.settimeout(30)
                            stream.readline()
                            connection.sendall(b'{"type":"connected"}\n')
                            stream.readline()  # request received, then the daemon exits without an answer
                    except BaseException as error:
                        errors.append(error)
                thread = threading.Thread(target=serve)
                thread.start()
                pinned = supervisor.identity(os.getpid())
                identities = iter([pinned, pinned, None])
                try:
                    with patch.object(supervisor, 'identity', side_effect=lambda _: next(identities)):
                        result = supervisor.read_health(path, pinned)
                finally:
                    thread.join(timeout=30)
                self.assertFalse(thread.is_alive())
                self.assertEqual(errors, [])
                self.assertEqual(supervisor.health_verdict([result]), 'unhealthy')

    def test_bad_handshake_cannot_hide_daemon_exit(self):
        import atm_supervisor as supervisor
        with tempfile.TemporaryDirectory(prefix='health-') as temporary:
            path = Path(temporary) / 'a.sock'
            with socket.socket(socket.AF_UNIX) as server:
                server.bind(str(path))
                server.listen()
                server.settimeout(30)
                errors = []
                def serve():
                    try:
                        with server.accept()[0] as connection, connection.makefile('r') as stream:
                            connection.settimeout(30)
                            stream.readline()
                            connection.sendall(b'{"type":"error"}\n')
                    except BaseException as error:
                        errors.append(error)
                thread = threading.Thread(target=serve)
                thread.start()
                pinned = supervisor.identity(os.getpid())
                identities = iter([pinned, pinned, None])
                try:
                    with patch.object(supervisor, 'identity', side_effect=lambda _: next(identities)):
                        result = supervisor.read_health(path, pinned)
                finally:
                    thread.join(timeout=30)
                self.assertFalse(thread.is_alive())
                self.assertEqual(errors, [])
                self.assertEqual(supervisor.health_verdict([result]), 'unhealthy')

    def test_raw_health_probe_uses_the_peer_and_never_a_cli(self):
        import atm_supervisor as supervisor
        with tempfile.TemporaryDirectory(prefix='health-') as temporary:
            path = Path(temporary) / 'a.sock'
            with socket.socket(socket.AF_UNIX) as server:
                server.bind(str(path))
                server.listen()
                server.settimeout(30)
                errors, requests = [], []
                def serve():
                    try:
                        with server.accept()[0] as connection, connection.makefile('r') as stream:
                            connection.settimeout(30)
                            requests.append(json.loads(stream.readline()))
                            connection.sendall(b'{"type":"connected"}\n')
                            requests.append(json.loads(stream.readline()))
                            connection.sendall(json.dumps(self.health()).encode() + b'\n')
                    except BaseException as error:
                        errors.append(error)
                thread = threading.Thread(target=serve)
                thread.start()
                try:
                    result = supervisor.read_health(path, supervisor.identity(os.getpid()))
                finally:
                    thread.join(timeout=30)
                self.assertFalse(thread.is_alive())
                self.assertEqual(errors, [])
                self.assertEqual(result['state'], 'ok')
                self.assertEqual(result['identity']['pid'], os.getpid())
                self.assertEqual([item['type'] for item in requests], ['connect', 'request'])
                self.assertEqual(requests[1]['method'], 'daemon.health')
                self.assertEqual(requests[1]['id'], 'health')
                self.assertNotIn('from_pane', requests[1])
            self.assertEqual(supervisor.read_health(path)['state'], 'refused')


class RehearsalKit(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.cache = tempfile.TemporaryDirectory(prefix='rehearsal-bins-', dir='/var/tmp')
        cls.addClassCleanup(cls.cache.cleanup)
        cls.built = Path(cls.cache.name)
        for name in NAMES:
            source = Path(os.environ.get('CANDIDATE_TEST_' + name.upper(),
                                         str(SCRIPTS.parent / 'target/debug' / name)))
            if not source.is_file():
                raise AssertionError('Cargo-built pair required: ' + str(source))
            subprocess.run(['strip', '-o', cls.built / name, source], check=True, timeout=90)

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='rehearsal-test-', dir='/var/tmp')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.stable, self.candidate = self.root / 'stable', self.root / 'candidate'
        for folder in (self.stable, self.candidate):
            folder.mkdir(mode=0o700)
            for name in NAMES:
                shutil.copy2(self.built / name, folder / name)

    def run_rehearsal(self):
        import atm_supervisor
        rehearsal = getattr(atm_supervisor, 'rehearse', None)
        self.assertTrue(callable(rehearsal), 'two-way rehearsal is not implemented')
        result = rehearsal(self.stable, self.candidate)
        runtime = Path(result['runtime'])
        self.assertTrue(runtime.is_relative_to(Path('/var/tmp')))
        self.assertNotEqual(runtime, self.root)
        self.assertEqual(result['cleanup'], dict(clean=True, processes=0, atmd=0, tmux=0))
        # Count independently, rather than trusting the runtime's own cleanup report.
        survivors = []
        for entry in Path('/proc').iterdir():
            if not entry.name.isdecimal() or int(entry.name) == os.getpid():
                continue
            try:
                state = (entry / 'stat').read_text().rsplit(')', 1)[1].split()[0]
                environ = (entry / 'environ').read_bytes()
                if state != 'Z' and os.fsencode(runtime) in environ:
                    survivors.append(entry.name)
            except (FileNotFoundError, ProcessLookupError, PermissionError):
                continue
        self.assertEqual(survivors, [], result)
        self.assertFalse(runtime.exists(), result)
        return result

    def test_rehearsal_watch_error_keeps_the_failing_frame(self):
        import atm_supervisor
        runtime = atm_supervisor.Rehearsal(self.root)
        error_frame = dict(type='error', message='synthetic watch refusal')
        @contextlib.contextmanager
        def connection():
            left, right = socket.socketpair()
            with left, right, io.StringIO(json.dumps(dict(type='connected')) + '\n'
                                           + json.dumps(error_frame) + '\n') as stream:
                yield left, stream
        replies = [json.dumps(dict(agent_id='project/rehearsal', state='ready')),
                   json.dumps(dict(message_id='m9'))]
        error = None
        with (patch.object(runtime, 'connection', connection),
              patch.object(runtime, 'command', side_effect=replies)):
            try:
                runtime.sequence('stable', {})
            except (ValueError, atm_supervisor.Refused) as caught:
                error = caught
        self.assertIsInstance(error, atm_supervisor.Refused)
        self.assertIn('synthetic watch refusal', str(error))
        self.assertEqual(runtime.steps[-1]['name'], 'stable.receipt')
        self.assertEqual(runtime.steps[-1]['result'], error_frame)

    def test_rehearsal_interrupt_cleans_and_propagates_instead_of_passing_one_way(self):
        import atm_supervisor
        roots = []
        initialize = atm_supervisor.Rehearsal.__init__
        def remember(runtime, root):
            roots.append(root)
            initialize(runtime, root)
        with (patch.object(atm_supervisor.Rehearsal, '__init__', remember),
              patch.object(atm_supervisor.Rehearsal, 'command', side_effect=KeyboardInterrupt)):
            with self.assertRaises(KeyboardInterrupt):
                atm_supervisor.rehearse(self.stable, self.candidate)
        self.assertEqual(len(roots), 1)
        self.assertFalse(any(root.exists() for root in roots))

    # Missing reverse-read checks or separate state directories falsely promise rollback safety.
    def test_rehearsal_real_pair_is_two_way_and_cleans_every_owned_process(self):
        result = self.run_rehearsal()
        self.assertEqual(result['compatibility'], 'two-way', result)
        self.assertIsNone(result['failure'])
        steps = {step['name']: step for step in result['steps']}
        for phase in ('stable', 'candidate'):
            for action in ('up', 'message', 'receipt', 'idle', 'clear', 'down'):
                self.assertIn(phase + '.' + action, steps)
        first = json.loads(steps['stable.message']['result']['stdout'])['message_id']
        second = json.loads(steps['candidate.message']['result']['stdout'])['message_id']
        self.assertGreater(int(second.removeprefix('m')), int(first.removeprefix('m')))
        self.assertIsInstance(json.loads(steps['stable-read.list']['result']['stdout']), list)
        agents = json.loads(steps['stable-read.agents']['result']['stdout'])
        self.assertEqual([(row['agent_id'], row['state']) for row in agents], [('project/rehearsal', 'down')])

    def test_rehearsal_old_build_refusing_candidate_log_is_one_way(self):
        old = self.stable / 'atmd'
        old.write_text(textwrap.dedent('''\
            #!/usr/bin/env python3
            import json, os, sys
            from pathlib import Path
            events = Path(os.environ['ATM_STATE_DIR']) / 'log/events'
            up = sum(json.loads(line).get('kind') == 'agent.up_requested'
                     for path in events.glob('*.jsonl') for line in path.read_text().splitlines())
            if up >= 2:
                print('old build refuses newer log', file=sys.stderr)
                sys.exit(41)
            os.execv(REAL, [REAL, *sys.argv[1:]])
            ''').replace('REAL', repr(str(self.built / 'atmd'))))
        result = self.run_rehearsal()
        self.assertEqual(result['compatibility'], 'ONE-WAY')
        self.assertEqual(result['failure']['step'], 'stable-read.start')
        self.assertIn('old build refuses newer log', result['failure']['output'])
        failed = next(step for step in result['steps'] if step['name'] == 'stable-read.start')
        self.assertEqual(failed['result']['returncode'], 41)

    def test_rehearsal_failed_final_stable_stop_is_not_two_way(self):
        (self.stable / 'atmd').write_text(final_stable_failure(self.built / 'atmd'))
        result = self.run_rehearsal()  # independently verifies zero survivors/removal
        stops = {step['name']: step['result'] for step in result['steps'] if step['name'].endswith('.stop')}
        self.assertEqual(stops['stable.stop']['returncode'], 0)
        self.assertEqual(stops['candidate.stop']['returncode'], 0)
        self.assertEqual(stops['stable-read.stop']['returncode'], 44)
        self.assertEqual(result['compatibility'], 'ONE-WAY')
        self.assertEqual(result['failure']['step'], 'stable-read.stop')
        self.assertIn('stable reverse-read daemon failed', result['failure']['output'])

    def test_rehearsal_failed_daemon_stop_is_not_two_way(self):
        wrapper = self.candidate / 'atmd'
        wrapper.write_text(textwrap.dedent('''\
            #!/usr/bin/env python3
            import signal, subprocess, sys
            child = subprocess.Popen([REAL, *sys.argv[1:]])
            stopping = False
            def stop(*_):
                global stopping
                stopping = True
                child.send_signal(signal.SIGTERM)
            signal.signal(signal.SIGTERM, stop)
            code = child.wait(timeout=90)
            if stopping:
                print('candidate failed while stopping', file=sys.stderr)
                sys.exit(43)
            sys.exit(code)
            ''').replace('REAL', repr(str(self.built / 'atmd'))))
        result = self.run_rehearsal()
        self.assertEqual(result['compatibility'], 'ONE-WAY')
        self.assertEqual(result['failure']['step'], 'candidate.stop')
        self.assertIn('candidate failed while stopping', result['failure']['output'])

    def test_rehearsal_mid_sequence_failure_names_the_command_and_cleans(self):
        wrapper = self.candidate / 'atm'
        wrapper.write_text(textwrap.dedent('''\
            #!/usr/bin/env python3
            import os, sys
            if sys.argv[1:3] == ['agent', 'clear']:
                print('injected candidate clear failure', file=sys.stderr)
                sys.exit(42)
            os.execv(REAL, [REAL, *sys.argv[1:]])
            ''').replace('REAL', repr(str(self.built / 'atm'))))
        result = self.run_rehearsal()
        self.assertEqual(result['compatibility'], 'ONE-WAY')
        self.assertEqual(result['failure']['step'], 'candidate.clear')
        self.assertIn('injected candidate clear failure', result['failure']['output'])
        failed = next(step for step in result['steps'] if step['name'] == 'candidate.clear')
        self.assertEqual(failed['result']['returncode'], 42)
        self.assertNotIn('candidate.down', [step['name'] for step in result['steps']])


# Reuse R's private binary/root/process rig; new synchronization is bind/EOF,
# committed delivery/subscription and process-exit events, never a settling delay.
import rollout_kit_test as rollout_tests


class PromoteRig(rollout_tests.Rig):
    def test_real_rollout_accepts_supported_health_and_preserves_the_state_prefix(self):
        self.real_promotion()

    def test_real_probation_rollback_preserves_state(self):
        self.real_promotion(recover=True)

    def test_real_dead_candidate_refuses_recovery_without_restart(self):
        self.real_promotion(recover='dead')

    def real_promotion(self, recover=False):
        import atm_supervisor as supervisor
        scripts = self.repo / 'scripts'
        for name in supervisor.KIT_FILES:
            shutil.copy2(SCRIPTS / name, scripts / name)
        support = self.repo / 'tests/support'
        support.mkdir(parents=True)
        (support / 'stand_in').symlink_to(SCRIPTS.parent / 'tests/support/stand_in', target_is_directory=True)
        kit_root = self.root / 'kit'
        kit_root.mkdir(mode=0o700)
        (kit_root / '.atm-candidate-owner').write_text(str(os.getuid()))
        installs = kit_root / 'installs'
        installs.mkdir(mode=0o700)
        for key, folder in (('ROOT', 'kit'), ('STATE_ROOT', 'kit-state'),
                            ('BUILD_ROOT', 'kit-build'), ('RUNTIME_ROOT', 'kit-run')):
            self.env['ATM_CANDIDATE_' + key] = str(self.root / folder)
        for commit, source in ((self.before, self.bin), (self.after, self.candidate / 'bin')):
            install = installs / (commit + '-debug')
            (install / 'bin').mkdir(mode=0o700, parents=True)
            install.chmod(0o700)
            for name in NAMES:
                shutil.copy2(source / name, install / 'bin' / name)
            (install / 'install.json').write_text(json.dumps(dict(
                id=install.name, sha=commit, profile='debug', prebuilt_provenance_unverified=False,
                hashes={name: digest(install / 'bin' / name) for name in NAMES})))
        self.start_tmux()
        ready, bound, logs = threading.Event(), [], []
        daemon = subprocess.Popen([self.bin / 'atmd', 'start'], env=self.env, stdout=subprocess.PIPE,
                                  stderr=subprocess.STDOUT, text=True, start_new_session=True)
        def read_log():
            for line in daemon.stdout:
                logs.append(line)
                if 'Daemon server listening' in line and str(self.socket) in line:
                    bound.append(True)
                    ready.set()
            daemon.wait(timeout=90)  # reap foreground stable as soon as R stops it
            ready.set()
        reader = threading.Thread(target=read_log, daemon=True)
        reader.start()
        def close_foreground():
            if daemon.poll() is None:
                self.stop_everything()
            daemon.wait(timeout=90)
            reader.join(timeout=90)
            self.assertFalse(reader.is_alive())
            daemon.stdout.close()
        self.addCleanup(close_foreground)
        self.assertTrue(ready.wait(90), 'private daemon readiness event')
        self.assertTrue(bound, ''.join(logs))
        runtime = supervisor.Rehearsal(self.root)
        runtime.env = self.env
        # One committed real task makes a nonempty state prefix without a model
        # or timing-dependent startup delivery. R still runs its genuine pi smoke.
        with runtime.connection() as (connection, stream):
            self.assertEqual(json.loads(stream.readline())['type'], 'connected')
            connection.sendall(json.dumps(dict(protocol_version=dict(major=1, minor=1),
                type='request', id='seed', method='task.create', params=dict(
                    title='Preserve this history', measure=dict(kind='mechanical', command='true')))).encode() + b'\n')
            frame = json.loads(stream.readline())
            self.assertIn('result', frame, frame)
        prefix = self.events()
        self.assertTrue(prefix)
        if recover:
            # Real foreground daemon output supplies the pre-rotation diagnostic.
            (self.root / 'state/atm.log').write_text(''.join(logs))
        with patch.dict(os.environ, self.env, clear=True), patch.object(supervisor, 'SCRIPTS', scripts):
            manager = supervisor.Supervisor(self.bin, supervisor.Kit(read_only=True))
            manager.metadata.mkdir(mode=0o700, exist_ok=True)
            baseline = manager.init(self.before, self.before + '-debug', 'fixture')
            kit, kit_hash = supervisor.retain_kit(manager.metadata)
            name = self.after + '-debug'
            _, binaries, manifest_hash = manager.installation(name)
            steps = [dict(name=step, command=['synthetic gate evidence'], result={})
                     for step in ('manifest', 'replay', 'frame_proof', 'gate-pass', 'rehearsal')]
            # Synthetic approval/provenance: upstream gate contracts test those steps.
            # This case executes the genuine R dry-run, swap, checks and raw probation.
            step = supervisor.gate_step(steps, 'rollout', [sys.executable, scripts / 'atm-rollout', installs / name,
                '--dry-run', '--bin-dir', self.bin, '--socket', self.socket, '--installed-commit', self.before])
            dry = supervisor.gate_call(step, self.env, self.repo, 300)
            self.assertEqual(dry.returncode, 0, dry.stdout + dry.stderr)
            step['result'].update(supervisor.rollout_evidence(dry.stdout, self.before, self.after))
            for item in steps:
                item['sha256'] = supervisor.checksum(item)
            report = dict(install=name, commit=self.after, pair=binaries, install_manifest_sha256=manifest_hash,
                          kit=kit, kit_sha256=kit_hash, report_kind='gate', result='passed',
                          compatibility='two-way', replaces=baseline['stable'], steps=steps)
            path = manager.metadata / 'reports/real-rollout.json'
            supervisor.save(path, report)
            clock = FakeClock()
            calls, reads = [], []
            original = supervisor.gate_call
            def rollout(step, *args):
                calls.append(step['command'])
                if '--rollback' in step['command']:
                    evidence = list((manager.metadata / 'evidence').glob('probation-*/*'))
                    self.assertEqual(sorted(file.name for file in evidence), ['atm.log', 'atm.log.1'])
                return original(step, *args)
            def health(pinned):
                reads.append(pinned)
                # Synthetic failed trigger, or a real exited daemon; in either
                # case R and the restored daemon's health remain real.
                if recover and len(reads) == 1:
                    if recover == 'dead':
                        self.checked([self.bin / 'atmd', 'stop'])
                        return supervisor.read_health(self.socket, pinned)
                    return dict(state='failed')
                return supervisor.read_health(self.socket, pinned)
            with patch.object(supervisor, 'gate_call', rollout):
                code, output = manager.promote(manager.read(), name, digest(path), self.socket, read=health,
                                               clock=clock.clock, wait=clock.wait, duration=30)
            if recover == 'dead':
                self.assertEqual(code, 4, output)
                self.assertEqual(output['probation']['reads'][0]['state'], 'dead')
                self.assertEqual(len(calls), 3)
                self.assertEqual(output['rollback']['returncode'], 1)
                self.assertIn('no daemon answers', output['rollback']['stderr'])
                self.assertIn(' start -d', output['manual_recovery'])
                self.assertEqual(len(reads), 1)
                self.assertEqual(len(calls), 3)
                self.assertEqual(sum('--rollback' in command for command in calls), 1)
                self.assertFalse(rollout_tests.atm_rollout.listening(self.socket))
                rejected = supervisor.load(manager.path)['slots']
                self.assertEqual(rejected['stable'], baseline['stable'])
                self.assertEqual(rejected['previous'], baseline['previous'])
                self.assertIsNone(rejected['candidate'])
                self.assertEqual(len(rejected['rejected']), 1)
                self.assertTrue(self.events().startswith(prefix))
                return
            self.assertEqual(code, 3 if recover else 0, output)
            accepted = manager.read()
            self.assertIsNone(accepted['candidate'])
            if recover:
                self.assertEqual(code, 3, output)
                self.assertEqual(output['result'], 'rolled-back')
                self.assertEqual(output['health']['state'], 'ok')
                self.assertEqual(output['restored_pair'], baseline['stable']['pair'])
                self.assertEqual(accepted['stable'], baseline['stable'])
                self.assertEqual(accepted['previous'], baseline['previous'])
                self.assertEqual(len(accepted['rejected']), 1)
                self.assertEqual(len(calls), 3)
                self.assertEqual(sum('--rollback' in command for command in calls), 1)
                self.assertEqual(len(reads), 2)
                self.assertNotEqual(reads[0], reads[1])
                json.loads(self.checked([self.bin / 'atm', 'agent', 'list', '--format', 'json']))
                json.loads(self.checked([self.bin / 'atm', 'list', '--format', 'json']))
                with self.assertRaisesRegex(supervisor.Refused, 'already used'):
                    manager.promote(accepted, name, digest(path), self.socket)
                self.assertEqual(len(calls), 3)
            else:
                self.assertEqual(code, 0, output)
                self.assertEqual(output['result'], 'accepted')
                self.assertEqual(output['probation']['note'], 'health probation passed')
                self.assertEqual([item['state'] for item in output['probation']['reads']], ['ok'] * 3)
                self.assertEqual(accepted['previous'], baseline['stable'])
                self.assertEqual(accepted['stable']['install'], name)
                self.assertEqual(len(output['probation']['reads']), 3)
            pid = self.daemon_pid()
            code, checked = manager.check(accepted, self.socket)
            self.assertEqual(code, 0)
            self.assertEqual(checked['verdict'], 'healthy')
            self.assertEqual(self.daemon_pid(), pid)
        self.assertTrue(self.events().startswith(prefix))


class SlotTransitions(unittest.TestCase):
    def setUp(self):
        import atm_supervisor
        self.supervisor = atm_supervisor
        self.stable = self.record('a', '1', 'f', kind='baseline')
        self.candidate = self.record('b', '3', 'c')
        self.previous = self.record('9', '7', 'e')
        self.state = dict(version=1, stable=self.stable, previous=self.previous,
                          candidate=None, rejected=[], used_approvals=[])

    @staticmethod
    def record(commit, pair_hash, report_hash, kind='gate'):
        return dict(commit=commit * 40, install=commit * 40 + '-release',
                    pair={'atm': {'sha256': pair_hash * 64, 'mode': 0o755},
                          'atmd': {'sha256': '2' * 64, 'mode': 0o755}},
                    kit='kits/' + 'd' * 64, kit_sha256='d' * 64,
                    install_manifest_sha256='0' * 64,
                    report='reports/' + report_hash + '.json',
                    report_sha256=report_hash * 64, report_kind=kind)

    # A gate/approval or rejection rotating stable loses the last-known-good pair.
    def test_staging_does_not_rotate_or_mutate_input(self):
        before = copy.deepcopy(self.state)
        candidate_before = copy.deepcopy(self.candidate)
        staged = self.supervisor.stage_candidate(self.state, self.candidate, 'c' * 64)
        self.assertEqual(staged['stable'], self.stable)
        self.assertEqual(staged['previous'], self.previous)
        self.assertEqual(staged['candidate'], self.candidate)
        self.assertEqual(staged['used_approvals'], ['c' * 64])
        self.assertEqual(self.state, before)
        staged['candidate']['pair']['atm']['mode'] = 0o700
        self.assertEqual(self.candidate, candidate_before)

    def test_accept_is_the_only_rotation_point(self):
        self.state['candidate'] = self.candidate
        self.state['used_approvals'] = ['c' * 64]
        before = copy.deepcopy(self.state)
        accepted = self.supervisor.accept_candidate(self.state)
        self.assertEqual(accepted['stable'], self.candidate)
        self.assertEqual(accepted['previous'], self.stable)
        self.assertIsNone(accepted['candidate'])
        self.assertEqual(accepted['used_approvals'], ['c' * 64])
        self.assertEqual(accepted['rejected'], [])
        self.assertEqual(self.state, before)
        accepted['previous']['pair']['atm']['mode'] = 0o700
        self.assertEqual(self.state, before)

    def test_reject_records_reason_and_evidence_without_rotation(self):
        self.state['candidate'] = self.candidate
        before = copy.deepcopy(self.state)
        evidence = {'log': 'evidence/failed.log'}
        rejected = self.supervisor.reject_candidate(self.state, 'lifecycle stopped', evidence)
        self.assertEqual(rejected['stable'], self.stable)
        self.assertEqual(rejected['previous'], self.previous)
        self.assertIsNone(rejected['candidate'])
        self.assertEqual(rejected['rejected'],
                         [dict(self.candidate, reason='lifecycle stopped', evidence=evidence)])
        self.assertEqual(self.state, before)
        rejected['rejected'][0]['evidence']['log'] = 'changed'
        self.assertEqual(evidence, {'log': 'evidence/failed.log'})

    def test_reject_keeps_last_three_but_never_discards_used_approvals(self):
        state = self.state
        for commit, pair_hash, report_hash in (('b', '3', '1'), ('c', '4', '2'),
                                                ('d', '5', '3'), ('e', '6', '4')):
            record = self.record(commit, pair_hash, report_hash)
            state = self.supervisor.stage_candidate(state, record, report_hash * 64)
            state = self.supervisor.reject_candidate(state, 'failed', {})
        self.assertEqual([item['commit'] for item in state['rejected']],
                         ['c' * 40, 'd' * 40, 'e' * 40])
        self.assertEqual(state['used_approvals'], ['1' * 64, '2' * 64, '3' * 64, '4' * 64])
        self.assertEqual(state['stable'], self.stable)
        self.assertEqual(state['previous'], self.previous)
        with self.assertRaisesRegex(self.supervisor.Refused, 'used'):
            self.supervisor.stage_candidate(state, self.record('f', '8', '1'), '1' * 64)

    def test_a_second_candidate_is_refused(self):
        self.state['candidate'] = self.candidate
        with self.assertRaises(self.supervisor.Refused):
            self.supervisor.stage_candidate(self.state, self.record('c', '4', 'e'), 'e' * 64)

    def test_an_approval_must_match_the_new_gate_report(self):
        with self.assertRaises(self.supervisor.Refused):
            self.supervisor.stage_candidate(self.state, self.candidate, 'e' * 64)

    def test_baseline_report_hash_can_never_be_an_approval(self):
        with self.assertRaisesRegex(self.supervisor.Refused, 'baseline'):
            self.supervisor.stage_candidate(self.state, self.stable, 'f' * 64)
        disguised = copy.deepcopy(self.candidate)
        disguised['report_sha256'] = 'f' * 64
        with self.assertRaisesRegex(self.supervisor.Refused, 'baseline'):
            self.supervisor.stage_candidate(self.state, disguised, 'f' * 64)
        self.assertEqual(self.state['used_approvals'], [])

    def test_reused_approval_is_refused(self):
        self.state['used_approvals'] = ['c' * 64]
        with self.assertRaisesRegex(self.supervisor.Refused, 'used'):
            self.supervisor.stage_candidate(self.state, self.candidate, 'c' * 64)

    def test_rejected_commit_and_pair_require_a_new_approved_report(self):
        self.state['rejected'] = [dict(self.candidate, reason='failed', evidence={})]
        renamed = copy.deepcopy(self.candidate)
        renamed['install'] = 'b' * 40 + '-debug'
        with self.assertRaisesRegex(self.supervisor.Refused, 'rejected'):
            self.supervisor.stage_candidate(self.state, renamed, 'c' * 64)
        new_report = copy.deepcopy(renamed)
        new_report.update(report='reports/new.json', report_sha256='e' * 64)
        staged = self.supervisor.stage_candidate(self.state, new_report, 'e' * 64)
        self.assertEqual(staged['candidate'], new_report)
        self.assertEqual(staged['stable'], self.stable)

    def test_reject_requires_a_reason_and_structured_evidence(self):
        self.state['candidate'] = self.candidate
        before = copy.deepcopy(self.state)
        for reason, evidence in (('', {}), (' ', {}), ('failed', None)):
            with self.subTest(reason=reason, evidence=evidence):
                with self.assertRaises(self.supervisor.Refused):
                    self.supervisor.reject_candidate(self.state, reason, evidence)
                self.assertEqual(self.state, before)

    def test_accept_and_reject_without_a_candidate_are_refused(self):
        with self.assertRaises(self.supervisor.Refused):
            self.supervisor.accept_candidate(self.state)
        with self.assertRaises(self.supervisor.Refused):
            self.supervisor.reject_candidate(self.state, 'failed', {})


if __name__ == '__main__':
    unittest.main()
