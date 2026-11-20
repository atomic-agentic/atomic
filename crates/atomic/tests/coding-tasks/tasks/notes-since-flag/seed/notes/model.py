"""The note record, and the timestamps it carries."""

from dataclasses import dataclass, field
from datetime import datetime, timezone


class NoteError(ValueError):
    """A note that cannot be read."""


def parse_timestamp(text):
    """An aware datetime from an ISO 8601 timestamp with an offset, such as 2026-03-04T10:15:00Z."""
    if not isinstance(text, str):
        raise NoteError(f"bad timestamp: {text!r}")
    value = text.strip()
    if value.endswith(("Z", "z")):
        value = value[:-1] + "+00:00"
    try:
        moment = datetime.fromisoformat(value)
    except ValueError:
        raise NoteError(f"bad timestamp: {text!r}") from None
    if moment.tzinfo is None:
        raise NoteError(f"timestamp without an offset: {text!r}")
    return moment


def format_timestamp(moment):
    """The form `add` stores: UTC, to the second, with a Z."""
    return moment.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def utc_day(moment):
    """The calendar day of an aware datetime, in UTC."""
    return moment.astimezone(timezone.utc).date()


@dataclass
class Note:
    id: int
    title: str
    body: str = ""
    tags: list = field(default_factory=list)
    created: str = ""

    @property
    def created_at(self):
        return parse_timestamp(self.created)

    @property
    def created_day(self):
        return utc_day(self.created_at)

    def has_tag(self, tag):
        wanted = tag.casefold()
        return any(t.casefold() == wanted for t in self.tags)

    def to_dict(self):
        return {"id": self.id, "title": self.title, "body": self.body, "tags": list(self.tags), "created": self.created}

    @classmethod
    def from_dict(cls, data):
        if not isinstance(data, dict):
            raise NoteError(f"a note must be an object, not {type(data).__name__}")
        try:
            note_id = data["id"]
            title = data["title"]
            created = data["created"]
        except KeyError as missing:
            raise NoteError(f"a note has no {missing.args[0]}") from None
        if not isinstance(note_id, int) or isinstance(note_id, bool) or note_id < 1:
            raise NoteError(f"bad note id: {note_id!r}")
        if not isinstance(title, str) or not title.strip():
            raise NoteError(f"note {note_id} has no title")
        tags = data.get("tags", [])
        if not isinstance(tags, list) or not all(isinstance(t, str) for t in tags):
            raise NoteError(f"note {note_id} has bad tags")
        parse_timestamp(created)
        return cls(id=note_id, title=title, body=str(data.get("body", "")), tags=tags, created=created)
