"""Free tests for the decision-test tooling, with no model call: grade.py
grades only a run that finished, and run.sh starts each run with fresh
outputs in a reused out dir.

Run: PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s evals/atm-skills -p 'test_*.py'
"""

import json
import os
from pathlib import Path
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


class NamedWakeGrading(unittest.TestCase):
    """A trace must reply successfully to the displayed target, then ack its wake."""

    def expects_reply_and_ack(self, calls):
        with tempfile.TemporaryDirectory() as tmp:
            out = os.path.join(tmp, "out")
            graders = os.path.join(tmp, "case", "graders")
            os.makedirs(graders)
            with open(os.path.join(graders, "01-named-replies-and-acks.md"), "w") as f:
                f.write("Reply once using the hint, then ack the delivered wake.\n")
            write_run(out, 0, FINISHED)
            with open(os.path.join(out, "outputs", "atm.log"), "w") as f:
                f.writelines(json.dumps(call) + "\n" for call in calls)
            return grade.main(os.path.dirname(graders), out)["expectations"][0]

    def test_wrong_id_order_duplicates_or_failed_mutation_do_not_pass(self):
        read = {"argv": ["inbox", "--id", "m47"], "rc": 0,
                "delivery": {"status": "delivered", "target": ["--reply", "m41"]}}
        reply = {"argv": ["post", "--reply", "m41", "--body", "Read."], "rc": 0}
        ack = {"argv": ["ack", "m47"], "rc": 0}
        good = self.expects_reply_and_ack([read, reply, ack])
        self.assertTrue(good["passed"], good["evidence"])
        for calls in (
            [], [read, ack, reply], [read, reply], [read, reply, reply, ack],
            [read, reply, ack, ack], [read, {**reply, "rc": 1}, ack],
            [read, {**reply, "argv": ["post", "--reply", "m90", "--body", "Read."]}, ack],
            [read, reply, {**ack, "argv": ["ack", "m90"]}],
        ):
            with self.subTest(calls=calls):
                self.assertFalse(self.expects_reply_and_ack(calls)["passed"])

    def test_pre_read_queued_wake_must_not_be_acked(self):
        read = {"argv": ["inbox", "--id", "m47"], "rc": 0,
                "delivery": {"status": "queued", "target": ["--reply", "m41"]}}
        reply = {"argv": ["post", "--reply", "m41", "--body", "Read."], "rc": 0}
        self.assertTrue(self.expects_reply_and_ack([read, reply])["passed"])
        # The real CLI can cancel queued delivery by acking; success is still wrong here.
        self.assertFalse(self.expects_reply_and_ack([read, reply, {"argv": ["ack", "m47"], "rc": 0}])["passed"])

    def test_direct_hint_does_not_authorize_a_cross_channel_thread_reply(self):
        read = {"argv": ["inbox", "--id", "m47"], "rc": 0,
                "delivery": {"status": "delivered", "target": ["--to", "other/lead"]}}
        reply = {"argv": ["post", "--to", "other/lead", "--body", "Read."], "rc": 0}
        ack = {"argv": ["ack", "m47"], "rc": 0}
        self.assertTrue(self.expects_reply_and_ack([read, reply, ack])["passed"])
        reply["argv"][1:3] = ["--reply", "m41"]
        self.assertFalse(self.expects_reply_and_ack([read, reply, ack])["passed"])


class NamedFixtureDriver(unittest.TestCase):
    """Run the real case packaging, stand-in CLI and graders with a fake driver."""

    DRIVER = r'''#!/usr/bin/python3
import json, os, shlex, subprocess, sys
from pathlib import Path

prompt, staged = sys.argv[1:]
top = Path.cwd().parent
assert Path(os.environ["HOME"]) == top / "home"
assert Path(os.environ["ATM_SOCKET"]) == top / "s" / "atm.sock"
assert "TMUX" not in os.environ
assert set(p.name for p in Path(staged).iterdir()) == {"atm-member", "atm-lead"}

def event(kind, **content):
    print(json.dumps({"type": "assistant", "message": {"content": [
        {"type": "tool_use", "name": kind, "id": "t" + str(event.count), "input": content}
    ]}}))
    event.count += 1

event.count = 0

def atm(*args):
    event("Bash", command=shlex.join(["atm", *args]))
    return subprocess.check_output(["atm", *args], text=True)

if prompt.startswith("/atm-skills:atm-member\n"):
    assert (Path(staged) / "atm-member/references/atm-named-agents.md").is_file()
    assert "Agent: project/member;" in atm("me")
    assert "[m47] Read: atm inbox --id m47" in prompt
    wake = atm("inbox", "--id", "m47")
    attach = Path(next(line.removeprefix("attach: ") for line in wake.splitlines() if line.startswith("attach: ")))
    event("Read", file_path=str(attach))
    body = attach.read_text().strip().split("with: ")[1]
    hint = next(shlex.split(line) for line in wake.splitlines() if line.startswith("reply: "))
    reply = hint[hint.index("--reply") + 1]
    assert "thread=m41" in atm("post", "--reply", reply, "--body", body)
    assert "1 marked read" in atm("ack", "m47")
else:
    assert prompt.startswith("/atm-skills:atm-lead\n")
    assert "Agent: project/lead;" in atm("me")
    attach = Path.cwd() / "briefs/dispatch.md"
    assert attach.is_file()
    assert "woke: project/member" in atm("post", "--kind", "brief", "--body", "Read the attached brief",
                                         "--tag", "project/member", "--attach", str(attach), "--task", "unit-1")
print(json.dumps({"type": "result", "subtype": "success", "result": "Done."}))
'''

    def test_two_named_cases_stage_the_named_skills_and_grade_actual_calls(self):
        skills = Path(HERE).parents[1] / "crates/atm/assets/atm-skills"
        for family, name in [("atm-member", "read-reply-ack"), ("atm-lead", "dispatch-tagged-brief")]:
            case = Path(HERE) / family / name
            with self.subTest(name=name), tempfile.TemporaryDirectory() as tmp:
                driver = Path(tmp) / "driver"
                driver.write_text(self.DRIVER)
                driver.chmod(0o755)
                out = Path(tmp) / "out"
                result = subprocess.run(
                    ["bash", os.path.join(HERE, "run.sh"), case, skills, out],
                    env={**os.environ, "ATM_EVAL_NAMED_DRIVER": str(driver)},
                    timeout=60, capture_output=True,
                )
                self.assertEqual(result.returncode, 0, (result.stderr, (out / "stderr.txt").read_text()))
                report = grade.main(case, out)
                self.assertEqual(report["summary"]["pass_rate"], 1.0, report["expectations"])
                # Even a rejected legacy call disqualifies a read/reply/ack trace.
                with (out / "outputs/atm.log").open("a") as log:
                    log.write(json.dumps({"argv": ["msg", "send"], "rc": 2}) + "\n")
                self.assertEqual(grade.main(case, out)["summary"]["failed"], 1)

    def test_named_case_without_driver_cannot_run_billed_fallback(self):
        with tempfile.TemporaryDirectory() as tmp:
            case = Path(HERE) / "atm-member/read-reply-ack"
            env = {**os.environ, "HOME": tmp, "ATM_EVAL_NAMED_DRIVER": ""}
            result = subprocess.run(["bash", os.path.join(HERE, "run.sh"), case, HERE,
                                     Path(tmp) / "out"], env=env, timeout=60, capture_output=True)
            self.assertEqual(result.returncode, 64)
            self.assertIn(b"named cases require ATM_EVAL_NAMED_DRIVER", result.stderr)


if __name__ == "__main__":
    unittest.main()
