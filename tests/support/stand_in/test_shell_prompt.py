"""A real PTY pins the shell-prompt event's terminal acknowledgment boundary."""
import json
import os
from pathlib import Path
import pty
import select
import socket
import subprocess
import tempfile
import unittest


class ShellPromptTests(unittest.TestCase):
    def test_prompt_event_waits_for_cursor_reply_and_names_exited_process(self):
        with tempfile.TemporaryDirectory() as directory, socket.socket(socket.AF_UNIX) as listener:
            root = Path(directory)
            identity = {"pane": "%1", "shell_for_pid": 123, "shell_for_start_time": "456"}
            (root / "shell-%1.json").write_text(json.dumps(identity))
            listener.bind(str(root / "events.sock"))
            listener.listen()
            master, slave = pty.openpty()
            child = subprocess.Popen(
                ["python3", str(Path(__file__).with_name("shell_prompt.py"))],
                stdin=slave, stdout=slave, stderr=subprocess.PIPE,
                env=dict(os.environ, ATM_STANDIN_ROOT=directory, TMUX_PANE="%1",
                         ATM_STANDIN_EVENTS=str(root / "events.sock")))
            os.close(slave)
            try:
                output = b""
                while not output.endswith(b"\x1b[6n"):
                    self.assertTrue(select.select([master], [], [], 30)[0], "prompt output")
                    output += os.read(master, 4096)
                self.assertEqual(output, b"stand-in-shell$ \x1b[6n")
                # The prompt bytes are not evidence of tmux parsing them yet.
                listener.setblocking(False)
                with self.assertRaises(BlockingIOError):
                    listener.accept()
                os.write(master, b"\x1b[1;17R")
                listener.settimeout(30)
                stream, _ = listener.accept()
                with stream, stream.makefile("r") as events:
                    stream.settimeout(30)
                    self.assertEqual(json.loads(events.readline()), dict(identity, kind="shell_prompt"))
                _, stderr = child.communicate(timeout=30)
                self.assertEqual(child.returncode, 0, stderr)
            finally:
                if child.poll() is None:
                    child.kill()
                child.communicate(timeout=30)
                os.close(master)


if __name__ == "__main__":
    unittest.main()
