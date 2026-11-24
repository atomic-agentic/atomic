#!/usr/bin/env python3
"""Prove a candidate commit's declared controls by removing them, as an exec step.

atomic's Controls workflow (docs/development.md) proves that every control has
a test that fails when the control is switched off. This is that proof for one
candidate change, run by an exec atom rather than by CI: it reads the control
rows the change declares, makes a clean copy of the candidate commit under its
own scratch directory, and hands each row to the trusted
`scripts/controls-check.sh`, which switches the control off, requires the row's
test to fail, puts the source back and requires it to pass. A row whose test
still passes with its control off fails the step.

    echo '{"source": "/src/atomic", "commit": "<sha>", "rows": ["git-dir"],
           "bound": {"reference": "refs/atomic/<run>/steps/<step>/1",
                     "base": "<sha>", "admitted": "<sha>"}}' \\
        | CONTROL_RUNNER=/harness/scripts/controls-check.sh \\
            CONTROL_RUNNER_SHA256=<64 hex> CONTROL_DRIVER_SHA256=<64 hex> \\
            python3 scripts/controls-exec.py

It reads its input as one JSON value on stdin and writes one JSON value to
stdout, as an exec atom's command does; a nonzero exit fails the step. It
exits 0 when every declared row passed, 1 when a row did not, and 2 when it
refused before running any.

What the step is bound to
-------------------------

Three things decide what is proved, and none of them may be the change's own
choice:

* **The candidate.** `bound.reference` is the ref atomic keeps the candidate
  commit under (`refs/atomic/<run>/steps/<step>/<attempt>`, written by
  `Worktrees::commit` and recorded in the step's metadata by
  `StepCommit::record`), and `bound.base` is the base it recorded beside it.
  The commit is required to be what that ref names in `source`, and to have
  exactly that base as its one parent. A step handed some other commit -- an
  easier one, or one from another repository, where its ref names nothing --
  refuses, because a proof about a commit no coding step made proves nothing
  about the change. `bound.admitted` is the frozen host admission commit,
  which may precede the immediate parent after earlier accepted steps;
  selected Cargo targets must retain its path and standard test harness.
* **The rows.** Every row named in `rows` is run: the input is the full
  declared set, and an input whose rows cannot all fit the step's own budget
  is refused rather than started. Which rows a change declares comes from the
  workflow's mapping over the run's own record (atomic-avd6h.4.12 derives that
  inventory); choosing them is not this step's decision.
* **The runner.** `CONTROL_RUNNER` names the `scripts/controls-check.sh` that
  does the switching and `CONTROL_RUNNER_SHA256` its exact bytes;
  `CONTROL_DRIVER_SHA256` names this file's exact bytes. Both are checked
  before anything runs, the runner is started from a fresh link in the step's
  own scratch, never from the candidate's copy of it, and it is hashed again
  when each row ends: a candidate that replaces its own copy of the proof
  runner to print a passing verdict gets nothing, and a row whose command
  swapped the runner while it ran ends as `runner-changed`. An absolute
  `CONTROL_RUNNER` is used as it is written; a relative one is taken against
  the directory of the driver this step is running, which is the entry point
  the atom's `exec.command` already pins -- so a declaration in a repository
  names the same runner after the checkout it lives in is moved, and still
  only the pinned bytes answer.

The trust these three rest on is the step's declaration: `exec.command` and
`env` are written in the atom file, and a run pins the hash of every file its
definitions came from (`Pinned::files`) when it starts, so the candidate has no
part in them. What this checks is that the bytes actually running are the ones
that declaration named, so a copy planted in the tree is refused rather than
trusted.

Why the copy is made at all
---------------------------

A coding worktree is an atom's own writing: what is checked out there may not
be what the candidate commit holds, and `scripts/controls-check.sh` refuses a
file that differs from HEAD because it would save that state, put it back and
read as a clean restore. So nothing here is read from `source`'s working tree.
The copy is written from the object store alone -- `git ls-tree` and
`git cat-file` for the commit's own tree, each object streamed straight to the
copy rather than held in memory. It is then *bound* to the candidate rather
than committed: the copy's own repository is given one object alternate, named
by git itself for `source` and checked to be a directory, and through it
`git read-tree` fills the copy's index with the candidate's tree and
`git update-ref` puts the candidate's commit in its HEAD. Two things are then
required before any row runs: that the copy's HEAD is the candidate's commit
and so names the candidate's tree, and that `git status` finds the copy's files
exactly as that tree holds them. A file skipped, a mode changed, a link
resolved or text normalised makes the copy something other than the change, and
refuses the run.

No commit is made here, inside the sandbox or outside it: atomic's rule is that
it makes git commits outside the sandbox, and a scratch copy has no need of one
-- the candidate's commit and tree are already in the store the alternate names,
so the copy needs a HEAD and an index, not a new commit object. A dirty
worktree, a stray file or a link an atom left cannot reach the run, and no git
runs over a tree an atom wrote to.

What it bounds
--------------

The command it starts declares no network, so the exec sandbox gives it none,
and it writes only its own scratch directory. The whole step has
`step_seconds`, which defaults to the exec command's own five minutes
(atomic_core::runtime::EXEC_TIME_LIMIT) and may not exceed it. Of that,
`setup_seconds` (40 s by default) is reserved for reading the commit out of git
and copying it, and `report_seconds` (20 s by default) is held back at the end for what has to
happen after the last row: stopping a group that will not die, which takes up
to twenty seconds, removing the copy, and writing the one JSON value the step
answers with. Nothing a row or a git command is ever given runs into that
reserve -- each is cut off `report_seconds` before the step's own end -- so a
row that reaches its limit is stopped, reported and cleaned up while the
command that runs this still has time left, rather than being killed by the
backend with no verdict written at all. What is left after the two reserves is
shared by the rows, each stopped at `row_seconds`. An input whose setup, rows
and report reserve cannot all fit inside `step_seconds` is refused before
anything is touched, and a row that finds no room left when its turn comes is
reported as `no-time` and fails the step, so no declared control is ever
quietly unrun and no accepted work outlives the command.

Every byte this reads is capped as it arrives, not after: a row's output, a
`git ls-tree` answer and every object streamed out of `git cat-file` all stop
their own command the moment a cap is passed, so the memory this uses does not
depend on how much a candidate writes. Memory and CPU are what the environment
running the step allows the command; nothing here raises them. Re-running the
same input gives the same answer: the copy is made afresh under a name of its
own, nothing outside the scratch is written, and the report holds no timing.
"""

import hashlib
import json
import os
import re
import shlex
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import threading
import time
import tomllib

EXIT_PASS = 0
EXIT_ROWS_FAILED = 1
EXIT_REFUSED = 2

#: How long one git question about the candidate may take, at most: what is
#: left of the step's own budget usually asks for less.
GIT_SECONDS = 120
#: The least time this will agree to work to. A limit under this is a mistake
#: in what the step was asked, not a fast run.
MIN_SECONDS = 5
#: How long one stopped process group is given to go away at each of the two
#: stops (`end_now`'s polite stop, then the kill), in seconds. Nothing that has
#: to be finished before this step writes its answer may take longer than both
#: of them, which is what `report_seconds` holds back for.
STOP_SECONDS = 10
#: How much of the step's wall time is held back at its end for the answer,
#: whatever the rows and the setup spent: its default and its bounds. The
#: default is what the worst case after the last row costs -- one group
#: stopped by `end_now`, the copy removed, and one JSON value written -- and a
#: row is never given time that runs into this, so an accepted input whose row
#: reaches its limit is still stopped, reported and cleaned up by this command
#: rather than by the backend that runs it.
REPORT_SECONDS_DEFAULT = 2 * STOP_SECONDS
REPORT_SECONDS_MIN = STOP_SECONDS
REPORT_SECONDS_MAX = 60
#: What one row, or one git answer, may write before it is refused, in bytes.
ROW_OUTPUT_LIMIT = 2_000_000
#: The most one object may hold, and the most the copied tree may hold in all.
#: Both are applied as the bytes are read, so they bound the memory here and
#: not just the disk.
BLOB_LIMIT_BYTES = 256 * 1024 * 1024
TREE_LIMIT_BYTES = 2 * 1024 * 1024 * 1024
#: The most one `git cat-file` header may hold, the most a link's target may
#: hold, and the most the proof runner may hold, in bytes.
HEADER_LIMIT = 4096
LINK_LIMIT = 4096
RUNNER_LIMIT_BYTES = 1024 * 1024
#: The cap on one row, in seconds: its default and its bounds. The ceiling is
#: the step's own ceiling less what the report reserve asks for at its minimum,
#: so that one row at its own limit always leaves the step somewhere to stop.
ROW_SECONDS_DEFAULT = 240
ROW_SECONDS_MIN = 5
ROW_SECONDS_MAX = 290 - REPORT_SECONDS_MIN
#: How much of the step's wall time the setup -- reading the commit out of git
#: and copying it -- reserves for itself, its default and its bounds. The three
#: defaults are what one step with one row asks for: they add up with the report
#: reserve to the step's own default and no more, so the common input is
#: accepted and still leaves the answer its reserve.
SETUP_SECONDS_DEFAULT = 40
SETUP_SECONDS_MIN = 10
SETUP_SECONDS_MAX = 180
#: The step's whole wall time. The default and the ceiling are the exec
#: command's own limit (atomic_core::runtime::EXEC_TIME_LIMIT), which nothing
#: raised from inside the command can outlive.
STEP_SECONDS_DEFAULT = 300
STEP_SECONDS_MIN = 30
STEP_SECONDS_MAX = 300

