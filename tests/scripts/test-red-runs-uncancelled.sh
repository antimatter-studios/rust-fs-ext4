#!/usr/bin/env bash
# A pull request's red run is never cancelled by the push that adds its fix.
#
# A pull request opens holding only its failing test, with a title that starts
# "red:", and the CI run of that commit is the red evidence. The fix is pushed
# to the same branch after the title loses "red:". ci.yml cancels a pull
# request's run when a newer push arrives, so without an exception the fix push
# would cancel the red run, often before it has failed, and the evidence would
# be gone. This holds the exception: while the title starts "red:" the run gets
# a concurrency group of its own commit and is never cancelled, and every other
# pull request run still cancels the one it supersedes.
#
# Keyed by the commit, not only by the pull request: GitHub keeps one pending
# run per group even with cancelling off, so a second push to a red: pull
# request sharing the first one's group would replace a red run still waiting
# for a runner.
#
# The concurrency expressions are evaluated here as GitHub evaluates them
# (startsWith ignores case; && and || return an operand; null reads as '';
# format fills {0}, {1} ...),
# for each event the workflow runs on, so a reworded expression is judged by
# what it does, not by how it reads.
#
#   bash tests/scripts/test-red-runs-uncancelled.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

fail() { echo "FAIL  $*" >&2; exit 1; }

command -v python3 >/dev/null 2>&1 || fail "python3 is required"
python3 -c 'import yaml' 2>/dev/null ||
    fail "the python3 yaml module is required (pip install pyyaml)"

python3 - "$REPO/.github/workflows/ci.yml" <<'PY' || exit 1
import re, sys, yaml

conc = (yaml.safe_load(open(sys.argv[1])) or {}).get("concurrency")
if not isinstance(conc, dict) or "group" not in conc or "cancel-in-progress" not in conc:
    sys.exit("FAIL  ci.yml has no concurrency group and cancel-in-progress")

def lookup(ctx, path):
    for key in path.split("."):
        ctx = ctx.get(key) if isinstance(ctx, dict) else None
    return ctx

def starts_with(s, prefix):
    return str("" if s is None else s).lower().startswith(str(prefix).lower())

def fill(template, *args):
    return re.sub(r"\{(\d+)\}", lambda m: text(args[int(m.group(1))]), template)

def evaluate(expr, ctx):
    py = re.sub(r"\bstartsWith\(", "_sw(", expr)
    py = re.sub(r"\bformat\(", "_fmt(", py)
    py = py.replace("&&", " and ").replace("||", " or ")
    py = re.sub(r"!(?!=)", " not ", py)
    py = re.sub(r"\bgithub((?:\.[A-Za-z_]+)+)", lambda m: "_g(%r)" % m.group(1)[1:], py)
    return eval(py, {"__builtins__": {}, "_sw": starts_with, "_fmt": fill, "_g": lambda p: lookup(ctx, p)})

def text(value):
    if value is None:
        return ""
    if isinstance(value, bool):
        return "true" if value else "false"
    return str(value)

def render(template, ctx):
    template = str(template)
    if template in ("true", "false"):
        return template
    return re.sub(r"\$\{\{(.*?)\}\}", lambda m: text(evaluate(m.group(1), ctx)), template)

def run(event_name, title=None, head="a" * 40):
    ctx = {"workflow": "CI", "event_name": event_name, "sha": head[::-1],
           "ref": "refs/heads/main", "ref_name": "main", "head_ref": "", "event": {}}
    if title is not None:
        ctx.update(ref="refs/pull/7/merge", ref_name="7/merge", head_ref="fix/run-end-bound")
        ctx["event"] = {"pull_request": {"number": 7, "title": title, "head": {"sha": head}}}
    return render(conc["group"], ctx), render(conc["cancel-in-progress"], ctx) == "true"

red_group, red_cancels = run("pull_request", "red: the run-end bound is checked")
red_again, _ = run("pull_request", "red: the run-end bound is checked", head="b" * 40)
fix_group, fix_cancels = run("pull_request", "the run-end bound is checked", head="c" * 40)
fix_again, _ = run("pull_request", "the run-end bound is checked", head="d" * 40)
problems = []
if red_cancels:
    problems.append("a red: pull request's run cancels the run before it")
if red_group == red_again:
    problems.append("two pushes to a red: pull request share group %r, so the second replaces the red run while it waits" % red_group)
if not fix_cancels or fix_group != fix_again:
    problems.append("an ordinary pull request run no longer cancels the run it supersedes")
if red_group == fix_group:
    problems.append("the red run and the fix push share group %r, so the fix push cancels the red run" % fix_group)
for event in ("push", "merge_group"):
    if run(event)[1]:
        problems.append("a %s run is cancelled by a newer one" % event)
if problems:
    sys.exit("\n".join("FAIL  " + p for p in problems))
PY

echo "red runs: a red: pull request's run is never cancelled; other pull request runs still are"
