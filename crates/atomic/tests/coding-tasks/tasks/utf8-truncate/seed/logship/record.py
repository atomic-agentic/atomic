"""Log records, as read from JSON lines."""

import json
from dataclasses import dataclass
from typing import Optional

SEVERITIES = {
    "emerg": 0,
    "alert": 1,
    "crit": 2,
    "err": 3,
    "warning": 4,
    "notice": 5,
    "info": 6,
    "debug": 7,
}


class RecordError(ValueError):
    pass


@dataclass(frozen=True)
class Record:
    time: int
    severity: str
    message: str
    host: Optional[str] = None
    app: Optional[str] = None

    def __post_init__(self):
        if isinstance(self.time, bool) or not isinstance(self.time, int):
            raise RecordError("time must be an integer")
        if self.severity not in SEVERITIES:
            raise RecordError(f"unknown severity {self.severity!r}")
        if not isinstance(self.message, str):
            raise RecordError("message must be a string")

    @property
    def severity_number(self):
        return SEVERITIES[self.severity]


def from_dict(data):
    if not isinstance(data, dict):
        raise RecordError("a record must be a JSON object")
    unknown = set(data) - {"time", "severity", "message", "host", "app"}
    if unknown:
        raise RecordError(f"unknown fields: {', '.join(sorted(unknown))}")
    try:
        return Record(
            time=data["time"],
            severity=data["severity"],
            message=data["message"],
            host=data.get("host"),
            app=data.get("app"),
        )
    except KeyError as missing:
        raise RecordError(f"missing field {missing.args[0]}") from None


def read_jsonl(lines):
    """Yields (line number, Record) for each non-blank line."""
    for number, line in enumerate(lines, start=1):
        if not line.strip():
            continue
        try:
            data = json.loads(line)
        except json.JSONDecodeError as error:
            raise RecordError(f"line {number}: {error.msg}") from None
        try:
            yield number, from_dict(data)
        except RecordError as error:
            raise RecordError(f"line {number}: {error}") from None
