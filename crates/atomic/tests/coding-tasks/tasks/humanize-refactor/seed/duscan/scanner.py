"""Walking a directory tree into entries: one per file and per directory below the root."""

import os
import stat
from dataclasses import dataclass


@dataclass(frozen=True)
class Entry:
    path: str  # relative to the root, with / between parts
    size: int  # bytes for a file, 0 for a directory
    is_dir: bool

    @property
    def name(self):
        return self.path.rsplit("/", 1)[-1]

    @property
    def depth(self):
        return self.path.count("/") + 1

    @property
    def parent(self):
        return self.path.rsplit("/", 1)[0] if "/" in self.path else ""


def scan(root, include_hidden=True):
    """Every file and directory under root, sorted by path, parents first. Links count as 0 bytes."""
    root = os.path.abspath(root)
    entries = []
    for directory, dirs, files in os.walk(root):
        dirs.sort()
        if not include_hidden:
            dirs[:] = [d for d in dirs if not d.startswith(".")]
        relative = os.path.relpath(directory, root)
        relative = "" if relative == "." else relative.replace(os.sep, "/")
        if relative:
            entries.append(Entry(relative, 0, True))
        for name in sorted(files):
            if not include_hidden and name.startswith("."):
                continue
            try:
                info = os.lstat(os.path.join(directory, name))
            except OSError:
                continue
            size = 0 if stat.S_ISLNK(info.st_mode) else info.st_size
            entries.append(Entry(f"{relative}/{name}" if relative else name, size, False))
    entries.sort(key=lambda e: e.path.split("/"))
    return entries


def entries_from_spec(spec):
    """Entries from a mapping of file path to size, with the directories implied, as scan would give them."""
    entries = {}
    for path, size in spec.items():
        parts = path.split("/")
        for i in range(1, len(parts)):
            directory = "/".join(parts[:i])
            entries.setdefault(directory, Entry(directory, 0, True))
        entries[path] = Entry(path, int(size), False)
    return [entries[p] for p in sorted(entries, key=lambda p: p.split("/"))]


def directory_sizes(entries):
    """Total bytes of the files under each directory, with '' for the root."""
    totals = {"": 0}
    for entry in entries:
        if entry.is_dir:
            totals.setdefault(entry.path, 0)
            continue
        parent = entry.parent
        totals[""] += entry.size
        while parent:
            totals[parent] = totals.get(parent, 0) + entry.size
            parent = parent.rsplit("/", 1)[0] if "/" in parent else ""
    return totals
