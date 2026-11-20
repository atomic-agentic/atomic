"""Reading log files: parse every line, drop duplicate writes, and put the events in order."""

from .events import BadLine, parse_line


class ConflictingEvents(ValueError):
    pass


def read_lines(path):
    """(where, text) for each non-blank line of a file."""
    try:
        with open(path, encoding="utf-8") as f:
            for number, text in enumerate(f, 1):
                if text.strip():
                    yield f"{path}:{number}", text
    except OSError as e:
        raise BadLine(f"cannot read {path}: {e.strerror}") from None


def parse_files(paths):
    """Every event in the files, as written."""
    return [parse_line(text, where) for path in paths for where, text in read_lines(path)]


def drop_duplicates(events):
    """The events without repeated writes. Two different events with one seq are an error."""
    by_seq = {}
    kept = []
    for event in events:
        seen = by_seq.get(event.seq)
        if seen is None:
            by_seq[event.seq] = event
            kept.append(event)
        elif seen != event:
            raise ConflictingEvents(f"conflicting events for seq {event.seq}")
    return kept


def in_order(events):
    """The events in the order they happened: by time, and by kind within a second, so that a replay
    of the same events always comes out the same."""
    return sorted(events, key=lambda event: (event.ts, event.kind))


def load(paths):
    """The events in the files, without duplicates, in order."""
    return in_order(drop_duplicates(parse_files(paths)))
