#!/usr/bin/env python3
"""Report protected net tree changes; changed and clean reports both exit zero."""
import argparse
from fnmatch import fnmatchcase
import json
import os
from pathlib import Path
import subprocess
import sys

MANIFEST = 'scripts/protected-files.txt'


def git(*args):
    return subprocess.check_output(
        ['git', *args], env={k: v for k, v in os.environ.items() if not k.startswith('GIT_')},
        stderr=subprocess.PIPE, timeout=30)


def patterns(text):
    result = set()
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith('#'):
            continue
        if line.startswith(('/', '!')) or any(part in ('', '.', '..') for part in line.split('/')):
            raise ValueError('invalid protected pattern: ' + repr(line))
        result.add(line)
    if not result:
        raise ValueError('empty protected-files list')
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('before', nargs='?', default='origin/main')
    parser.add_argument('after', nargs='?', default='HEAD')
    args = parser.parse_args()
    try:
        before, after = (git('rev-parse', '--verify', '--end-of-options', ref + '^{commit}')
                         .decode('ascii').strip() for ref in (args.before, args.after))
        rules = patterns(Path(__file__).with_name('protected-files.txt').read_text())
        for revision in (before, after):
            # An absent historical list is normal before this tool was introduced.
            if git('ls-tree', '-z', revision, '--', MANIFEST):
                rules.update(patterns(git('show', revision + ':' + MANIFEST).decode('utf-8')))
        fields = git('diff', '--no-ext-diff', '--no-textconv', '--name-status', '-z',
                     '--no-renames', before, after, '--').split(b'\0')
        if fields.pop() != b'' or len(fields) % 2:
            raise ValueError('invalid NUL-delimited Git diff')
        entries = iter(fields)
        changes = [(status.decode('ascii'), os.fsdecode(path)) for status, path in zip(entries, entries)
                   if any(fnmatchcase(os.fsdecode(path), rule) for rule in rules)]
        print(f'Comparing {before}..{after}')
        for status, path in changes:
            print(status + '\t' + json.dumps(path))
        print(f'{len(changes)} protected change(s)')
        return 0
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f'protected-files: {error}', file=sys.stderr)
        return 1


if __name__ == '__main__':
    sys.exit(main())
