#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Run the v0.1 gate scenario end to end, and print a transcript of it.

    scripts/v01-gate.py --atomic target/release/atomic
    scripts/v01-gate.py --atomic target/debug/atomic --keep target/v01-gate

One developer runs the order-report example (examples/03-order-report): an exec
atom totals the order, a lite atom writes a note about the total, a gate waits
for a person to accept the report, and an exec atom renders it. The scenario:

1. `atomic run` starts the run under a fixed id and is killed with SIGKILL in
   the middle of a step: while the lite atom's model call is in flight.
2. `atomic runs resume` recovers it, and is killed with SIGKILL while the run
   waits at its gate.
3. `atomic runs resume` recovers it again, and a second invocation,
   `atomic gates approve`, approves the gate with the operator's enrolled
   credential. The run finishes.
4. `atomic eval --live` records the lite atom's answer, and `atomic eval`
   replays it: both green.

It then checks, from what the store recorded (`atomic runs show`, `atomic gates
list` and the usage ledger in the project's database), that the run finished
under the id it started with; that no completed step ran again (each step was
recorded by one execution, each exec command ran once, and the model was called
again only for the step the first kill cut off); that what the run used is
within the caps it started with, which did not change; and that the gate kept
its id, opening time and deadline across the kill and was decided before that
deadline.

The model is a scripted fake served on a loopback port by this script, so the
scenario needs no provider and no network, and it counts every call. The
binary under test is an ordinary build of atomic, with no test features. On
Linux, exec atoms run in atomic's sandbox, so the sandbox must work where this
runs (inside scripts/dev.sh, with ATOMIC_DEV_SANDBOX=on). Everything the run
writes goes under a throwaway directory in the checkout's target/, removed at
the end unless `--keep` names one. It is never under the system's temporary
directory, where atomic refuses to put an atom's scratch: a work directory
there is refused, whether `--keep` names it or it is the checkout's own
`target/`, which is under it when the checkout is.

However the scenario ends, every atomic it started is killed and waited for
before its directory is removed: on success, on a failed check, on an error,
on Ctrl-C, and on SIGTERM or SIGHUP, which end it as SystemExit. A SIGHUP the
caller already had ignored, as `nohup` leaves it, stays ignored and does not
stop the scenario.

Exits 0 when every check holds; 1 when any fails; 2 when the scenario could not
be set up or run, or what atomic recorded is not in the shape this script
reads; and 128 plus the signal's number when SIGTERM, or a SIGHUP that was not
already ignored, stops it.
"""

import argparse
import datetime
import http.server
import json
import os
import pty
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
EXAMPLE = os.path.join(ROOT, "examples", "03-order-report")
RUN_ID = "v01-gate"
WORKFLOW = "order-report"
# The caps the run starts under, written into the project's atomic.yaml. One
# model call reserves 80,000 tokens with the default call bound, and the call
# the first kill cuts off may stay charged at that bound, so two calls must fit.
TOKENS_CAP = 200_000
MINUTES_CAP = 20
WAIT = 180  # seconds any one wait may take before the scenario fails
# The ledger's model name for an exec command's metered time.
WORK_MODEL = "(command)"

# What in the caller's environment would change what atomic does, or where
# it sends what it records: every variable with one of these prefixes, and
# these by name. OTEL_ goes too, so a developer's collector, whose headers can
# carry a key, never hears of the scenario. Others still pass through, such as
# PATH, RUST_LOG, KUBECONFIG, TMPDIR and the proxy variables.
CLEARED_PREFIXES = ("ATOMIC_", "CODEX_", "OPENAI_", "DBOS__", "XDG_", "OTEL_")
CLEARED = ("DATABASE_URL",)
# The checkout's own scratch, where the scenario works by default.
TARGET = os.path.join(ROOT, "target")


def fixture(atom, which):
    with open(os.path.join(EXAMPLE, "fixtures", atom, f"ordinary.{which}.json")) as f:
        return json.load(f)


# What the lite atom answers: the example's own golden fixture.
ANSWER = fixture("assess-order", "output")


class Failed(Exception):
    """The scenario could not go on."""


class Unexpected(Exception):
    """What atomic recorded is not in the shape this script reads."""


def say(line=""):
    print(line, flush=True)


def quoted(text):
    return "\n".join(f"  | {line}" for line in text.splitlines())


# ---------------------------------------------------------------------------
# The fake model


class Fake:
    """An OpenAI-compatible endpoint on a loopback port that answers every
    model call with ANSWER and counts the calls. While `hold` is set, it holds
    the first call it gets unanswered, so atomic can be killed with it in
    flight."""

    def __init__(self):
        self.calls = 0
        self.answered = 0
        self.hold = threading.Event()
        self.held = threading.Event()  # set once a call is held
        self.release = threading.Event()
        self.lock = threading.Lock()
        fake = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):
                pass

            def do_GET(self):
                self.reply(404, {})

            def do_POST(self):
                length = int(self.headers.get("Content-Length") or 0)
                body = json.loads(self.rfile.read(length) or b"null") or {}
                path = self.path.split("?")[0]
                chat = path.endswith("/chat/completions")
                if not (chat or path.endswith("/responses")):
                    self.reply(404, {})
                    return
                with fake.lock:
                    fake.calls += 1
                    held = fake.hold.is_set() and not fake.held.is_set()
                    if held:
                        fake.held.set()
                if held:
                    # Never answered: the caller kills the asker meanwhile.
                    fake.release.wait(WAIT)
                    return
                text = json.dumps(ANSWER)
                model = body.get("model", "synthetic")
                if chat:
                    out = {
                        "id": "chatcmpl-fake", "object": "chat.completion", "model": model,
                        "choices": [{"index": 0, "finish_reason": "stop",
                                     "message": {"role": "assistant", "content": text}}],
                        "usage": {"prompt_tokens": 120, "completion_tokens": 30,
                                  "total_tokens": 150},
                    }
                else:
                    out = {
                        "id": "resp-fake", "object": "response", "status": "completed",
                        "model": model,
                        "output": [{"type": "message", "id": "msg-fake", "role": "assistant",
                                    "status": "completed",
                                    "content": [{"type": "output_text", "text": text,
                                                 "annotations": []}]}],
                        "usage": {"input_tokens": 120, "output_tokens": 30,
                                  "total_tokens": 150},
                    }
                with fake.lock:
                    fake.answered += 1
                self.reply(200, out)

            def reply(self, status, doc):
                data = json.dumps(doc).encode()
                try:
                    self.send_response(status)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                except OSError:
                    pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    @property
    def base_url(self):
        return f"http://127.0.0.1:{self.server.server_address[1]}/v1"

    def count(self):
        with self.lock:
            return self.calls, self.answered

    def stop(self):
        self.release.set()
        self.server.shutdown()
        self.server.server_close()


# ---------------------------------------------------------------------------
# Running atomic


class Scenario:
    def __init__(self, atomic, work):
        self.atomic = atomic
        self.project = os.path.join(work, "project")
        self.home = os.path.join(work, "home")
        self.logs = os.path.join(work, "logs")
        self.env = {
            k: v for k, v in os.environ.items()
            if k not in CLEARED and not k.startswith(CLEARED_PREFIXES)
        }
        self.env.update({
            "HOME": self.home,
            "ATOMIC_HOME": os.path.join(work, "atomic-home"),
            "XDG_CONFIG_HOME": os.path.join(self.home, ".config"),
            "USER": "operator",
            # No local Ollama is probed, whatever runs on this machine.
            "ATOMIC_DETECT_OLLAMA_ADDR": "127.0.0.1:9",
            # The operator's credential is kept in a directory only they can
            # read: a container has no OS keyring.
            "ATOMIC_CREDENTIAL_STORE": "file",
            # And no telemetry is sent, whatever else is set.
            "OTEL_SDK_DISABLED": "true",
        })
        self.passed = []
        self.failed = []
        # Every atomic started in the background, and its log, so none is
        # left running, or writing, when the scenario ends however it ends.
        self.procs = []
        self.log_files = []

    def run(self, args, quiet=False, stdin=subprocess.DEVNULL):
        """`atomic <args>` to completion. Fails the scenario if it fails."""
        out = subprocess.run(
            [self.atomic, *args], cwd=self.project, env=self.env, stdin=stdin,
            capture_output=True, text=True, timeout=WAIT,
        )
        if not quiet or out.returncode != 0:
            say(f"$ atomic {' '.join(args)}")
            say(f"  exit {out.returncode}")
            if out.stdout + out.stderr:
                say(quoted(out.stdout + out.stderr))
        if out.returncode != 0:
            raise Failed(f"atomic {' '.join(args)} exited {out.returncode}")
        return out

    def spawn(self, name, args):
        """`atomic <args>` in the background, its output in logs/<name>.log."""
        say(f"$ atomic {' '.join(args)} &")
        log = open(os.path.join(self.logs, f"{name}.log"), "w")
        self.log_files.append(log)
        proc = subprocess.Popen(
            [self.atomic, *args], cwd=self.project, env=self.env,
            stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT,
        )
        self.procs.append(proc)
        return proc

    def reap(self):
        """Kill every background atomic still running, wait for each, and
        close their logs: done before the work directory is removed."""
        for proc in self.procs:
            if proc.poll() is None:
                proc.kill()
            try:
                proc.wait(10)
            except subprocess.TimeoutExpired:
                say(f"v01-gate: atomic {proc.pid} did not stop after SIGKILL")
        for log in self.log_files:
            log.close()

    def log(self, name):
        with open(os.path.join(self.logs, f"{name}.log")) as f:
            return f.read()

    def kill(self, proc, name, why):
        proc.send_signal(signal.SIGKILL)
        code = proc.wait(WAIT)
        say(f"  SIGKILL {why}: exit {code}")
        if self.log(name):
            say(quoted(self.log(name)))

    def show(self):
        return json.loads(self.run(["--json", "runs", "show", RUN_ID], quiet=True).stdout)

    def gates(self):
        out = self.run(["gates", "list", "--all", "--json"], quiet=True)
        return [g for g in json.loads(out.stdout) if g.get("run") == RUN_ID]

    def database(self):
        private = os.path.join(self.env["ATOMIC_HOME"], "private")
        found = [os.path.join(private, d, "atomic.db") for d in sorted(os.listdir(private))]
        found = [p for p in found if os.path.exists(p)]
        if len(found) != 1:
            raise Failed(f"expected one project database under {private}, found {found}")
        return found[0]

    def ledger(self):
        """The run's allowance and its attempts, as the usage ledger has
        them. Read only while no atomic process of this scenario runs."""
        db = sqlite3.connect(f"file:{self.database()}?mode=ro", uri=True)
        try:
            db.row_factory = sqlite3.Row
            allowance = db.execute(
                "SELECT * FROM atomic_usage_allowances WHERE scope = ?", (f"run:{RUN_ID}",)
            ).fetchone()
            attempts = db.execute(
                "SELECT request_id, attempt, model, state, reserved_tokens, charged_tokens, "
                "charged_millis FROM atomic_usage_attempts WHERE run = ? "
                "ORDER BY admitted_at, request_id, attempt", (RUN_ID,)
            ).fetchall()
        finally:
            db.close()
        if allowance is None:
            raise Failed(f"the usage ledger has no allowance for run {RUN_ID}")
        return dict(allowance), [dict(a) for a in attempts]

    def runs(self):
        """Every run the project's store holds, by id."""
        db = sqlite3.connect(f"file:{self.database()}?mode=ro", uri=True)
        try:
            return sorted(r[0] for r in db.execute("SELECT workflow_uuid FROM workflow_status"))
        finally:
            db.close()

    def check(self, clause, ok, evidence):
        say(f"  [{'PASS' if ok else 'FAIL'}] {clause}: {evidence}")
        (self.passed if ok else self.failed).append(clause)


def wait_until(what, test, proc, log):
    """Poll `test` until it gives something true. Fail after WAIT seconds, or
    as soon as `proc` exits."""
    started = time.monotonic()
    while True:
        got = test()
        if got:
            return got
        if proc.poll() is not None:
            raise Failed(f"atomic exited {proc.returncode} before {what}:\n{log()}")
        if time.monotonic() - started > WAIT:
            raise Failed(f"timed out waiting for {what}")
        time.sleep(0.1)


def enroll(s):
    """`atomic gates enroll`, a person's command that refuses to run with no
    terminal: its standard input is a pseudo-terminal here."""
    leader, follower = pty.openpty()
    try:
        s.run(["gates", "enroll"], stdin=follower)
    finally:
        os.close(follower)
        os.close(leader)


def prepare(s, fake):
    shutil.copytree(EXAMPLE, s.project)
    for d in (s.home, s.logs):
        os.makedirs(d)
    with open(os.path.join(s.project, "atomic.yaml"), "a") as f:
        f.write(
            "model:\n"
            f"  base_url: {fake.base_url}\n"
            "  name: synthetic\n"
            "budget:\n"
            "  workflow:\n"
            f"    tokens: {TOKENS_CAP}\n"
            f"    minutes: {MINUTES_CAP}\n"
        )


def when(ms):
    """A time the store recorded, milliseconds since the Unix epoch, as UTC,
    or "none" for none. The gate store keeps times as such integers, not as
    text."""
    if ms is None:
        return "none"
    if not isinstance(ms, int) or isinstance(ms, bool):
        raise Unexpected(f"a recorded time is {ms!r}, not milliseconds since the epoch")
    try:
        return datetime.datetime.fromtimestamp(ms / 1000, datetime.timezone.utc).isoformat()
    except (OverflowError, ValueError, OSError) as e:
        raise Unexpected(f"a recorded time, {ms}, is out of range: {e}") from e


def of(name):
    """The workflow step a checkpoint belongs to: `total` for `total.turn`."""
    return name.split(".")[0]


def open_gate(s):
    return next((g for g in s.gates() if g["resolution"] is None), None)


def scenario(s, fake):
    say("== setup")
    say(f"binary: {s.atomic}")
    s.run(["--version"])
    say("project: a copy of examples/03-order-report; its atomic.yaml names the fake "
        f"model at {fake.base_url} and caps the workflow at {TOKENS_CAP} tokens and "
        f"{MINUTES_CAP} minutes")
    s.run(["check"])
    enroll(s)
    say()

    say("== kill 1: mid-step, with the lite atom's model call in flight")
    fake.hold.set()
    first = s.spawn("run", ["run", WORKFLOW, "--run-id", RUN_ID,
                            "--input", "fixtures/total-order/ordinary.input.json"])
    wait_until("the model call", fake.held.is_set, first, lambda: s.log("run"))
    s.kill(first, "run", "with the model call unanswered")
    fake.hold.clear()
    fake.release.set()
    killed_1 = s.show()
    caps_1, _ = s.ledger()
    steps_1 = {st["name"]: st for st in killed_1["steps"]}
    s.check("killed mid-run",
            killed_1["status"] == "unfinished" and "total.publish" in steps_1
            and not any(of(name) == "assess" for name in steps_1),
            f"run {killed_1['run']} {killed_1['status']}; steps recorded: {list(steps_1)}")
    say()

    say("== kill 2: while the run waits at its gate")
    second = s.spawn("resume-1", ["runs", "resume", RUN_ID])
    gate = wait_until("the gate to open", lambda: open_gate(s), second,
                      lambda: s.log("resume-1"))
    # Waiting at it: still running, a moment after the gate opened.
    time.sleep(2)
    if second.poll() is not None:
        raise Failed(f"resume exited {second.returncode} at the gate:\n{s.log('resume-1')}")
    s.kill(second, "resume-1", "waiting at the gate")
    killed_2 = s.show()
    calls_at_gate = fake.count()
    s.check("killed at a waiting gate",
            killed_2["status"] == "unfinished" and gate["deadline"] is not None
            and open_gate(s) is not None,
            f"run {killed_2['run']} {killed_2['status']}; gate {gate['id']} at step "
            f"{gate['step']} open, opened {when(gate['opened_at'])}, deadline "
            f"{when(gate['deadline'])}")
    say()

    say("== recover, and approve the gate from a second invocation")
    third = s.spawn("resume-2", ["runs", "resume", RUN_ID])
    s.run(["gates", "list"])
    s.run(["gates", "approve", gate["id"]])
    code = third.wait(WAIT)
    say(f"  runs resume exited {code}")
    say(quoted(s.log("resume-2")))
    if code != 0:
        raise Failed(f"the recovering resume exited {code}")
    done = s.show()
    s.run(["runs", "show", RUN_ID])
    caps_end, ledger_end = s.ledger()
    decided = s.gates()
    stored = s.runs()
    say()

    say("== evals")
    before_eval = fake.count()
    s.run(["eval", "--live"])
    live = fake.count()
    s.run(["eval"])
    after_eval = fake.count()
    say()

    say("== checks")
    expected = fixture("render-report", "output")
    s.check("finished", done["status"] == "succeeded" and done["output"] == expected,
            f"status {done['status']}, output {json.dumps(done['output'])}")
    s.check("under the same run id",
            killed_1["run"] == killed_2["run"] == done["run"] == RUN_ID
            and stored == [RUN_ID],
            f"{killed_1['run']} at kill 1, {killed_2['run']} at kill 2, "
            f"{done['run']} when it finished; the store holds these runs: {stored}")
    executions = [(a["attempt"], a["ran"], a["replayed"]) for a in done["attempts"]]
    s.check("recovered with atomic runs resume, twice", len(done["attempts"]) == 3,
            f"executions (attempt, steps it ran, steps it replayed): {executions}")
    # Each workflow step's checkpoints, `<step>.<part>`, by the execution
    # that recorded them. The gate's wait for its decision, durare's own
    # receive, belongs to no step.
    by = {}
    for st in done["steps"]:
        if st["name"] != "DBOS.recv":
            by.setdefault(of(st["name"]), set()).add(st["attempt"])
    names = [st["name"] for st in done["steps"]]
    expected_by = {"pin": {1}, "total": {1}, "assess": {2}, "accept-report": {2, 3},
                   "render": {3}, "DBOS": {2}}
    unchanged = all(
        next((d for d in done["steps"] if d["name"] == name), {}).get("completed_at")
        == st["completed_at"]
        for name, st in steps_1.items()
    )
    s.check("no completed step repeated",
            len(set(names)) == len(names) and by == expected_by and unchanged,
            "each checkpoint recorded once; by execution: "
            f"{ {k: sorted(v) for k, v in by.items()} } (accept-report: the gate opened "
            "by the second, decided in the third); the first execution's checkpoints "
            "unchanged")
    work = {}
    for a in ledger_end:
        if a["model"] == WORK_MODEL:
            work.setdefault(a["request_id"], []).append(a["attempt"])
    s.check("each exec command ran once",
            len(work) == 2 and all(v == [1] for v in work.values()),
            f"attempts at each metered command: {work}")
    model = [(a["attempt"], a["state"], a["charged_tokens"])
             for a in ledger_end if a["model"] != WORK_MODEL]
    s.check("the model was called again only for the step kill 1 cut off",
            calls_at_gate == (2, 1) and before_eval == calls_at_gate,
            f"fake model: {calls_at_gate[0]} calls, {calls_at_gate[1]} answered, by kill 2; "
            f"none after it; ledger (attempt, state, charged tokens): {model}")
    s.check("within the original budgets",
            (caps_end["tokens_cap"], caps_end["millis_cap"])
            == (caps_1["tokens_cap"], caps_1["millis_cap"])
            == (TOKENS_CAP, MINUTES_CAP * 60_000)
            and caps_end["tokens_charged"] <= caps_end["tokens_cap"]
            and caps_end["millis_charged"] <= caps_end["millis_cap"],
            f"tokens {caps_end['tokens_charged']} of {caps_end['tokens_cap']}, active ms "
            f"{caps_end['millis_charged']} of {caps_end['millis_cap']}, caps as at kill 1; "
            f"runs show usage {json.dumps(done['usage'])}")
    if len(decided) != 1:
        raise Failed(f"the run has {len(decided)} gates, not one: {decided}")
    final = decided[0]
    decision = (final.get("resolution") or {}).get("decided") or {}
    s.check("within the original gate deadline",
            # A gate recorded without its deadline fails this check rather
            # than reaching the time comparisons after it, which need both.
            final["deadline"] is not None
            and (final["id"], final["opened_at"], final["deadline"])
            == (gate["id"], gate["opened_at"], gate["deadline"])
            and decision.get("verdict") == "approve"
            and isinstance(decision.get("at"), int)
            and final["opened_at"] <= decision["at"] < final["deadline"],
            f"gate {final['id']} kept its opening, {when(final['opened_at'])}, and its deadline, "
            f"{when(final['deadline'])}; {decision.get('verdict')} by {decision.get('by')} "
            f"at {when(decision['at']) if 'at' in decision else None}")
    s.check("evals green", live[0] - before_eval[0] == 1 and after_eval == live,
            f"atomic eval --live and atomic eval exited 0; the live eval made "
            f"{live[0] - before_eval[0]} model call(s), the replay {after_eval[0] - live[0]}")


def stopped(signum, _frame):
    """End the scenario on `signum` as an exception would, so `finally` runs."""
    raise SystemExit(128 + signum)


def under_system_temp(path):
    """Whether `path` is inside /tmp or $TMPDIR, as each really is."""
    real = os.path.realpath(path)
    temps = {os.path.realpath("/tmp"), os.path.realpath(tempfile.gettempdir())}
    if os.environ.get("TMPDIR"):
        temps.add(os.path.realpath(os.environ["TMPDIR"]))
    return any(os.path.commonpath([real, t]) == t for t in temps)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--atomic", default=os.path.join(ROOT, "target", "release", "atomic"),
                        help="the atomic binary to test (default: target/release/atomic)")
    parser.add_argument("--keep", metavar="DIR",
                        help="work in DIR, which must not exist yet, and keep it")
    args = parser.parse_args()
    # SIGTERM and SIGHUP end the scenario as an exception does, so what it
    # started is reaped and its directory removed: set before anything starts.
    # A caller who started this with nohup has SIGHUP ignored on purpose, to
    # keep a detached run going when its terminal closes, so leave that alone.
    signal.signal(signal.SIGTERM, stopped)
    if signal.getsignal(signal.SIGHUP) is not signal.SIG_IGN:
        signal.signal(signal.SIGHUP, stopped)
    atomic = os.path.abspath(args.atomic)
    if not os.access(atomic, os.X_OK):
        say(f"v01-gate: no atomic binary at {atomic}; build one, or name it with --atomic")
        return 2
    try:
        if args.keep:
            work = os.path.abspath(args.keep)
        else:
            os.makedirs(TARGET, exist_ok=True)
            work = tempfile.mkdtemp(prefix="v01-gate-", dir=TARGET)
    except OSError as e:
        say(f"v01-gate: no directory to work in: {e}")
        return 2
    # Either branch's directory, not just a --keep: atomic refuses an atom's
    # scratch under the system's temporary directory, so a checkout that lives
    # there would fail the run mid-step rather than here.
    if under_system_temp(work):
        if not args.keep:
            shutil.rmtree(work, ignore_errors=True)  # empty; the one just made
        say(f"v01-gate: the directory to work in, {work}, is under the system's temporary "
            "directory, where atomic refuses an atom's scratch; work in a directory "
            "elsewhere, named with --keep or in a checkout that is not under it")
        return 2
    fake = Fake()
    s = Scenario(atomic, work)
    try:
        prepare(s, fake)
        scenario(s, fake)
    except Failed as e:
        say(f"v01-gate: FAILED: {e}")
        s.failed.append(str(e))
    except (OSError, subprocess.SubprocessError, sqlite3.Error, json.JSONDecodeError) as e:
        say(f"v01-gate: the scenario could not run: {e!r}")
        return 2
    except (KeyError, IndexError, TypeError, AttributeError, Unexpected) as e:
        say(f"v01-gate: what atomic recorded is not in the shape this script reads: {e!r}")
        return 2
    finally:
        # First: a second SIGTERM or SIGHUP must not cut the cleanup short.
        # Its default action would end the process mid-reap or mid-removal,
        # leaving an atomic running and its directory behind.
        for signum in (signal.SIGTERM, signal.SIGHUP):
            signal.signal(signum, signal.SIG_IGN)
        s.reap()
        fake.stop()
        if not args.keep:
            shutil.rmtree(work, ignore_errors=True)
    say()
    if s.failed:
        say(f"v01-gate: {len(s.failed)} check(s) failed: {s.failed}")
        return 1
    say(f"v01-gate: all {len(s.passed)} checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
