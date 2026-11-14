"""Actual hook-child ancestry, held at Bash startup rather than a timing window."""
import os
from pathlib import Path
import select
import socket
import tempfile
import threading
import unittest
from unittest.mock import patch

from test_model import make
from test_runtime import runtime


class HookTests(unittest.TestCase):
    def test_real_hook_bash_is_direct_child_of_harness_process(self):
        # BASH_ENV is a private observation barrier, not a replacement hook. It
        # reports the actual Bash PID/PPID before running the real forwarder.
        for mode in ["claude", "codex"]:
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                report, release = root / "report", root / "release"
                os.mkfifo(report)
                os.mkfifo(release)
                report_fd = os.open(report, os.O_RDWR | os.O_NONBLOCK)
                release_fd = os.open(release, os.O_RDWR | os.O_NONBLOCK)
                barrier = root / "bash-env"
                barrier.write_text('printf "%s %s\\n" "$$" "$PPID" > "$HOOK_REPORT"\nread -r _ < "$HOOK_RELEASE"\n')
                errors = []
                original_comm = Path("/proc/self/comm").read_text().strip()
                r = runtime()
                r.mode, r.root = mode, root
                payload = next(e["data"] for e in make(mode).announce() if e["kind"] == "event")
                with socket.socket(socket.AF_UNIX) as listener, patch.dict(os.environ, {
                    "ATM_SOCKET": str(root / "hook.sock"), "BASH_ENV": str(barrier),
                    "HOOK_REPORT": str(report), "HOOK_RELEASE": str(release),
                    "TMUX_PANE": "%fixture", "PATH": "/usr/local/bin:/usr/bin:/bin",
                    "HOME": str(root), "TMPDIR": str(root),
                    "ATM_STATE_DIR": str(root / "state"),
                }, clear=True):
                    listener.bind(str(root / "hook.sock"))
                    listener.listen()
                    def forward():
                        try:
                            r.forward(payload)
                        except Exception as error:
                            errors.append(error)
                    thread = threading.Thread(target=forward)
                    try:
                        Path("/proc/self/comm").write_text(mode)
                        thread.start()
                        self.assertTrue(select.select([report_fd], [], [], 30)[0], "hook startup barrier")
                        child, parent = map(int, os.read(report_fd, 4096).split())
                        stat = Path(f"/proc/{child}/stat").read_text().rsplit(")", 1)[1].split()
                        self.assertEqual(int(stat[1]), parent)
                        self.assertEqual(parent, os.getpid(), "U5 hook must not have an intermediate sh parent")
                    finally:
                        os.write(release_fd, b"continue\n")
                        thread.join(30)
                        Path("/proc/self/comm").write_text(original_comm)
                        os.close(report_fd)
                        os.close(release_fd)
                    self.assertFalse(thread.is_alive())
                    self.assertEqual(errors, [])


if __name__ == "__main__":
    unittest.main()
