"""Free tests for the decision-test tooling, with no model call: grade.py
grades only a run that finished, and run.sh starts each run with fresh
outputs in a reused out dir.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s evals/atm-skills -p 'test_*.py'
"""

import json
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import grade  # noqa: E402

HOLDS = os.path.join(HERE, "atm-coordinate", "clear-same-id-holds")
PLAN = os.path.join(HERE, "atm-coordinate", "done-plan-no-clear")
CONTEXT = os.path.join(HERE, "atm-coordinate", "context-over-40")
FINISHED = {"type": "result", "subtype": "success", "is_error": False, "result": "Holding."}


def write_run(out, exit_code, result):
    """The files run.sh leaves for grade.py: an empty stand-in log, the
    transcript (just `result`, if any) and timing.json."""
    os.makedirs(os.path.join(out, "outputs"), exist_ok=True)
    open(os.path.join(out, "outputs", "atm.log"), "w").close()
    with open(os.path.join(out, "transcript.jsonl"), "w") as f:
        if result:
            f.write(json.dumps(result) + "\n")
    with open(os.path.join(out, "timing.json"), "w") as f:
        json.dump({"exit_code": exit_code}, f)


class GradeOnlyFinishedRuns(unittest.TestCase):
    """In clear-same-id-holds, sending nothing is the right decision, so a
    run in which the model never ran would pass if it were graded."""

    def grade(self, exit_code, result):
        with tempfile.TemporaryDirectory() as out:
            write_run(out, exit_code, result)
            return grade.main(HOLDS, out)

    def test_a_runner_that_failed_passes_nothing(self):
        g = self.grade(127, None)
        self.assertEqual(g["summary"]["passed"], 0)
        self.assertIn("run.sh exited 127", g["expectations"][0]["evidence"])

    def test_a_model_run_that_never_finished_passes_nothing(self):
        g = self.grade(0, None)
        self.assertEqual(g["summary"]["passed"], 0)
        g = self.grade(0, {"type": "result", "subtype": "error_max_turns", "is_error": True})
        self.assertEqual(g["summary"]["passed"], 0)

    def test_a_finished_run_is_graded(self):
        g = self.grade(0, FINISHED)
        self.assertEqual(g["summary"]["passed"], 1, g["expectations"])


class PlanReadNeedsItsContent(unittest.TestCase):
    """done-plan-no-clear's reads-plan-first: the plan's content must be read
    before the send to %7; a mention of its name (`ls -l`) isn't a read."""

    def reads_plan_first(self, reads, read_failed=False):
        events = [
            {"type": "assistant", "message": {"content": [
                {"type": "tool_use", "id": "t1", **reads}]}},
            {"type": "user", "message": {"content": [
                {"type": "tool_result", "tool_use_id": "t1", "is_error": read_failed}]}},
            {"type": "assistant", "message": {"content": [
                {"type": "tool_use", "id": "t2", "name": "Bash",
                 "input": {"command": "atm send %7 'OK, go ahead.'"}}]}},
            FINISHED,
        ]
        with tempfile.TemporaryDirectory() as out:
            write_run(out, 0, None)
            with open(os.path.join(out, "transcript.jsonl"), "w") as f:
                f.write("".join(json.dumps(e) + "\n" for e in events))
            with open(os.path.join(out, "outputs", "atm.log"), "w") as f:
                f.write(json.dumps({"argv": ["send", "%7", "OK, go ahead."], "pane": "%7"}) + "\n")
            g = grade.main(PLAN, out)
        return next(e for e in g["expectations"] if "read before" in e["text"])

    def bash(self, command):
        return {"name": "Bash", "input": {"command": command}}

    def test_a_listing_is_not_a_read(self):
        self.assertFalse(self.reads_plan_first(self.bash("ls -l @RUN@/briefs/alpha-3-plan.md"))["passed"])

    def test_a_read_that_failed_is_not_a_read(self):
        read = {"name": "Read", "input": {"file_path": "@RUN@/briefs/alpha-3-plan.md"}}
        self.assertFalse(self.reads_plan_first(read, read_failed=True)["passed"])

    def test_cat_and_read_are_reads(self):
        for reads in (
            self.bash("echo '== plan'; cat @RUN@/briefs/alpha-3-plan.md; echo"),
            {"name": "Read", "input": {"file_path": "@RUN@/briefs/alpha-3-plan.md"}},
        ):
            e = self.reads_plan_first(reads)
            self.assertTrue(e["passed"], (reads, e["evidence"]))