NAME_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,79}$")
SHA_RE = re.compile(r"^[0-9a-f]{40}([0-9a-f]{24})?$")
SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
#: One part of a ref atomic keeps: `worktree::component`, kept as it is when it
#: is plain and rewritten as `_`-prefix-plus-hash when it is not.
REF_PART = r"(?:[A-Za-z0-9][A-Za-z0-9_-]{0,63}|_[A-Za-z0-9_-]{1,33}-[0-9a-f]{12})"
#: The refs `Worktrees::commit` keeps a step's attempts under, and nothing
#: else. A commit this step will accept is one of those names it.
BOUND_RE = re.compile(
    r"^refs/atomic/" + REF_PART + r"/steps/" + REF_PART + r"/[1-9][0-9]*$"
)

#: How `scripts/controls-check.sh` ends a row, in its own words, and what that
#: says about the control. An unlisted reason is never a pass: a control whose
#: message moves fails here until this list moves with it, which a case in
#: scripts/controls-exec-test.sh shows.
REASONS = [
    ("its test fails with the control off and passes with it on", "passed"),
    ("its test passed with the control off", "blind"),
    ("its switch no longer applies to", "switch-stale"),
    ("with the control off the code does not build", "build-failed"),
    ("no test ran", "no-test"),
    ("with the control off, these tests were not seen failing", "not-seen-failing"),
    ("with the control on, no test passed", "fails-when-on"),
    ("with the control on, these tests were not seen passing", "fails-when-on"),
    ("its test fails with the control on", "fails-when-on"),
]
#: The verdict this step exists to catch: a test that never noticed its
#: control being removed.
BLIND = "blind"
#: A row the step's own clock stopped it from running, and a row whose command
#: swapped the proof runner out from under it. Both name themselves, because a
#: control left unproved or untrusted is never a control proved.
NO_TIME = "no-time"
RUNNER_CHANGED = "runner-changed"

ROWS_DIR = "scripts/controls"
CHECK_SCRIPT = "scripts/controls-check.sh"
#: What the link to the trusted runner is held in, inside the copy: one of
#: these per row, so that no row starts from a file another row left behind.
RUNNER_HOLDER = ".controls-exec-runner"
#: The directory `scripts/controls-check.sh` keeps its originals in, relative to
#: the tree it edits. Finding it after a row ended means the run did not finish.
STATE_DIR = os.path.join("target", "controls-check")
#: Where the trusted runner and this file are pinned from: the atom's own
#: declaration, which a candidate has no part in and a run pins the hash of.
#: They are read from the environment only, never from the input, so that what
#: the change carries can never name the thing that proves it.
RUNNER_ENV = "CONTROL_RUNNER"
RUNNER_SHA_ENV = "CONTROL_RUNNER_SHA256"
DRIVER_SHA_ENV = "CONTROL_DRIVER_SHA256"


class Refused(Exception):
    """Something this step cannot do anything about; no row ran."""


def git_env(base):
    """`base` with git's own configuration out of it.

    A repository's or a user's git config can point git at a filter driver, a
    fsmonitor daemon, a signing key or a pager, and the tree read here is not
    one atomic wrote. Every git started here is given `--no-replace-objects`
    and GIT_HARDENING on top of this.
    """
    env = dict(base)
    env["GIT_CONFIG_GLOBAL"] = os.devnull
    env["GIT_CONFIG_NOSYSTEM"] = "1"
    env["GIT_OPTIONAL_LOCKS"] = "0"
    env["GIT_TERMINAL_PROMPT"] = "0"
    env["LC_ALL"] = "C"
    return env


GIT_HARDENING = [
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=" + os.devnull,
    "-c",
    "commit.gpgSign=false",
    "-c",
    "tag.gpgSign=false",
    "-c",
    "core.pager=cat",
    "-c",
    "core.autocrlf=false",
]


def stop(proc, sig):
    """Signal `proc`'s whole group, and wait for the group to be gone."""
    try:
        os.killpg(proc.pid, sig)
    except ProcessLookupError:
        return
    deadline = time.monotonic() + STOP_SECONDS
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            return
        time.sleep(0.05)


def end_now(proc):
    """Stop `proc`'s group now: the polite stop, and the kill if that is ignored.

    Both stops wait `STOP_SECONDS` for the group to go, so a command that will
    not take the polite one costs this much: `report_seconds` is what the step
    keeps back at its end to pay for it.
    """
    stop(proc, signal.SIGTERM)
    stop(proc, signal.SIGKILL)


def wait_out(proc, seconds):
    """`proc`'s exit code, waited for no longer than `seconds`, reaped either way.

    A command that is ending on its own is waited for rather than stopped:
    killing a process while it is on its way out can give it a signalled exit
    in place of the code it earned. A command that is still there when `seconds`
    is up is stopped, and then reaped, because no child of this process may be
    left neither waited for nor reaped.
    """
    try:
        return proc.wait(timeout=seconds)
    except subprocess.TimeoutExpired:
        pass
    end_now(proc)
    try:
        return proc.wait(timeout=STOP_SECONDS)
    except subprocess.TimeoutExpired:
        return None


def drain(stream, limit, stop_now):
    """Read `stream` to its end, keeping at most `limit` bytes of it.

    The cap is applied as the bytes arrive and the command is stopped the
    moment it is passed, so that a limit here bounds what is read as well as
    what is kept: emptying a pipe first and measuring it after would hold
    whatever a candidate chose to write in memory on the way to refusing it.
    An answer that overflows keeps only its beginning, which `classify` and the
    git readers refuse for what is missing from them, and reading stops a few
    cap-lengths in rather than emptying the pipe into a buffer that is thrown
    away.

    Answers (what was kept, whether the cap was passed, how much it wrote).
    """
    keep = bytearray()
    total = 0
    overflow = False
    while True:
        try:
            chunk = stream.read(65536)
        except (OSError, ValueError):
            break
        if not chunk:
            break
        total += len(chunk)
        if total > limit:
            if not overflow:
                overflow = True
                stop_now()
            elif total > limit * 4:
                break
        room = limit - len(keep)
        if room > 0:
            keep += chunk[:room]
    return bytes(keep), overflow, total


