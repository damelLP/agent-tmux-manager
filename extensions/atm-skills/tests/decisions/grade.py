"""Grades one decision-test run from its stand-in log and transcript.

Usage: python3 grade.py <case-dir> <run-out-dir>

Each grader file in <case-dir>/graders/ is checked by the function named after
it below (its file name without the number). The result goes to
<run-out-dir>/grading.json in skill-creator's format: `text`, `passed` and
`evidence` per assertion. A grader with no function here is reported as not
passed, with evidence saying it needs a reading by hand.
"""

import json
import os
import re
import sys

WRITES = {"send", "reply", "interrupt", "kill", "spawn"}
MARKER = re.compile(r"^[\s*_`>]*(QUESTION|BLOCKED|WAITING|DONE):")
CLEAR = re.compile(r"^\s*/(clear|new)\s*$")


class Run:
    def __init__(self, out):
        self.calls = []
        with open(os.path.join(out, "outputs", "atm.log")) as f:
            for line in f:
                if line.strip():
                    self.calls.append(json.loads(line))
        self.events = []
        with open(os.path.join(out, "transcript.jsonl")) as f:
            for line in f:
                try:
                    self.events.append(json.loads(line))
                except json.JSONDecodeError:
                    pass
        self.tool_uses = [
            c
            for e in self.events
            if e.get("type") == "assistant"
            for c in e.get("message", {}).get("content", [])
            if c.get("type") == "tool_use"
        ]
        self.texts = [
            c.get("text", "")
            for e in self.events
            if e.get("type") == "assistant"
            for c in e.get("message", {}).get("content", [])
            if c.get("type") == "text"
        ]
        result = next((e for e in self.events if e.get("type") == "result"), {})
        self.final = result.get("result") or (self.texts[-1] if self.texts else "")
        self.created = []
        created_dir = os.path.join(out, "outputs", "created")
        for root, _, files in os.walk(created_dir):
            for name in files:
                self.created.append(os.path.relpath(os.path.join(root, name), created_dir))

    def writes(self, pane):
        return [c for c in self.calls if c["argv"] and c["argv"][0] in WRITES and c.get("pane") == pane]

    def sends(self, pane):
        return [" ".join(c["argv"][2:]) for c in self.writes(pane) if c["argv"][0] == "send"]

    def first_index(self, pred):
        for i, t in enumerate(self.tool_uses):
            if pred(t):
                return i
        return None


def mentions(tool, needle):
    return needle in json.dumps(tool.get("input", {}))


def is_atm_send(tool, pane):
    """A Bash call that sends to the pane (not `atm send --help`, not a peek of it)."""
    cmd = tool.get("input", {}).get("command", "") if tool.get("name") == "Bash" else ""
    return re.search(r"atm send\s+['\"]?" + re.escape(pane) + r"\b", cmd) is not None


def quote(s, n=160):
    s = " ".join(s.split())
    return s if len(s) <= n else s[: n - 1] + "…"


# --- checks, one per grader file name -------------------------------------


def no_clear(r):
    clears = [s for s in r.sends("%7") if CLEAR.match(s)]
    return not clears, f"sends to %7: {[quote(s, 80) for s in r.sends('%7')] or 'none'}"


def reads_plan_first(r):
    read = r.first_index(lambda t: mentions(t, "alpha-3-plan.md") and not is_atm_send(t, "%7"))
    send = r.first_index(lambda t: is_atm_send(t, "%7"))
    ok = read is not None and (send is None or read < send)
    return ok, f"first read of the plan at tool call {read}, first send to %7 at {send}"


def answers_plan(r):
    answers = [s for s in r.sends("%7") if not CLEAR.match(s)]
    return bool(answers), f"sends to %7: {[quote(s, 120) for s in answers] or 'none'}"


def reads_own_context(r):
    text = "\n".join(r.texts)
    hit = re.search(r"(4[0-9]|[5-9][0-9])(\.\d+)?\s*%[^\n]{0,80}", text)
    near = hit and re.search(r"context", text[max(0, hit.start() - 200) : hit.end() + 200], re.I)
    return bool(near), f"mention: {quote(hit.group(0)) if hit else 'no percentage of 40 or more in the replies'}"


def writes_handover(r):
    new = [p for p in r.created if p.startswith("handovers/") and p != "handovers/handover-1.md"]
    return bool(new), f"files created: {r.created or 'none'}"


def sends_skill_call(r):
    calls = [s for s in r.sends("%7") if "/atm:implement" in s and "alpha-5.md" in s]
    return bool(calls), f"sends to %7: {[quote(s, 120) for s in r.sends('%7')] or 'none'}"


def no_second_clear(r):
    return no_clear(r)


def no_skill_call(r):
    calls = [s for s in r.sends("%7") if "/atm:implement" in s]
    return not calls, f"sends to %7: {[quote(s, 120) for s in r.sends('%7')] or 'none'}"


def asks_for_marker(r):
    writes = r.writes("%7")
    if not writes:
        return False, "nothing was sent to %7"
    first = writes[0]["argv"]
    text = " ".join(first[2:]) if first[0] == "send" else ""
    named = [m for m in ("QUESTION", "BLOCKED", "WAITING", "DONE") if m in text]
    return first[0] == "send" and len(named) >= 2, f"first write to %7: {quote(' '.join(first), 200)}"


def nothing_sent(r):
    writes = r.writes("%7")
    return not writes, f"writes to %7: {[quote(' '.join(w['argv']), 100) for w in writes] or 'none'}"


def tells_user(r):
    final = r.final
    ok = "%7" in final and bool(re.search(r"\bpi\b|gadgets|another|different|no longer|doesn.t match|mismatch", final, re.I))
    return ok, f"final message: {quote(final, 300)}"


def last_line_marker(r):
    lines = [l for l in r.final.splitlines() if l.strip()]
    last = lines[-1] if lines else ""
    return bool(MARKER.match(last)), f"last line: {quote(last, 200) or '(empty)'}"


def no_dialog(r):
    used = [t["name"] for t in r.tool_uses if t.get("name") in ("AskUserQuestion", "EnterPlanMode", "ExitPlanMode")]
    return not used, f"dialog tools called: {used or 'none'}"


def main(case_dir, out):
    r = Run(out)
    expectations = []
    gdir = os.path.join(case_dir, "graders")
    for name in sorted(os.listdir(gdir)):
        with open(os.path.join(gdir, name)) as f:
            text = f.read().split("---", 2)[-1].strip()
        fn = globals().get(re.sub(r"^\d+-", "", name[:-3]).replace("-", "_"))
        if fn is None:
            expectations.append({"text": text, "passed": False, "evidence": "no check in grade.py: read it by hand"})
            continue
        passed, evidence = fn(r)
        expectations.append({"text": text, "passed": bool(passed), "evidence": evidence})
    passed = sum(e["passed"] for e in expectations)
    tools = {}
    for t in r.tool_uses:
        tools[t["name"]] = tools.get(t["name"], 0) + 1
    grading = {
        "expectations": expectations,
        "summary": {
            "passed": passed,
            "failed": len(expectations) - passed,
            "total": len(expectations),
            "pass_rate": round(passed / len(expectations), 2) if expectations else 0,
        },
        "execution_metrics": {
            "tool_calls": tools,
            "total_tool_calls": len(r.tool_uses),
            "atm_calls": len(r.calls),
            "output_chars": len(r.final),
        },
    }
    with open(os.path.join(out, "grading.json"), "w") as f:
        json.dump(grading, f, indent=2)
    return grading


if __name__ == "__main__":
    g = main(sys.argv[1], sys.argv[2])
    print(json.dumps(g["summary"]))
