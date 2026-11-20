# jobsim

A simulator for the batch scheduler that runs Quillmoor's nightly jobs. Before the real scheduler's
settings change, the operations team writes the night as a scenario and reads the trace this prints,
so the trace must follow the scheduler's rules exactly.

```
python3 -m jobsim simulate examples/scenario-retry.json   # the trace
python3 -m jobsim check examples/*.json                   # check scenarios without running them
python3 -m jobsim backoff examples/scenario-retry.json    # the retry waits: 5s 10s 20s ...
```

Exit codes: 0 when every job succeeded (or every scenario is good), 1 when a job gave up, 2 for bad
usage or a scenario that cannot be read.

Run the tests with:

```
python3 -m unittest discover -s tests -t .
```

## Scenarios

```json
{
  "retry": {"base": 5, "factor": 2, "max_delay": 60},
  "jobs": [
    {"name": "sync-ledger", "at": 0, "priority": 1, "duration": 4, "outcomes": ["fail", "ok"]}
  ]
}
```

Times are whole seconds from the start of the night.

- `at`: when the job is first due. Required.
- `priority`: a whole number, 0 by default. Higher goes first.
- `duration`: how long each attempt takes, 1 by default. A failing attempt takes as long as one that
  succeeds.
- `outcomes`: the outcome of attempts 1, 2, ... Attempts past the end of the list succeed.
- `max_attempts`: 3 by default. After that many failures the scheduler gives up on the job.
- `retry`: the backoff. The wait after attempt n fails is `base * factor ** (n - 1)` seconds, but
  never more than `max_delay`. The defaults are 5, 2 and 300.

## The scheduler's rules

1. There is one worker. It runs one attempt at a time, to the end, and an attempt that becomes due
   while another runs waits for the worker.
2. When the worker is free it takes the waiting attempt that has been due the longest, that is the
   one with the earliest due time. Among attempts due at the same moment, the one with the higher
   priority goes first, and among those with the same priority, the one that was **queued first**.
3. Every job's first attempt is queued when the night starts, in the order the scenario lists the
   jobs. A retry is queued when the attempt before it fails.
4. A retry is due `delay` seconds after the failed attempt **finished**, where `delay` is the wait
   the retry policy gives for that attempt's number.

## The trace

One line when an attempt starts, with its start time and its outcome, one when the scheduler gives
up on a job, at the moment the last attempt finished, and a last line once nothing is left:

```
t=<start> run <job> attempt <n> -> ok|fail
t=<finish> give up <job> after <n> attempts
done at t=<time>: <n> ok, <n> gave up, <n> runs
```

For `examples/scenario-retry.json` the trace is:

```
t=0 run sync-ledger attempt 1 -> fail
t=4 run build-index attempt 1 -> ok
t=9 run sync-ledger attempt 2 -> fail
t=13 run mail-digest attempt 1 -> fail
t=15 run purge-tmp attempt 1 -> ok
t=20 run mail-digest attempt 2 -> ok
t=23 run sync-ledger attempt 3 -> ok
done at t=27: 4 ok, 0 gave up, 7 runs
```

sync-ledger's first attempt runs from 0 to 4, so its retry is due at 4 + 5 = 9. Its second runs
from 9 to 13, so the third is due at 13 + 10 = 23.

## Layout

- `jobsim/clock.py`: simulated time.
- `jobsim/jobs.py`: jobs and queued attempts.
- `jobsim/queue.py`: the queue of attempts.
- `jobsim/retry.py`: the retry policy.
- `jobsim/runner.py`: the worker.
- `jobsim/trace.py`: the trace.
- `jobsim/scenario.py`: reading scenarios.
- `jobsim/cli.py`: the command line.