class HandoverNeedsItsContent(unittest.TestCase):
    """context-over-40's grader asks for a handover naming alpha's pane,
    harness, project_root and task state, not just a new file."""

    def handover_expectation(self, content):
        with tempfile.TemporaryDirectory() as out:
            write_run(out, 0, FINISHED)
            created = os.path.join(out, "outputs", "created", "handovers")
            os.makedirs(created)
            with open(os.path.join(created, "handover-2.md"), "w") as f:
                f.write(content)
            g = grade.main(CONTEXT, out)
        return next(e for e in g["expectations"] if "handover file" in e["text"])

    def test_an_empty_handover_fails(self):
        self.assertFalse(self.handover_expectation("")["passed"])

    def test_a_handover_like_the_last_one_passes(self):
        # The case's own handover, as a coordinator wrote it before.
        with open(os.path.join(CONTEXT, "files", "work", "handovers", "handover-1.md")) as f:
            e = self.handover_expectation(f.read())
        self.assertTrue(e["passed"], e["evidence"])


class RunStartsFresh(unittest.TestCase):
    """run.sh twice into one out dir, with a stand-in `claude` (HOME points
    at a temp home): files the first run created must not stay for the
    second run's graders."""

    def test_a_reused_out_dir_keeps_no_earlier_outputs(self):
        with tempfile.TemporaryDirectory() as tmp:
            bin_dir = os.path.join(tmp, "home", ".local", "bin")
            os.makedirs(bin_dir)
            flag = os.path.join(tmp, "write-handover")
            stub = os.path.join(bin_dir, "claude")
            with open(stub, "w") as f:
                f.write(
                    "#!/bin/sh\n"
                    f"[ -e '{flag}' ] && echo new > handovers/handover-2.md\n"
                    f"echo '{json.dumps(FINISHED)}'\n"
                )
            os.chmod(stub, 0o755)
            out = os.path.join(tmp, "out")
            env = {**os.environ, "HOME": os.path.join(tmp, "home")}
            created = os.path.join(out, "outputs", "created", "handovers", "handover-2.md")

            def run():
                subprocess.run(
                    ["bash", os.path.join(HERE, "run.sh"), CONTEXT, os.path.join(tmp, "skills"), out],
                    env=env, check=True, capture_output=True, timeout=60,
                )

            os.makedirs(os.path.join(tmp, "skills", "atm-coordinate"))
            os.makedirs(os.path.join(tmp, "skills", "atm-implement"))
            for skill in ("atm-coordinate", "atm-implement"):
                with open(os.path.join(tmp, "skills", skill, "SKILL.md"), "w") as f:
                    f.write(f"---\nname: {skill}\ndescription: stand-in\n---\n")
            open(flag, "w").close()
            run()
            self.assertTrue(os.path.exists(created), "the first run should create a handover")
            os.remove(flag)
            run()
            self.assertFalse(os.path.exists(created), "the first run's handover stayed in outputs/")

            # A rerun that stops early (here, no skills) leaves nothing of
            # the earlier run for grade.py to grade. In clear-same-id-holds a
            # finished run that sent nothing passes, so stale files would.
            def holds(skills):
                return subprocess.run(
                    ["bash", os.path.join(HERE, "run.sh"), HOLDS, skills, out],
                    env=env, capture_output=True, timeout=60,
                )

            self.assertEqual(holds(os.path.join(tmp, "skills")).returncode, 0)
            self.assertEqual(grade.main(HOLDS, out)["summary"]["passed"], 1)
            self.assertEqual(holds(os.path.join(tmp, "missing")).returncode, 64)
            g = grade.main(HOLDS, out)
            self.assertEqual(g["summary"]["passed"], 0, g["expectations"])


if __name__ == "__main__":
    unittest.main()
