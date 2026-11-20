"""Reading and checking scenario files."""

import json
from dataclasses import dataclass

from .jobs import FAIL, OK, Job
from .retry import RetryPolicy


@dataclass(frozen=True)
class Scenario:
    jobs: tuple
    retry: RetryPolicy


def load(path):
    try:
        with open(path, encoding="utf-8") as f:
            data = json.load(f)
    except OSError as e:
        raise ValueError(f"cannot read {path}: {e.strerror}") from None
    except json.JSONDecodeError as e:
        raise ValueError(f"{path} is not JSON: {e.msg} at line {e.lineno}") from None
    return from_dict(data)


def from_dict(data):
    if not isinstance(data, dict):
        raise ValueError("a scenario must be a JSON object")
    retry = _retry(data.get("retry", {}))
    raw_jobs = data.get("jobs")
    if not isinstance(raw_jobs, list) or not raw_jobs:
        raise ValueError("a scenario needs a non-empty list of jobs")
    jobs = tuple(_job(index, raw) for index, raw in enumerate(raw_jobs, 1))
    names = [job.name for job in jobs]
    duplicates = sorted({name for name in names if names.count(name) > 1})
    if duplicates:
        raise ValueError(f"job names must be unique: {', '.join(duplicates)}")
    return Scenario(jobs=jobs, retry=retry)


def _whole(raw, key, default, where, minimum=0):
    value = raw.get(key, default)
    if not isinstance(value, int) or isinstance(value, bool) or value < minimum:
        raise ValueError(f"{where}: {key} must be a whole number of at least {minimum}, not {value!r}")
    return value


def _retry(raw):
    if not isinstance(raw, dict):
        raise ValueError("retry must be an object")
    unknown = sorted(set(raw) - {"base", "factor", "max_delay"})
    if unknown:
        raise ValueError(f"retry: unknown {', '.join(unknown)}")
    defaults = RetryPolicy()
    return RetryPolicy(
        base=_whole(raw, "base", defaults.base, "retry", 1),
        factor=_whole(raw, "factor", defaults.factor, "retry", 1),
        max_delay=_whole(raw, "max_delay", defaults.max_delay, "retry", 1),
    )


def _job(index, raw):
    where = f"job {index}"
    if not isinstance(raw, dict):
        raise ValueError(f"{where}: must be an object")
    name = raw.get("name")
    if not isinstance(name, str) or not name or " " in name:
        raise ValueError(f"{where}: name must be a non-empty string without spaces")
    where = f"job {name}"
    outcomes = raw.get("outcomes", [])
    if not isinstance(outcomes, list) or any(o not in (OK, FAIL) for o in outcomes):
        raise ValueError(f"{where}: outcomes must be a list of \"ok\" and \"fail\"")
    priority = raw.get("priority", 0)
    if not isinstance(priority, int) or isinstance(priority, bool):
        raise ValueError(f"{where}: priority must be a whole number")
    return Job(
        name=name,
        at=_whole(raw, "at", None, where),
        priority=priority,
        duration=_whole(raw, "duration", 1, where),
        outcomes=tuple(outcomes),
        max_attempts=_whole(raw, "max_attempts", 3, where, 1),
    )
