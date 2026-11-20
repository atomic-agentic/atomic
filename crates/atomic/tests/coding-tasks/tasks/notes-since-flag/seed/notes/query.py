"""Choosing notes: the filters `list` and `export` share."""

from collections import Counter


def matches_tags(note, tags):
    """True when the note carries every tag, ignoring case."""
    return all(note.has_tag(tag) for tag in tags)


def matches_text(note, text):
    """True when the title or the body contains text, ignoring case."""
    wanted = text.casefold()
    return wanted in note.title.casefold() or wanted in note.body.casefold()


def filter_notes(notes, tags=(), text=None, limit=None):
    """The notes that pass every filter, oldest first, then by number, cut to limit."""
    chosen = []
    for note in notes:
        if tags and not matches_tags(note, tags):
            continue
        if text and not matches_text(note, text):
            continue
        chosen.append(note)
    chosen.sort(key=lambda n: (n.created_at, n.id))
    if limit is not None:
        chosen = chosen[:limit]
    return chosen


def tag_counts(notes):
    """(tag, count) pairs, most used first, then by name. Tags differing only in case count as one."""
    counts = Counter()
    names = {}
    for note in notes:
        for tag in {t.casefold(): t for t in note.tags}.values():
            key = tag.casefold()
            counts[key] += 1
            names.setdefault(key, tag)
    return sorted(((names[k], c) for k, c in counts.items()), key=lambda pair: (-pair[1], pair[0].casefold()))