def run(cmd, cwd, env, seconds, out_limit, feed=None, deadline=None, reaper=None):
    """Run `cmd` in its own process group, within `seconds` and `out_limit`.

    Answers (exit code, stdout, stderr, timed_out, overflow). Both pipes are
    read on threads of their own while the command runs, so that neither what
    a command writes nor what this step holds grows with it; `feed`, when
    given, is an iterator of chunks written to stdin on a third thread, so
    asking git for every object of a tree costs one chunk of memory rather
    than the list. The whole group is stopped, not just the child:
    `scripts/controls-check.sh` runs cargo in the background and cargo starts
    test binaries of its own, and a build left running after a stopped row
    would still be switching the copy's files.

    `deadline`, when given, is the moment this command must have stopped by
    however its own `seconds` fell out: the timer stops the group at whichever
    comes first, and goes back the moment the command ends instead of sleeping
    out the rest of a cap nothing is waiting for. `reaper`, when given, is what
    to do with the group when the wait for it cannot finish -- the same ending
    either way, named so that the caller says what it stops. Either way the
    child is waited for or stopped by this function, never left for whoever
    comes next to reap.
    """
    try:
        proc = subprocess.Popen(
            cmd,
            cwd=cwd,
            env=env,
            stdin=subprocess.PIPE if feed is not None else subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
    except OSError as e:
        raise Refused(f"cannot start {cmd[0]}: {e.strerror or e}") from e
    answers = {"out": (b"", False), "err": (b"", False)}
    ran = {"past": False}
    done = threading.Event()

    def read_pipe(stream):
        """Empty one pipe, capping it, and stopping the command at the cap."""

        def over():
            end_now(proc)

        keep, overflow, _total = drain(stream, out_limit, over)
        return keep, overflow

    def feed_stdin():
        try:
            with proc.stdin as handle:
                for chunk in feed:
                    handle.write(chunk)
        except (OSError, ValueError):
            # The command read none or not all of what it was given: it
            # answered and went, or it was stopped. What it answered with
            # decides, and an answer with something missing is refused for
            # that rather than trusted.
            pass

    def watch(when):
        """Stop the command at `when`, unless it has gone by then.

        Waking on `done` rather than sleeping the whole cap hands the thread
        back as soon as the command ends, and the stop happens at the step's
        own deadline as well as at the command's cap.
        """
        if done.wait(when):
            return
        ran["past"] = True
        end_now(proc)

    def start(target, *args):
        thread = threading.Thread(target=target, args=args, daemon=True)
        thread.start()
        return thread

    def join(thread):
        thread.join(timeout=seconds + 30)
        if thread.is_alive():
            # A grandchild holding the pipe open past the kill: give up on it
            # rather than wait, which closes the stream under the reader.
            end_now(proc)
            thread.join(timeout=10)

    out_thread = start(lambda: answers.update(out=read_pipe(proc.stdout)))
    err_thread = start(lambda: answers.update(err=read_pipe(proc.stderr)))
    feeder = start(feed_stdin) if feed is not None else None
    when = seconds if deadline is None else min(seconds, max(0.0, deadline - time.monotonic()))
    watcher = start(watch, when)
    try:
        join(out_thread)
        join(err_thread)
        if feeder is not None:
            feeder.join(timeout=10)
        code = wait_out(proc, 15)
    finally:
        # However this ended -- an answer, a reader that raised, a refusal on
        # the way out -- the command has been waited for or stopped, and the
        # thread holding its deadline has been called back.
        done.set()
        watcher.join(timeout=STOP_SECONDS + 2)
        if proc.poll() is None:
            (reaper or end_now)(proc)
    out, out_over = answers["out"]
    err, err_over = answers["err"]
    return code, out, err, ran["past"], out_over or err_over


def git_run(git, where, env, args, seconds):
    """Run git in `where`, hardened, and refuse a nonzero exit.

    The quiet commands answer nothing, so their answer is not checked here.
    """
    label = " ".join(args)
    code, out, err, timed_out, overflow = run(
        [git, "--no-replace-objects", *GIT_HARDENING, *args],
        cwd=where,
        env=env,
        seconds=seconds,
        out_limit=65536,
    )
    if timed_out:
        raise Refused(f"git {label} in {where} ran past its {seconds}s")
    if overflow:
        raise Refused(f"git {label} in {where} wrote more than its limit")
    if code != 0:
        why = err.decode("utf-8", "replace").strip().splitlines()
        raise Refused(f"git {label} in {where} failed: {why[0] if why else f'exit {code}'}")
    return out.decode("utf-8", "replace")


def git_text(git, where, env, args, seconds):
    """Ask git for one answer, and refuse anything but the one line it gives."""
    text = git_run(git, where, env, args, seconds).strip()
    if not text:
        raise Refused(f"git {' '.join(args)} in {where} answered nothing")
    return text


def tree_of(git, where, env, rev, seconds):
    """The tree hash `rev` names in `where`, verified as an object id."""
    text = git_text(
        git, where, env, ["rev-parse", "--verify", "--quiet", f"{rev}^{{tree}}"], seconds
    )
    if not SHA_RE.fullmatch(text):
        raise Refused(f"git gave {text!r} for the tree of {rev}, which is not an object id")
    return text


def ref_target(git, where, env, reference, seconds):
    """The commit `reference` names in `where`, or nothing when it names none.

    Only git's own empty answer to `--verify --quiet` means "no such ref". A
    git that could not be asked, was stopped, or answered something that is not
    a commit id is a refusal and not an absence: the difference between a
    candidate no coding step made and a repository this cannot read at all.
    """
    label = f"rev-parse --verify {reference}"
    code, out, err, timed_out, overflow = run(
        [
            git,
            "--no-replace-objects",
            *GIT_HARDENING,
            "rev-parse",
            "--verify",
            "--quiet",
            f"{reference}^{{commit}}",
        ],
        cwd=where,
        env=env,
        seconds=seconds,
        out_limit=4096,
    )
    if timed_out:
        raise Refused(f"git {label} in {where} ran past its {seconds}s")
    if overflow:
        raise Refused(f"git {label} in {where} wrote more than its limit")
    if code == 1 and not out.strip():
        return ""
    if code != 0:
        why = err.decode("utf-8", "replace").strip().splitlines()
        raise Refused(f"git {label} in {where} failed: {why[0] if why else f'exit {code}'}")
    text = out.decode("utf-8", "replace").strip()
    if not SHA_RE.fullmatch(text):
        raise Refused(f"git gave {text!r} for {reference}, which is not a commit id")
    return text


def parents_of(git, where, env, commit, seconds):
    """`commit`'s parents, as git lists them beside it. An empty answer, or one
    that does not start with the commit asked for, is a refusal: a merge, or a
    commit whose history cannot be read, is not a step's candidate."""
    text = git_text(git, where, env, ["rev-list", "--parents", "-n", "1", commit], seconds)
    fields = text.split()
    if fields[0] != commit:
        raise Refused(f"git gave {text!r} for the parents of {commit}")
    return fields[1:]


def check_bound(git, source, env, commit, reference, base, seconds):
    """Require the candidate to be the commit atomic recorded, and nothing else.

    `reference` is the ref `Worktrees::commit` keeps a coding step's attempt
    under and `base` the commit it recorded beside it, both taken from the run's
    own record rather than from anything the change says. The commit must be
    what that ref names in `source`, and must have exactly that base as its one
    parent: an easier commit from another branch, or one from another
    repository where the ref names nothing, is refused here rather than proved
    in the change's place. A merge is refused too, since no step's attempt
    makes one.
    """
    named = ref_target(git, source, env, reference, seconds)
    if not named:
        raise Refused(
            f"{reference} names no commit in {source}, so this is not the candidate atomic "
            "recorded for the step being proved"
        )
    if named != commit:
        raise Refused(
            f"{reference} names {named}, not the {commit} this step was asked to prove, so "
            "what is proved here is not the change that step made"
        )
    got = parents_of(git, source, env, commit, seconds)
    if got != [base]:
        raise Refused(
            f"{commit} has {' and '.join(got) if got else 'no parent'} as its parent, not the "
            f"{base} the step recorded as its base, so it is not the commit that attempt made"
        )


def ls_tree(git, source, env, commit, seconds):
    """Every entry of `commit`'s tree, as (mode, path, blob id), from the store.

    `git ls-tree -r` lists what the commit holds and nothing else: not what a
    working tree has added, removed, or left dirty.
    """
    code, out, err, timed_out, overflow = run(
        [git, "--no-replace-objects", *GIT_HARDENING, "ls-tree", "-r", "-z", "--full-name", commit],
        cwd=source,
        env=env,
        seconds=seconds,
        out_limit=ROW_OUTPUT_LIMIT * 4,
    )
    if timed_out:
        raise Refused(f"git ls-tree {commit} ran past its {seconds}s")
    if overflow:
        raise Refused(f"git ls-tree {commit} wrote more than its limit")
    if code != 0:
        raise Refused(f"git ls-tree {commit} failed: {err.decode('utf-8', 'replace').strip()}")
    entries = []
    for record in out.split(b"\0"):
        if not record:
            continue
        try:
            meta, raw_path = record.split(b"\t", 1)
            mode, kind, blob = meta.split(b" ", 2)
            path, mode, kind, blob = (
                raw_path.decode("utf-8"),
                mode.decode("ascii"),
                kind.decode("ascii"),
                blob.decode("ascii"),
            )
        except (UnicodeDecodeError, ValueError) as e:
            raise Refused(f"git ls-tree {commit} answered something unreadable: {e}") from e
        if kind != "blob":
            raise Refused(
                f"{commit} holds {path!r} as a {kind}, and only a file or a link can be "
                "copied out of a commit"
            )
        if mode not in ("100644", "100755", "120000"):
            raise Refused(f"{commit} holds {path!r} with mode {mode}, which is not one this copies")
        if not SHA_RE.fullmatch(blob):
            raise Refused(f"{commit} holds {path!r} as {blob!r}, which is not an object id")
        if not path or os.path.isabs(path) or ".." in path.split("/"):
            raise Refused(f"{commit} holds {path!r}, which is not a path this can write")
        entries.append((mode, path, blob))
    return entries


def link_target(blob, path):
    """Where `path`'s link points, refusing one that leaves the copy."""
    try:
        target = blob.decode("utf-8")
    except UnicodeDecodeError as e:
        raise Refused(f"the link {path!r} is not a path: {e}") from e
    if not target or "\0" in target:
        raise Refused(f"the link {path!r} points at nothing readable")
    if os.path.isabs(target) or ".." in target.split("/"):
        raise Refused(
            f"the link {path!r} points at {target!r}, which leaves the copy, so the copy "
            "could be written outside its own directory"
        )
    return target


def copy_out(git, source, env, entries, where, seconds):
    """Write every file and link of the candidate's tree into `where`, one
    object at a time, straight out of `git cat-file --batch`.

    Each object goes to the file it belongs in as it is read rather than being
    held here: the whole of a repository's tree is more than this should hold
    in memory, and a cap applied after a batch of objects has been read caps
    nothing that mattered. A `missing` reply, a header this cannot read, an
    object that ends early, one longer than its own header, an id that was not
    asked for and a tree past `limit` in all all refuse the run: a copy with
    something wrong in it is not the candidate, and nothing is switched off in
    it. The sizes git declares are checked against the bytes written.
    """
    wants = {}
    for mode, path, blob in entries:
        wants.setdefault(blob, []).append((mode, path))
    try:
        proc = subprocess.Popen(
            # Stream replies as Git produces them. This keeps response latency
            # bounded by each object rather than by Git's batch buffering.
            [git, "--no-replace-objects", *GIT_HARDENING, "cat-file", "--batch"],
            cwd=source,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
    except OSError as e:
        raise Refused(f"cannot start git cat-file: {e.strerror or e}") from e
    stream = proc.stdout
    copied = {"bytes": 0}

    order = list(wants)

    def give():
        for oid in order:
            yield (oid + "\n").encode()

    def read_line(what):
        raw = stream.readline(HEADER_LIMIT + 1)
        if raw == b"":
            raise Refused(f"git cat-file --batch ended where {what} was expected")
        if not raw.endswith(b"\n") or len(raw) > HEADER_LIMIT:
            raise Refused(f"git cat-file --batch answered more than {HEADER_LIMIT} bytes for {what}")
        return raw[:-1]

    def take(size, oid):
        """Read exactly `size` bytes of one object, capping as they come."""
        left = size
        while left:
            chunk = stream.read(min(left, 1 << 16))
            if not chunk:
                raise Refused(f"git cat-file --batch ended inside the blob {oid}")
            yield chunk
            left -= len(chunk)

    def write_one(path, mode, oid, size, chunks):
        dest = os.path.join(where, path)
        try:
            parent = os.path.dirname(dest)
            if parent:
                os.makedirs(parent, exist_ok=True)
            fd = os.open(dest, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        except OSError as e:
            raise Refused(f"cannot write {path!r} into the copy: {e.strerror or e}") from e
        wrote = 0
        try:
            for chunk in chunks:
                os.write(fd, chunk)
                wrote += len(chunk)
                copied["bytes"] += len(chunk)
                if copied["bytes"] > TREE_LIMIT_BYTES:
                    raise Refused(
                        f"the tree holds more than {TREE_LIMIT_BYTES} bytes, which this "
                        "will not copy"
                    )
        except OSError as e:
            raise Refused(f"cannot write {path!r} into the copy: {e.strerror or e}") from e
        finally:
            os.close(fd)
        if wrote != size:
            raise Refused(f"git cat-file --batch gave {wrote} bytes for {oid}, not its {size}")
        try:
            os.chmod(dest, 0o755 if mode == "100755" else 0o644)
        except OSError as e:
            raise Refused(f"cannot set the mode of {path!r} in the copy: {e.strerror or e}") from e
        return dest

    def one(header):
        if header.endswith(b" missing"):
            raise Refused(f"git cat-file --batch has no object {header.split()[0].decode()}")
        try:
            oid, kind, size = header.decode("ascii").split(" ")
            size = int(size)
        except (UnicodeDecodeError, ValueError) as e:
            raise Refused(f"git cat-file --batch answered {header!r}, not a header") from e
        if kind != "blob" or not SHA_RE.fullmatch(oid):
            raise Refused(f"git cat-file --batch answered {oid} as a {kind}, not a blob")
        if size < 0:
            raise Refused(f"git cat-file --batch gives {oid} a size of {size}")
        if size > BLOB_LIMIT_BYTES:
            raise Refused(
                f"{oid} holds {size} bytes, more than the {BLOB_LIMIT_BYTES} this copies"
            )
        plans = wants.pop(oid, None)
        if plans is None:
            raise Refused(f"git cat-file --batch answered {oid}, which was not asked for")
        links = [path for mode, path in plans if mode == "120000"]
        if links and len(links) != len(plans):
            raise Refused(
                f"{oid} is held as a link at {links[0]!r} and as a file elsewhere, which is "
                "not one thing this can copy"
            )
        # The object's bytes are read exactly once, however many paths hold it,
        # and the newline git ends its answer with is read once too: reading it
        # per path would eat the next object's header.
        if links:
            if size > LINK_LIMIT:
                raise Refused(
                    f"the link {links[0]!r} holds {size} bytes, more than a path of "
                    f"{LINK_LIMIT}"
                )
            data = b"".join(take(size, oid))
            for path in links:
                try:
                    parent = os.path.dirname(os.path.join(where, path))
                    if parent:
                        os.makedirs(parent, exist_ok=True)
                    os.symlink(link_target(data, path), os.path.join(where, path))
                except OSError as e:
                    raise Refused(
                        f"cannot write the link {path!r} into the copy: {e.strerror or e}"
                    ) from e
        else:
            first = None
            for mode, path in plans:
                if first is None:
                    first = write_one(path, mode, oid, size, take(size, oid))
                    continue
                # The same object at another path: copied from the file just
                # written, in chunks, rather than held here.
                dest = os.path.join(where, path)
                try:
                    os.makedirs(os.path.dirname(dest), exist_ok=True)
                    shutil.copyfile(first, dest)
                    os.chmod(dest, 0o755 if mode == "100755" else 0o644)
                except OSError as e:
                    raise Refused(f"cannot write {path!r} into the copy: {e.strerror or e}") from e
                copied["bytes"] += size
                if copied["bytes"] > TREE_LIMIT_BYTES:
                    raise Refused(
                        f"the tree holds more than {TREE_LIMIT_BYTES} bytes, which this "
                        "will not copy"
                    )
        if links:
            copied["bytes"] += size * len(links)
            if copied["bytes"] > TREE_LIMIT_BYTES:
                raise Refused(
                    f"the tree holds more than {TREE_LIMIT_BYTES} bytes, which this will not copy"
                )
        if stream.read(1) != b"\n":
            raise Refused(f"git cat-file --batch ended inside the object {oid}")

    def read_out():
        while wants:
            one(read_line("an object header"))
        extra = stream.read(1)
        if extra == b"":
            return
        if extra != b"\n":
            raise Refused("git cat-file --batch has more objects to give than were asked for")
        rest = stream.read(1)
        if rest != b"":
            raise Refused("git cat-file --batch went on after answering every object")

    def feed_stdin():
        try:
            with proc.stdin as handle:
                for chunk in give():
                    handle.write(chunk)
        except (OSError, ValueError):
            # git read not all of the ids: it answers with what it got, and
            # every object that is missing from the copy is refused below.
            pass

    def watch(when):
        """Stop git at `when`, unless it has gone by then.

        `done` is set on the way out of the copy below, so that a git that
        answered every object quickly does not keep this thread for the rest of
        a cap nobody is waiting for any more.
        """
        if done.wait(when):
            return
        end_now(proc)

    #: git's complaints, read as they come so that a git that says a great deal
    #: is never left waiting for someone to empty its stderr.
    said = [b""]
    code = [None]
    done = threading.Event()

    def talk():
        said[0], _over, _total = drain(proc.stderr, 65536, lambda: end_now(proc))

    def read_all():
        try:
            read_out()
        except OSError as e:
            raise Refused(f"git cat-file --batch could not be read: {e.strerror or e}") from e
        if wants:
            raise Refused(
                f"git cat-file --batch answered no object for {next(iter(wants))}, so the copy "
                "would be missing a file the candidate holds"
            )

    feeder = threading.Thread(target=feed_stdin, daemon=True)
    feeder.start()
    watcher = threading.Thread(target=watch, args=(seconds,), daemon=True)
    watcher.start()
    talker = threading.Thread(target=talk, daemon=True)
    talker.start()
    complete = False
    try:
        read_all()
        complete = True
    finally:
        # However the reading ended -- every object in place, a `missing`
        # answer, a header that is not one, an object bigger than this copies,
        # a stream that stopped short -- git is ended here, because a refusal
        # that returned while its child still sat writing the rest of a blob is
        # a cap that stopped nothing. The threads are joined before the answer
        # so none of them outlives it. On a
        # refusal, stop git before touching the buffered stdin stream: the
        # feeder may hold its lock while blocked writing to a full pipe, and
        # closing that same stream here would then wait forever for the lock.
        done.set()
        if not complete:
            end_now(proc)
        feeder.join(timeout=10)
        if feeder.is_alive():
            end_now(proc)
            feeder.join(timeout=STOP_SECONDS + 2)
        if not feeder.is_alive():
            try:
                proc.stdin.close()
            except OSError:
                pass
        watcher.join(timeout=STOP_SECONDS + 2)
        if complete:
            # git has every id it was going to be given and has answered all of
            # them: wait for it to leave on its own, so that stopping it here
            # cannot turn the exit of 0 it earned into a signalled one, and stop
            # the group only if it will not go.
            code[0] = wait_out(proc, 15)
        talker.join(timeout=15)
    if code[0] not in (0, None):
        raise Refused(
            f"git cat-file --batch exited {code[0]}: {said[0].decode('utf-8', 'replace').strip()}"
        )
    return copied["bytes"]


def object_store(git, source, env, room):
    """The directory `source`'s objects live in, as git itself spells it.

    Asked of git rather than guessed at, because the answer is git's: a linked
    worktree keeps its objects with the repository they belong to and spells
    that out, while a plain one answers `.git/objects` against the directory it
    was asked in. This is the one store the copy is allowed to reach through an
    alternate, and it is the store the candidate was just read out of, so the
    copy cannot reach an object the change's own repository does not hold.
    """
    found = git_text(git, source, env, ["rev-parse", "--git-path", "objects"], room())
    store = found if os.path.isabs(found) else os.path.join(source, found)
    store = os.path.abspath(store)
    try:
        info = os.stat(store)
    except FileNotFoundError:
        raise Refused(
            f"{source} answers that its objects are in {store}, which is not there"
        ) from None
    except OSError as e:
        raise Refused(
            f"{source} answers that its objects are in {store}, which cannot be read: "
            f"{e.strerror or e}"
        ) from e
    if not stat.S_ISDIR(info.st_mode):
        raise Refused(f"{source} answers that its objects are in {store}, which is not a directory")
    return store


def bind_copy(git, where, env, store, commit, tree, room):
    """Bind the copy to the candidate without making a commit in it.

    The copy's own repository is given one alternate -- `store`, the object
    store of the `source` the candidate was just read out of, named by git and
    checked to be a directory -- and through it `git read-tree` fills the copy's
    index with the candidate's tree and `git update-ref` gives its HEAD the
    candidate's commit. That is all the state the copy needs:
    `scripts/controls-check.sh` asks a file how it stands against HEAD, and a
    scratch copy has no history of its own to want.

    Two things are then required, before any control is switched off in the
    copy. Its HEAD must name the candidate's tree exactly, so the copy is bound
    to the commit this step was asked about and not to something near it. And
    `git status` must find nothing to report: every file, mode and link as the
    candidate's tree holds it. That is where a copy that is not the candidate --
    a file skipped, a mode changed, a link resolved, text normalised on the way
    to the disk -- is caught, and it is caught without a commit being made
    anywhere: atomic makes git commits outside the sandbox, and this runs where
    the sandbox does. `room` answers how long each command may still take.
    """
    git_run(git, where, env, ["init", "-q", "--initial-branch=main"], room())
    alternate = os.path.join(where, ".git", "objects", "info", "alternates")
    try:
        os.makedirs(os.path.dirname(alternate), exist_ok=True)
        with open(alternate, "w") as handle:
            handle.write(store + "\n")
    except OSError as e:
        raise Refused(
            f"cannot give the copy an alternate for {store}: {e.strerror or e}"
        ) from e
    git_run(git, where, env, ["read-tree", tree], room())
    git_run(git, where, env, ["update-ref", "HEAD", commit], room())
    status = git_run(git, where, env, ["status", "--porcelain"], room()).strip()
    if status:
        raise Refused(
            f"the copy holds a file git does not hold as committed: {status.splitlines()[0]}"
        )
    here = tree_of(git, where, env, "HEAD", room())
    if here != tree:
        raise Refused(
            f"the copy's tree is {here}, not the candidate's {tree}, so the copy is not the "
            "change this step was asked about"
        )


def hashed(path, limit, what):
    """The SHA-256 of `path`'s bytes, read in chunks through the name given.

    A file this is about to run is trusted for its exact bytes or not at all:
    whatever can name the file that decides a verdict can print any verdict, so
    the bytes are compared with what the step's declaration pinned.

    The bytes are read through the name, not through a link resolved first,
    because that is how the row is started: bash opens the name and follows the
    link, so a write through that link is a write to what bash ran, and it shows
    here as a hash that is not the pinned one. Only a regular file answers: a
    directory or a device would stand for some other thing's bytes. The size
    comes from the open file, and after reading the name is asked what it holds
    now, so a file that is rewritten or replaced under this while it is read is
    refused rather than passed on a hash of half of it. Nothing longer than
    `limit` is read at all, so hashing costs what a script costs and not what a
    candidate can write.
    """
    try:
        handle = open(path, "rb")
    except FileNotFoundError:
        raise Refused(f"{what} is {path!r}, which is not there") from None
    except OSError as e:
        raise Refused(f"{what} is {path!r}, which cannot be read: {e.strerror or e}") from e
    digest = hashlib.sha256()
    read = 0
    with handle:
        try:
            info = os.fstat(handle.fileno())
        except OSError as e:
            raise Refused(f"{what} is {path!r}, which cannot be read: {e.strerror or e}") from e
        if not stat.S_ISREG(info.st_mode):
            raise Refused(
                f"{what} is {path!r}, which is not a regular file, so its bytes are not its own"
            )
        if info.st_size > limit:
            raise Refused(
                f"{what} is {path!r}, which holds more than the {limit} bytes this runs"
            )
        while True:
            try:
                chunk = handle.read(65536)
            except OSError as e:
                raise Refused(
                    f"{what} is {path!r}, which cannot be read: {e.strerror or e}"
                ) from e
            if not chunk:
                break
            read += len(chunk)
            if read > limit:
                raise Refused(
                    f"{what} is {path!r}, which holds more than the {limit} bytes this runs"
                )
            digest.update(chunk)
        try:
            now = os.stat(path)
        except FileNotFoundError:
            raise Refused(f"{what} is {path!r}, which went while it was read") from None
        except OSError as e:
            raise Refused(
                f"{what} is {path!r}, which cannot be read: {e.strerror or e}"
            ) from e
    if (now.st_dev, now.st_ino, now.st_size) != (info.st_dev, info.st_ino, read):
        raise Refused(
            f"{what} is {path!r}, which changed while it was read: {info.st_size} bytes were "
            f"there and {read} were hashed"
        )
    return digest.hexdigest()


def resolved(path, what):
    """`path` with every link in it resolved, refused if it cannot be.

    The name a script is reached by is part of what is being trusted, so the
    answer comes from the file system and not from a string: a link that cannot
    be resolved, or that loops, is a refusal rather than a name to try anyway.
    """
    try:
        return os.path.realpath(path, strict=True)
    except OSError as e:
        raise Refused(f"{what} is {path!r}, which cannot be resolved: {e.strerror or e}") from e


def outside(candidate, homes, what):
    """Refuse `candidate` standing inside any of `homes`.

    The proof runner and this driver have to come from outside the change and
    outside the scratch the change's own commands can write in, or a candidate
    could simply supply the thing that proves it.
    """
    for home, name in homes:
        if candidate == home or candidate.startswith(home + os.sep):
            raise Refused(
                f"{what} is {candidate!r}, inside {name} ({home}), so {name} could hold the "
                "thing that is supposed to prove it"
            )


def check_driver(env):
    """Refuse unless this file's own bytes are the ones the step pins.

    The copy of the driver that runs is named by the atom's `exec.command`, in a
    file whose hash the run pinned when it started; `CONTROL_DRIVER_SHA256`,
    which comes from the same declaration, says what those bytes must be. A
    driver planted in the tree being checked -- one that prints a passing verdict
    for every row -- refuses here instead of answering, which is what makes the
    pair worth having: what is checked is that the bytes running are the bytes
    that were declared.
    """
    want = env.get(DRIVER_SHA_ENV, "")
    if not SHA256_RE.fullmatch(want or ""):
        raise Refused(
            f"{DRIVER_SHA_ENV} is {want!r}, which is not this file's SHA-256 in lowercase "
            "hexadecimal, so there is nothing to check the running driver against"
        )
    here = resolved(__file__, "this step's driver")
    got = hashed(here, RUNNER_LIMIT_BYTES, "this step's driver")
    if got != want:
        raise Refused(
            f"the driver running is {here}, whose bytes hash to {got}, not the {want} "
            f"{DRIVER_SHA_ENV} pins, so it is not the driver this step was declared with"
        )
    return here


def trusted_runner(env, driver, outside_of):
    """The proof runner this step will start, checked against its pinned hash.

    `CONTROL_RUNNER` names it and `CONTROL_RUNNER_SHA256` its exact bytes, both
    from the step's own declaration rather than from the input, so nothing the
    change carries can name the file that decides its verdict. The candidate's
    copy of `scripts/controls-check.sh` is never started: a change could put a
    stub there that prints the passing line for every row without switching
    anything off, which is the very thing this step exists to catch.

    The name may be written either way. An absolute one is used as it stands. A
    relative one is taken against `driver`'s own directory, which is the entry
    point the atom's `exec.command` already names and whose bytes the step pins:
    a declaration in a repository can only hold a path of its own, and a
    checkout gets moved, copied to another machine and mounted in a container,
    so a path resolved against this file is the same runner before and after
    while an absolute one would name whatever stands where it was written.
    Resolution is against the driver, never against the working directory or
    `PATH`, and the pinned hash is checked over the bytes either way.

    Answers (the runner's own path, its hash), the hash being what every row is
    checked against again when it has ended.
    """
    named = env.get(RUNNER_ENV, "")
    want = env.get(RUNNER_SHA_ENV, "")
    if not named:
        raise Refused(
            f"{RUNNER_ENV} is not set to a path, so there is no {CHECK_SCRIPT} this step may run"
        )
    if not os.path.isabs(named):
        named = os.path.join(os.path.dirname(driver), named)
    if not SHA256_RE.fullmatch(want or ""):
        raise Refused(
            f"{RUNNER_SHA_ENV} is {want!r}, which is not a SHA-256 in lowercase hexadecimal, "
            "so there is nothing to check the proof runner against"
        )
    here = resolved(named, "the proof runner")
    outside(here, outside_of, "the proof runner")
    got = hashed(here, RUNNER_LIMIT_BYTES, "the proof runner")
    if got != want:
        raise Refused(
            f"the proof runner is {here}, whose bytes hash to {got}, not the {want} "
            f"{RUNNER_SHA_ENV} pins, so it is not the runner this step was declared with"
        )
    return here, got


def place_runner(runner, where, name):
    """Where to start the proof runner for row `name`: a fresh link to it, held
    in a directory of that row's own inside the copy.

    `scripts/controls-check.sh` takes the tree it edits from its own location,
    so the runner has to be started from somewhere inside the copy. It is a link
    rather than a copy of the file, so that the bytes the kernel reads are the
    pinned ones, and it is a fresh link for every row, so that a file a row's own
    command left behind cannot be what the next row starts. What a row's command
    can still do is write *through* this link, which is why its bytes are hashed
    again when the row ends.
    """
    holder = os.path.join(where, f"{RUNNER_HOLDER}-{name}")
    script = os.path.join(holder, os.path.basename(CHECK_SCRIPT))
    try:
        os.mkdir(holder)
        os.symlink(runner, script)
    except OSError as e:
        raise Refused(f"cannot place the proof runner for {name}: {e.strerror or e}") from e
    return script


def row_files(where, names):
    """The rows named, read from the copy, refusing any that is not there.

    The rows come from the candidate commit, the same tree the controls are
    switched off in, so a change cannot declare a control its own tree does not
    hold. The file each row names is returned with it, because two rows naming
    one file cannot run in one tree.
    """
    files = {}
    for name in names:
        path = os.path.join(where, ROWS_DIR, f"{name}.row")
        try:
            info = os.lstat(path)
        except FileNotFoundError:
            raise Refused(
                f"{name}: the candidate holds no {ROWS_DIR}/{name}.row, so it declares a "
                "control that is not in the change"
            ) from None
        except OSError as e:
            raise Refused(f"{name}: {ROWS_DIR}/{name}.row cannot be read: {e.strerror or e}") from e
        if not stat.S_ISREG(info.st_mode):
            raise Refused(
                f"{name}: {ROWS_DIR}/{name}.row is not a regular file, so it holds no control"
            )
        try:
            with open(path, "rb") as handle:
                raw = handle.read(65536)
        except OSError as e:
            raise Refused(f"{name}: {ROWS_DIR}/{name}.row cannot be read: {e.strerror or e}") from e
        fields = raw.decode("utf-8", "replace").split("\n")[0].split("|")
        if len(fields) != 5:
            raise Refused(
                f"{name}: {ROWS_DIR}/{name}.row does not hold the five fields a row must; "
                f"{CHECK_SCRIPT} reads it the same way and refuses it too"
            )
        files[name] = fields[1]
    return files


def selected_targets(row_args):
    """The Cargo packages and test targets a row asks the trusted runner to run.

    A positional libtest filter is deliberately ignored here: it can narrow
    cases within a target, but cannot change the target's harness or source.
    """
    try:
        words = shlex.split(row_args)
    except ValueError as e:
        raise Refused(f"control row has malformed Cargo arguments: {e}") from e
    packages, targets = [], []
    workspace = False
    index = 0
    while index < len(words):
        word = words[index]
        if word == "--":
            break
        if word in ("-p", "--package", "--test", "--bin", "--example", "--features"):
            if index + 1 >= len(words):
                raise Refused(f"control row ends after Cargo option {word}")
            value = words[index + 1]
            if word in ("-p", "--package"):
                packages.append(value)
            elif word in ("--test", "--bin", "--example"):
                targets.append((word[2:], value))
            index += 2
            continue
        if word.startswith("--package="):
            packages.append(word.split("=", 1)[1])
        elif word.startswith(("--test=", "--bin=", "--example=")):
            kind, value = word[2:].split("=", 1)
            targets.append((kind, value))
        elif word.startswith("--features="):
            pass
        elif word in ("--lib", "--bins", "--tests", "--examples", "--doc", "--all-targets"):
            targets.append((word[2:], None))
        elif word == "--workspace":
            workspace = True
        elif word in ("--all-features", "--no-default-features"):
            pass
        elif word.startswith("-"):
            raise Refused(f"control row uses unsupported Cargo selector {word}")
        index += 1
    if not targets:
        raise Refused("control row has no explicit Cargo target selector; default target enumeration is unsupported")
    if any(kind in ("tests", "examples", "all-targets") for kind, _ in targets):
        raise Refused("control row uses a broad Cargo target selector whose effective libtest set is unsupported")
    if workspace and packages:
        raise Refused("control row combines --workspace with -p; its effective package selection is unsupported")
    return packages, workspace, targets


def manifest_targets(git, source, env, commit, paths, room):
    """Resolve the workspace's ordinary Cargo targets from pinned tree bytes."""
    if "Cargo.toml" not in paths:
        raise Refused(f"{commit} has no Cargo.toml for selected control targets")

    def read(path):
        if path not in paths:
            raise Refused(f"{commit} has no {path} for selected control targets")
        try:
            return tomllib.loads(git_run(git, source, env, ["show", f"{commit}:{path}"], room()))
        except tomllib.TOMLDecodeError as e:
            raise Refused(f"{commit}:{path} is not a valid Cargo manifest: {e}") from e

    root = read("Cargo.toml")
    members = [""] if "package" in root else []
    workspace = root.get("workspace", {})
    if "default-members" in workspace:
        raise Refused(f"{commit}: workspace default-members need effective Cargo resolution")
    workspace_members = workspace.get("members", [])
    if not isinstance(workspace_members, list):
        raise Refused(f"{commit}: workspace members are not an explicit list")
    for member in workspace_members:
        if not isinstance(member, str) or not member or any(ch in member for ch in "*?[]"):
            raise Refused(f"{commit}: workspace member {member!r} is not an explicit path")
        members.append(member.rstrip("/"))
    excludes = workspace.get("exclude", [])
    if not isinstance(excludes, list) or any(
        not isinstance(path, str) or any(ch in path for ch in "*?[]") for path in excludes
    ):
        raise Refused(f"{commit}: workspace excludes need effective Cargo resolution")
    excludes = {path.rstrip("/") for path in excludes}
    workspace_dependencies = workspace.get("dependencies", {})
    if not isinstance(workspace_dependencies, dict):
        raise Refused(f"{commit}: workspace dependencies are malformed")

    def dependency_maps(manifest):
        for section in (
            "dependencies", "dev-dependencies", "build-dependencies",
            "dev_dependencies", "build_dependencies",
        ):
            values = manifest.get(section, {})
            if isinstance(values, dict):
                yield values
        targets = manifest.get("target", {})
        if isinstance(targets, dict):
            for cfg in targets.values():
                if isinstance(cfg, dict):
                    for section in (
                        "dependencies", "dev-dependencies", "build-dependencies",
                        "dev_dependencies", "build_dependencies",
                    ):
                        values = cfg.get(section, {})
                        if isinstance(values, dict):
                            yield values

    # Cargo automatically includes in-tree path dependencies as workspace
    # members. Close over every committed dependency table, including target
    # and workspace dependencies, before comparing the selected package set.
    index = 0
    while index < len(members):
        member = members[index]
        index += 1
        manifest = root if not member else read(f"{member}/Cargo.toml")
        for deps, origin in [
            (workspace_dependencies, ""),
            *((values, member) for values in dependency_maps(manifest)),
        ]:
            for alias, spec in deps.items():
                if not isinstance(spec, dict):
                    continue
                dep_origin = origin
                if spec.get("workspace") is True:
                    spec = workspace_dependencies.get(alias, {})
                    dep_origin = ""
                dep_path = spec.get("path") if isinstance(spec, dict) else None
                if dep_path is None:
                    continue
                if not isinstance(dep_path, str) or not dep_path or os.path.isabs(dep_path):
                    raise Refused(f"{commit}: path dependency {alias} cannot be resolved")
                dep_member = os.path.normpath(os.path.join(dep_origin, dep_path)).replace(os.sep, "/")
                if dep_member in (".", ""):
                    dep_member = ""
                if dep_member.startswith("../") or dep_member == "..":
                    raise Refused(
                        f"{commit}: path dependency {alias} escapes the admitted workspace"
                    )
                if dep_member in excludes or dep_member in members:
                    continue
                if f"{dep_member}/Cargo.toml" in paths:
                    members.append(dep_member)
                else:
                    raise Refused(
                        f"{commit}: in-tree path dependency {alias} at {dep_member} has no committed Cargo.toml"
                    )
    packages = {}

    def target_path(member, path):
        if not isinstance(path, str) or not path or os.path.isabs(path):
            raise Refused(f"{commit}: Cargo target path {path!r} is not relative")
        result = os.path.normpath(os.path.join(member, path)).replace(os.sep, "/")
        if result.startswith("../") or result == "..":
            raise Refused(f"{commit}: Cargo target path {path!r} escapes its package")
        return result

    for member in members:
        manifest_path = f"{member}/Cargo.toml" if member else "Cargo.toml"
        manifest = read(manifest_path) if member else root
        package = manifest.get("package", {})
        name = package.get("name")
        if not isinstance(name, str) or not name:
            raise Refused(f"{commit}:{manifest_path} has no package name")
        if name in packages:
            raise Refused(f"{commit} has duplicate Cargo package {name}")
        found = {}

        def add(kind, target_name, path, spec):
            if not isinstance(target_name, str) or not target_name:
                raise Refused(f"{commit}:{manifest_path} has an unnamed {kind} target")
            path = target_path(member, path)
            if path not in paths:
                raise Refused(f"{commit}:{manifest_path} selects absent {kind} target {path}")
            harness = spec.get("harness", True)
            enabled = spec.get("test", True)
            doctest = spec.get("doctest", True)
            if not all(isinstance(value, bool) for value in (harness, enabled, doctest)):
                raise Refused(f"{commit}:{manifest_path} has a non-boolean target gate")
            key = (kind, target_name)
            if key in found:
                raise Refused(f"{commit}:{manifest_path} selects {kind} {target_name} twice")
            found[key] = (path, harness, enabled, doctest, paths[path])

        lib = manifest.get("lib")
        lib_default = target_path(member, "src/lib.rs")
        if lib is not None or lib_default in paths:
            spec = lib or {}
            add("lib", spec.get("name", name.replace("-", "_")), spec.get("path", "src/lib.rs"), spec)
        for kind, directory in (("test", "tests"), ("bin", "src/bin"), ("example", "examples")):
            explicit = manifest.get(kind, [])
            if not isinstance(explicit, list):
                raise Refused(f"{commit}:{manifest_path} has malformed [[{kind}]] targets")
            for spec in explicit:
                target_name = spec.get("name")
                if not isinstance(target_name, str) or not target_name:
                    raise Refused(f"{commit}:{manifest_path} has an unnamed [[{kind}]]")
                add(kind, target_name, spec.get("path", f"{directory}/{target_name}.rs"), spec)
            auto = package.get({"test": "autotests", "bin": "autobins", "example": "autoexamples"}[kind], True)
            if not isinstance(auto, bool):
                raise Refused(f"{commit}:{manifest_path} has a non-boolean auto target setting")
            if auto:
                prefix = f"{member}/{directory}/" if member else f"{directory}/"
                for path in paths:
                    if not path.startswith(prefix) or not path.endswith(".rs"):
                        continue
                    tail = path[len(prefix):]
                    if "/" not in tail:
                        target_name = tail[:-3]
                    elif tail.endswith("/main.rs") and tail.count("/") == 1:
                        target_name = tail.split("/", 1)[0]
                    else:
                        continue
                    if (kind, target_name) not in found:
                        add(kind, target_name, path[len(member) + 1:] if member else path, {})
        main = target_path(member, "src/main.rs")
        if main in paths and ("bin", name) not in found:
            add("bin", name, "src/main.rs", {})
        packages[name] = found
    return packages


def verify_row_harnesses(git, source, env, base, commit, copy, names, room):
    """Refuse a selected Cargo target that is custom or changed from base."""
    base_paths = {path: mode for mode, path, _ in ls_tree(git, source, env, base, room())}
    candidate_paths = {path: mode for mode, path, _ in ls_tree(git, source, env, commit, room())}
    base_packages = manifest_targets(git, source, env, base, base_paths, room)
    candidate_packages = manifest_targets(git, source, env, commit, candidate_paths, room)
    for name in names:
        row = os.path.join(copy, ROWS_DIR, f"{name}.row")
        with open(row, encoding="utf-8") as handle:
            fields = handle.readline().rstrip("\n").split("|")
        if len(fields) != 5:
            raise Refused(f"{name} has no valid Cargo target declaration")
        package_names, _workspace, selectors = selected_targets(fields[4])
        if not package_names and set(base_packages) != set(candidate_packages):
            raise Refused(f"{name} changes the workspace package set selected by its Cargo row")
        selected_packages = package_names or list(base_packages)
        if not selected_packages:
            raise Refused(f"{name} selects no Cargo package")
        for package_name in selected_packages:
            if package_name not in base_packages or package_name not in candidate_packages:
                raise Refused(f"{name} selects package {package_name}, absent at base or candidate")
        for selector, wanted in selectors:
            matched = False
            for package_name in selected_packages:
                before, after = base_packages[package_name], candidate_packages[package_name]
                if selector in ("lib", "doc"):
                    keys = [key for key in before if key[0] == "lib"]
                elif selector in ("test", "bin", "example"):
                    key = (selector, wanted)
                    keys = [key] if key in before or key in after else []
                elif selector in ("tests", "bins", "examples"):
                    kind = selector[:-1]
                    keys = [key for key in before if key[0] == kind]
                else:
                    keys = list(before)
                matched |= bool(keys)
                if selector in ("lib", "doc", "tests", "bins", "examples", "all"):
                    kind = {"lib": "lib", "doc": "lib", "tests": "test", "bins": "bin", "examples": "example"}.get(selector)
                    after_keys = [key for key in after if kind is None or key[0] == kind]
                    if set(keys) != set(after_keys):
                        raise Refused(f"{name} changes selected Cargo target set for {package_name}")
                for key in keys:
                    old, new = before.get(key), after.get(key)
                    if new is not None and (not new[1] or not new[2] or (selector == "doc" and not new[3])):
                        raise Refused(f"{name} selects a nonstandard or disabled libtest harness for {package_name}:{key}")
                    if old is not None and (not old[1] or not old[2] or (selector == "doc" and not old[3])):
                        raise Refused(f"{name} selects a nonstandard or disabled libtest harness at base for {package_name}:{key}")
                    if (new is not None and new[4] == "120000") or (old is not None and old[4] == "120000"):
                        raise Refused(f"{name} selects a symlinked Cargo target source for {package_name}:{key}")
                    if old is None or new is None or old != new:
                        raise Refused(f"{name} changes selected Cargo target {package_name}:{key}")
            if not matched:
                raise Refused(f"{name} selects {selector} {wanted or ''} absent at base")


def classify(name, code, timed_out, overflow, out, err):
    """One row's verdict, from how `scripts/controls-check.sh` ended.

    Only the script's own last word about this row decides. Anything else -- no
    result line, a reason this list does not hold, an ok line under a nonzero
    exit, a FAIL line that still exited 0, an output too big to have read -- is
    an error, which fails the step.
    """
    text = (out + b"\n" + err).decode("utf-8", "replace")
    if timed_out:
        return "timed-out", f"ran past its cap and was stopped; the last line was: {tail(text)}"
    if overflow:
        return "errored", "wrote more than its output limit, so its result was not read"
    if code is None:
        return "errored", "was stopped without ever giving an exit code, so its result is unknown"
    ok_prefix, fail_prefix = f"ok {name}: ", f"FAIL {name}: "
    line = ""
    for candidate in text.splitlines():
        if candidate.startswith(ok_prefix) or candidate.startswith(fail_prefix):
            line = candidate
    if not line:
        return "errored", f"ended with no result line (exit {code}); the last line was: {tail(text)}"
    ok = line.startswith(ok_prefix)
    reason = line[len(ok_prefix) if ok else len(fail_prefix) :]
    verdict = next((found for needle, found in REASONS if reason.startswith(needle)), None)
    if verdict is None:
        return "errored", f"ended with a reason this step does not know: {reason}"
    if ok and verdict != "passed":
        return "errored", f"ended ok while saying {reason}"
    if verdict == "passed" and code != 0:
        return "errored", f"ended with its ok line and exit {code}: {reason}"
    if not ok and code == 0:
        return "errored", f"ended {reason} and still exited 0"
    return verdict, reason


def tail(text, limit=300):
    lines = [line for line in text.strip().splitlines() if line.strip()]
    return lines[-1][:limit] if lines else "nothing"


def parse_input(raw):
    """What the step was asked to prove, or a refusal.

    The commit and the ref it is bound to are checked before either reaches git,
    so that a value from a workflow can only ever be an id or a ref atomic keeps
    under `refs/atomic/`, and never an option or a revision that means something
    else. Every row named is run: the input is the whole declared set, so an
    input whose rows cannot all fit the step's own wall time is refused here,
    where the reason can be said, rather than started and left half-proved.
    """
    try:
        data = json.loads(raw)
    except (json.JSONDecodeError, UnicodeDecodeError) as e:
        raise Refused(f"the input is not JSON: {e}") from e
    if not isinstance(data, dict):
        raise Refused("the input is not a JSON object")
    unknown = sorted(
        set(data)
        - {
            "source",
            "commit",
            "bound",
            "rows",
            "row_seconds",
            "setup_seconds",
            "report_seconds",
            "step_seconds",
        }
    )
    if unknown:
        raise Refused(f"the input holds fields this step does not read: {', '.join(unknown)}")
    for field in ("source", "commit", "bound", "rows"):
        if field not in data:
            raise Refused(f"the input holds no {field}")
    source, commit, names, bound = data["source"], data["commit"], data["rows"], data["bound"]
    if not isinstance(source, str) or not source:
        raise Refused("source is not a path")
    if not os.path.isabs(source):
        raise Refused(f"source is {source!r}, which is not an absolute path")
    if not isinstance(commit, str) or not SHA_RE.fullmatch(commit):
        raise Refused(
            f"commit is {commit!r}, which is not a lowercase hexadecimal commit id"
        )
    if not isinstance(bound, dict):
        raise Refused(
            "bound is not an object naming the ref the candidate is kept under and the base "
            "recorded with it"
        )
    unknown = sorted(set(bound) - {"reference", "base", "admitted"})
    if unknown:
        raise Refused(f"bound holds fields this step does not read: {', '.join(unknown)}")
    for field in ("reference", "base", "admitted"):
        if field not in bound:
            raise Refused(f"bound holds no {field}")
    reference, base, admitted = bound["reference"], bound["base"], bound["admitted"]
    if not isinstance(reference, str) or not BOUND_RE.fullmatch(reference):
        raise Refused(
            f"bound.reference is {reference!r}, which is not a ref atomic keeps a step's "
            "attempt under, of the form refs/atomic/<run>/steps/<step>/<attempt>"
        )
    if not isinstance(base, str) or not SHA_RE.fullmatch(base):
        raise Refused(
            f"bound.base is {base!r}, which is not a lowercase hexadecimal commit id"
        )
    if not isinstance(admitted, str) or not SHA_RE.fullmatch(admitted):
        raise Refused(
            f"bound.admitted is {admitted!r}, which is not a lowercase hexadecimal commit id"
        )
    if not isinstance(names, list) or not names:
        raise Refused(
            f"rows is not a non-empty list of control row names, so no control would be checked"
        )
    seen = []
    for name in names:
        if not isinstance(name, str) or not NAME_RE.fullmatch(name):
            raise Refused(f"{name!r} is not a control row name")
        if name in seen:
            raise Refused(f"{name} is declared twice")
        seen.append(name)

    def whole(field, default, low, high, why):
        if field not in data:
            return default
        found = data[field]
        if not isinstance(found, int) or isinstance(found, bool) or not low <= found <= high:
            raise Refused(
                f"{field} is {found!r}, which is not a whole number from {low} to {high}; {why}"
            )
        return found

    row_seconds = whole(
        "row_seconds", ROW_SECONDS_DEFAULT, ROW_SECONDS_MIN, ROW_SECONDS_MAX,
        "the rows share the step's own wall time",
    )
    setup_seconds = whole(
        "setup_seconds", SETUP_SECONDS_DEFAULT, SETUP_SECONDS_MIN, SETUP_SECONDS_MAX,
        "reading the candidate out of git and copying it needs room of its own",
    )
    report_seconds = whole(
        "report_seconds", REPORT_SECONDS_DEFAULT, REPORT_SECONDS_MIN, REPORT_SECONDS_MAX,
        "the last row has to be stopped, the copy removed and the answer written before "
        "the command ends",
    )
    step_seconds = whole(
        "step_seconds", STEP_SECONDS_DEFAULT, STEP_SECONDS_MIN, STEP_SECONDS_MAX,
        "the exec command this runs inside has five minutes and no more",
    )
    if setup_seconds + row_seconds * len(seen) + report_seconds > step_seconds:
        raise Refused(
            f"{len(seen)} row(s) at {row_seconds}s beside a {setup_seconds}s setup reserve "
            f"and a {report_seconds}s reserve for the answer do not fit the step's "
            f"{step_seconds}s, and a row with no room left is a control that is never "
            "proved; give each row a step of its own"
        )
    return {
        "source": source,
        "commit": commit,
        "reference": reference,
        "base": base,
        "admitted": admitted,
        "rows": seen,
        "row_seconds": row_seconds,
        "setup_seconds": setup_seconds,
        "report_seconds": report_seconds,
        "step_seconds": step_seconds,
    }


def report(payload, note):
    """Write the result: one JSON value on stdout, one short line per row on
    stderr, and the summary last.

    atomic keeps the end of a failed command's stderr in the step's failure, so
    what decides the step is said last, in as few lines as it takes. The answer
    is flushed before anything else happens, because the step's verdict matters
    more than its tidiness: what comes after this is the removal of the copy,
    which can take longer than the report reserve the step kept back for it, and
    a command killed halfway through a write leaves the verdict unreadable.
    """
    json.dump(payload, sys.stdout, sort_keys=True)
    sys.stdout.write("\n")
    sys.stdout.flush()
    for row in payload["rows"]:
        print(f"{row['name']}: {row['verdict']}: {row['detail']}", file=sys.stderr)
    counts = ", ".join(f"{row['name']} {row['verdict']}" for row in payload["rows"])
    print(f"{note}: {counts}" if counts else note, file=sys.stderr)
    sys.stderr.flush()


def answer_room(want, payload, left):
    """Refuse the answer the step has no room left to write, rather than start one.

    Called when the rows have ended and the JSON value is next. `report_seconds`
    of the step's wall time was held back for exactly this moment, so reaching
    it with less than `MIN_SECONDS` left means something outside this step's
    accounting took the time -- and a verdict written under a kill that is
    already on its way is worse than no verdict, because it may be read as a
    half of one. So every row already gathered is dropped, and what is reported
    is that nothing can be claimed: a row this step did not see stopped is a
    control in an unknown state, and it is named as unknown rather than as
    whatever it happened to say last.
    """
    if left >= MIN_SECONDS:
        return
    payload["rows"] = []
    payload["passed"] = 0
    payload["failed"] = 0
    payload["ok"] = False
    raise Refused(
        f"the step's own {want['step_seconds']}s had {left:.1f}s left when its rows ended, "
        f"less than the {MIN_SECONDS}s it needs to write an answer at all, so no row's "
        "verdict is claimed"
    )


def check_scratch(source):
    """The directory this step may write in, refusing anything else.

    An exec atom's command is given its own `TMPDIR` and may write nowhere
    else; the copy, and every control switched off in it, has to live there.
    """
    tmp = os.environ.get("TMPDIR", "")
    if not tmp or not os.path.isabs(tmp):
        raise Refused(
            "TMPDIR is not set to a directory of its own, which is where this step's copy "
            "and every control it switches off must live"
        )
    try:
        info = os.stat(tmp)
    except OSError as e:
        raise Refused(f"TMPDIR is {tmp!r}, which cannot be read: {e.strerror or e}") from e
    if not stat.S_ISDIR(info.st_mode):
        raise Refused(f"TMPDIR is {tmp!r}, which is not a directory")
    if not os.access(tmp, os.W_OK | os.X_OK):
        raise Refused(f"TMPDIR is {tmp!r}, which this step cannot write")
    if os.path.realpath(tmp) == os.path.realpath(source):
        raise Refused("TMPDIR is the source, so the copy would be the change it switches off")
    return tmp


def main():
    raw = sys.stdin.buffer.read()
    payload = {
        "ok": False,
        "error": None,
        "source": None,
        "commit": None,
        "tree": None,
        "bound": None,
        "rows": [],
        "passed": 0,
        "failed": 0,
    }
    scratch = None
    note = "controls-exec: refused"
    try:
        git = shutil.which("git")
        if not git:
            raise Refused("git is not on PATH, so no candidate can be read out of it")
        bash = shutil.which("bash")
        if not bash:
            raise Refused("bash is not on PATH, so the proof runner cannot be started")
        env = git_env(os.environ)
        want = parse_input(raw.decode("utf-8"))
        source, commit, names = want["source"], want["commit"], want["rows"]
        payload.update({"source": source, "commit": commit})
        tmp = check_scratch(source)
        try:
            info = os.stat(source)
        except OSError as e:
            raise Refused(f"source is {source!r}, which cannot be read: {e.strerror or e}") from e
        if not stat.S_ISDIR(info.st_mode):
            raise Refused(f"source is {source!r}, which is not a directory")
        # What is proved, and with what, is settled before anything is read out
        # of git or switched off: the candidate must be the commit atomic
        # recorded, and both this file and the proof runner must be the bytes
        # the step's declaration named.
        driver = check_driver(env)
        runner, runner_sha = trusted_runner(
            env,
            driver,
            [
                (resolved(source, "the source"), "the change"),
                (resolved(tmp, "TMPDIR"), "this step's own scratch"),
            ],
        )
        payload["bound"] = {
            "reference": want["reference"],
            "base": want["base"],
            "admitted": want["admitted"],
        }
        deadline = time.monotonic() + want["step_seconds"]
        # Everything this step starts has to be finished, and its group stopped,
        # before the answer is written: `report_seconds` of the step's wall time
        # is held back for that, so nothing is ever given time that runs into
        # the end the backend enforces. A row that reaches its own limit is
        # stopped, reported and cleaned up inside this command's own budget,
        # rather than by the kill that ends the command.
        stop_by = deadline - want["report_seconds"]

        def room():
            """How long the next thing may still take, refused once the step's
            own budget has no real work left in it. The answer and whatever has
            to happen before it -- stopping a group, removing the copy -- live
            in the reserve under `stop_by`, so setup stops short of that."""
            left = stop_by - time.monotonic()
            if left < MIN_SECONDS:
                raise Refused(
                    f"the step's own {want['step_seconds']}s had less than {MIN_SECONDS}s "
                    f"left for work once its {want['report_seconds']}s answer reserve was "
                    "kept back, so nothing more was started and no control was switched off"
                )
            return min(GIT_SECONDS, left)

        check_bound(git, source, env, commit, want["reference"], want["base"], room())
        tree = tree_of(git, source, env, commit, room())
        payload["tree"] = tree
        entries = ls_tree(git, source, env, commit, room())
        if not entries:
            raise Refused(f"{commit} holds no files, so it holds no control to switch off")
        store = object_store(git, source, env, room)
        try:
            scratch = tempfile.mkdtemp(prefix="controls-exec-", dir=tmp)
        except OSError as e:
            raise Refused(f"cannot make a copy under {tmp}: {e.strerror or e}") from e
        copy = os.path.join(scratch, "copy")
        os.mkdir(copy)
        copy_out(git, source, env, entries, copy, room())
        bind_copy(git, copy, env, store, commit, tree, room)
        held = {}
        for name, path in row_files(copy, names).items():
            if path == CHECK_SCRIPT:
                raise Refused(
                    f"{name} switches {CHECK_SCRIPT} itself, which is the script that proves "
                    "controls: that control needs a check of its own, because the runner "
                    "cannot prove itself"
                )
            if path in held:
                raise Refused(
                    f"{name} and {held[path]} both switch {path}, and one run of "
                    f"{CHECK_SCRIPT} saves what the other switches off and puts that back; "
                    "give them steps of their own"
                )
            held[path] = name
        # A row can claim to run a test that is really a custom executable:
        # `harness = false` can print libtest-shaped lines without running any
        # test. Compare only the packages and targets the row selects, using
        # committed manifests at both the admitted base and candidate.
        verify_row_harnesses(git, source, env, want["admitted"], commit, copy, names, room)
        for index, name in enumerate(names):
            open_now = stop_by - time.monotonic()
            if open_now < MIN_SECONDS:
                # Every declared row gets an answer, even the ones the step's
                # own clock stopped it from running: a control left unproved is
                # reported as unproved and fails the step, never left out.
                for skipped in names[index:]:
                    payload["rows"].append(
                        {
                            "name": skipped,
                            "verdict": NO_TIME,
                            "detail": (
                                f"the step's own {want['step_seconds']}s had less than "
                                f"{MIN_SECONDS}s of work left in it, once its "
                                f"{want['report_seconds']}s answer reserve is kept back, "
                                "when its turn came, so its control was never switched off"
                            ),
                        }
                    )
                break
            script = place_runner(runner, copy, name)
            code, out, err, timed_out, overflow = run(
                [bash, script, name],
                cwd=copy,
                env=env,
                seconds=min(open_now, want["row_seconds"]),
                out_limit=ROW_OUTPUT_LIMIT,
                deadline=stop_by,
            )
            verdict, detail = classify(name, code, timed_out, overflow, out, err)
            # The row's command runs inside the copy, where it can reach the
            # link its own row was started through and write through it to the
            # trusted file. The bytes behind that link are checked again here,
            # so that a row which replaced its own proof reports that instead of
            # a verdict, and nothing after it is believed.
            try:
                again = hashed(script, RUNNER_LIMIT_BYTES, "the proof runner")
                changed, why = again != runner_sha, f"its bytes are now {again}"
            except Refused as e:
                changed, why = True, f"it can no longer be read: {e}"
            if changed:
                verdict = RUNNER_CHANGED
                detail = (
                    f"the proof runner is not the {runner_sha} this step started with, so "
                    f"nothing this row reported is the trusted runner's: {why}; " + detail
                )
            if os.path.exists(os.path.join(copy, STATE_DIR)):
                verdict = "left-switched"
                detail = (
                    "left its saved originals in the copy, so a control may still be off; "
                    + detail
                )
            payload["rows"].append({"name": name, "verdict": verdict, "detail": detail})
            if verdict == RUNNER_CHANGED:
                for skipped in names[index + 1 :]:
                    payload["rows"].append(
                        {
                            "name": skipped,
                            "verdict": RUNNER_CHANGED,
                            "detail": (
                                "was never run, because the trusted proof runner had already "
                                "been replaced by the time its turn came"
                            ),
                        }
                    )
                break
        payload["passed"] = sum(1 for row in payload["rows"] if row["verdict"] == "passed")
        payload["failed"] = len(payload["rows"]) - payload["passed"]
        payload["ok"] = payload["failed"] == 0
        counted = [(BLIND, "blind"), (NO_TIME, "unrun"), (RUNNER_CHANGED, "runner replaced")]
        note = f"controls-exec: {payload['passed']} of {len(names)} rows passed"
        for verdict, word in counted:
            named = [row["name"] for row in payload["rows"] if row["verdict"] == verdict]
            if named:
                note += f"; {len(named)} {word} ({', '.join(named)})"
        # Last of all, and still inside the reporting: if the step's own clock
        # has no room left for an answer, say that instead of writing verdicts
        # the backend's kill may reach before they are read.
        answer_room(want, payload, deadline - time.monotonic())
    except Refused as e:
        payload["error"] = str(e)
        note = f"controls-exec: refused: {e}"
    except Exception as e:  # never a traceback on stdout, and never a pass
        payload["error"] = f"{type(e).__name__}: {e}"
        note = f"controls-exec: refused: {payload['error']}"
    report(payload, note)
    if scratch:
        shutil.rmtree(scratch, ignore_errors=True)
    if payload["ok"]:
        return EXIT_PASS
    return EXIT_REFUSED if payload["error"] else EXIT_ROWS_FAILED


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)
