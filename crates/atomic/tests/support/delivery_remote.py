#!/usr/bin/python3
# SPDX-License-Identifier: Apache-2.0
"""Stateful test adapter, installed only in an isolated project's private state."""

import fcntl
import json
import os
import pathlib
import sys

store = pathlib.Path("remote-state.json")
message = json.load(sys.stdin)
slot = message["slot"]
key = message["key"]
request = message["request"]
number = message["number"]
assert message["version"] == 1
assert len(key) == 64

# File lock and atomic replace make the fake's own mutations and counters
# visible across independent CLI processes and SIGKILL recovery.
with open("remote-state.lock", "a") as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    state = json.loads(store.read_text()) if store.exists() else {
        "refs": {}, "prs": {}, "queue": {}, "keys": {}, "next_pr": 1,
        "mutations": {}, "apply_calls": {}, "inspect_calls": {},
    }
    ref = json.dumps([request["repository"], request["branch_ref"]])
    pr = json.dumps([request["repository"], request["branch_ref"], request.get("base_ref")])
    if message["method"] == "inspect":
        counts = state["inspect_calls"]
        counts[slot] = counts.get(slot, 0) + 1
    elif message["method"] == "apply":
        counts = state["apply_calls"]
        counts[slot] = counts.get(slot, 0) + 1
        identity = [slot, request, number]
        if key in state["keys"]:
            assert state["keys"][key] == identity
        else:
            if slot == "push":
                if state["refs"].get(ref) != request["expected_old"]:
                    sys.exit(4)
                state["refs"][ref] = request["head"]
            elif slot == "pull_request":
                assert state["refs"].get(ref) == request["head"]
                old = state["prs"].get(pr)
                expected = request["expected_number"]
                if expected is not None:
                    assert old is not None and old["number"] == expected
                if old is None:
                    pr_number = state["next_pr"]
                    state["next_pr"] += 1
                else:
                    assert old["open"]
                    pr_number = old["number"]
                state["prs"][pr] = dict(request, kind="pull_request", number=pr_number, open=True)
                del state["prs"][pr]["expected_number"]
            else:
                assert slot == "enqueue"
                actual = state["prs"][pr]
                assert actual["number"] == number and actual["open"]
                assert actual["head"] == state["refs"][ref] == request["head"]
                state["queue"][pr] = dict(request, kind="queue", number=number, status="queued")
            counts = state["mutations"]
            counts[slot] = counts.get(slot, 0) + 1
            state["keys"][key] = identity
    else:
        sys.exit(2)

    staging = store.with_suffix(".tmp")
    staging.write_text(json.dumps(state))
    os.replace(staging, store)
    if message["method"] == "apply" and pathlib.Path("drop-response-" + slot).exists():
        sys.exit(5)
    if message["method"] == "inspect" and pathlib.Path("unreadable-" + slot).exists():
        sys.exit(6)
    if slot == "push":
        head = state["refs"].get(ref)
        observed = None if head is None else dict(kind="push", repository=request["repository"],
                                                  branch_ref=request["branch_ref"], head=head)
    elif slot == "pull_request":
        observed = state["prs"].get(pr)
    else:
        observed = state["queue"].get(pr)
    print(json.dumps({"version": 1, "key": key, "state": observed}))
